use std::ops::Range;

use crate::text::Text;
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, TextEdit};

#[derive(Clone, Debug)]
pub struct Argument {
    pub value: String,
    pub span: Range<usize>,
    /// Includes delimiters when present; equals `span` for bare arguments.
    pub outer_span: Range<usize>,
}

/// pdfTeX's primitive filename scanner cannot read UTF-8 active characters.
/// Braced LaTeX input uses the filename expansion path instead.
pub(crate) fn input_filename_needs_braces(source: &str, argument: &Argument, value: &str) -> bool {
    (argument.span == argument.outer_span && value.chars().any(char::is_whitespace))
        || (!value.is_ascii() && source.as_bytes().get(argument.outer_span.start) != Some(&b'{'))
}

pub(crate) fn input_filename_edit(text: &Text, argument: &Argument, value: String) -> TextEdit {
    let grouped = input_filename_needs_braces(&text.source, argument, &value);
    let mut span = if grouped {
        argument.outer_span.clone()
    } else {
        argument.span.clone()
    };
    let mut new_text = if grouped {
        format!("{{{value}}}")
    } else {
        value
    };
    if grouped {
        // The primitive filename scanner consumes the terminating space and
        // leaves TeX ignoring the rest of that line's blanks. A braced input
        // would typeset them. Keep line endings, suppressing their space with
        // a comment, or include horizontal delimiter blanks in the edit.
        let blanks = text.source[span.end..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        if matches!(
            text.source.as_bytes().get(span.end + blanks),
            Some(b'\r' | b'\n')
        ) {
            new_text.push('%');
        } else {
            span.end += blanks;
        }
    }
    TextEdit {
        range: text.range(span.start, span.end),
        new_text,
    }
}

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub span: Range<usize>,
    pub argument: Option<Argument>,
}

/// A literal `\csname name\endcsname`; names involving expansion, comments,
/// or embedded whitespace remain unresolved. Inspect only the next command
/// so malformed input cannot cause repeated scans of the remaining document.
pub(crate) fn literal_csname(
    source: &str,
    commands: &[Command],
    index: usize,
) -> Option<Range<usize>> {
    let command = commands
        .get(index)
        .filter(|command| command.name == "csname")?;
    let end = commands
        .get(index + 1)
        .filter(|command| command.name == "endcsname")?;
    // Spaces after the control word delimit it and are discarded by TeX.
    // Subsequent spaces, including those before endcsname, name a different
    // command and must not be folded into an ordinary control word.
    let span = skip_space(source, command.span.end).min(end.span.start)..end.span.start;
    is_static_key(&source[span.clone()]).then_some(span)
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub span: Range<usize>,
    pub detail: String,
    pub is_variable: bool,
}

#[derive(Clone, Debug)]
pub struct CitationField {
    pub name: String,
    pub argument: Argument,
}

#[derive(Clone, Debug, Default)]
pub struct Syntax {
    pub commands: Vec<Command>,
    /// Unexpanded declaration/copy/inspection and known macro operands remain references,
    /// but do not read arguments or perform the named command's operations.
    /// Also retain control symbols and unfinished backslashes for completion;
    /// these positions need not correspond to an indexed command/definition.
    pub control_operands: std::collections::BTreeSet<usize>,
    /// Literal braces and math shifts consumed by direct token inspection or
    /// copying, rather than acting as grouping or formula delimiters.
    pub(crate) punctuation_operands: std::collections::BTreeSet<usize>,
    pub labels: Vec<Entry>,
    pub anchors: Vec<Entry>,
    pub anchor_references: Vec<Entry>,
    pub citations: Vec<Entry>,
    /// Static IDS secondary keys do not declare independent database entries.
    pub(crate) bibliography_aliases: Vec<Entry>,
    pub(crate) bibliography_alias_values: Vec<Argument>,
    pub citation_fields: Vec<CitationField>,
    /// Complete BibTeX constructs, including special entries. Reuse the same
    /// quote/brace/header boundaries for folding as for citation indexing.
    pub(crate) bibliography_entries: Vec<(Range<usize>, bool)>,
    pub(crate) bibliography_links: Vec<crate::bibliography::bib_links::Link>,
    pub(crate) bibliography_completion: crate::bibliography::bib_completion::Index,
    pub(crate) bibliography_strings: crate::bibliography::bib_strings::Index,
    pub citation_references: Vec<Entry>,
    pub definitions: Vec<Entry>,
    pub environments: Vec<Entry>,
    pub counters: Vec<Entry>,
    pub conditionals: Vec<Entry>,
    pub toggles: Vec<Entry>,
    pub saved_text: Vec<Entry>,
    pub saved_text_references: Vec<Entry>,
    pub moving_text: Vec<Entry>,
    pub moving_text_references: Vec<Entry>,
    pub colors: Vec<Entry>,
    pub color_references: Vec<Entry>,
    /// Command identities used while expanding otherwise opaque name arguments.
    pub name_command_references: Vec<Entry>,
    /// Static counter operands expanded inside otherwise opaque name arguments.
    pub name_counter_references: Vec<Entry>,
    pub diagnostics: Vec<Diagnostic>,
    pub ignored: Vec<Range<usize>>,
    /// Literal conversion data stays available to filename completion, but
    /// should not offer executable command templates within the quoted text.
    pub literal_conversions: Vec<Range<usize>>,
    /// Unexpanded string data whose owning predicate still offers a signature.
    pub literal_tests: Vec<Range<usize>>,
    /// Captured data arity at a direct string call; None retains mixed meanings.
    pub(crate) string_test_arities: std::collections::BTreeMap<usize, Option<usize>>,
    /// Data boundaries that differ between project/load contexts.
    pub(crate) ambiguous_string_tests: Vec<Range<usize>>,
    /// Whether this syntax was rebuilt with project-specific string semantics.
    pub(crate) contextual_string_tests: bool,
    /// Native predicate tokens consumed as operands require context on refresh.
    pub(crate) contextual_string_operands: bool,
    /// Exact imported/custom parameter grammar when all execution contexts agree.
    pub(crate) contextual_command_shapes:
        std::collections::BTreeMap<usize, std::sync::Arc<crate::symbols::commands::CommandShape>>,
    /// Known undefinition calls; a missing static target keeps effects uncertain.
    pub(crate) command_undefinitions: std::collections::BTreeMap<usize, Option<Entry>>,
    /// Actual comments, distinguished from ignored URLs and verbatim text.
    pub comments: Vec<Range<usize>>,
    /// Resource lists read with biblatex file-verb catcodes, before tokenization.
    pub(crate) raw_resource_brackets: std::collections::BTreeSet<usize>,
    pub(crate) contextual_resource_readers: crate::project::resource_context::Readers,
    pub fixes: Vec<QuickFix>,
    /// Ordinary environment-stack fixes, replaceable by project-aware analysis.
    /// Verbatim closing-marker errors remain in the general diagnostics.
    pub environment_fixes: Vec<QuickFix>,
}

#[derive(Clone, Debug)]
pub struct QuickFix {
    pub title: String,
    pub diagnostic: Diagnostic,
    pub edit: TextEdit,
    pub additional_edits: Vec<TextEdit>,
}

impl QuickFix {
    pub fn edits(&self) -> Vec<TextEdit> {
        std::iter::once(&self.edit)
            .chain(&self.additional_edits)
            .cloned()
            .collect()
    }
}

pub fn diagnostic(text: &Text, span: Range<usize>, message: String) -> Diagnostic {
    Diagnostic {
        range: text.range(span.start, span.end),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("texquill".into()),
        message,
        ..Diagnostic::default()
    }
}

pub(crate) fn skip_space(source: &str, mut i: usize) -> usize {
    let bytes = source.as_bytes();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'%') {
            return i;
        }
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
    }
}

/// Skip horizontal space and comment continuations while ordinary line
/// endings remain active tokens during verbatim header lookahead.
pub(crate) fn skip_space_with_active_eol(source: &str, mut at: usize) -> usize {
    loop {
        while matches!(source.as_bytes().get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        if source.as_bytes().get(at) != Some(&b'%') {
            return at;
        }
        let Some(newline) = source[at..].find('\n') else {
            return source.len();
        };
        at += newline + 1;
    }
}

/// Read a literal TeX integer without expanding arbitrary controls. The caller
/// identifies controls that prove the end of a numeric token sequence.
pub(crate) fn literal_integer(
    source: &str,
    after: usize,
    terminates: impl Fn(&str) -> bool,
) -> Option<(i32, usize)> {
    let bytes = source.as_bytes();
    let mut at = skip_space(source, after);
    if bytes.get(at) == Some(&b'=') {
        at = skip_space(source, at + 1);
    }
    let mut negative = false;
    while matches!(bytes.get(at), Some(b'+' | b'-')) {
        negative ^= bytes[at] == b'-';
        at = skip_space(source, at + 1);
    }
    if bytes.get(at) == Some(&b'`') {
        at += 1;
        let token = token(source, at)?;
        let literal = token.strip_prefix('\\').unwrap_or(token);
        if literal.len() != 1 || !literal.is_ascii() {
            return None;
        }
        let value = i32::from(literal.as_bytes()[0]);
        return Some((if negative { -value } else { value }, at + token.len()));
    }
    let radix = match bytes.get(at) {
        Some(b'\'') => {
            at += 1;
            8
        }
        Some(b'"') => {
            at += 1;
            16
        }
        _ => 10,
    };
    let mut value = 0i32;
    let mut digits = 0;
    loop {
        if bytes.get(at) == Some(&b'%') {
            at = skip_space(source, at);
        }
        let digit = match bytes.get(at) {
            Some(b'0'..=b'9') => u32::from(bytes[at] - b'0'),
            Some(b'A'..=b'F') if radix == 16 => u32::from(bytes[at] - b'A') + 10,
            _ => break,
        };
        if digit >= radix {
            break;
        }
        value = value.checked_mul(radix as i32)?.checked_add(digit as i32)?;
        digits += 1;
        at += 1;
    }
    if digits == 0 {
        return None;
    }
    // A macro following the digits can expand to more digits. Whitespace,
    // ordinary characters and these known primitive boundaries end the number.
    if bytes.get(at) == Some(&b'\\') {
        let span = control_word(source, at)?;
        let name = &source[span.start + 1..span.end];
        if !terminates(name) {
            return None;
        }
    }
    Some((if negative { -value } else { value }, at))
}

/// Read a balanced group, ignoring comments and escaped delimiters.
pub(crate) fn group(source: &str, i: usize, open: u8, close: u8) -> Option<(Argument, usize)> {
    group_mode(source, i, open, close, true, true)
}

pub(crate) fn verbatim_file_group(source: &str, at: usize) -> Option<(Argument, usize)> {
    group_mode(source, at, b'{', b'}', false, true)
}

/// TeX optional notes end at the first unbraced closing bracket. An opening
/// bracket inside a note is ordinary text, not another grouping delimiter.
pub(crate) fn optional_note_end(source: &str, at: usize) -> Option<usize> {
    bracket_argument(source, at).map(|(_, end)| end)
}

pub(crate) fn bracket_argument(source: &str, at: usize) -> Option<(Argument, usize)> {
    group_mode(source, at, b'[', b']', true, false)
}

pub(crate) fn verbatim_file_bracket(source: &str, at: usize) -> Option<(Argument, usize)> {
    group_mode(source, at, b'[', b']', false, false)
}

pub(crate) fn parenthesis_note(source: &str, at: usize) -> Option<(Argument, usize)> {
    group_mode(source, at, b'(', b')', true, false)
}

fn group_mode(
    source: &str,
    i: usize,
    open: u8,
    close: u8,
    comments: bool,
    nested_delimiters: bool,
) -> Option<(Argument, usize)> {
    if source.as_bytes().get(i) != Some(&open) {
        return None;
    }
    let Some(context) = crate::analysis::AnalysisContext::current() else {
        return uncached_group_mode(source, i, open, close, comments, nested_delimiters);
    };
    let key = super::group_cache::Key::new(source, i, open, close, comments, nested_delimiters);
    if let Some(result) = context.groups.borrow().get(key, source) {
        return Some(result);
    }
    let result = uncached_group_mode(source, i, open, close, comments, nested_delimiters)?;
    context.groups.borrow_mut().insert(key, &result);
    Some(result)
}

fn uncached_group_mode(
    source: &str,
    i: usize,
    open: u8,
    close: u8,
    comments: bool,
    nested_delimiters: bool,
) -> Option<(Argument, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(i) != Some(&open) {
        return None;
    }
    let start = i + 1;
    let mut i = start;
    let mut depth = 1;
    let mut braces = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += source[i..].chars().next()?.len_utf8();
                }
                continue;
            }
            b'%' if comments => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'{' if open != b'{' => braces += 1,
            b'}' if open != b'{' => braces = braces.saturating_sub(1),
            c if c == open && braces == 0 && nested_delimiters => depth += 1,
            c if c == close && braces == 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some((
                        Argument {
                            value: source[start..i].into(),
                            span: start..i,
                            outer_span: start - 1..i + 1,
                        },
                        i + 1,
                    ));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Literal TeX filename forms: `\input chapter.tex` and `\input "my file"`.
/// Stop at token boundaries and leave macro-expanded names to the recorder.
fn input_argument(source: &str, start: usize) -> Option<(Argument, usize)> {
    let bytes = source.as_bytes();
    let quoted = bytes.get(start) == Some(&b'"');
    let content = start + usize::from(quoted);
    let mut end = content;
    while end < bytes.len() {
        let byte = bytes[end];
        if quoted && byte == b'"'
            || matches!(byte, b'\n' | b'\r')
            || !quoted && byte.is_ascii_whitespace()
        {
            break;
        }
        if matches!(
            byte,
            b'\\' | b'{' | b'}' | b'%' | b'#' | b'$' | b'&' | b'^' | b'~'
        ) || !quoted && byte == b'"'
        {
            return None;
        }
        end += 1;
    }
    let after = end + usize::from(quoted && bytes.get(end) == Some(&b'"'));
    if quoted
        && bytes
            .get(after)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        return None;
    }
    Some((
        Argument {
            value: source[content..end].into(),
            span: content..end,
            outer_span: start..after,
        },
        after,
    ))
}

pub(crate) fn source_input_argument(source: &str, name: &str, end: usize) -> Option<Argument> {
    let start = skip_space(source, end);
    group(source, start, b'{', b'}')
        .or_else(|| {
            (name == "input" && source.as_bytes().get(start) != Some(&b'{'))
                .then(|| input_argument(source, start))
                .flatten()
        })
        .map(|(mut argument, _)| {
            if let Some(span) = literal_file_span(source, argument.span.clone(), false) {
                argument.value = source[span.clone()].into();
                argument.span = span;
            }
            argument
        })
}

pub fn is_reference(name: &str) -> bool {
    warns_on_undefined_reference(name)
        || matches!(
            name,
            "getrefnumber"
                | "getpagerefnumber"
                | "getrefbykeydefault"
                | "IfRefUndefinedExpandable"
                | "IfRefUndefinedBabel"
        )
}

pub(crate) fn warns_on_undefined_reference(name: &str) -> bool {
    matches!(
        name,
        "ref"
            | "subref"
            | "Ref"
            | "eqref"
            | "pageref"
            | "autopageref"
            | "autoref"
            | "cref"
            | "Cref"
            | "cpageref"
            | "Cpageref"
            | "crefrange"
            | "Crefrange"
            | "cpagerefrange"
            | "Cpagerefrange"
            | "vref"
            | "Vref"
            | "vpageref"
            | "fullref"
            | "vrefrange"
            | "vpagerefrange"
            | "nameref"
            | "refused"
    )
}

pub(crate) fn is_single_reference(name: &str) -> bool {
    is_reference(name) && !matches!(name, "cref" | "Cref" | "cpageref" | "Cpageref")
}

pub(crate) fn reference_accepts_star(name: &str) -> bool {
    matches!(
        name,
        "ref"
            | "subref"
            | "Ref"
            | "pageref"
            | "autopageref"
            | "autoref"
            | "nameref"
            | "vref"
            | "Vref"
            | "vpageref"
            | "vpagerefrange"
            | "cref"
            | "Cref"
            | "cpageref"
            | "Cpageref"
            | "crefrange"
            | "Crefrange"
            | "cpagerefrange"
            | "Cpagerefrange"
    )
}

pub(crate) fn reference_note_count(name: &str) -> usize {
    match name {
        "vref" | "Vref" | "vrefrange" | "vpagerefrange" => 1,
        "vpageref" => 2,
        _ => 0,
    }
}

/// Locate a reference's label without treating unsupported flags or extra
/// optional notes as part of a call. Varioref's notes precede its label keys.
pub(crate) fn reference_argument_start(
    source: &str,
    name: &str,
    command_end: usize,
) -> Option<usize> {
    let mut at = skip_space(source, command_end);
    if source.as_bytes().get(at) == Some(&b'*') {
        if !reference_accepts_star(name) {
            return None;
        }
        at = skip_space(source, at + 1);
    }
    for _ in 0..reference_note_count(name) {
        let Some(end) = optional_note_end(source, at) else {
            break;
        };
        at = skip_space(source, end);
    }
    Some(at)
}

pub(crate) fn is_range_reference(name: &str) -> bool {
    matches!(
        name,
        "vrefrange"
            | "vpagerefrange"
            | "crefrange"
            | "Crefrange"
            | "cpagerefrange"
            | "Cpagerefrange"
    )
}

/// Range endpoints have separate names; never treat their optional note as a
/// label or split either endpoint as a comma-separated list.
pub(crate) fn range_reference_end_start(source: &str, command: &Command) -> Option<usize> {
    if !is_range_reference(&command.name) {
        return None;
    }
    Some(skip_space(
        source,
        command.argument.as_ref()?.outer_span.end,
    ))
}

pub(crate) fn range_reference_end(source: &str, command: &Command) -> Option<Argument> {
    group(
        source,
        range_reference_end_start(source, command)?,
        b'{',
        b'}',
    )
    .map(|(argument, _)| single_reference_argument(source, argument))
}

pub(crate) fn is_counter_reference(name: &str) -> bool {
    matches!(
        name,
        "setcounterref" | "addtocounterref" | "setcounterpageref" | "addtocounterpageref"
    )
}

pub(crate) fn counter_reference_label_start(source: &str, command: &Command) -> Option<usize> {
    if !is_counter_reference(&command.name) {
        return None;
    }
    let (_, end) = group(source, skip_space(source, command.span.end), b'{', b'}')?;
    Some(skip_space(source, end))
}

pub(crate) fn counter_reference_label(source: &str, command: &Command) -> Option<Argument> {
    group(
        source,
        counter_reference_label_start(source, command)?,
        b'{',
        b'}',
    )
    .map(|(argument, _)| single_reference_argument(source, argument))
}

/// These commands keep their label outside the ordinary first required argument.
pub(crate) fn has_separate_label_argument(name: &str) -> bool {
    is_counter_reference(name) || name == "hyperref"
}

pub(crate) fn separate_label_argument_start(
    source: &str,
    command: &Command,
) -> Option<(usize, u8)> {
    if matches!(command.name.as_str(), "hyperref" | "hyperdef") {
        let start = skip_space(source, command.span.end);
        if source.as_bytes().get(start) != Some(&b'[') {
            return None;
        }
        let inner_start = skip_comments(source, start + 1);
        if source.as_bytes().get(inner_start) == Some(&b'{') {
            if group(source, inner_start, b'{', b'}').is_some_and(|(_, end)| {
                let end = skip_comments(source, end);
                end < source.len() && source.as_bytes().get(end) != Some(&b']')
            }) {
                return None;
            }
            Some((inner_start, b'{'))
        } else {
            Some((start, b'['))
        }
    } else {
        counter_reference_label_start(source, command).map(|start| (start, b'{'))
    }
}

pub(crate) fn separate_label_argument(source: &str, command: &Command) -> Option<Argument> {
    if !matches!(command.name.as_str(), "hyperref" | "hyperdef") {
        return counter_reference_label(source, command);
    }
    let outer_start = skip_space(source, command.span.end);
    let (outer, _) = bracket_argument(source, outer_start)?;
    if let Some((mut inner, end)) =
        group(source, skip_comments(source, outer.span.start), b'{', b'}')
    {
        if skip_comments(source, end) == outer.span.end {
            inner.outer_span = outer.outer_span;
            return Some(single_reference_argument(source, inner));
        }
    }
    Some(single_reference_argument(source, outer))
}

/// Drop only comments: literal spaces around a protected label remain tokens.
fn skip_comments(source: &str, mut at: usize) -> usize {
    let bytes = source.as_bytes();
    while bytes.get(at) == Some(&b'%') {
        while at < bytes.len() && bytes[at] != b'\n' {
            at += 1;
        }
        if at < bytes.len() {
            at += 1;
        }
    }
    at
}

/// Comments have no tokens, whereas literal spaces belong to a single key's
/// name. Keep only a contiguous name bounded by comments, without trimming it.
pub(crate) fn single_key_argument(source: &str, argument: Argument) -> Argument {
    literal_key_argument(source, argument, is_static_key)
}

pub(crate) fn single_reference_argument(source: &str, argument: Argument) -> Argument {
    literal_key_argument(source, argument, is_static_reference_key)
}

pub(crate) fn single_citation_argument(source: &str, argument: Argument) -> Argument {
    literal_key_argument(source, argument, is_static_citation_key)
}

fn literal_key_argument(source: &str, mut argument: Argument, valid: fn(&str) -> bool) -> Argument {
    if !argument.value.contains('%') {
        return argument;
    }
    let pieces = name_literal_pieces(source, argument.span.clone());
    let mut names = pieces.into_iter().filter(|span| !span.is_empty());
    if let Some(span) = names.next().filter(|span| valid(&source[span.clone()])) {
        if names.next().is_none() {
            argument.value = source[span.clone()].into();
            argument.span = span;
        }
    }
    argument
}

/// A begin/end name is a required TeX argument: either a group or one token.
/// Only ASCII literal tokens have the same ungrouped boundary across engines.
pub(crate) fn environment_name_argument(source: &str, end: usize) -> Option<Argument> {
    let at = skip_space(source, end);
    let argument = if source.as_bytes().get(at) == Some(&b'{') {
        single_key_argument(source, group(source, at, b'{', b'}')?.0)
    } else {
        let byte = *source.as_bytes().get(at)?;
        if !byte.is_ascii() || byte.is_ascii_control() {
            return None;
        }
        let span = at..at + 1;
        Argument {
            value: source[span.clone()].into(),
            outer_span: span.clone(),
            span,
        }
    };
    is_static_key(&argument.value).then_some(argument)
}

/// Literal runs separated by TeX comments, preserving each comment's newline.
pub(crate) fn name_literal_pieces(source: &str, span: Range<usize>) -> Vec<Range<usize>> {
    let bytes = source.as_bytes();
    let mut pieces = Vec::new();
    let mut start = span.start;
    let mut at = start;
    while at < span.end {
        match bytes[at] {
            b'\\' => {
                at += 1;
                if at < span.end {
                    at += source[at..].chars().next().unwrap().len_utf8();
                }
                continue;
            }
            b'%' => {
                pieces.push(start..at);
                while at < span.end && bytes[at] != b'\n' {
                    at += 1;
                }
                if at < span.end {
                    at += 1;
                }
                start = at;
                continue;
            }
            _ => at += 1,
        }
    }
    pieces.push(start..span.end);
    pieces
}

pub(crate) fn label_name_text(source: &str, command: &Command, name: &str) -> String {
    if matches!(command.name.as_str(), "hyperref" | "hyperdef")
        && name.contains(['[', ']'])
        && separate_label_argument_start(source, command).is_some_and(|(_, open)| open == b'[')
    {
        format!("{{{name}}}")
    } else {
        name.into()
    }
}

pub(crate) fn label_replacement_text(
    text: &Text,
    commands: &[Command],
    range: lsp_types::Range,
    name: &str,
) -> String {
    if !name.contains(['[', ']']) {
        return name.into();
    }
    commands
        .iter()
        .find(|command| {
            matches!(command.name.as_str(), "hyperref" | "hyperdef")
                && separate_label_argument(&text.source, command).is_some_and(|argument| {
                    text.range(argument.span.start, argument.span.end) == range
                })
        })
        .map_or_else(
            || name.into(),
            |command| label_name_text(&text.source, command, name),
        )
}

pub(crate) fn is_static_key(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(|ch| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '\\' | '{'
                        | '}'
                        | ','
                        | '%'
                        | '#'
                        | '$'
                        | '&'
                        | '^'
                        | '~'
                        | '"'
                        | '('
                        | ')'
                        | '='
                )
        })
}

/// Parentheses are ordinary tokens in literal reference keys and PDF destinations.
/// General identifiers retain their stricter namespace rules.
pub(crate) fn is_static_reference_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .split(['(', ')'])
            .filter(|part| !part.is_empty())
            .all(is_static_key)
}

/// Split a TeX key list without interpreting commas inside comments, groups,
/// or control symbols. Ranges always refer to the original document.
pub(crate) fn key_list_segments(source: &str, span: Range<usize>) -> Vec<Range<usize>> {
    key_list_segments_mode(source, span, true)
}

pub(crate) fn verbatim_list_segments(source: &str, span: Range<usize>) -> Vec<Range<usize>> {
    key_list_segments_mode(source, span, false)
}

fn key_list_segments_mode(source: &str, span: Range<usize>, comments: bool) -> Vec<Range<usize>> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut start = span.start;
    let mut at = start;
    let mut depth = 0usize;
    while at < span.end {
        match bytes[at] {
            b'\\' => {
                at += 1;
                if at < span.end {
                    at += source[at..].chars().next().unwrap().len_utf8();
                }
                continue;
            }
            b'%' if comments => {
                while at < span.end && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                result.push(start..at);
                start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    result.push(start..span.end);
    result
}

fn key_list_name(
    source: &str,
    segment: Range<usize>,
    valid: fn(&str) -> bool,
) -> Option<Range<usize>> {
    let bounded = &source[..segment.end];
    let start = skip_space(bounded, segment.start);
    let end = source[start..segment.end]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace() || *ch == '%')
        .map_or(segment.end, |(offset, _)| start + offset);
    (skip_space(bounded, end) == segment.end && valid(&source[start..end])).then_some(start..end)
}

/// Only contiguous literal names can be edited. Comments surrounding a key
/// stay outside its range; macro expansion and comment-spliced names do not
/// become partial references.
pub(crate) fn key_list_spans(source: &str, span: Range<usize>) -> Vec<Range<usize>> {
    key_list_segments(source, span)
        .into_iter()
        .filter_map(|segment| key_list_name(source, segment, is_static_key))
        .collect()
}

pub(crate) fn reference_list_spans(source: &str, span: Range<usize>) -> Vec<Range<usize>> {
    key_list_segments(source, span)
        .into_iter()
        .filter_map(|segment| key_list_name(source, segment, is_static_reference_key))
        .collect()
}

pub(crate) fn citation_list_spans(source: &str, span: Range<usize>) -> Vec<Range<usize>> {
    key_list_segments(source, span)
        .into_iter()
        .filter_map(|segment| key_list_name(source, segment, is_static_citation_key))
        .collect()
}

pub(crate) fn literal_file_span(
    source: &str,
    segment: Range<usize>,
    compact: bool,
) -> Option<Range<usize>> {
    let mut pieces = name_literal_pieces(source, segment)
        .into_iter()
        .map(|span| trimmed_span(source, span))
        .filter(|span| !span.is_empty());
    let span = pieces.next()?;
    if pieces.next().is_some() {
        return None;
    }
    // Retain the literal escapes already understood by filename lookup. Use
    // harmless placeholders only for validation: a decoded percent must not
    // become a source comment, and arbitrary control sequences remain invalid.
    let mut literal = if compact {
        loader_name(&source[span.clone()])
    } else {
        source[span.clone()].to_owned()
    };
    for character in ['_', '%', '#', '&', '$'] {
        literal = literal.replace(&format!("\\{character}"), "_");
    }
    is_literal_file_path(&literal).then_some(span)
}

/// BibTeX rejects spaces written to `bibstyle`. Only indentation after a
/// comment is ignored by TeX: trimming ordinary leading/trailing whitespace
/// would create a link to a style the compiler cannot use. Return an empty
/// span for an empty argument so completion can insert a name there.
fn bibliography_style_span(source: &str, span: Range<usize>) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    let mut name: Option<Range<usize>> = None;
    let mut new_line = false;
    let mut continuous = true;
    let mut at = span.start;
    while at < span.end {
        match bytes[at] {
            b'%' => {
                while at < span.end && bytes[at] != b'\n' {
                    at += 1;
                }
                if at < span.end {
                    at += 1;
                }
                new_line = true;
                continuous = false;
            }
            b' ' | b'\t' if new_line => at += 1,
            b' ' | b'\t' | b'\r' | b'\n' => return None,
            _ => {
                if name.is_some() && !continuous {
                    return None;
                }
                let end = at + source[at..].chars().next()?.len_utf8();
                name.get_or_insert(at..end).end = end;
                new_line = false;
                continuous = true;
                at = end;
            }
        }
    }
    let name = name.unwrap_or(span.start..span.start);
    (name.is_empty() || is_literal_file_path(&source[name.clone()])).then_some(name)
}

pub(crate) fn active_bibliography_style_prefix(
    source: &str,
    start: usize,
    cursor: usize,
) -> Option<Range<usize>> {
    if source.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let span = group(source, start, b'{', b'}')
        .map_or(start + 1..source.len(), |(argument, _)| argument.span);
    if cursor < span.start || cursor > span.end {
        return None;
    }
    let name = bibliography_style_span(source, span)?;
    if name.is_empty() {
        Some(cursor..cursor)
    } else {
        (name.start <= cursor && cursor <= name.end).then_some(name.start..cursor)
    }
}

/// Package and class loaders accept one optional options argument; bibliography
/// and chapter-selection lists start directly with their mandatory group.
pub(crate) fn file_name_argument_start(
    source: &str,
    name: &str,
    command_end: usize,
) -> Option<usize> {
    if crate::bibliography::bib_resources::references_file(name) {
        return crate::bibliography::bib_resources::start(source, command_end);
    }
    if name == "inputminted" {
        return crate::parser::minted::language(source, command_end)
            .map(|argument| skip_space(source, argument.outer_span.end));
    }
    let mut start = skip_space(source, command_end);
    if source.as_bytes().get(start) == Some(&b'*') {
        if name != "includegraphics"
            && name != "verbatiminput"
            && !crate::parser::fancyvrb::is_input(name)
            && !crate::parser::fancyvrb::is_use(name)
            && !crate::parser::fancyvrb::is_inline(name)
        {
            return None;
        }
        start = skip_space(source, start + 1);
    }
    if name == "includegraphics" {
        for _ in 0..2 {
            if source.as_bytes().get(start) == Some(&b'[') {
                start = skip_space(source, optional_note_end(source, start)?);
            }
        }
        return Some(start);
    }
    if (loader_extension(name).is_some()
        || name == "lstinputlisting"
        || crate::parser::fancyvrb::is_input(name)
        || crate::parser::fancyvrb::is_use(name)
        || crate::parser::fancyvrb::is_inline(name))
        && !loader_forwards_options(name)
        && source.as_bytes().get(start) == Some(&b'[')
    {
        start = skip_space(source, optional_note_end(source, start)?);
    }
    Some(start)
}

fn file_list_argument<'a>(source: &str, command: &'a Command) -> Option<&'a Argument> {
    if !file_list(&command.name) {
        return None;
    }
    let start = file_name_argument_start(source, &command.name, command.span.end)?;
    command
        .argument
        .as_ref()
        .filter(|argument| argument.outer_span.start == start)
}

/// Keep comments and their commas outside each editable name. Bibliography
/// and package names discard TeX spaces, while chapter names preserve them.
/// Do not infer macro expansion or join separate pieces across a comment.
pub(crate) fn file_list_spans(source: &str, command: &Command) -> Option<Vec<Range<usize>>> {
    if crate::bibliography::bib_section_files::file_command(&command.name) {
        return command
            .argument
            .as_ref()
            .map(|argument| vec![argument.span.clone()]);
    }

    if !file_list(&command.name) {
        return None;
    }
    let Some(argument) = file_list_argument(source, command) else {
        return Some(vec![]);
    };
    Some(
        key_list_segments(source, argument.span.clone())
            .into_iter()
            .filter_map(|segment| {
                literal_file_span(source, segment, file_name_ignores_spaces(&command.name))
            })
            .collect(),
    )
}

/// Compiler errors and chapter filters require every item to be static. A
/// dynamic sibling could produce the reported filename or change selection.
/// Leave TeX escape expansion unresolved in these stricter consumers.
pub(crate) fn complete_file_list_spans(
    source: &str,
    command: &Command,
) -> Option<Vec<Range<usize>>> {
    if crate::bibliography::bib_section_files::file_command(&command.name) {
        return command
            .argument
            .as_ref()
            .map(|argument| vec![argument.span.clone()]);
    }

    let argument = file_list_argument(source, command)?;
    let compact = file_name_ignores_spaces(&command.name);
    key_list_segments(source, argument.span.clone())
        .into_iter()
        .filter(|segment| {
            name_literal_pieces(source, segment.clone())
                .iter()
                .any(|span| !source[span.clone()].trim().is_empty())
        })
        .map(|segment| {
            let span = literal_file_span(source, segment, compact)?;
            let raw = &source[span.clone()];
            let literal = if compact {
                loader_name(raw)
            } else {
                raw.into()
            };
            is_literal_file_path(&literal).then_some(span)
        })
        .collect()
}

pub(crate) fn includeonly_names(source: &str, command: &Command) -> Option<Vec<String>> {
    if command.name != "includeonly" {
        return None;
    }
    Some(
        complete_file_list_spans(source, command)?
            .into_iter()
            .map(|span| {
                let name = &source[span];
                name.strip_suffix(".tex").unwrap_or(name).to_owned()
            })
            .collect(),
    )
}

pub(crate) fn active_file_prefix(
    source: &str,
    start: usize,
    cursor: usize,
    compact: bool,
    multiple: bool,
) -> Option<Range<usize>> {
    if source.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let span = group(source, start, b'{', b'}')
        .map_or(start + 1..source.len(), |(argument, _)| argument.span);
    let segments = if multiple {
        key_list_segments(source, span)
    } else {
        vec![span]
    };
    let segment = segments
        .into_iter()
        .find(|segment| segment.start <= cursor && cursor <= segment.end)?;
    if let Some(span) = literal_file_span(source, segment.clone(), compact) {
        // The path completer does not decode TeX escapes. Replacing only the
        // prefix before an escape would leave a duplicate filename suffix.
        if source[span.clone()].contains('\\') {
            return None;
        }
        return (span.start <= cursor && cursor <= span.end).then_some(span.start..cursor);
    }
    name_literal_pieces(source, segment)
        .iter()
        .all(|span| source[span.clone()].trim().is_empty())
        .then_some(cursor..cursor)
}

pub(crate) fn active_key_list_prefix(
    source: &str,
    start: usize,
    cursor: usize,
    valid: fn(&str) -> bool,
) -> Option<Range<usize>> {
    if source.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let span = group(source, start, b'{', b'}')
        .map_or(start + 1..source.len(), |(argument, _)| argument.span);
    let segment = key_list_segments(source, span)
        .into_iter()
        .find(|segment| segment.start <= cursor && cursor <= segment.end)?;
    if let Some(name) = key_list_name(source, segment.clone(), valid) {
        return (name.start <= cursor && cursor <= name.end).then_some(name.start..cursor);
    }
    (skip_space(&source[..segment.end], segment.start) == segment.end).then_some(cursor..cursor)
}

/// Files displayed as literal code, rather than executed as TeX source.
pub(crate) fn code_input_command(name: &str) -> bool {
    matches!(name, "lstinputlisting" | "verbatiminput" | "inputminted")
        || crate::parser::fancyvrb::is_input(name)
}

pub fn file_extensions(name: &str) -> Option<&'static [&'static str]> {
    if name == crate::bibliography::bib_section_files::DEPENDENCY {
        return None;
    }
    Some(match name {
        "input" | "InputIfFileExists" | "loadglsentries" => &[
            "tex", "ltx", "sty", "cls", "def", "clo", "fd", "ldf", "bbx", "cbx", "lbx", "dbx",
        ],
        "IfFileExists" => &["tex"],
        // Empty suffix means an exact arbitrary file, never an appended dot.
        name if code_input_command(name) => &[""],
        "include" | "includeonly" | "subfile" => &["tex"],
        "bibliography" | "nobibliography" => &["bib"],
        name if crate::bibliography::bib_resources::references_file(name) => &["bib"],
        "bibliographystyle" => &["bst"],
        "usepackage" | "RequirePackage" | "RequirePackageWithOptions" => &["sty"],
        "documentclass" | "LoadClass" | "LoadClassWithOptions" => &["cls"],
        "PassOptionsToPackage" => &["sty"],
        "PassOptionsToClass" => &["cls"],
        "includegraphics" => &["pdf", "png", "jpg", "jpeg", "eps", "svg"],
        _ => return None,
    })
}

/// A literal filename that can be replaced without introducing TeX tokens.
pub(crate) fn is_literal_file_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && !value.starts_with(' ')
        && !value.ends_with(' ')
        && !value.contains("  ")
        && !value.chars().any(|ch| ch.is_whitespace() && ch != ' ')
        && !value.contains([
            '\\', '{', '}', '%', '#', '$', '&', '^', '~', '"', '\r', '\n', '\0',
        ])
}

pub fn file_list(name: &str) -> bool {
    crate::bibliography::bib_section_files::file_command(name)
        || matches!(
            name,
            "bibliography"
                | "nobibliography"
                | "usepackage"
                | "RequirePackage"
                | "RequirePackageWithOptions"
                | "includeonly"
        )
}

/// Completion can offer explicit source extensions that TeX does not infer.
pub(crate) fn inferred_file_extensions(name: &str) -> Option<&'static [&'static str]> {
    let extensions = file_extensions(name)?;
    Some(
        if crate::bibliography::bib_resources::exact_filename(name) {
            &[""]
        } else if matches!(name, "input" | "InputIfFileExists") {
            &extensions[..1]
        } else {
            extensions
        },
    )
}

/// Package and class loaders append their suffix even to a base name that
/// already ends in it; unlike input, the argument is not an explicit filename.
pub(crate) fn loader_extension(command: &str) -> Option<&'static str> {
    match command {
        "usepackage" | "RequirePackage" | "RequirePackageWithOptions" => Some("sty"),
        "documentclass" | "LoadClass" | "LoadClassWithOptions" => Some("cls"),
        _ => None,
    }
}

pub(crate) fn passes_options(command: &str) -> bool {
    matches!(command, "PassOptionsToPackage" | "PassOptionsToClass")
}

/// Option forwarding names the same package/class file without loading it.
pub(crate) fn package_class_extension(command: &str) -> Option<&'static str> {
    match command {
        "PassOptionsToPackage" => Some("sty"),
        "PassOptionsToClass" => Some("cls"),
        _ => loader_extension(command),
    }
}

pub(crate) fn forwarded_file_argument_start(source: &str, command: &Command) -> Option<usize> {
    passes_options(&command.name).then_some(())?;
    Some(skip_space(
        source,
        command.argument.as_ref()?.outer_span.end,
    ))
}

pub(crate) fn file_argument<'a>(
    source: &str,
    command: &'a Command,
) -> Option<std::borrow::Cow<'a, Argument>> {
    if crate::bibliography::bib_resources::references_file(&command.name) {
        return crate::bibliography::bib_resources::argument(source, command)
            .map(std::borrow::Cow::Owned);
    }
    if passes_options(&command.name) {
        let start = forwarded_file_argument_start(source, command)?;
        let (argument, _) = group(source, start, b'{', b'}')?;
        // These names are exact option-table keys. Do not trim whitespace
        // into a different forwarding declaration.
        (is_literal_file_path(&argument.value) && !argument.value.chars().any(char::is_whitespace))
            .then_some(std::borrow::Cow::Owned(argument))
    } else if matches!(
        command.name.as_str(),
        "input" | "include" | "InputIfFileExists" | "IfFileExists"
    ) {
        let argument = command.argument.as_ref()?;
        // include takes its mandatory filename immediately. Generic command
        // parsing may have skipped an unsupported star or optional note; the
        // later group is ordinary body text, not a file reference.
        if command.name == "include"
            && argument.outer_span.start != skip_space(source, command.span.end)
        {
            return None;
        }
        let span = (source.as_bytes().get(argument.outer_span.start) == Some(&b'{'))
            .then(|| literal_file_span(source, argument.span.clone(), false))
            .flatten();
        // Normalize literal names surrounded by comments, retaining the raw
        // argument for dynamic-name diagnostics and primitive quoted scanning.
        Some(span.map_or_else(
            || std::borrow::Cow::Borrowed(argument),
            |span| {
                std::borrow::Cow::Owned(Argument {
                    value: source[span.clone()].into(),
                    span,
                    outer_span: argument.outer_span.clone(),
                })
            },
        ))
    } else if command.name == "includegraphics" {
        crate::project::graphics::argument(source, command).map(std::borrow::Cow::Owned)
    } else if loader_extension(&command.name) == Some("cls")
        || command.name == "bibliographystyle"
        || code_input_command(&command.name)
    {
        let start = file_name_argument_start(source, &command.name, command.span.end)?;
        let argument = command
            .argument
            .as_ref()
            .filter(|argument| argument.outer_span.start == start)?;
        let span = if crate::project::code_inputs::wrapped_filename(&command.name) {
            crate::project::code_inputs::filename(
                source,
                &command.name,
                argument.span.clone(),
                false,
            )?
        } else if code_input_command(&command.name) {
            is_literal_file_path(&argument.value).then_some(argument.span.clone())?
        } else if command.name == "bibliographystyle" {
            bibliography_style_span(source, argument.span.clone())
                .filter(|span| !span.is_empty())?
        } else {
            literal_file_span(source, argument.span.clone(), false)?
        };
        Some(std::borrow::Cow::Owned(Argument {
            value: source[span.clone()].into(),
            span,
            outer_span: argument.outer_span.clone(),
        }))
    } else {
        command.argument.as_ref().map(std::borrow::Cow::Borrowed)
    }
}

pub(crate) fn loader_forwards_options(command: &str) -> bool {
    matches!(
        command,
        "RequirePackageWithOptions" | "LoadClassWithOptions"
    )
}

/// Package lists and bibliography database declarations discard TeX spaces.
pub(crate) fn loader_name(value: &str) -> String {
    value.replace([' ', '\t', '\n', '\r'], "")
}

pub(crate) fn file_name_ignores_spaces(command: &str) -> bool {
    loader_extension(command) == Some("sty") || matches!(command, "bibliography" | "nobibliography")
}

pub(crate) fn file_lookup_names<'a>(
    command: &str,
    value: &'a str,
) -> impl Iterator<Item = std::borrow::Cow<'a, str>> {
    let recognized = std::path::Path::new(value)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["tex", "sty", "cls", "def", "clo", "fd", "aux", "bbl", "ldf"].contains(&extension)
        });
    let loader = package_class_extension(command);
    let normalized = file_name_ignores_spaces(command).then(|| loader_name(value));
    let literal = normalized.as_deref().unwrap_or(value);
    let empty = literal.is_empty();
    let extension = loader.unwrap_or(match command {
        "bibliography" | "nobibliography" | crate::bibliography::bib_section_files::INFERRED => {
            "bib"
        }
        "bibliographystyle" => "bst",
        _ => "tex",
    });
    let append = !empty
        && match command {
            "include" | "includeonly" => !value.ends_with(".tex"),
            "input" | "InputIfFileExists" | "IfFileExists" => !recognized,
            name if code_input_command(name) => !recognized,
            "bibliography"
            | "nobibliography"
            | "bibliographystyle"
            | crate::bibliography::bib_section_files::INFERRED => {
                !literal.ends_with(&format!(".{extension}"))
            }
            _ => loader.is_some(),
        };
    let preferred = if append {
        std::borrow::Cow::Owned(format!("{literal}.{extension}"))
    } else if let Some(normalized) = normalized {
        std::borrow::Cow::Owned(normalized)
    } else {
        std::borrow::Cow::Borrowed(value)
    };
    // LaTeX preserves style-name spaces in bibstyle, and BibTeX rejects the
    // resulting auxiliary command instead of opening a spaced filename.
    let valid_style = command != "bibliographystyle" || !value.contains([' ', '\t', '\r', '\n']);
    let valid_forward = !passes_options(command)
        || is_literal_file_path(value) && !value.chars().any(char::is_whitespace);
    std::iter::once(preferred)
        .filter(move |_| !empty && valid_style && valid_forward)
        .chain(
            ((matches!(command, "input" | "InputIfFileExists" | "IfFileExists")
                || code_input_command(command))
                && append)
                .then_some(std::borrow::Cow::Borrowed(value)),
        )
}

fn declares_command(name: &str) -> bool {
    declares_register(name)
        || declares_traditional_command(name)
        || declares_font_switch(name)
        || declares_paired_delimiter(name)
        || declares_document_command(name)
        || declares_command_copy(name)
        || declares_text_command(name)
        || matches!(
            name,
            "DeclareMathOperator"
                | "DeclareMathAccent"
                | "DeclareMathRadical"
                | "DeclareMathAlphabet"
                | "DeclareSymbolFontAlphabet"
                | "letcs"
        )
}

pub(crate) fn declares_traditional_command(name: &str) -> bool {
    matches!(
        name,
        "newcommand"
            | "renewcommand"
            | "providecommand"
            | "newrobustcmd"
            | "renewrobustcmd"
            | "providerobustcmd"
            | "DeclareRobustCommand"
    )
}

pub(crate) fn declares_document_command(name: &str) -> bool {
    matches!(
        name,
        "NewDocumentCommand"
            | "RenewDocumentCommand"
            | "ProvideDocumentCommand"
            | "DeclareDocumentCommand"
            | "NewExpandableDocumentCommand"
            | "RenewExpandableDocumentCommand"
            | "ProvideExpandableDocumentCommand"
            | "DeclareExpandableDocumentCommand"
    )
}

pub(crate) fn declares_command_copy(name: &str) -> bool {
    matches!(
        name,
        "NewCommandCopy" | "RenewCommandCopy" | "DeclareCommandCopy"
    )
}

pub(crate) fn declares_text_command(name: &str) -> bool {
    matches!(
        name,
        "DeclareTextCommand"
            | "ProvideTextCommand"
            | "DeclareTextCommandDefault"
            | "ProvideTextCommandDefault"
            | "DeclareTextFontCommand"
            | "DeclareTextSymbol"
            | "DeclareTextAccent"
    )
}

pub(crate) fn declares_font_switch(name: &str) -> bool {
    matches!(
        name,
        "newfontfamily"
            | "renewfontfamily"
            | "providefontfamily"
            | "setfontfamily"
            | "newfontface"
            | "renewfontface"
            | "providefontface"
            | "setfontface"
    )
}

pub(crate) fn declares_paired_delimiter(name: &str) -> bool {
    matches!(
        name,
        "DeclarePairedDelimiter" | "DeclarePairedDelimiterX" | "DeclarePairedDelimiterXPP"
    )
}

pub(crate) fn declares_register(name: &str) -> bool {
    matches!(
        name,
        "newlength"
            | "newsavebox"
            | "newfont"
            | "newcount"
            | "newdimen"
            | "newskip"
            | "newmuskip"
            | "newtoks"
            | "newbox"
            | "newread"
            | "newwrite"
            | "newinsert"
            | "newfam"
            | "chardef"
            | "mathchardef"
            | "countdef"
            | "dimendef"
            | "skipdef"
            | "muskipdef"
            | "toksdef"
            | "font"
            | "DeclareFixedFont"
            | "DeclareMathSymbol"
            | "DeclareMathDelimiter"
            | "DeclareTextSymbol"
    )
}

pub fn declares_environment(name: &str) -> bool {
    declares_environment_copy(name)
        || matches!(
            name,
            "newenvironment"
                | "renewenvironment"
                | "provideenvironment"
                | "NewDocumentEnvironment"
                | "RenewDocumentEnvironment"
                | "ProvideDocumentEnvironment"
                | "DeclareDocumentEnvironment"
                | "NewEnviron"
                | "RenewEnviron"
                | "newtheorem"
        )
}

pub(crate) fn declares_environment_copy(name: &str) -> bool {
    matches!(
        name,
        "NewEnvironmentCopy" | "RenewEnvironmentCopy" | "DeclareEnvironmentCopy"
    )
}

pub(crate) fn environment_copy_source_start(source: &str, command: &Command) -> Option<usize> {
    if !declares_environment_copy(&command.name) {
        return None;
    }
    let argument = command.argument.as_ref()?;
    Some(skip_space(source, argument.outer_span.end))
}

pub(crate) fn environment_copy_source(source: &str, command: &Command) -> Option<Argument> {
    let start = environment_copy_source_start(source, command)?;
    group(source, start, b'{', b'}').map(|(argument, _)| argument)
}

pub(crate) fn is_verbatim_environment(name: &str) -> bool {
    matches!(
        name,
        "verbatim" | "verbatim*" | "lstlisting" | "minted" | "comment"
    ) || crate::parser::fancyvrb::is_environment(name)
}

pub(crate) fn active_static_argument(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
) -> Option<Range<usize>> {
    active_static_argument_mode(source, start, open, cursor, true, is_static_key)
}

fn active_static_argument_mode(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
    nested_delimiters: bool,
    valid: fn(&str) -> bool,
) -> Option<Range<usize>> {
    if source.as_bytes().get(start) != Some(&open) || cursor <= start {
        return None;
    }
    let close = if open == b'{' { b'}' } else { b']' };
    let span = if let Some((argument, _)) =
        group_mode(source, start, open, close, true, nested_delimiters)
    {
        argument.span
    } else {
        start + 1..source.len()
    };
    if cursor > span.end {
        return None;
    }
    let prefix = source[span.start..cursor].trim_start();
    if !prefix.is_empty() && !valid(prefix) {
        return None;
    }
    let value = &source[span.clone()];
    let value = if source.as_bytes().get(span.end) == Some(&close) {
        value
    } else {
        value.split(['\r', '\n']).next().unwrap_or_default()
    }
    .trim();
    if !value.is_empty() && !valid(value) {
        return None;
    }
    let start = cursor - prefix.len();
    let end = cursor
        + source[cursor..span.end]
            .chars()
            .take_while(|ch| {
                !ch.is_whitespace()
                    && !matches!(
                        ch,
                        '{' | '}' | '[' | ']' | '\\' | '%' | ',' | '#' | '$' | '&' | '^' | '~'
                    )
            })
            .map(char::len_utf8)
            .sum::<usize>();
    Some(start..end)
}

/// Selecting a name completion replaces the entire argument content. Outer
/// whitespace in that content belongs to the old name, not the selected one.
pub(crate) fn active_name_argument(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
) -> Option<Range<usize>> {
    active_name_argument_mode(source, start, open, cursor, true, is_static_key)
}

pub(crate) fn active_label_argument(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
) -> Option<Range<usize>> {
    active_name_argument_mode(source, start, open, cursor, open != b'[', is_static_key)
}

pub(crate) fn active_reference_argument(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
) -> Option<Range<usize>> {
    active_name_argument_mode(
        source,
        start,
        open,
        cursor,
        open != b'[',
        is_static_reference_key,
    )
}

pub(crate) fn active_citation_argument(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
) -> Option<Range<usize>> {
    active_name_argument_mode(
        source,
        start,
        open,
        cursor,
        open != b'[',
        is_static_citation_key,
    )
}

/// Complete a single key without deleting comments. Whitespace outside the
/// literal name is removed separately, since accepting a name must not retain
/// spaces that would make it refer to a different key.
pub(crate) fn active_commented_name_argument(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
    nested_delimiters: bool,
    valid: fn(&str) -> bool,
) -> Option<(Range<usize>, Vec<Range<usize>>)> {
    if source.as_bytes().get(start) != Some(&open) || cursor <= start {
        return None;
    }
    let close = if open == b'{' { b'}' } else { b']' };
    let span = group_mode(source, start, open, close, true, nested_delimiters)
        .map_or(start + 1..source.len(), |(argument, _)| argument.span);
    if cursor > span.end || !source[span.clone()].contains('%') {
        return None;
    }
    let pieces = name_literal_pieces(source, span);
    let mut nonempty = pieces
        .iter()
        .enumerate()
        .filter(|(_, piece)| !source[(*piece).clone()].trim().is_empty());
    let selected = if let Some((index, name)) = nonempty.next() {
        if !valid(source[name.clone()].trim()) || nonempty.next().is_some() {
            return None;
        }
        index
    } else {
        pieces
            .iter()
            .position(|piece| piece.start <= cursor && cursor <= piece.end)?
    };
    let name = pieces.get(selected)?.clone();
    if cursor < name.start || cursor > name.end {
        return None;
    }
    let prefix = source[name.start..cursor].trim_start();
    if !prefix.is_empty() && !valid(prefix) {
        return None;
    }
    let removals = pieces
        .into_iter()
        .enumerate()
        .filter(|(index, piece)| *index != selected && !piece.is_empty())
        .map(|(_, piece)| piece)
        .collect();
    Some((name, removals))
}

fn active_name_argument_mode(
    source: &str,
    start: usize,
    open: u8,
    cursor: usize,
    nested_delimiters: bool,
    valid: fn(&str) -> bool,
) -> Option<Range<usize>> {
    active_static_argument_mode(source, start, open, cursor, nested_delimiters, valid)?;
    let close = if open == b'{' { b'}' } else { b']' };
    let span = group_mode(source, start, open, close, true, nested_delimiters).map_or_else(
        || {
            let end = source[cursor..]
                .find(['\r', '\n'])
                .map_or(source.len(), |length| cursor + length);
            start + 1..end
        },
        |(argument, _)| argument.span,
    );
    let value = source[span.clone()].trim();
    (value.is_empty() || valid(value)).then_some(span)
}

/// LSP requires a completion's main edit to stay on the request line.
pub(crate) fn name_completion_ranges(
    text: &Text,
    span: Range<usize>,
    cursor: usize,
) -> Option<(lsp_types::Range, Vec<TextEdit>)> {
    let line_start = text.source[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text.source[cursor..]
        .find(['\r', '\n'])
        .map_or(text.source.len(), |length| cursor + length);
    let main = span.start.max(line_start)..span.end.min(line_end);
    // An empty request line between two deletion ranges cannot anchor a
    // nonoverlapping main insertion. Keep that ambiguous completion unresolved.
    if main.is_empty() && (span.start < main.start || main.end < span.end) {
        return None;
    }
    let additional = [span.start..main.start, main.end..span.end]
        .into_iter()
        .filter(|range| !range.is_empty())
        .map(|range| TextEdit {
            range: text.range(range.start, range.end),
            new_text: String::new(),
        })
        .collect();
    Some((text.range(main.start, main.end), additional))
}

pub(crate) fn control_word(source: &str, start: usize) -> Option<Range<usize>> {
    if source.as_bytes().get(start) != Some(&b'\\') {
        return None;
    }
    let length = source[start + 1..]
        .bytes()
        .take_while(|byte| byte.is_ascii_alphabetic() || *byte == b'@')
        .count();
    (length > 0).then_some(start..start + 1 + length)
}

pub(crate) fn token(source: &str, start: usize) -> Option<&str> {
    let ch = source.get(start..)?.chars().next()?;
    let end = if ch == '\\' {
        control_word(source, start).map_or_else(
            || {
                source[start + 1..]
                    .chars()
                    .next()
                    .map(|ch| start + 1 + ch.len_utf8())
            },
            |span| Some(span.end),
        )?
    } else {
        start + ch.len_utf8()
    };
    Some(&source[start..end])
}

/// Inline verbatim readers cannot change the catcodes of tokens already
/// collected as a macro argument. Match ordinary tokens, skipping comments
/// and protecting delimiters inside balanced groups or control symbols.
pub(crate) fn tokenized_code_end(source: &str, start: usize) -> Option<usize> {
    let delimiter = token(source, start)?;
    if delimiter == "{" {
        return group(source, start, b'{', b'}').map(|(_, end)| end);
    }
    if delimiter.starts_with('\\') || delimiter == "}" || delimiter.chars().any(char::is_whitespace)
    {
        return None;
    }
    tokenized_delimited_end(source, start + delimiter.len(), delimiter)
}

pub(crate) fn tokenized_delimited_end(
    source: &str,
    mut at: usize,
    delimiter: &str,
) -> Option<usize> {
    loop {
        at = skip_space(source, at);
        let value = token(source, at)?;
        if value == delimiter {
            return Some(at + value.len());
        }
        at = match value {
            "{" => group(source, at, b'{', b'}')?.1,
            "}" => return None,
            _ => at + value.len(),
        };
    }
}

/// Only infer ungrouped tokens whose boundaries agree across the engines. In
/// pdfTeX a Unicode character consists of several UTF-8 tokens; XeTeX reads one
/// character and may include Unicode letters in a control word instead.
pub(crate) fn required_token(source: &str, start: usize) -> Option<&str> {
    let rest = source.get(start..)?;
    if rest.starts_with("^^") || rest.starts_with("\\^^") {
        return None;
    }
    let value = token(source, start)?;
    if !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
        || (value.contains(' ') && value != "\\ ")
        || matches!(value, "{" | "}" | "%" | "\\par")
    {
        return None;
    }
    if control_word(value, 0).is_some() {
        let following = &source[start + value.len()..];
        if following.starts_with("^^") || following.chars().next().is_some_and(|ch| !ch.is_ascii())
        {
            return None;
        }
    }
    Some(value)
}

pub(crate) fn conditional_declaration(
    source: &str,
    commands: &[Command],
    command: &Command,
) -> Option<Range<usize>> {
    if declares_boolean(&command.name) {
        let span = trimmed_span(source, command.argument.as_ref()?.span.clone());
        return is_static_key(&source[span.clone()]).then_some(span);
    }
    if command.name != "newif" {
        return None;
    }
    let span = control_word(source, skip_space(source, command.span.end))?;
    if let Some(name) = source[span.start + 1..span.end].strip_prefix("if") {
        return (!name.is_empty()).then_some(span.start + 3..span.end);
    }
    let index = commands
        .binary_search_by_key(&command.span.start, |command| command.span.start)
        .ok()?;
    let previous = commands.get(index.checked_sub(1)?)?;
    let target = commands.get(index + 1)?;
    if previous.name != "expandafter"
        || skip_space(source, previous.span.end) != command.span.start
        || skip_space(source, command.span.end) != target.span.start
    {
        return None;
    }
    let span = literal_csname(source, commands, index + 1)?;
    let name = source[span.clone()].strip_prefix("if")?;
    (!name.is_empty()).then_some(span.start + 2..span.end)
}

pub(crate) fn declares_boolean(name: &str) -> bool {
    matches!(
        name,
        "newboolean" | "provideboolean" | "newbool" | "providebool"
    )
}

pub(crate) fn boolean_reference(name: &str) -> bool {
    matches!(
        name,
        "boolean" | "setboolean" | "setbool" | "booltrue" | "boolfalse" | "ifbool" | "notbool"
    )
}

pub(crate) fn declares_toggle(name: &str) -> bool {
    matches!(name, "newtoggle" | "providetoggle")
}

pub(crate) fn toggle_reference(name: &str) -> bool {
    matches!(
        name,
        "settoggle" | "toggletrue" | "togglefalse" | "iftoggle" | "nottoggle"
    )
}

pub fn is_citation(name: &str) -> bool {
    crate::bibliography::citations::is_structured(name)
        || matches!(
            name,
            "cite"
                | "bibentry"
                | "citep"
                | "citet"
                | "citeauthor"
                | "citeyear"
                | "citealt"
                | "citealp"
                | "citenum"
                | "citeyearpar"
                | "citefullauthor"
                | "Citet"
                | "Citep"
                | "Citealt"
                | "Citealp"
                | "Citeauthor"
                | "citetalias"
                | "citepalias"
                | "defcitealias"
                | "autocite"
                | "parencite"
                | "textcite"
                | "fullcite"
                | "footfullcite"
                | "nocite"
                | "footcite"
                | "Cite"
                | "Parencite"
                | "Footcite"
                | "footcitetext"
                | "Footcitetext"
                | "smartcite"
                | "Smartcite"
                | "supercite"
                | "Textcite"
                | "Autocite"
                | "citetitle"
                | "Citetitle"
                | "citedate"
                | "citeurl"
                | "notecite"
                | "pnotecite"
                | "fnotecite"
                | "Notecite"
                | "Pnotecite"
                | "Fnotecite"
        )
}

/// Inline entries, aliases and data contexts name one key rather than a list.
pub(crate) fn is_single_citation(name: &str) -> bool {
    matches!(
        name,
        "bibentry"
            | "defcitealias"
            | "entrydata"
            | "ifentryexists"
            | "ifentryinbib"
            | "ifentrycategory"
    )
}

pub(crate) fn warns_on_undefined_citation(name: &str) -> bool {
    is_citation(name)
        && !matches!(
            name,
            "defcitealias"
                | "assignrefcontextentries"
                | "addtocategory"
                | "defbibentryset"
                | "entrydata"
                | "ifentryexists"
                | "ifentryinbib"
                | "ifentrycategory"
        )
}

pub(crate) fn citation_accepts_star(name: &str) -> bool {
    if let Some(style) = crate::bibliography::citations::style(name) {
        return style.accepts_star();
    }
    is_citation(name)
        && !matches!(
            name,
            "nocite"
                | "bibentry"
                | "citenum"
                | "citeyearpar"
                | "citefullauthor"
                | "citetalias"
                | "citepalias"
                | "defcitealias"
        )
}

pub(crate) fn citation_accepts_notes(name: &str) -> bool {
    is_citation(name)
        && !matches!(
            name,
            "nocite"
                | "citenum"
                | "bibentry"
                | "defcitealias"
                | "addtocategory"
                | "defbibentryset"
                | "entrydata"
                | "ifentryexists"
                | "ifentryinbib"
                | "ifentrycategory"
        )
}

/// Citation keys follow at most two optional notes. Leave any extra bracket
/// in place so callers cannot treat the following prose as a key argument.
pub(crate) fn citation_argument_start(
    source: &str,
    name: &str,
    command_end: usize,
) -> Option<usize> {
    if let Some(style) = crate::bibliography::citations::style(name) {
        return crate::bibliography::citations::arguments(source, command_end, style)
            .keys()
            .next()
            .map(|argument| argument.outer_span.start);
    }
    let mut at = skip_space(source, command_end);
    if source.as_bytes().get(at) == Some(&b'*') {
        if !citation_accepts_star(name) {
            return None;
        }
        at = skip_space(source, at + 1);
    }
    if citation_accepts_notes(name) {
        for _ in 0..2 {
            let Some(end) = optional_note_end(source, at) else {
                break;
            };
            at = skip_space(source, end);
        }
    }
    Some(at)
}

pub fn section_level(name: &str) -> Option<u32> {
    match name {
        "part" => Some(0),
        "chapter" => Some(1),
        "section" => Some(2),
        "subsection" => Some(3),
        "subsubsection" => Some(4),
        "paragraph" => Some(5),
        "subparagraph" => Some(6),
        _ => None,
    }
}

impl Syntax {
    pub(crate) fn inspection_operand(&self, source: &str, command: &Command) -> bool {
        self.control_operands.contains(&command.span.start)
            && self
                .commands
                .binary_search_by_key(&command.span.start, |item| item.span.start)
                .ok()
                .and_then(|index| index.checked_sub(1))
                .map(|index| &self.commands[index])
                .is_some_and(|previous| {
                    previous.name == "meaning"
                        && !self.control_operands.contains(&previous.span.start)
                        && skip_space(source, previous.span.end) == command.span.start
                })
    }

    /// File discovery keeps possible paths when another source can rebind an
    /// apparent quoting primitive. Reconstruct that candidate without making
    /// the operand an executed call in the symbol index.
    pub(crate) fn potential_file_command<'a>(
        &self,
        source: &str,
        command: &'a Command,
    ) -> std::borrow::Cow<'a, Command> {
        if command.argument.is_some() || !self.inspection_operand(source, command) {
            return std::borrow::Cow::Borrowed(command);
        }
        let argument = (|| {
            let start = file_name_argument_start(source, &command.name, command.span.end)?;
            let grouped = if crate::bibliography::bib_resources::references_file(&command.name) {
                verbatim_file_group(source, start)
            } else {
                group(source, start, b'{', b'}')
            };
            grouped
                .or_else(|| {
                    (command.name == "input")
                        .then(|| input_argument(source, start))
                        .flatten()
                })
                .map(|(argument, _)| argument)
        })();
        match argument {
            Some(argument) => std::borrow::Cow::Owned(Command {
                argument: Some(argument),
                ..command.clone()
            }),
            None => std::borrow::Cow::Borrowed(command),
        }
    }

    fn fix(
        &mut self,
        text: &Text,
        span: Range<usize>,
        message: String,
        title: String,
        edit: TextEdit,
    ) {
        let fix = syntax_fix(text, span, message, title, edit);
        self.diagnostics.push(fix.diagnostic.clone());
        self.fixes.push(fix);
    }

    pub fn latex(text: &Text) -> Self {
        Self::latex_with_string_tests(text, |command, known| {
            crate::symbols::string_tests::data_count(&command.name).filter(|_| known)
        })
    }

    pub(crate) fn latex_with_string_tests(
        text: &Text,
        literal_test: impl FnMut(&Command, bool) -> Option<usize>,
    ) -> Self {
        Self::latex_with_resource_context(text, literal_test, &Default::default())
    }

    pub(crate) fn latex_with_readers(
        text: &Text,
        readers: &crate::project::resource_context::Readers,
    ) -> Self {
        Self::latex_with_resource_context(
            text,
            |command, known| {
                crate::symbols::string_tests::data_count(&command.name).filter(|_| known)
            },
            readers,
        )
    }

    pub(crate) fn latex_with_resource_context(
        text: &Text,
        mut literal_test: impl FnMut(&Command, bool) -> Option<usize>,
        readers: &crate::project::resource_context::Readers,
    ) -> Self {
        let source = &text.source;
        let bytes = source.as_bytes();
        let mut result = Self {
            contextual_resource_readers: readers.clone(),
            ..Self::default()
        };
        let mut braces = Vec::new();
        let mut environments = Vec::new();
        let mut literal_arguments = std::collections::BTreeMap::new();
        let mut control_operands = std::collections::BTreeSet::new();
        let mut punctuation_operands = std::collections::BTreeSet::new();
        let mut assigned_controls = std::collections::BTreeSet::<String>::new();
        let mut conversion_arguments = std::collections::BTreeMap::new();
        let mut inline_tokenization = crate::symbols::commands::InlineTokenization::default();
        let mut fancyverb_context = crate::parser::fancyvrb::InlineContext::default();
        let mut raw_resource_end = 0;
        let mut escape_codes = super::catcodes::EscapeCodes::default();
        let mut i = 0;
        while i < bytes.len() {
            // Literal conversions suppress command indexing but retain source
            // ranges for consumers such as quoted filename completion.
            if let Some(end) = conversion_arguments.remove(&i) {
                result.literal_conversions.push(i..end);
                i = end;
                continue;
            }
            // Data arguments stay opaque while the normal scan still visits
            // preceding options and comments.
            if let Some((span, end)) = literal_arguments.remove(&i) {
                result.ignored.push(span);
                i = end;
                continue;
            }
            match bytes[i] {
                b'%' if i < raw_resource_end => i += 1,
                b'%' if !escape_codes.is_escape(b'%') => {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    result.ignored.push(start..i);
                    result.comments.push(start..i);
                }
                // A consumed brace token is data, not a group. Retain its
                // position so later operands do not read it as an argument.
                b'{' | b'}' if punctuation_operands.contains(&i) => {
                    i += 1;
                }
                b'{' if !escape_codes.is_escape(b'{') => {
                    escape_codes.enter();
                    braces.push(i);
                    fancyverb_context.enter();
                    i += 1;
                }
                b'}' if !escape_codes.is_escape(b'}') => {
                    escape_codes.leave();
                    fancyverb_context.leave();
                    if braces.pop().is_none() {
                        result.fix(
                            text,
                            i..i + 1,
                            "Unexpected closing brace".into(),
                            "Remove unexpected `}`".into(),
                            TextEdit {
                                range: text.range(i, i + 1),
                                new_text: String::new(),
                            },
                        );
                    }
                    i += 1;
                }
                escape if escape_codes.is_escape(escape) => {
                    let start = i;
                    i += 1;
                    let name_start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'@') {
                        i += 1;
                    }
                    if name_start == i {
                        if i < bytes.len() {
                            i += source[i..].chars().next().unwrap().len_utf8();
                        }
                        continue;
                    }
                    let name = &source[name_start..i];
                    let command_end = i;
                    let reader = readers.get(&start);
                    let tokenized_resource =
                        crate::bibliography::bib_resources::references_file(name)
                            && inline_tokenization.contains(
                                source,
                                &result,
                                &control_operands,
                                start,
                            );
                    let native_resource = crate::bibliography::bib_resources::references_file(name)
                        && !reader.is_some_and(Option::is_none)
                        && !tokenized_resource;
                    if control_operands.contains(&start) {
                        result.commands.push(Command {
                            name: name.into(),
                            span: start..command_end,
                            argument: None,
                        });
                        continue;
                    }
                    // A previous lexical assignment invalidates this keyword's
                    // operand grammar without simulating scope restoration.
                    let operands_known = !assigned_controls.contains(name);
                    let probe = Command {
                        name: name.into(),
                        span: start..command_end,
                        argument: None,
                    };
                    if let Some(count) = literal_test(&probe, operands_known) {
                        // Captured string meanings replace the destination's
                        // lexical grammar, including file/definition/verb names.
                        result.string_test_arities.insert(start, Some(count));
                        for argument in
                            crate::symbols::string_tests::arguments(source, command_end, count)
                        {
                            result.literal_tests.push(argument.span.clone());
                            literal_arguments.insert(argument.start, (argument.span, argument.end));
                        }
                        result.commands.push(probe);
                        continue;
                    }
                    if operands_known
                        && matches!(
                            name,
                            "catcode" | "begingroup" | "endgroup" | "bgroup" | "egroup"
                        )
                        && !inline_tokenization.contains(source, &result, &control_operands, start)
                    {
                        match name {
                            "catcode" => {
                                if let Some(assignment) =
                                    escape_codes.assignment(source, command_end)
                                {
                                    let global = !assigned_controls.contains("global")
                                        && result.commands.last().is_some_and(|previous| {
                                            previous.name == "global"
                                                && !control_operands.contains(&previous.span.start)
                                                && skip_space(source, previous.span.end) == start
                                        });
                                    escape_codes.assign(&assignment, global);
                                    i = assignment.end;
                                    result.commands.push(probe);
                                    continue;
                                }
                            }
                            "begingroup" | "bgroup" => escape_codes.enter(),
                            "endgroup" | "egroup" => escape_codes.leave(),
                            _ => unreachable!(),
                        }
                    }
                    // Honor custom escape token boundaries without interpreting
                    // their operands with helpers that assume ordinary '\\'.
                    if escape != b'\\' {
                        if matches!(
                            name,
                            "global" | "begingroup" | "endgroup" | "bgroup" | "egroup"
                        ) {
                            result.commands.push(probe);
                        }
                        continue;
                    }
                    if operands_known {
                        if let Some(named) = crate::symbols::named_commands::undefines(name) {
                            let target =
                                crate::symbols::named_commands::undefinition(source, &probe);
                            if let Some(target) = &target {
                                assigned_controls.insert(target.name.clone());
                            }
                            result.command_undefinitions.insert(start, target);
                            if !named {
                                let mut at = skip_space(source, command_end);
                                if bytes.get(at) == Some(&b'{') {
                                    at = skip_space(source, at + 1);
                                }
                                if bytes.get(at) == Some(&b'\\') {
                                    control_operands.insert(at);
                                }
                            }
                        }
                    }
                    let inline_name = if operands_known
                        && (matches!(name, "lstinline" | "Verb" | "EscVerb")
                            || crate::parser::minted::is_inline(name))
                    {
                        Some(name)
                    } else if !control_operands.contains(&start) && assigned_controls.contains(name)
                    {
                        inline_tokenization.literal_name(source, &result, name)
                    } else {
                        None
                    };
                    if inline_name == Some("EscVerb") {
                        let ordinary = crate::parser::fancyvrb::escaped_inline_start(
                            source,
                            command_end,
                            true,
                        );
                        let tokenized = ordinary.is_some_and(|at| {
                            inline_tokenization.contains(source, &result, &control_operands, at + 1)
                        });
                        if let Some(at) = crate::parser::fancyvrb::escaped_inline_start(
                            source,
                            command_end,
                            tokenized,
                        ) {
                            for argument in crate::symbols::string_tests::arguments(source, at, 1) {
                                literal_arguments
                                    .insert(argument.start, (argument.span, argument.end));
                            }
                        }
                        result.commands.push(probe);
                        continue;
                    }
                    if inline_name == Some("Verb") {
                        let ordinary = fancyverb_context.verb_header(source, command_end, true);
                        let body = ordinary.as_ref().map_or(source.len(), |header| {
                            if header.extra {
                                source[header.code..]
                                    .chars()
                                    .next()
                                    .map_or(header.code, |delimiter| {
                                        header.code + delimiter.len_utf8()
                                    })
                            } else {
                                crate::parser::fancyvrb::inline_header(source, header.code)
                                    .map_or(header.code, |header| header.body)
                            }
                        });
                        let tokenized =
                            inline_tokenization.contains(source, &result, &control_operands, body);
                        let header = if tokenized {
                            ordinary
                        } else {
                            fancyverb_context.verb_header(source, command_end, false)
                        };
                        if let Some(header) = header {
                            if header.extra {
                                if let Some(span) = crate::parser::minted::inline_span(
                                    source,
                                    header.code,
                                    tokenized,
                                ) {
                                    literal_arguments.insert(span.start, (span.clone(), span.end));
                                }
                            } else if let Some(header) = crate::parser::fancyvrb::inline_header_mode(
                                source,
                                header.code,
                                header.prefetched,
                            ) {
                                if tokenized {
                                    let open = header.start..header.body;
                                    literal_arguments.insert(open.start, (open.clone(), open.end));
                                    if let Some(close) = header.closing_marker(source) {
                                        literal_arguments
                                            .insert(close.start, (close.clone(), close.end));
                                    }
                                } else {
                                    let span = header.raw_span(source);
                                    literal_arguments.insert(span.start, (span.clone(), span.end));
                                }
                            }
                        }
                        result.commands.push(probe);
                        continue;
                    }
                    if inline_name.is_some_and(crate::parser::minted::is_inline) {
                        let language = crate::parser::minted::language(source, command_end);
                        if let Some(argument) = &language {
                            literal_arguments.insert(
                                argument.outer_span.start,
                                (argument.span.clone(), argument.outer_span.end),
                            );
                            let code = skip_space(source, argument.outer_span.end);
                            let body = source[code..]
                                .chars()
                                .next()
                                .map_or(code, |delimiter| code + delimiter.len_utf8());
                            let tokenized = inline_tokenization.contains(
                                source,
                                &result,
                                &control_operands,
                                body,
                            );
                            if let Some(span) = crate::parser::minted::inline_span(
                                source,
                                argument.outer_span.end,
                                tokenized,
                            ) {
                                literal_arguments.insert(span.start, (span.clone(), span.end));
                            }
                        }
                        result.commands.push(Command {
                            argument: language,
                            ..probe
                        });
                        continue;
                    }
                    if inline_name == Some("lstinline") {
                        if let Some(at) = crate::parser::listings::inline_start(source, command_end)
                        {
                            let tokenized = inline_tokenization.contains(
                                source,
                                &result,
                                &control_operands,
                                at + source[at..].chars().next().unwrap().len_utf8(),
                            );
                            let span = crate::parser::listings::inline_body(source, at, tokenized);
                            literal_arguments.insert(span.start, (span.clone(), span.end));
                        }
                        result.commands.push(probe);
                        continue;
                    }
                    if operands_known && name == "inputminted" {
                        if let Some(argument) = crate::parser::minted::language(source, command_end)
                        {
                            literal_arguments.insert(
                                argument.outer_span.start,
                                (argument.span, argument.outer_span.end),
                            );
                        }
                    }
                    let starred = !matches!(
                        name,
                        "input"
                            | "InputIfFileExists"
                            | "IfFileExists"
                            | "detokenize"
                            | "string"
                            | "meaning"
                    ) && crate::symbols::string_tests::data_count(name).is_none()
                        && !passes_options(name)
                        && bytes.get(i) == Some(&b'*');
                    if starred {
                        i += 1;
                    }
                    if name == "verb" {
                        let raw_start = i;
                        if let Some(delimiter) =
                            source[i..].chars().next().filter(|ch| !ch.is_whitespace())
                        {
                            i += delimiter.len_utf8();
                            while i < bytes.len() && bytes[i] != b'\n' {
                                let ch = source[i..].chars().next().unwrap();
                                i += ch.len_utf8();
                                if ch == delimiter {
                                    break;
                                }
                            }
                        }
                        result.ignored.push(raw_start..i);
                        continue;
                    }
                    let mut arg_start = skip_space(source, i);
                    if !starred
                        && (reference_accepts_star(name) || citation_accepts_star(name))
                        && bytes.get(arg_start) == Some(&b'*')
                    {
                        arg_start = skip_space(source, arg_start + 1);
                    }
                    if is_reference(name) {
                        arg_start = reference_argument_start(source, name, command_end)
                            .unwrap_or(source.len());
                    } else if is_citation(name) {
                        arg_start = citation_argument_start(source, name, command_end)
                            .unwrap_or(source.len());
                    } else if crate::bibliography::bib_resources::references_file(name)
                        || loader_extension(name).is_some()
                        || name == "bibliographystyle"
                        || code_input_command(name)
                        || crate::parser::fancyvrb::is_use(name)
                        || crate::parser::fancyvrb::is_inline(name)
                    {
                        arg_start = file_name_argument_start(source, name, command_end)
                            .unwrap_or(source.len());
                    } else if name == "nobibliography" {
                        // The starred form reads the existing bibliography
                        // without taking a database argument. Neither form
                        // accepts optional notes before the database list.
                        arg_start = skip_space(source, command_end);
                        if bytes.get(arg_start) == Some(&b'*') {
                            arg_start = source.len();
                        }
                    } else if name == "bibitem" {
                        // One optional label ends at the first unbraced `]`.
                        // Extra notes and unsupported stars leave prose behind.
                        if starred {
                            arg_start = source.len();
                        } else if bytes.get(arg_start) == Some(&b'[') {
                            arg_start = optional_note_end(source, arg_start)
                                .map_or(source.len(), |end| skip_space(source, end));
                        }
                    } else if matches!(name, "hyperref" | "hyperdef") {
                        if let Some((_, end)) = bracket_argument(source, arg_start) {
                            arg_start = skip_space(source, end);
                        }
                    } else if !matches!(name, "input" | "InputIfFileExists" | "IfFileExists")
                        && !passes_options(name)
                        && !loader_forwards_options(name)
                    {
                        while let Some((_, end)) = group(source, arg_start, b'[', b']') {
                            arg_start = skip_space(source, end);
                        }
                    }
                    let url_argument = matches!(name, "url" | "href")
                        || name == "hyperref"
                            && bytes.get(skip_space(source, command_end)) == Some(&b'{');
                    let parsed = if name == "meaning" && operands_known {
                        None
                    } else if operands_known
                        && (crate::parser::fancyvrb::is_inline_save(name)
                            || crate::parser::fancyvrb::is_use(name))
                    {
                        crate::parser::fancyvrb::name_argument(source, arg_start)
                    } else {
                        group_mode(
                            source,
                            arg_start,
                            b'{',
                            b'}',
                            !(url_argument || native_resource),
                            true,
                        )
                        .or_else(|| {
                            (name == "input" && bytes.get(arg_start) != Some(&b'{'))
                                .then(|| input_argument(source, arg_start))
                                .flatten()
                        })
                        .filter(|_| !(starred && loader_extension(name).is_some()))
                    };
                    let argument = parsed.as_ref().map(|(argument, _)| {
                        if name == "bibitem" || is_single_citation(name) {
                            single_citation_argument(source, argument.clone())
                        } else if name == "label" || is_single_reference(name) {
                            single_reference_argument(source, argument.clone())
                        } else {
                            argument.clone()
                        }
                    });
                    let mut command = Command {
                        name: name.into(),
                        span: start..command_end,
                        argument,
                    };
                    if tokenized_resource
                        && command.argument.as_ref().is_some_and(|argument| {
                            crate::bibliography::bib_resources::changes_when_tokenized(
                                &argument.value,
                            )
                        })
                    {
                        command.argument = None;
                    }
                    if let Some(Some(reader)) =
                        reader.filter(|_| !matches!(name, "csuse" | "csname" | "endcsname"))
                    {
                        if !reader.bracket && !reader.opaque {
                            command.argument = verbatim_file_group(source, reader.start)
                                .map(|(argument, _)| argument);
                        }
                    }
                    if operands_known {
                        fancyverb_context.observe(&command);
                    }
                    if name == "newif" && operands_known {
                        let operand = skip_space(source, command_end);
                        if bytes.get(operand) == Some(&b'\\') {
                            control_operands.insert(operand);
                        }
                        if let Some(target) = control_word(source, operand).filter(|span| {
                            source[span.start + 1..span.end]
                                .strip_prefix("if")
                                .is_some_and(|name| !name.is_empty())
                        }) {
                            assigned_controls.insert(source[target.start + 1..target.end].into());
                        }
                    }
                    if operands_known
                        && matches!(name, "iffieldequals" | "iflistequals" | "ifnameequals")
                    {
                        // The second argument names a macro whose replacement
                        // text is compared; that control token is not called.
                        if let Some((_, end)) = group(
                            source,
                            crate::bibliography::citations::space(source, command_end),
                            b'{',
                            b'}',
                        ) {
                            let mut operand = crate::bibliography::citations::space(source, end);
                            if bytes.get(operand) == Some(&b'{') {
                                operand =
                                    crate::bibliography::citations::space(source, operand + 1);
                            }
                            if bytes.get(operand) == Some(&b'\\') {
                                control_operands.insert(operand);
                            }
                        }
                    }
                    if operands_known
                        && matches!(name, "ifdefcounter" | "ifdeflength" | "ifdefdimen")
                    {
                        // etoolbox inspects this control token with meaning;
                        // declarations and verbatim commands are not executed.
                        let mut operand = skip_space(source, command_end);
                        if bytes.get(operand) == Some(&b'{') {
                            operand = skip_space(source, operand + 1);
                        }
                        if bytes.get(operand) == Some(&b'\\') {
                            control_operands.insert(operand);
                        }
                    }
                    if name == "meaning" && operands_known {
                        let operand = skip_space(source, command_end);
                        if bytes.get(operand) == Some(&b'\\') {
                            control_operands.insert(operand);
                        } else if matches!(bytes.get(operand), Some(b'{' | b'}' | b'$'))
                            && !result.commands.iter().any(|previous| {
                                !control_operands.contains(&previous.span.start)
                                    && !punctuation_operands
                                        .contains(&skip_space(source, previous.span.end))
                                    && crate::symbols::commands::reads_brace(
                                        source, previous, operand,
                                    )
                            })
                        {
                            punctuation_operands.insert(operand);
                        }
                    }
                    if let Some(style) = crate::bibliography::citations::style(name) {
                        let arguments =
                            crate::bibliography::citations::arguments(source, command_end, style);
                        if style.set_definition() && arguments.keys().next().is_some() {
                            if let Some(field) =
                                arguments.fields.first().filter(|field| field.complete)
                            {
                                let argument =
                                    single_citation_argument(source, field.argument.clone());
                                if is_static_citation_key(&argument.value) {
                                    result.citations.push(Entry {
                                        name: argument.value,
                                        span: argument.span,
                                        detail: "Dynamic bibliography set".into(),
                                        is_variable: false,
                                    });
                                }
                            }
                        }
                        for argument in arguments.keys() {
                            for span in
                                crate::bibliography::citations::key_spans(source, argument, style)
                            {
                                result.citation_references.push(Entry {
                                    name: source[span.clone()].into(),
                                    span,
                                    detail: format!("\\{name}"),
                                    is_variable: false,
                                });
                            }
                        }
                    }
                    if name == "detokenize" {
                        if let Some((argument, end)) =
                            group(source, skip_space(source, command_end), b'{', b'}')
                        {
                            conversion_arguments.insert(argument.outer_span.start, end);
                        }
                    } else if name == "string" {
                        let at = skip_space(source, command_end);
                        if at < source.len() {
                            let end = control_word(source, at).map_or_else(
                                || {
                                    let first = source[at..].chars().next().unwrap();
                                    let after = at + first.len_utf8();
                                    if first == '\\' && after < source.len() {
                                        after + source[after..].chars().next().unwrap().len_utf8()
                                    } else {
                                        after
                                    }
                                },
                                |word| word.end,
                            );
                            conversion_arguments.insert(at, end);
                        }
                    }
                    if operands_known
                        && (crate::parser::fancyvrb::is_inline_save(name)
                            || crate::parser::fancyvrb::is_use(name))
                    {
                        if let Some((argument, end)) = &parsed {
                            literal_arguments
                                .insert(argument.outer_span.start, (argument.span.clone(), *end));
                        }
                    }
                    if operands_known && crate::parser::fancyvrb::is_inline_save(name) {
                        let start = parsed.as_ref().map(|(_, end)| *end);
                        if name != "SaveGVerb" && fancyverb_context.uses_extra(source, command_end)
                        {
                            if let Some(start) = start {
                                let code = skip_space(source, start);
                                let body = source[code..]
                                    .chars()
                                    .next()
                                    .map_or(code, |delimiter| code + delimiter.len_utf8());
                                let tokenized = inline_tokenization.contains(
                                    source,
                                    &result,
                                    &control_operands,
                                    body,
                                );
                                if let Some(span) =
                                    crate::parser::minted::inline_span(source, start, tokenized)
                                {
                                    literal_arguments.insert(span.start, (span.clone(), span.end));
                                }
                            }
                        } else if let Some(header) =
                            start.and_then(|at| crate::parser::fancyvrb::inline_header(source, at))
                        {
                            let span = header.raw_span(source);
                            literal_arguments.insert(span.start, (span.clone(), span.end));
                        }
                    }
                    if let Some((declaration, entry)) =
                        crate::symbols::anchors::entry(source, &command)
                    {
                        if declaration {
                            result.anchors.push(entry);
                        } else {
                            result.anchor_references.push(entry);
                        }
                    }
                    if name == "hyperdef" {
                        if let Some(argument) = separate_label_argument(source, &command)
                            .filter(|argument| is_static_reference_key(&argument.value))
                        {
                            result.labels.push(Entry {
                                name: argument.value,
                                span: argument.span,
                                detail: "Defined with \\hyperdef".into(),
                                is_variable: false,
                            });
                        }
                    }
                    let declaration = if declares_command(name) {
                        if let Some(argument) = &command.argument {
                            let bounded = &source[..argument.span.end];
                            control_word(bounded, skip_space(bounded, argument.span.start))
                                .filter(|word| skip_space(bounded, word.end) == argument.span.end)
                        } else {
                            control_word(source, arg_start)
                        }
                    } else if matches!(name, "def" | "gdef" | "edef" | "xdef" | "let" | "futurelet")
                    {
                        control_word(source, arg_start)
                    } else {
                        None
                    };
                    let expanded_target = declaration.as_ref().and_then(|span| {
                        if source[span.clone()] != *"\\csname"
                            || skip_space(source, command.span.end) != span.start
                            || !result.commands.last().is_some_and(|previous| {
                                previous.name == "expandafter"
                                    && !control_operands.contains(&previous.span.start)
                                    && skip_space(source, previous.span.end) == command.span.start
                            })
                        {
                            return None;
                        }
                        let at = skip_space(source, span.end);
                        let end = at + source[at..].find('\\')?;
                        let closing = control_word(source, end)?;
                        (source[closing.clone()] == *"\\endcsname"
                            && is_static_key(&source[at..end]))
                        .then_some((span.start, closing))
                    });
                    let declaration = declaration.filter(|span| {
                        // expandafter expands a csname target before the
                        // declaration sees it; it does not define `csname`.
                        !(source[span.clone()] == *"\\csname"
                            && skip_space(source, command.span.end) == span.start
                            && result.commands.last().is_some_and(|previous| {
                                previous.name == "expandafter"
                                    && !control_operands.contains(&previous.span.start)
                                    && skip_space(source, previous.span.end) == command.span.start
                            }))
                    });
                    if operands_known {
                        if let Some(span) = &declaration {
                            control_operands.insert(span.start);
                        } else if declares_command(name)
                            || matches!(
                                name,
                                "def" | "gdef" | "edef" | "xdef" | "let" | "futurelet"
                            )
                        {
                            // Retain the first name token even before its group
                            // is complete, without inventing a declaration.
                            let at =
                                if declares_command(name) && bytes.get(arg_start) == Some(&b'{') {
                                    skip_space(source, arg_start + 1)
                                } else {
                                    arg_start
                                };
                            if bytes.get(at) == Some(&b'\\') {
                                control_operands.insert(at);
                            }
                        }
                        if let Some((start, end)) = &expanded_target {
                            control_operands.extend([*start, end.start]);
                        }
                    }
                    // futurelet replays its two lookahead tokens, which can
                    // execute; only its destination is a declaration operand.
                    if operands_known
                        && (name == "let" || declares_command_copy(name) || name == "cslet")
                    {
                        let after_target = expanded_target
                            .as_ref()
                            .map(|(_, end)| end.end)
                            .or_else(|| {
                                parsed
                                    .as_ref()
                                    .filter(|_| name != "let")
                                    .map(|(_, end)| *end)
                            })
                            .or_else(|| declaration.as_ref().map(|span| span.end))
                            .or_else(|| {
                                (name == "cslet")
                                    .then(|| control_word(source, arg_start).map(|span| span.end))
                                    .flatten()
                            });
                        if let Some(after_target) = after_target {
                            let mut at = skip_space(source, after_target);
                            if name == "let" && bytes.get(at) == Some(&b'=') {
                                at = skip_space(source, at + 1);
                            }
                            let operand = if name == "let" {
                                // Primitive let copies one token, including a
                                // brace; a following group is not its source.
                                at
                            } else if bytes.get(at) == Some(&b'{') {
                                // The source group may still be unfinished;
                                // its first token is already the name operand.
                                skip_space(source, at + 1)
                            } else {
                                at
                            };
                            if bytes.get(operand) == Some(&b'\\') {
                                control_operands.insert(operand);
                            } else if name == "let"
                                && matches!(bytes.get(operand), Some(b'{' | b'}' | b'$'))
                                && !result.commands.iter().any(|previous| {
                                    !control_operands.contains(&previous.span.start)
                                        && !punctuation_operands
                                            .contains(&skip_space(source, previous.span.end))
                                        && crate::symbols::commands::reads_brace(
                                            source, previous, operand,
                                        )
                                })
                            {
                                punctuation_operands.insert(operand);
                            }
                        }
                    }
                    if let Some((start, end)) = &expanded_target {
                        assigned_controls.insert(
                            source[skip_space(source, start + r"\csname".len())..end.start]
                                .to_owned(),
                        );
                    }
                    if let Some(span) = declaration {
                        assigned_controls.insert(source[span.start + 1..span.end].to_owned());
                        result.definitions.push(Entry {
                            name: source[span.start + 1..span.end].into(),
                            span,
                            detail: format!("Defined with \\{name}"),
                            is_variable: declares_register(name),
                        });
                    }
                    let environment_argument = matches!(name, "begin" | "end")
                        .then(|| environment_name_argument(source, command.span.end))
                        .flatten();
                    if let Some(arg) = environment_argument.as_ref().or(command.argument.as_ref()) {
                        let entry = || Entry {
                            name: arg.value.trim().into(),
                            span: trimmed_span(source, arg.span.clone()),
                            detail: format!("\\{name}"),
                            is_variable: false,
                        };
                        if declares_environment(name) && is_static_key(&arg.value) {
                            assigned_controls
                                .extend([arg.value.clone(), format!("end{}", arg.value)]);
                            result.environments.push(Entry {
                                detail: format!("Defined with \\{name}"),
                                ..entry()
                            });
                        }
                        match name {
                            "newcounter" | "newaliascnt" if is_static_key(&arg.value) => {
                                result.counters.push(Entry {
                                    is_variable: true,
                                    ..entry()
                                });
                            }
                            "label" if is_static_reference_key(&arg.value) => {
                                result.labels.push(entry())
                            }
                            "bibitem" if is_static_citation_key(&arg.value) => {
                                result.citations.push(entry())
                            }
                            "begin" => {
                                let env = arg.value.as_str();
                                if is_verbatim_environment(env) {
                                    let body_start = arg.outer_span.end;
                                    let ending = format!("\\end{{{env}}}");
                                    let verb_environment = (env == "VerbEnv").then(|| {
                                        crate::parser::fancyvrb::verb_environment_end(
                                            source, body_start,
                                        )
                                    });
                                    let end = if let Some(environment) = &verb_environment {
                                        environment.closing
                                    } else if crate::parser::fancyvrb::is_environment(env) {
                                        crate::parser::fancyvrb::environment_end(
                                            source, body_start, env,
                                        )
                                    } else if env == "minted" {
                                        crate::parser::minted::environment_end(source, body_start)
                                    } else if env == "lstlisting" {
                                        crate::parser::listings::environment_end(source, body_start)
                                    } else {
                                        source[body_start..]
                                            .find(&ending)
                                            .map(|relative| body_start + relative + ending.len())
                                    };
                                    if let Some(end) = end {
                                        i = end;
                                    } else if let Some(environment) = verb_environment {
                                        i = environment.consumed;
                                        // An invalid VerbEnv closing line is raw data,
                                        // but later source executes in its still-open group.
                                        if operands_known && environment_argument.is_some() {
                                            fancyverb_context.enter_environment(env);
                                        }
                                        environments.push((
                                            true,
                                            arg.clone(),
                                            start..arg.outer_span.end,
                                        ));
                                    } else {
                                        result.fix(
                                            text,
                                            arg.span.clone(),
                                            format!("Unclosed environment `{env}`"),
                                            format!("Close environment `{env}`"),
                                            environment_end(text, env),
                                        );
                                        i = source.len();
                                    }
                                    result.ignored.push(body_start..i);
                                    if env == "comment" {
                                        result.comments.push(body_start..i);
                                    }
                                    if end.is_some()
                                        && (crate::parser::fancyvrb::is_environment(env)
                                            || env == "minted")
                                    {
                                        // FancyVerb discards the rest of its closing line.
                                        // Keep the marker boundary in the first raw span
                                        // for folding and linked editing of delimiters.
                                        let after = source[i..]
                                            .find('\n')
                                            .map_or(source.len(), |newline| i + newline + 1);
                                        if i < after {
                                            result.ignored.push(i..after);
                                        }
                                        i = after;
                                    }
                                } else {
                                    if operands_known && environment_argument.is_some() {
                                        fancyverb_context.enter_environment(env);
                                    }
                                    environments.push((
                                        true,
                                        arg.clone(),
                                        start..arg.outer_span.end,
                                    ));
                                }
                            }
                            "end" => {
                                if operands_known && environment_argument.is_some() {
                                    fancyverb_context.leave_environment(&arg.value);
                                }
                                environments.push((false, arg.clone(), start..arg.outer_span.end));
                            }
                            _ => {}
                        }
                    }
                    if native_resource {
                        if let Some((_, end)) = parsed.as_ref() {
                            i = *end;
                        }
                    }
                    if url_argument {
                        if let Some((argument, end)) = parsed {
                            result.ignored.push(argument.span);
                            i = end;
                        }
                    }
                    if let Some(span) =
                        crate::symbols::named_commands::declaration(source, &command)
                    {
                        assigned_controls.insert(source[span].to_owned());
                    }
                    result.commands.push(command);
                    match reader {
                        Some(Some(reader)) if reader.opaque => {
                            result.ignored.push(reader.start..reader.end);
                            i = i.max(reader.end);
                        }
                        Some(Some(reader)) if reader.bracket => {
                            raw_resource_end = reader.end;
                            result.raw_resource_brackets.insert(reader.start);
                        }
                        Some(Some(reader)) => {
                            if verbatim_file_group(source, reader.start).is_none() {
                                braces.push(reader.start);
                            }
                            i = i.max(reader.end);
                        }
                        Some(None) => {}
                        None => {
                            if let Some(at) = inline_tokenization.resource_bracket(
                                source,
                                &result,
                                &control_operands,
                            ) {
                                raw_resource_end = verbatim_file_bracket(source, at)
                                    .map_or(source.len(), |(_, end)| end);
                                result.raw_resource_brackets.insert(at);
                            }
                        }
                    }
                }
                _ => i += 1,
            }
        }
        let comment_at_end =
            result.ignored.last().is_some_and(|span| {
                span.end == source.len() && source[span.clone()].starts_with('%')
            }) || result.literal_tests.last().is_some_and(|span| {
                span.end == source.len()
                    && crate::symbols::string_tests::ends_in_comment(source, span)
            });
        for start in braces {
            result.fix(
                text,
                start..start + 1,
                "Unclosed brace".into(),
                "Insert missing `}`".into(),
                TextEdit {
                    range: text.range(source.len(), source.len()),
                    new_text: if comment_at_end {
                        format!("{}}}", text.newline())
                    } else {
                        "}".into()
                    },
                },
            );
        }
        result.environment_fixes = environment_stack_fixes(text, environments);
        result.diagnostics.extend(
            result
                .environment_fixes
                .iter()
                .map(|fix| fix.diagnostic.clone()),
        );
        result
            .fixes
            .extend(result.environment_fixes.iter().cloned());
        result.control_operands = control_operands;
        result.punctuation_operands = punctuation_operands;
        for (index, commands) in result.commands.windows(3).enumerate() {
            let [expand, declaration, target] = commands else {
                unreachable!()
            };
            if expand.name == "expandafter"
                && !result.control_operands.contains(&expand.span.start)
                && !result.control_operands.contains(&declaration.span.start)
                && (declares_command(&declaration.name)
                    || matches!(
                        declaration.name.as_str(),
                        "def" | "gdef" | "edef" | "xdef" | "let" | "futurelet"
                    ))
                && skip_space(source, expand.span.end) == declaration.span.start
                && skip_space(source, declaration.span.end) == target.span.start
            {
                if let Some(span) = literal_csname(source, &result.commands, index + 2) {
                    result.definitions.push(Entry {
                        name: source[span.clone()].into(),
                        span,
                        detail: format!("Defined with \\{}", declaration.name),
                        is_variable: declares_register(&declaration.name),
                    });
                }
            }
        }
        for command in result
            .commands
            .iter()
            .filter(|command| !result.control_operands.contains(&command.span.start))
        {
            if let Some(span) = crate::symbols::named_commands::declaration(source, command) {
                result.definitions.push(Entry {
                    name: source[span.clone()].into(),
                    span,
                    detail: format!("Defined with \\{}", command.name),
                    is_variable: false,
                });
            }
            if declares_toggle(&command.name) {
                if let Some(argument) = &command.argument {
                    let span = trimmed_span(source, argument.span.clone());
                    if is_static_key(&source[span.clone()]) {
                        result.toggles.push(Entry {
                            name: source[span.clone()].into(),
                            span,
                            detail: format!("Defined with \\{}", command.name),
                            is_variable: false,
                        });
                    }
                }
            }
            if let Some(span) = conditional_declaration(source, &result.commands, command) {
                result.conditionals.push(Entry {
                    name: source[span.clone()].into(),
                    span,
                    detail: format!("Defined with \\{}", command.name),
                    is_variable: false,
                });
            }
        }
        result.definitions.sort_by_key(|entry| entry.span.start);
        crate::symbols::saved_text::index(source, &mut result);
        crate::symbols::named_colors::index(source, &mut result);
        result
    }

    pub fn bibtex(text: &Text) -> Self {
        let mut result = Self::default();
        let source = &text.source;
        let bytes = source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                result.comments.push(start..i);
                continue;
            }
            if bytes[i] != b'@' {
                i += 1;
                continue;
            }
            let entry_start = i;
            let comment_start = result.comments.len();
            i += 1;
            while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            let kind_start = i;
            // A second entry marker belongs to the next construct, even when
            // whitespace follows an unfinished header's first marker.
            if bytes.get(i) != Some(&b'@') {
                i = bib_identifier_end(source, i);
            }
            let kind = &source[kind_start..i];
            let kind_span = kind_start..i;
            i = bib_space(source, i, &mut result.comments);
            result
                .bibliography_completion
                .types
                .push(crate::bibliography::bib_completion::Type {
                    prefix_start: entry_start + 1,
                    span: kind_span,
                    has_body: matches!(bytes.get(i), Some(b'{' | b'(')),
                    entry_span: None,
                    key: None,
                });
            let Some(&open @ (b'{' | b'(')) = bytes.get(i) else {
                continue;
            };
            let close = if open == b'{' { b'}' } else { b')' };
            let content = i + 1;
            let special = kind.is_empty()
                || kind.as_bytes().first().is_some_and(u8::is_ascii_digit)
                || matches!(
                    kind.to_ascii_lowercase().as_str(),
                    "comment" | "string" | "preamble"
                );
            let header = (!special)
                .then(|| bib_literal_header(source, content, close))
                .flatten();
            // A proven literal header can contain parentheses, including the
            // entry's closing delimiter. Start balancing after its comma,
            // or at the final brace of a fieldless entry.
            // Unproven headers keep the conservative delimiter recovery.
            let body_start = header.as_ref().map_or(content, |(_, delimiter)| {
                delimiter + usize::from(bytes[*delimiter] == b',')
            });
            if header.is_some() {
                bib_space(source, content, &mut result.comments);
            }
            let (end, closed) = bib_entry_end(source, body_start, close, &mut result.comments);
            result
                .bibliography_completion
                .types
                .last_mut()
                .unwrap()
                .entry_span = Some(entry_start..end);
            if closed
                && !kind.is_empty()
                && !kind.as_bytes().first().is_some_and(u8::is_ascii_digit)
            {
                result
                    .bibliography_entries
                    .push((entry_start..end, kind.eq_ignore_ascii_case("comment")));
            }
            if kind.eq_ignore_ascii_case("comment") {
                result.comments.truncate(comment_start);
                result.comments.push(entry_start..end);
            }
            let body_end = if closed { end - 1 } else { end };
            if kind.eq_ignore_ascii_case("string") {
                result
                    .bibliography_strings
                    .declaration(source, content, body_end, closed);
            } else if kind.eq_ignore_ascii_case("preamble") {
                result
                    .bibliography_strings
                    .preamble(source, content, body_end);
            }
            if !special {
                let key_start = skip_space(source, content);
                let mut key_end = key_start;
                while key_end < end && !matches!(bytes[key_end], b',' | b'}' | b')' | b'\n') {
                    key_end += 1;
                }
                let (span, key_end) =
                    header.unwrap_or_else(|| (trimmed_span(source, key_start..key_end), key_end));
                let has_fields = key_end < end && bytes.get(key_end) == Some(&b',');
                let empty_body = closed
                    && open == b'{'
                    && key_end == body_end
                    && is_static_citation_key(&source[span.clone()]);
                if !span.is_empty() && (has_fields || empty_body) {
                    result.bibliography_completion.types.last_mut().unwrap().key =
                        Some(span.clone());
                    let metadata = if has_fields {
                        result.bibliography_fields(source, key_end + 1, body_end, kind)
                    } else {
                        [None, None, None]
                    };
                    let mut detail = format!("BibTeX {kind}");
                    for (label, value) in ["Title", "Author", "Year"].into_iter().zip(metadata) {
                        if let Some(value) = value {
                            detail.push_str(&format!(" · {label}: {value}"));
                        }
                    }
                    result.citations.push(Entry {
                        name: source[span.clone()].into(),
                        span,
                        detail,
                        is_variable: false,
                    });
                }
            }
            i = end;
        }
        result.ignored = result.comments.clone();
        (
            result.bibliography_aliases,
            result.bibliography_alias_values,
        ) = crate::bibliography::bib_aliases::index(source, &result);
        result
    }

    fn bibliography_fields(
        &mut self,
        source: &str,
        mut i: usize,
        end: usize,
        kind: &str,
    ) -> [Option<String>; 3] {
        let bytes = source.as_bytes();
        let mut metadata = [None, None, None];
        let mut seen = [false; 3];
        let mut links = std::collections::BTreeMap::new();
        let mut completion = crate::bibliography::bib_completion::Entry {
            kind: kind.to_ascii_lowercase(),
            ..Default::default()
        };
        while i <= end {
            let gap_start = i;
            i = skip_space(source, i).min(end);
            completion.gaps.push(gap_start..i);
            if i == end {
                break;
            }
            if bytes.get(i) == Some(&b',') {
                i += 1;
                continue;
            }
            let start = i;
            i = bib_identifier_end(source, i).min(end);
            if !crate::bibliography::bib_strings::identifier(&source[start..i]) {
                break;
            }
            let name = source[start..i].to_ascii_lowercase();
            let span = start..i;
            i = skip_space(source, i);
            completion
                .fields
                .push(crate::bibliography::bib_completion::Field {
                    name: name.clone(),
                    range: span.clone(),
                    span,
                    assigned: bytes.get(i) == Some(&b'='),
                    value_valid: false,
                });
            if bytes.get(i) != Some(&b'=') {
                break;
            }
            self.bibliography_strings.operator_spans.push(i..i + 1);
            let value_start = i + 1;
            let start = skip_space(source, value_start);
            let value_end = bib_value_end(source, start, end);
            let field = completion.fields.last_mut().unwrap();
            field.range.end =
                field.span.start + source[field.span.start..value_end].trim_end().len();
            field.value_valid =
                self.bibliography_strings
                    .expression(source, value_start, value_end);
            self.bibliography_strings
                .selection_spans
                .push(field.span.start..value_end);
            i = value_end + 1;
            if matches!(name.as_str(), "url" | "doi") {
                match links.entry(name.clone()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(crate::bibliography::bib_links::field(
                            source, &name, start, value_end,
                        ));
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        // Duplicate fields are ambiguous even when only one
                        // of their values is a parseable literal link.
                        entry.insert(None);
                    }
                }
                continue;
            }
            let metadata_index = match name.as_str() {
                "title" => Some(0),
                "author" => Some(1),
                "year" => Some(2),
                _ => None,
            };
            if let Some(index) = metadata_index {
                metadata[index] = if seen[index] {
                    None
                } else {
                    bibliography_preview(source, start, value_end, name == "year")
                };
                seen[index] = true;
                continue;
            }
            if !matches!(
                name.as_str(),
                "crossref" | "xref" | "xdata" | "related" | "entryset"
            ) {
                continue;
            }
            let Some(argument) = bib_literal(source, start, value_end) else {
                continue;
            };
            // Macro concatenations, escaped values and nested TeX groups cannot
            // be renamed safely without evaluating BibTeX's string language.
            if argument.value.contains(['\\', '{', '}', '%', '#']) {
                continue;
            }
            let multiple = matches!(name.as_str(), "xdata" | "related" | "entryset");
            let mut offset = argument.span.start;
            for key in if multiple {
                argument.value.split(',').collect::<Vec<_>>()
            } else {
                vec![argument.value.as_str()]
            } {
                let span = trimmed_span(source, offset..offset + key.len());
                if is_static_citation_key(&source[span.clone()]) {
                    self.citation_references.push(Entry {
                        name: source[span.clone()].into(),
                        span,
                        detail: format!("BibTeX {name} reference"),
                        is_variable: false,
                    });
                }
                offset += key.len() + 1;
            }
            self.citation_fields.push(CitationField { name, argument });
        }
        self.bibliography_links
            .extend(links.into_values().flatten());
        self.bibliography_completion.entries.push(completion);
        metadata
    }

    pub fn is_ignored(&self, offset: usize) -> bool {
        self.ignored
            .iter()
            .any(|span| span.start <= offset && offset < span.end)
    }
}

pub(crate) fn syntax_fix(
    text: &Text,
    span: Range<usize>,
    message: String,
    title: String,
    edit: TextEdit,
) -> QuickFix {
    let mut diagnostic = diagnostic(text, span, message);
    diagnostic.code = Some(NumberOrString::String("syntax".into()));
    QuickFix {
        title,
        diagnostic,
        edit,
        additional_edits: vec![],
    }
}

/// Match ordered environment calls. The invocation span includes an indirect
/// command's complete spelling so removing an unexpected end never leaves it behind.
pub(crate) fn environment_stack_fixes(
    text: &Text,
    calls: impl IntoIterator<Item = (bool, Argument, Range<usize>)>,
) -> Vec<QuickFix> {
    let mut stack: Vec<(String, Range<usize>)> = Vec::new();
    let mut fixes = Vec::new();
    for (begin, argument, invocation) in calls {
        let env = argument.value;
        if begin {
            stack.push((env, argument.span));
        } else if let Some(index) = stack.iter().rposition(|(name, _)| *name == env) {
            for (name, span) in stack.drain(index + 1..) {
                fixes.push(syntax_fix(
                    text,
                    span,
                    format!("Unclosed environment `{name}` before \\end{{{env}}}"),
                    format!("Close environment `{name}` before `{env}`"),
                    TextEdit {
                        range: text.range(invocation.start, invocation.start),
                        new_text: format!("\\end{{{name}}}{}", text.newline()),
                    },
                ));
            }
            stack.pop();
        } else {
            fixes.push(syntax_fix(
                text,
                argument.span,
                format!("Unexpected \\end{{{env}}}"),
                format!("Remove unexpected \\end{{{env}}}"),
                TextEdit {
                    range: text.range(invocation.start, invocation.end),
                    new_text: String::new(),
                },
            ));
        }
    }
    for (name, span) in stack {
        fixes.push(syntax_fix(
            text,
            span,
            format!("Unclosed environment `{name}`"),
            format!("Close environment `{name}`"),
            environment_end(text, &name),
        ));
    }
    fixes
}

fn environment_end(text: &Text, name: &str) -> TextEdit {
    let source = &text.source;
    let newline = text.newline();
    let prefix = if source.ends_with('\n') { "" } else { newline };
    TextEdit {
        range: text.range(source.len(), source.len()),
        new_text: format!("{prefix}\\end{{{name}}}{newline}"),
    }
}

/// BibTeX identifiers are byte tokens. Punctuation and non-ASCII bytes
/// belong to the name; a leading ASCII digit is rejected by the caller.
pub(crate) fn bib_identifier_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut end = start;
    while let Some(&byte) = bytes.get(end) {
        if byte.is_ascii_control()
            || byte.is_ascii_whitespace()
            || matches!(
                byte,
                b'"' | b'#' | b'%' | b'\'' | b'(' | b')' | b',' | b'=' | b'{' | b'}'
            )
        {
            break;
        }
        end += 1;
    }
    end
}

/// Prove a literal key followed only by whitespace and a comma, or the final
/// brace of a fieldless entry. BibTeX requires a comma for parenthesized entries.
/// A closing parenthesis may belong to the key; a line break may separate it
/// from the comma. Never cross another token, group or comment to find it.
fn bib_literal_header(source: &str, content: usize, close: u8) -> Option<(Range<usize>, usize)> {
    let start = skip_space(source, content);
    let end = source[start..]
        .find(|ch: char| ch.is_ascii_whitespace() || matches!(ch, ',' | '{' | '}'))
        .map(|offset| start + offset)?;
    let span = trimmed_span(source, start..end);
    if !is_static_citation_key(&source[span.clone()]) {
        return None;
    }
    let bytes = source.as_bytes();
    let mut comma = end;
    while bytes.get(comma).is_some_and(u8::is_ascii_whitespace) {
        comma += 1;
    }
    (bytes.get(comma) == Some(&b',') || close == b'}' && bytes.get(comma) == Some(&b'}'))
        .then_some((span, comma))
}

/// Literal citation keys also allow quotes. Keep label and general identifier
/// validation separate, while sharing the remaining static-name restrictions.
pub(crate) fn is_static_citation_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .split('"')
            .filter(|part| !part.is_empty())
            .all(is_static_reference_key)
}

/// Record only the comments consumed as whitespace, never literal contents.
fn bib_space(source: &str, start: usize, comments: &mut Vec<Range<usize>>) -> usize {
    let end = skip_space(source, start);
    let mut at = start;
    while let Some(offset) = source[at..end].find('%') {
        let begin = at + offset;
        at = source[begin..end]
            .find('\n')
            .map_or(end, |offset| begin + offset);
        comments.push(begin..at);
    }
    end
}

fn bib_entry_end(
    source: &str,
    start: usize,
    close: u8,
    comments: &mut Vec<Range<usize>>,
) -> (usize, bool) {
    let bytes = source.as_bytes();
    let mut depth = 0u32;
    let mut quoted = false;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'%' if depth == 0 && !quoted => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                comments.push(start..i);
                continue;
            }
            b'"' if depth == 0 => quoted = !quoted,
            b'{' => depth += 1,
            b'}' if depth > 0 => depth -= 1,
            b if b == close && depth == 0 && !quoted => return (i + 1, true),
            _ => {}
        }
        i += 1;
    }
    (bytes.len(), false)
}

pub(crate) fn bib_value_end(source: &str, start: usize, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut i = start;
    while i < end {
        match bytes[i] {
            b'\\' => {
                i = (i + 2).min(end);
                continue;
            }
            b'%' if depth == 0 && !quoted => {
                while i < end && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' if depth == 0 => quoted = !quoted,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 && !quoted => return i,
            _ => {}
        }
        i += 1;
    }
    end
}

fn bib_literal(source: &str, start: usize, end: usize) -> Option<Argument> {
    bib_literal_mode(source, start, end, true)
}

pub(crate) fn bib_literal_mode(
    source: &str,
    start: usize,
    end: usize,
    allow_incomplete: bool,
) -> Option<Argument> {
    let bytes = source.as_bytes();
    let (argument, after) = match bytes.get(start)? {
        b'{' => group_mode(source, start, b'{', b'}', false, true).or_else(|| {
            (allow_incomplete && end == source.len()).then(|| {
                (
                    Argument {
                        value: source[start + 1..end].into(),
                        span: start + 1..end,
                        outer_span: start..end,
                    },
                    end,
                )
            })
        })?,
        b'"' => {
            let mut i = start + 1;
            let mut depth = 0usize;
            while i < end {
                match bytes[i] {
                    b'\\' => {
                        i = (i + 2).min(end);
                        continue;
                    }
                    b'{' => depth += 1,
                    b'}' => depth = depth.saturating_sub(1),
                    b'"' if depth == 0 => break,
                    _ => {}
                }
                i += 1;
            }
            if bytes.get(i) != Some(&b'"') && (!allow_incomplete || end != source.len()) {
                return None;
            }
            (
                Argument {
                    value: source[start + 1..i].into(),
                    span: start + 1..i,
                    outer_span: start..(i + 1).min(end),
                },
                (i + 1).min(end),
            )
        }
        _ => return None,
    };
    (after <= end && skip_space(source, after) >= end).then_some(argument)
}

/// Preview source literals without evaluating BibTeX strings or TeX commands.
fn bibliography_preview(source: &str, start: usize, end: usize, numeric: bool) -> Option<String> {
    let raw = source[start..end].trim();
    let value = if numeric && !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        raw.to_owned()
    } else {
        bib_literal_mode(source, start, end, false)?.value
    };
    let mut words = value.split_whitespace();
    let first = words.next()?;
    let mut characters = first
        .chars()
        .chain(words.flat_map(|word| std::iter::once(' ').chain(word.chars())));
    let mut preview: String = characters.by_ref().take(200).collect();
    if characters.next().is_some() {
        preview.push('…');
    }
    Some(preview)
}

pub fn trimmed_span(source: &str, span: Range<usize>) -> Range<usize> {
    let value = &source[span.clone()];
    let start = span.start + value.len() - value.trim_start().len();
    start..start + value.trim().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_groups_preserve_each_reader_mode_and_unicode_boundaries() {
        crate::analysis::AnalysisContext::lexical().run(|_| {
            for source in [
                "{中😀{nested}\\}tail}",
                "{a% } ignored\r\n b}",
                "[a[b]{c]d}e]tail",
                "(a{b)c}d)tail",
                "{unfinished\\}",
                "[unfinished{]",
                "{\\\\}tail",
            ] {
                for (at, _) in source.char_indices() {
                    for (open, close) in [(b'{', b'}'), (b'[', b']'), (b'(', b')')] {
                        for comments in [false, true] {
                            for nested in [false, true] {
                                let view = |value: Option<(Argument, usize)>| {
                                    value.map(|(a, end)| (a.value, a.span, a.outer_span, end))
                                };
                                let expected = view(uncached_group_mode(
                                    source, at, open, close, comments, nested,
                                ));
                                for _ in 0..2 {
                                    assert_eq!(
                                        view(group_mode(source, at, open, close, comments, nested)),
                                        expected,
                                        "{source:?}, {at}, {comments}, {nested}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn lexical_groups_recheck_reused_scratch_storage_and_incomplete_reads() {
        crate::analysis::AnalysisContext::lexical().run(|_| {
            let mut source = String::with_capacity(64);
            let pointer = source.as_ptr();
            for (text, value) in [
                ("{abc}tail", Some("abc")),
                ("{a}c}tail", Some("a")),
                ("{a{c}tail", None),
                ("{abc}tail", Some("abc")),
                ("[abc]tail", None),
            ] {
                source.clear();
                source.push_str(text);
                assert_eq!(source.as_ptr(), pointer);
                assert_eq!(
                    group(&source, 0, b'{', b'}')
                        .map(|(a, _)| a.value)
                        .as_deref(),
                    value
                );
            }
        });
    }

    #[test]
    fn comments_escapes_and_verbatim_are_opaque() {
        let text = Text::new(
            r"\% {ok} % } \label{fake}
\verb|{\label{fake}}| \begin{verbatim}
} \label{fake}
\end{verbatim}
\label{real} \section[short]{A {nested} title}"
                .into(),
        );
        let syntax = Syntax::latex(&text);
        assert!(syntax.diagnostics.is_empty(), "{:?}", syntax.diagnostics);
        assert_eq!(syntax.labels.len(), 1);
        assert_eq!(syntax.labels[0].name, "real");
        assert_eq!(
            syntax
                .commands
                .last()
                .unwrap()
                .argument
                .as_ref()
                .unwrap()
                .value,
            "A {nested} title"
        );
    }

    #[test]
    fn malformed_environments_and_braces_recover() {
        let syntax = Syntax::latex(&Text::new(r"\begin{a}\begin{b}\end{a}} {".into()));
        assert_eq!(syntax.diagnostics.len(), 3);
        assert!(syntax
            .diagnostics
            .iter()
            .any(|d| d.message.contains("environment `b`")));
    }

    #[test]
    fn bare_environment_names_balance_and_unexpected_end_fixes_keep_following_text() {
        for (begin, end) in [("x", "x"), ("x", "{x}"), ("{x}", "x")] {
            let source = format!("\\begin% name\r\n{begin}Body\\end {end}Tail");
            let syntax = Syntax::latex(&Text::new(source));
            assert!(syntax.diagnostics.is_empty(), "{:?}", syntax.diagnostics);
        }
        for end in ["x", "{% name\r\nx% keep\r\n}"] {
            let source = format!("\\end {end}Tail");
            let text = Text::new(source);
            let syntax = Syntax::latex(&text);
            assert_eq!(syntax.diagnostics.len(), 1);
            let edit = &syntax.fixes[0].edit;
            assert_eq!(&text.source[text.offset(edit.range.end).unwrap()..], "Tail");
            assert_eq!(edit.new_text, "");
        }
    }

    #[test]
    fn bibtex_skips_fields_and_special_entries() {
        let text = Text::new("@comment{not,a key}\n@article{中文2026, title={An @book{fake, title} example}}\n@book(other, title=\"Text\")".into());
        let syntax = Syntax::bibtex(&text);
        assert_eq!(
            syntax
                .citations
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["中文2026", "other"]
        );
    }

    #[test]
    fn inline_options_and_unicode_delimiters_preserve_following_commands() {
        let text = Text::new(
            r"\lstinline[language=TeX]|{\label{fake}}| \verb§}\label{fake}§ \label{real}".into(),
        );
        let syntax = Syntax::latex(&text);
        assert!(syntax.diagnostics.is_empty());
        assert_eq!(syntax.labels.len(), 1);
        assert_eq!(syntax.labels[0].name, "real");
        for span in syntax.ignored {
            assert!(text.source.is_char_boundary(span.start));
            assert!(text.source.is_char_boundary(span.end));
        }
    }

    #[test]
    fn incomplete_unicode_inputs_keep_valid_spans() {
        let fragments = [
            "中", "😀", "é", "\\", "{", "}", "%", "\n", "[", "]", "@", "(", ")", ",", "\"", " ",
            "label", "begin", "end", "verb", "article", "input", "\\input ",
        ];
        let mut seed = 12345u32;
        for _ in 0..512 {
            let mut source = String::new();
            for _ in 0..64 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                source.push_str(fragments[seed as usize % fragments.len()]);
            }
            let text = Text::new(source);
            for syntax in [Syntax::latex(&text), Syntax::bibtex(&text)] {
                let spans = syntax
                    .commands
                    .iter()
                    .map(|cmd| cmd.span.clone())
                    .chain(
                        syntax
                            .commands
                            .iter()
                            .filter_map(|cmd| cmd.argument.as_ref())
                            .flat_map(|arg| [arg.span.clone(), arg.outer_span.clone()]),
                    )
                    .chain(
                        syntax
                            .labels
                            .iter()
                            .chain(&syntax.citations)
                            .chain(&syntax.citation_references)
                            .chain(&syntax.definitions)
                            .chain(&syntax.environments)
                            .chain(&syntax.counters)
                            .chain(&syntax.conditionals)
                            .chain(&syntax.toggles)
                            .map(|entry| entry.span.clone()),
                    )
                    .chain(syntax.ignored.iter().cloned());
                for span in spans {
                    assert!(text.source.get(span).is_some());
                }
                for diagnostic in syntax.diagnostics {
                    assert!(diagnostic.range.start <= diagnostic.range.end);
                    assert!(text.offset(diagnostic.range.start).is_some());
                    assert!(text.offset(diagnostic.range.end).is_some());
                }
            }
        }
    }
}
