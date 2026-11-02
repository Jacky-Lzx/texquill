//! Shared command meanings, parameter readers and static forwarding analysis.
use crate::{
    parser::syntax::{self, required_token, token, Entry},
    project::{
        catalog::COMMANDS,
        workspace::{Document, Workspace},
    },
};
use lsp_types::Url;
mod facts;
mod memo;
pub(crate) use facts::Declarations;
mod tokenization;
pub(crate) use tokenization::InlineTokenization;

mod builtins;
pub(crate) use builtins::HOOK_CONDITIONS;
use builtins::{builtin, kernel_hook_parameters};
mod declarations;
use declarations::{
    body_alias, definition, definition_arguments_start, describe_definition,
    environment_start_signature, source_definition, static_invocation_name,
};
pub(crate) use declarations::{
    environment_bodies, literal_copy_target, literal_invocation_name, source_alias,
};

use std::{
    borrow::Cow,
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
};

#[derive(Clone, PartialEq)]
pub(crate) enum Kind {
    Required,
    RequiredGroup,
    CitationKeys(crate::bibliography::citations::Style),
    CommandToken,
    RequiredBracket,
    Optional,
    UnstarredOptional,
    Star,
    Flag(String),
    Delimited {
        open: String,
        close: String,
        optional: bool,
    },
}

#[derive(Clone, PartialEq)]
pub(crate) struct Parameter {
    pub(crate) obey_spaces: bool,
    pub(crate) kind: Kind,
    pub(crate) label: String,
}

#[derive(Clone, PartialEq)]
pub(crate) struct Signature {
    pub(crate) name: String,
    pub(crate) parameters: Vec<Parameter>,
    pub(crate) documentation: String,
    pub(crate) starred: bool,
    pub(crate) nested_brackets: bool,
}

impl Signature {
    pub(crate) fn label(&self) -> String {
        format!(
            "\\{}{}{}",
            self.name,
            if self
                .parameters
                .first()
                .is_some_and(|parameter| starts_letter(&parameter.label))
            {
                " "
            } else {
                ""
            },
            self.parameters
                .iter()
                .map(|parameter| parameter.label.as_str())
                .collect::<String>()
        )
    }
}

pub(crate) fn starts_letter(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphabetic() || ch == '@')
}

// Native wrappers execute text only with their original punctuation readers.
// Both delimiter types participate because nested wrappers alternate them.
// Keep dependencies before their forwarders so invalidation propagates.
const BIBLATEX_TEXT_DEPENDENCIES: &[(&str, &[&str])] = &[
    (
        "mkbibparens",
        &[
            "bibopenparen",
            "bibcloseparen",
            "bibleftparen",
            "bibrightparen",
            "bibleftbracket",
            "bibrightbracket",
        ],
    ),
    (
        "mkbibbrackets",
        &[
            "bibopenbracket",
            "bibclosebracket",
            "bibleftparen",
            "bibrightparen",
            "bibleftbracket",
            "bibrightbracket",
        ],
    ),
    ("parentext", &["mkbibparens"]),
    ("brackettext", &["mkbibbrackets"]),
    ("bibfootnotewrapper", &["bibsentence"]),
    (
        "blxmkbibnote",
        &[
            "bibfootnotewrapper",
            "blx@blxinit",
            "blx@setsfcodes",
            "blx@postpunct@agroup",
        ],
    ),
    (
        "mkbibfootnote",
        &[
            "footnote",
            "@footnotetext",
            "@mpfootnotetext",
            "@makefntext",
            "protecting",
            "blxmkbibnote",
            "blx@mkbibfootnote",
            "mkbibparens",
            "unspace",
        ],
    ),
    (
        "mkbibfootnotetext",
        &[
            "footnotetext",
            "@footnotetext",
            "@mpfootnotetext",
            "@makefntext",
            "protecting",
            "blxmkbibnote",
            "blx@mkbibfootnote",
            "mkbibparens",
            "unspace",
        ],
    ),
];

pub(crate) type StoredCodeAliases = BTreeMap<String, BTreeSet<&'static str>>;

/// Environment declarations also define directly callable start/end controls.
/// These edges only describe possible forwarding, including captured copies;
/// the boolean marks a start control's additional, unconfirmed operand grammar.
fn environment_forwarders(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
) -> Vec<(String, String, bool)> {
    if let Some((target, _)) = literal_copy_target(source, parsed, entry, true) {
        return vec![
            (entry.name.clone(), target.clone(), false),
            (format!("end{}", entry.name), format!("end{target}"), false),
        ];
    }
    let Some((begin, end)) = environment_bodies(source, parsed, entry) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    if let Some(target) = body_alias(source, &begin) {
        let consuming = CommandShape::environment_definition(source, parsed, entry)
            .is_none_or(|shape| !shape.takes_no_arguments());
        result.push((entry.name.clone(), target, consuming));
    }
    if let Some(target) = body_alias(source, &end) {
        result.push((format!("end{}", entry.name), target, false));
    }
    result
}

/// Possible forwarding/copy edges only revoke string meanings. They never
/// certify a public signature or assume a declaration has already executed.
pub(crate) fn possible_stored_code_aliases<'a>(
    documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
) -> Option<StoredCodeAliases> {
    possible_builtin_aliases(
        documents,
        [
            "AddToHook",
            "AddToHookWithArguments",
            "AddToHookNext",
            "AddToHookNextWithArguments",
            "AtBeginDocument",
            "AtEndDocument",
            "mkbibfootnote",
            "mkbibfootnotetext",
        ]
        .into_iter()
        .chain(HOOK_CONDITIONS.iter().copied())
        .chain(crate::symbols::macro_mutations::COMMANDS.iter().copied()),
        false,
    )
}

pub(crate) fn possible_builtin_aliases<'a>(
    documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
    builtins: impl IntoIterator<Item = &'static str>,
    argument_consuming: bool,
) -> Option<StoredCodeAliases> {
    let mut users = BTreeMap::<String, Vec<String>>::new();
    let mut work = 0usize;
    for (source, parsed) in documents {
        work += source.len()
            + (parsed.definitions.len()
                + usize::from(argument_consuming) * parsed.environments.len())
                * 64;
        if work > 32 * 1024 * 1024 {
            return None;
        }
        for entry in &parsed.definitions {
            if let Some(target) = source_alias(source, parsed, entry)
                .or_else(|| {
                    argument_consuming
                        .then(|| {
                            command_definition_body(source, parsed, entry)
                                .and_then(|body| body_alias(source, &body))
                        })
                        .flatten()
                })
                .or_else(|| literal_copy_target(source, parsed, entry, false).map(|(name, _)| name))
            {
                users.entry(target).or_default().push(entry.name.clone());
            }
        }
        if argument_consuming {
            for entry in &parsed.environments {
                for (name, target, _) in environment_forwarders(source, parsed, entry) {
                    users.entry(target).or_default().push(name);
                }
            }
        }
    }
    let mut result = StoredCodeAliases::new();
    let mut pending: Vec<_> = builtins
        .into_iter()
        .map(|name| (name.to_owned(), name))
        .collect();
    let mut visits = 0;
    while let Some((name, target)) = pending.pop() {
        work += name.len() + 64;
        visits += 1;
        if work > 32 * 1024 * 1024 || visits > 100_000 {
            return None;
        }
        if !result.entry(name.clone()).or_default().insert(target) {
            continue;
        }
        pending.extend(
            users
                .get(&name)
                .into_iter()
                .flatten()
                .map(|user| (user.clone(), target)),
        );
    }
    Some(result)
}

/// Source aliases can consume their own operands before forwarding an
/// interface. Their call does not have the primitive's argument boundary.
pub(crate) fn argument_consuming_forwarders<'a>(
    documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
) -> Option<BTreeSet<String>> {
    let mut work = 0usize;
    let mut aliases = Vec::new();
    let mut uncertain = BTreeSet::new();
    for (source, parsed) in documents {
        work = work.saturating_add(
            source.len() + (parsed.definitions.len() + parsed.environments.len()) * 64,
        );
        if work > 32 * 1024 * 1024 {
            return None;
        }
        for entry in &parsed.definitions {
            if let Some(target) = command_definition_body(source, parsed, entry)
                .and_then(|body| body_alias(source, &body))
            {
                if CommandShape::definition(source, parsed, entry)
                    .is_none_or(|shape| !shape.takes_no_arguments())
                {
                    uncertain.insert(entry.name.clone());
                }
                aliases.push((entry.name.clone(), target));
            } else if let Some((target, _)) = literal_copy_target(source, parsed, entry, false) {
                aliases.push((entry.name.clone(), target));
            }
        }
        for entry in &parsed.environments {
            for (name, target, consuming) in environment_forwarders(source, parsed, entry) {
                if consuming {
                    uncertain.insert(name.clone());
                }
                aliases.push((name, target));
            }
        }
    }
    loop {
        let mut changed = false;
        for (alias, target) in &aliases {
            work = work.saturating_add(64);
            if work > 32 * 1024 * 1024 {
                return None;
            }
            if uncertain.contains(target) {
                changed |= uncertain.insert(alias.clone());
            }
        }
        if !changed {
            return Some(uncertain);
        }
    }
}

/// Captured parameter grammar for a command's current, unexpanded meaning.
#[derive(Clone, PartialEq)]
pub(crate) struct CommandShape {
    signature: Option<Signature>,
    tex: Option<TexOperands>,
}

impl std::fmt::Debug for CommandShape {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandShape")
            .field("signature", &self.signature.as_ref().map(Signature::label))
            .field("tex", &self.tex.is_some())
            .finish()
    }
}

pub(crate) struct ParameterSpans {
    pub spans: Vec<std::ops::Range<usize>>,
    pub simple: bool,
    pub end: usize,
}

impl CommandShape {
    pub(crate) fn cache_bytes(&self) -> usize {
        self.signature.as_ref().map_or(0, grammar_signature_bytes)
            + self.tex.as_ref().map_or(0, grammar_tex_bytes)
    }

    pub(crate) fn environment_definition(
        source: &str,
        parsed: &syntax::Syntax,
        entry: &Entry,
    ) -> Option<Self> {
        Some(Self {
            signature: Some(environment_start_signature(source, parsed, entry)?),
            tex: None,
        })
    }

    /// An undefined control consumes no operands; retaining its former alias
    /// grammar would hide the following executable token or file load.
    pub(crate) fn no_arguments() -> Self {
        Self {
            signature: None,
            tex: Some(TexOperands {
                prefix: vec![],
                delimiters: vec![],
            }),
        }
    }

    pub(crate) fn takes_no_arguments(&self) -> bool {
        self.tex
            .as_ref()
            .is_some_and(|tex| tex.prefix.is_empty() && tex.delimiters.is_empty())
            || self
                .signature
                .as_ref()
                .is_some_and(|signature| signature.parameters.is_empty())
    }

    pub(crate) fn definition(source: &str, parsed: &syntax::Syntax, entry: &Entry) -> Option<Self> {
        let mut signature = source_definition(source, parsed, entry, false);
        let definition = TexOperands::definition(source, parsed, entry);
        let known = definition.is_some();
        let tex = definition
            .filter(|(operands, _)| operands.delimited() || signature.is_none())
            .map(|(operands, _)| operands);
        if TexOperands::declares(entry) && (tex.is_some() || !known) {
            signature = None;
        }
        (signature.is_some() || tex.is_some()).then_some(Self { signature, tex })
    }

    pub(crate) fn parameter_spans(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        invocation: std::ops::Range<usize>,
        control_spaces: bool,
    ) -> Option<ParameterSpans> {
        let tex = self.tex.as_ref();
        let mut spans = Vec::new();
        let mut before = invocation.end;
        if let Some(tex) = tex {
            if tex.delimiters.len() > 9 {
                return None;
            }
            for count in 1..=tex.delimiters.len() {
                let mut prefix = tex.clone();
                prefix.delimiters.truncate(count);
                let end = prefix.end(source, invocation.end, control_spaces)?;
                spans.push(before..end);
                before = end;
            }
            // A fixed TeX prefix is consumed even with zero parameters.
            before = tex.end(source, invocation.end, control_spaces)?;
        } else {
            let signature = self.signature.as_ref()?;
            if signature.parameters.len() > 9 {
                return None;
            }
            for count in 1..=signature.parameters.len() {
                let mut prefix = signature.clone();
                prefix.parameters.truncate(count);
                let read = argument_tokens(
                    (source, parsed),
                    invocation.end,
                    control_spaces,
                    &prefix,
                    false,
                    Some(usize::MAX),
                    &mut BTreeSet::new(),
                );
                if read.unfinished {
                    return None;
                }
                spans.push(before..read.end);
                before = read.end;
            }
        }
        let simple = self.tex.is_none()
            && self.signature.as_ref().is_some_and(|signature| {
                signature
                    .parameters
                    .iter()
                    .all(|parameter| !matches!(parameter.kind, Kind::Delimited { .. }))
            });
        Some(ParameterSpans {
            spans,
            simple,
            end: before,
        })
    }

    fn argument_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        command: &syntax::Command,
        literal: Option<usize>,
        result: &mut BTreeSet<usize>,
    ) {
        if let Some(end) = self
            .tex
            .as_ref()
            .and_then(|operands| operands.end(source, command.span.end, true))
        {
            mark_operand_tokens(parsed, command.span.end, end, literal, result);
        } else if let Some(signature) = &self.signature {
            argument_tokens(
                (source, parsed),
                command.span.end,
                true,
                signature,
                false,
                literal,
                result,
            );
        } else {
            unknown_argument_token(source, command.span.end, parsed, literal, true, result);
        }
    }
}

/// Tokens declaring an unexpanded, argument-free file loader alias.
/// Its body forwards the loader rather than performing a dynamic file load.
pub(crate) fn file_load_alias_body_positions(
    source: &str,
    parsed: &syntax::Syntax,
) -> BTreeSet<usize> {
    parsed
        .definitions
        .iter()
        .filter_map(|entry| {
            if matches!(
                entry.detail.strip_prefix("Defined with \\"),
                Some("edef" | "xdef" | "csedef" | "csxdef")
            ) {
                return None;
            }
            let signature = source_definition(source, parsed, entry, false)?;
            if !signature.parameters.is_empty() {
                return None;
            }
            let body = source_alias_body(source, parsed, entry)?;
            let target = body_alias(source, &body)?;
            (matches!(target.as_str(), "input" | "include")
                || syntax::loader_extension(&target).is_some())
            .then(|| syntax::skip_space(source, body.span.start))
        })
        .collect()
}

fn source_alias_body(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
) -> Option<syntax::Argument> {
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    let mut at = definition_arguments_start(source, parsed, entry, false)?;
    if syntax::declares_traditional_command(declaration)
        || matches!(
            declaration,
            "DeclareTextCommand"
                | "ProvideTextCommand"
                | "DeclareTextCommandDefault"
                | "ProvideTextCommandDefault"
        )
    {
        if let Some((_, end)) = syntax::bracket_argument(source, at) {
            at = syntax::skip_space(source, end);
        }
    } else if syntax::declares_document_command(declaration) {
        at = syntax::group(source, at, b'{', b'}')?.1;
    } else if !matches!(
        declaration,
        "def" | "gdef" | "edef" | "xdef" | "csdef" | "csgdef" | "csedef" | "csxdef"
    ) {
        return None;
    }
    syntax::group(source, syntax::skip_space(source, at), b'{', b'}').map(|(body, _)| body)
}

/// Definition bodies are not executed where they occur. Keep their names
/// uncertain during environment matching without excluding rename references.
pub(crate) fn definition_body_spans(
    source: &str,
    parsed: &syntax::Syntax,
) -> Vec<std::ops::Range<usize>> {
    definition_spans(source, parsed, false)
}

/// Parameter headers and replacement text are data, not executed math shifts.
pub(crate) fn definition_data_spans(
    source: &str,
    parsed: &syntax::Syntax,
) -> Vec<std::ops::Range<usize>> {
    let mut spans = definition_spans(source, parsed, true);
    // Symbol targets and unfinished definitions need not enter the symbol
    // index, but their parameter/replacement tokens are still data.
    for command in &parsed.commands {
        if parsed.control_operands.contains(&command.span.start) {
            continue;
        }
        let primitive = matches!(
            command.name.as_str(),
            "def" | "gdef" | "edef" | "xdef" | "csdef" | "csgdef" | "csedef" | "csxdef"
        );
        let environment = matches!(
            command.name.as_str(),
            "newenvironment"
                | "renewenvironment"
                | "provideenvironment"
                | "NewDocumentEnvironment"
                | "RenewDocumentEnvironment"
                | "ProvideDocumentEnvironment"
                | "DeclareDocumentEnvironment"
        );
        let document = syntax::declares_document_command(&command.name)
            || (environment && command.name.contains("Document"));
        if !primitive
            && !environment
            && !syntax::declares_traditional_command(&command.name)
            && !document
        {
            continue;
        }
        let end = (|| {
            let mut at = syntax::skip_space(source, command.span.end);
            if source.as_bytes().get(at) == Some(&b'*') {
                at = syntax::skip_space(source, at + 1);
            }
            at = if source.as_bytes().get(at) == Some(&b'{') {
                syntax::group(source, at, b'{', b'}')?.1
            } else {
                at + syntax::token(source, at)?.len()
            };
            at = syntax::skip_space(source, at);
            if primitive {
                for _ in 0..4096 {
                    at = syntax::skip_space(source, at);
                    if source.as_bytes().get(at) == Some(&b'{') {
                        break;
                    }
                    at += syntax::token(source, at)?.len();
                }
            } else if document {
                at = syntax::group(source, at, b'{', b'}')?.1;
            } else {
                for _ in 0..2 {
                    at = syntax::skip_space(source, at);
                    if source.as_bytes().get(at) == Some(&b'[') {
                        at = syntax::bracket_argument(source, at)?.1;
                    }
                }
            }
            for _ in 0..if environment { 2 } else { 1 } {
                at = syntax::skip_space(source, at);
                at = syntax::group(source, at, b'{', b'}')?.1;
            }
            Some(at)
        })()
        .unwrap_or(source.len());
        spans.push(command.span.start..end);
    }
    spans
}

/// Start of the declaration control word, before its destination name.
pub(crate) fn command_declaration_start(parsed: &syntax::Syntax, entry: &Entry) -> Option<usize> {
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    parsed.commands[..parsed
        .commands
        .partition_point(|command| command.span.start < entry.span.start)]
        .iter()
        .rev()
        .find(|command| {
            command.name == declaration && !parsed.control_operands.contains(&command.span.start)
        })
        .map(|command| command.span.start)
}

/// Complete replacement text for a supported user command declaration.
pub(crate) fn command_definition_body(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
) -> Option<syntax::Argument> {
    if TexOperands::declares(entry) {
        let (_, at) = TexOperands::definition(source, parsed, entry)?;
        return syntax::group(source, at, b'{', b'}').map(|(body, _)| body);
    }
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    let mut at = definition_arguments_start(source, parsed, entry, false)?;
    if syntax::declares_traditional_command(declaration) {
        for _ in 0..2 {
            if let Some((_, end)) = syntax::bracket_argument(source, at) {
                at = syntax::skip_space(source, end);
            }
        }
    } else if syntax::declares_document_command(declaration) {
        at = syntax::group(source, at, b'{', b'}')?.1;
    } else {
        return None;
    }
    syntax::group(source, syntax::skip_space(source, at), b'{', b'}').map(|(body, _)| body)
}

/// Defaults are also possible inputs when a data-bearing optional parameter
/// is omitted at a call. Preserve their original source locations.
pub(crate) fn parameter_defaults(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
    environment: bool,
) -> Option<Vec<(usize, std::ops::Range<usize>)>> {
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    if TexOperands::declares(entry) {
        return Some(Vec::new());
    }
    source_definition(source, parsed, entry, environment)?;
    let at = definition_arguments_start(source, parsed, entry, environment)?;
    if syntax::declares_traditional_command(declaration)
        || matches!(
            declaration,
            "newenvironment" | "renewenvironment" | "provideenvironment"
        )
    {
        let Some((_, end)) = syntax::bracket_argument(source, at) else {
            return Some(Vec::new());
        };
        return Some(
            syntax::bracket_argument(source, syntax::skip_space(source, end))
                .map(|(default, _)| (0, default.span))
                .into_iter()
                .collect(),
        );
    }
    if !syntax::declares_document_command(declaration)
        && !matches!(
            declaration,
            "NewDocumentEnvironment"
                | "RenewDocumentEnvironment"
                | "ProvideDocumentEnvironment"
                | "DeclareDocumentEnvironment"
        )
    {
        return Some(Vec::new());
    }
    let (specification, _) = syntax::group(source, at, b'{', b'}')?;
    let spec = &source[specification.span.clone()];
    let mut at = 0;
    let mut number = 0;
    let mut defaults = Vec::new();
    while at < spec.len() {
        at = syntax::skip_space(spec, at);
        let Some(&kind) = spec.as_bytes().get(at) else {
            break;
        };
        at += 1;
        if matches!(kind, b'+' | b'!') {
            continue;
        }
        match kind {
            b'm' | b'o' | b's' => {}
            b't' => {
                at = delimiter_spec(spec, at)?.1;
            }
            b'd' | b'D' | b'r' | b'R' => {
                at = delimiter_spec(spec, at)?.1;
                at = delimiter_spec(spec, at)?.1;
            }
            b'O' => {}
            _ => return None,
        }
        if matches!(kind, b'O' | b'D' | b'R') {
            let (default, end) = syntax::group(spec, syntax::skip_space(spec, at), b'{', b'}')?;
            defaults.push((
                number,
                specification.span.start + default.span.start
                    ..specification.span.start + default.span.end,
            ));
            at = end;
        }
        number += 1;
    }
    Some(defaults)
}

fn definition_spans(
    source: &str,
    parsed: &syntax::Syntax,
    include_headers: bool,
) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    for entry in &parsed.definitions {
        let body = command_definition_body(source, parsed, entry);
        if let Some(body) = body {
            spans.push(if include_headers {
                entry.span.start..body.outer_span.end
            } else {
                body.span
            });
        }
    }
    for entry in &parsed.environments {
        if let Some((begin, end)) = environment_bodies(source, parsed, entry) {
            if include_headers {
                spans.push(entry.span.start..end.outer_span.end);
            } else {
                spans.extend([begin.span, end.span]);
            }
        }
    }
    spans
}

fn merged_spans(mut spans: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    spans.sort_unstable_by_key(|span| span.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for span in spans {
        if let Some(previous) = merged
            .last_mut()
            .filter(|previous| span.start <= previous.end)
        {
            previous.end = previous.end.max(span.end);
        } else {
            merged.push(span);
        }
    }
    merged
}

fn span_contains(spans: &[std::ops::Range<usize>], at: usize) -> bool {
    spans
        .partition_point(|span| span.start <= at)
        .checked_sub(1)
        .is_some_and(|index| spans[index].contains(&at))
}

fn short(value: &str) -> String {
    let mut literal = String::new();
    let mut at = 0;
    let mut control_word = false;
    let mut pending_space = false;
    while at < value.len() {
        if value.as_bytes()[at] == b'%' {
            at = tex_operand_comments(value, at);
            // Removing a comment must not turn two tokens into a longer
            // control word when the preview is read as TeX source.
            if control_word
                && value[at..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_alphabetic() || ch == '@')
            {
                pending_space = true;
            }
            continue;
        }
        let end = if value.as_bytes()[at] == b'\\' {
            let word = syntax::control_word(value, at);
            let end = word
                .as_ref()
                .map(|word| word.end)
                .or_else(|| syntax::token(value, at).map(|token| at + token.len()))
                .unwrap_or(at + 1);
            control_word = word.is_some();
            end
        } else {
            let ch = value[at..].chars().next().unwrap();
            control_word = false;
            if ch.is_whitespace() {
                pending_space = true;
                at += ch.len_utf8();
                continue;
            }
            at + ch.len_utf8()
        };
        if pending_space && !literal.is_empty() && !literal.ends_with(' ') {
            literal.push(' ');
        }
        pending_space = false;
        literal.push_str(&value[at..end]);
        at = end;
    }
    literal.chars().take(80).collect()
}

/// Read one lexical TeX token without expanding it. In particular, a delimiter
/// control word must not match a prefix of a longer control word.
/// Static TeX parameter text, used only to establish which tokens are operands.
/// Signature help deliberately keeps its existing, smaller set of signatures.
#[derive(Clone, PartialEq)]
struct TexOperands {
    prefix: Vec<String>,
    delimiters: Vec<Vec<String>>,
}

impl TexOperands {
    fn declares(entry: &Entry) -> bool {
        matches!(
            entry.detail.strip_prefix("Defined with \\"),
            Some("def" | "gdef" | "edef" | "xdef" | "csdef" | "csgdef" | "csedef" | "csxdef")
        )
    }

    fn delimited(&self) -> bool {
        !self.prefix.is_empty()
            || self
                .delimiters
                .iter()
                .any(|delimiter| !delimiter.is_empty())
    }

    fn definition(source: &str, parsed: &syntax::Syntax, entry: &Entry) -> Option<(Self, usize)> {
        let (operands, at) = Self::declaration(source, parsed, entry)?;
        syntax::group(source, at, b'{', b'}')?;
        Some((operands, at))
    }

    /// A valid parameter header already starts tokenizing replacement text,
    /// even while its closing brace has not yet been typed.
    fn declaration(source: &str, parsed: &syntax::Syntax, entry: &Entry) -> Option<(Self, usize)> {
        if !Self::declares(entry) {
            return None;
        }
        let mut at = entry.span.end;
        let named = entry.detail.starts_with("Defined with \\cs");
        if named {
            // A closing name group leaves the mouth in mid-line state.
            // Unlike a literal control word, it does not discard a prefix space.
            if source.as_bytes().get(at) != Some(&b'}') {
                return None;
            }
            at += 1;
        } else {
            if source.as_bytes().get(entry.span.start) != Some(&b'\\') {
                let start = syntax::skip_space(source, at);
                at = parsed
                    .commands
                    .iter()
                    .find(|command| command.span.start == start && command.name == "endcsname")?
                    .span
                    .end;
            }
            at = optional_start(source, at, true);
        }
        let mut prefix = vec![];
        let mut delimiters: Vec<Vec<String>> = vec![];
        let mut tokens = 0;
        loop {
            at = tex_operand_comments(source, at);
            match source.as_bytes().get(at)? {
                b'{' => break,
                b'#' => {
                    let number = delimiters.len() + 1;
                    let (digit, end) = tex_operand_token(source, at + 1)?;
                    if number > 9 || digit.as_bytes() != [b'0' + number as u8] {
                        return None;
                    }
                    delimiters.push(vec![]);
                    at = end;
                }
                _ => {
                    let (value, end) = tex_operand_token(source, at)?;
                    tokens += 1;
                    if tokens > 64 {
                        return None;
                    }
                    let target = delimiters.last_mut().unwrap_or(&mut prefix);
                    target.push(value.to_owned());
                    at = end;
                }
            }
        }
        Some((Self { prefix, delimiters }, at))
    }

    fn end(&self, source: &str, at: usize, control_spaces: bool) -> Option<usize> {
        self.end_mode(source, at, control_spaces, false)
    }

    fn end_mode(
        &self,
        source: &str,
        at: usize,
        control_spaces: bool,
        unfinished: bool,
    ) -> Option<usize> {
        let mut at = tex_operand_match(
            source,
            optional_start(source, at, control_spaces),
            &self.prefix,
        )?;
        for delimiter in &self.delimiters {
            if delimiter.is_empty() {
                at = tex_operand_skip_spaces(source, at)?;
                at = if source.as_bytes().get(at) == Some(&b'{') {
                    match syntax::group(source, at, b'{', b'}') {
                        Some((_, end)) => end,
                        None => return unfinished.then_some(source.len()),
                    }
                } else {
                    tex_operand_token(source, at)
                        .map(|(_, end)| end)
                        .or_else(|| (unfinished && at == source.len()).then_some(at))?
                };
            } else {
                loop {
                    at = tex_operand_comments(source, at);
                    if let Some(end) = tex_operand_match(source, at, delimiter) {
                        at = end;
                        break;
                    }
                    at = match source.as_bytes().get(at) {
                        Some(b'{') => match syntax::group(source, at, b'{', b'}') {
                            Some((_, end)) => end,
                            None => return unfinished.then_some(source.len()),
                        },
                        Some(_) => tex_operand_token(source, at)?.1,
                        None => return unfinished.then_some(source.len()),
                    };
                }
            }
        }
        Some(at)
    }
}

fn tex_operand_comments(source: &str, mut at: usize) -> usize {
    while source.as_bytes().get(at) == Some(&b'%') {
        at = source[at..]
            .find('\n')
            .map_or(source.len(), |end| at + end + 1);
        while matches!(source.as_bytes().get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
    }
    at
}

fn tex_operand_blank_line(source: &str, at: usize) -> bool {
    let horizontal = |byte: &u8| matches!(byte, b' ' | b'\t');
    let before = source.as_bytes()[..at]
        .iter()
        .rev()
        .find(|byte| !horizontal(byte));
    let after = source.as_bytes()[at..]
        .iter()
        .find(|byte| !horizontal(byte));
    matches!(before, None | Some(b'\r' | b'\n')) && matches!(after, Some(b'\r' | b'\n'))
}

/// Normalize one ordinary TeX space token, retaining its source end. Comments
/// remove the endline and indentation; lexical blanks after a control word (or
/// control-space) disappear. Paragraph tokens and catcode spellings stay unknown.
fn tex_operand_token(source: &str, at: usize) -> Option<(&str, usize)> {
    let at = tex_operand_comments(source, at);
    if matches!(
        source.as_bytes().get(at),
        Some(b' ' | b'\t' | b'\r' | b'\n')
    ) {
        if tex_operand_blank_line(source, at) {
            return None;
        }
        return Some((" ", optional_start(source, at, true)));
    }
    let value = required_token(source, at)?;
    let end = at + value.len();
    let end = if syntax::control_word(value, 0).is_some() || value == "\\ " {
        optional_start(source, end, true)
    } else {
        end
    };
    Some((value, end))
}

fn tex_operand_skip_spaces(source: &str, mut at: usize) -> Option<usize> {
    loop {
        at = tex_operand_comments(source, at);
        if !matches!(
            source.as_bytes().get(at),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            return Some(at);
        }
        at = tex_operand_token(source, at)?.1;
    }
}

fn tex_operand_match(source: &str, mut at: usize, pattern: &[String]) -> Option<usize> {
    for expected in pattern {
        let (value, end) = tex_operand_token(source, at)?;
        if value != expected {
            return None;
        }
        at = end;
    }
    Some(at)
}

/// Resolve command arities once across the documents in the analyzed project.
pub(crate) struct CommandSignatures<'a> {
    local: BTreeMap<Cow<'a, str>, Option<Signature>>,
    tex_operands: BTreeMap<Cow<'a, str>, Option<TexOperands>>,
    literal_operands: BTreeMap<Cow<'a, str>, Vec<LiteralOperands>>,
    unverified_names: BTreeSet<Cow<'a, str>>,
    detokenize_boundary_known: bool,
    identity_parameters: BTreeSet<String>,
    hook_arities: crate::symbols::hook_arguments::Arities,
    common_grammars: std::sync::OnceLock<BTreeMap<String, Option<LiteralOperands>>>,
    memo: memo::Memo,
}

/// Possible declarations only exclude literal edits; they never establish a
/// unique command meaning for navigation, source loading or signature help.
#[derive(Clone, PartialEq)]
enum LiteralOperands {
    Signature(Signature),
    Tex(TexOperands),
    Alias(String),
    Copy(String),
    Unknown,
}

impl LiteralOperands {
    /// Labels, default values and documentation do not change how parameters
    /// are collected. Copies can capture an older grammar and remain unknown.
    fn same_grammar(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Signature(a), Self::Signature(b)) => {
                a.starred == b.starred
                    && a.nested_brackets == b.nested_brackets
                    && a.parameters.len() == b.parameters.len()
                    && a.parameters
                        .iter()
                        .zip(&b.parameters)
                        .all(|(a, b)| a.obey_spaces == b.obey_spaces && a.kind == b.kind)
            }
            (Self::Tex(a), Self::Tex(b)) => a == b,
            (Self::Alias(a), Self::Alias(b)) => a == b,
            _ => false,
        }
    }
}

fn insert_literal_operands<'a>(
    entries: &mut BTreeMap<Cow<'a, str>, Vec<LiteralOperands>>,
    name: Cow<'a, str>,
    signature: Option<&Signature>,
    tex: Option<&TexOperands>,
    alias: Option<&str>,
) {
    let operands = if let Some(alias) = alias {
        Some(LiteralOperands::Alias(alias.into()))
    } else if let Some(tex) = tex {
        Some(LiteralOperands::Tex(tex.clone()))
    } else {
        signature.cloned().map(LiteralOperands::Signature)
    };
    entries
        .entry(name)
        .or_default()
        .push(operands.unwrap_or(LiteralOperands::Unknown));
}

#[derive(Clone)]
pub(crate) struct EnvironmentCall {
    pub argument: syntax::Argument,
    pub span: std::ops::Range<usize>,
    pub begin: bool,
    pub direct: bool,
    pub operand: bool,
    pub deferred: bool,
}

/// Preserve dynamic name arguments as boundaries for ordered pairing.
pub(crate) struct EnvironmentBoundary {
    pub argument: Option<syntax::Argument>,
    pub span: std::ops::Range<usize>,
    pub begin: bool,
    pub direct: bool,
    pub operand: bool,
    pub deferred: bool,
}

impl EnvironmentBoundary {
    pub(crate) fn into_call(self) -> Option<EnvironmentCall> {
        Some(EnvironmentCall {
            argument: self.argument?,
            span: self.span,
            begin: self.begin,
            direct: self.direct,
            operand: self.operand,
            deferred: self.deferred,
        })
    }
}

#[derive(Clone)]
pub(crate) struct SourceCall {
    pub command: syntax::Command,
    pub certain: bool,
}

fn grammar_strings_bytes(values: &Vec<String>) -> usize {
    values.capacity() * std::mem::size_of::<String>()
        + values.iter().map(String::capacity).sum::<usize>()
}
fn grammar_signature_bytes(value: &Signature) -> usize {
    value.name.capacity()
        + value.documentation.capacity()
        + value.parameters.capacity() * std::mem::size_of::<Parameter>()
        + value
            .parameters
            .iter()
            .map(|parameter| {
                parameter.label.capacity()
                    + match &parameter.kind {
                        Kind::Flag(value) => value.capacity(),
                        Kind::Delimited { open, close, .. } => open.capacity() + close.capacity(),
                        _ => 0,
                    }
            })
            .sum::<usize>()
}
fn grammar_tex_bytes(value: &TexOperands) -> usize {
    grammar_strings_bytes(&value.prefix)
        + value.delimiters.capacity() * std::mem::size_of::<Vec<String>>()
        + value
            .delimiters
            .iter()
            .map(grammar_strings_bytes)
            .sum::<usize>()
}
fn grammar_literal_bytes(value: &LiteralOperands) -> usize {
    match value {
        LiteralOperands::Signature(value) => grammar_signature_bytes(value),
        LiteralOperands::Tex(value) => grammar_tex_bytes(value),
        LiteralOperands::Alias(value) | LiteralOperands::Copy(value) => value.capacity(),
        LiteralOperands::Unknown => 0,
    }
}

impl<'a> CommandSignatures<'a> {
    pub(crate) fn same_meanings(&self, other: &Self) -> bool {
        self.local == other.local
            && self.tex_operands == other.tex_operands
            && self.literal_operands == other.literal_operands
            && self.unverified_names == other.unverified_names
            && self.detokenize_boundary_known == other.detokenize_boundary_known
            && self.identity_parameters == other.identity_parameters
            && self.hook_arities == other.hook_arities
    }

    /// Retain an analysis within an immutable query without borrowing sources.
    pub(crate) fn into_owned(self) -> CommandSignatures<'static> {
        CommandSignatures {
            local: self
                .local
                .into_iter()
                .map(|(name, value)| (Cow::Owned(name.into_owned()), value))
                .collect(),
            tex_operands: self
                .tex_operands
                .into_iter()
                .map(|(name, value)| (Cow::Owned(name.into_owned()), value))
                .collect(),
            literal_operands: self
                .literal_operands
                .into_iter()
                .map(|(name, value)| (Cow::Owned(name.into_owned()), value))
                .collect(),
            unverified_names: self
                .unverified_names
                .into_iter()
                .map(|name| Cow::Owned(name.into_owned()))
                .collect(),
            detokenize_boundary_known: self.detokenize_boundary_known,
            identity_parameters: self.identity_parameters,
            hook_arities: self.hook_arities,
            common_grammars: self.common_grammars,
            memo: self.memo,
        }
    }

    /// Owned capacities plus conservative tree-entry accounting for query
    /// retention. Include copied grammars, delimiters and hook declarations.
    pub(crate) fn cache_bytes(&self) -> usize {
        let key = |name: &Cow<'_, str>| match name {
            Cow::Borrowed(_) => 0,
            Cow::Owned(name) => name.capacity(),
        };
        fn tree<K, V>(entries: &BTreeMap<K, V>) -> usize {
            entries.len() * (12 * std::mem::size_of::<(K, V)>() + 256)
        }
        // Initialize before retention so lazy grammar storage is charged to
        // the same byte budget as the declarations that produced it.
        let common = self.common_parameter_grammars();
        std::mem::size_of::<Self>()
            + memo::MAX_BYTES
            + tree(common)
            + common
                .iter()
                .map(|(name, value)| {
                    name.capacity() + value.as_ref().map_or(0, grammar_literal_bytes)
                })
                .sum::<usize>()
            + tree(&self.local)
            + self
                .local
                .iter()
                .map(|(name, value)| key(name) + value.as_ref().map_or(0, grammar_signature_bytes))
                .sum::<usize>()
            + tree(&self.tex_operands)
            + self
                .tex_operands
                .iter()
                .map(|(name, value)| key(name) + value.as_ref().map_or(0, grammar_tex_bytes))
                .sum::<usize>()
            + tree(&self.literal_operands)
            + self
                .literal_operands
                .iter()
                .map(|(name, values)| {
                    key(name)
                        + values.capacity() * std::mem::size_of::<LiteralOperands>()
                        + values.iter().map(grammar_literal_bytes).sum::<usize>()
                })
                .sum::<usize>()
            + self
                .unverified_names
                .iter()
                .map(|name| key(name) + 12 * std::mem::size_of::<Cow<'_, str>>() + 256)
                .sum::<usize>()
            + self
                .identity_parameters
                .iter()
                .map(|name| name.capacity() + 12 * std::mem::size_of::<String>() + 256)
                .sum::<usize>()
            + self.hook_arities.cache_bytes()
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.local.keys().map(|name| name.as_ref())
    }

    pub(crate) fn new(documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>) -> Self {
        Self::with_ambiguous_names(documents, std::iter::empty())
    }

    /// Paused declarations can exclude operands without certifying a meaning.
    pub(crate) fn with_literal_sources(
        mut self,
        documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
    ) -> Self {
        self.common_grammars = Default::default();
        self.memo = Default::default();
        for (name, operands) in Self::new(documents).literal_operands {
            self.literal_operands
                .entry(name)
                .or_default()
                .extend(operands);
        }
        self
    }

    pub(crate) fn with_ambiguous_names(
        documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
        ambiguous: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self::with_ambiguous_environments(documents, ambiguous, std::iter::empty())
    }

    pub(crate) fn with_ambiguous_environments(
        documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
        ambiguous: impl IntoIterator<Item = &'a str>,
        environments: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self::collect(documents, ambiguous, environments, None)
    }

    pub(crate) fn source_context(
        documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
    ) -> Self {
        let documents: Vec<_> = documents.into_iter().collect();
        let preliminary = Self::new(documents.iter().copied());
        Self::source_context_with(documents, &preliminary)
    }

    /// Exact ordered execution prefixes; retaining full document identities is
    /// intentional because suffix text participates in reader and work budgets.
    pub(crate) fn at_prefixes(
        documents: &[(&Document, usize)],
    ) -> std::sync::Arc<CommandSignatures<'static>> {
        let binding =
            crate::analysis::AnalysisContext::current().and_then(|context| context.binding.clone());
        let inputs: Option<crate::analysis_cache::PrefixInputs> = documents
            .iter()
            .map(|(doc, end)| {
                Some((
                    binding.as_ref()?.document(&doc.text.source, &doc.syntax)?,
                    *end,
                ))
            })
            .collect();
        if let Some(value) = binding
            .as_ref()
            .zip(inputs.as_ref())
            .and_then(|(binding, inputs)| binding.cache.prefix(inputs))
        {
            return value;
        }
        let prefixes: Vec<_> = documents
            .iter()
            .map(|(doc, end)| {
                if *end >= doc.text.source.len() {
                    return Cow::Borrowed(&doc.syntax);
                }
                let mut parsed = doc.syntax.clone();
                parsed.commands.retain(|command| command.span.start <= *end);
                parsed.definitions.retain(|entry| entry.span.start < *end);
                parsed.environments.retain(|entry| entry.span.start < *end);
                parsed.command_undefinitions.retain(|at, _| *at < *end);
                Cow::Owned(parsed)
            })
            .collect();
        let value = CommandSignatures::source_context(
            documents
                .iter()
                .zip(&prefixes)
                .map(|((doc, _), parsed)| (doc.text.source.as_str(), parsed.as_ref())),
        )
        .into_owned();
        if let Some((binding, inputs)) = binding.zip(inputs) {
            binding.cache.insert_prefix(inputs, value)
        } else {
            std::sync::Arc::new(value)
        }
    }

    /// The preliminary editing grammar must cover these exact ordered sources.
    /// Filtering still creates a separate execution grammar with its own cache.
    pub(crate) fn source_context_with(
        documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
        preliminary: &Self,
    ) -> Self {
        Self::collect(
            documents,
            std::iter::empty(),
            std::iter::empty(),
            Some(preliminary),
        )
    }

    fn collect(
        documents: impl IntoIterator<Item = (&'a str, &'a syntax::Syntax)>,
        ambiguous: impl IntoIterator<Item = &'a str>,
        environments: impl IntoIterator<Item = &'a str>,
        source_context: Option<&Self>,
    ) -> Self {
        let documents: Vec<_> = documents.into_iter().collect();
        // Source-context filtering may omit definitions installed by a later
        // macro call. Never infer the primitive's arity from that filtered map.
        let mut raw_names = BTreeSet::new();
        let detokenize_boundary_known = documents.iter().any(|(_, parsed)| {
            parsed.commands.iter().any(|command| {
                matches!(command.name.as_str(), "detokenize" | "csuse" | "endcsname")
            })
        }) && !documents.iter().any(|(source, parsed)| {
            parsed.definitions.iter().any(|entry| {
                matches!(
                    entry.name.as_str(),
                    "detokenize" | "csuse" | "csname" | "endcsname"
                ) || !raw_names.insert(entry.name.as_str())
            }) || parsed.commands.iter().enumerate().any(|(index, command)| {
                let invoked = match command.name.as_str() {
                    "csuse" => command
                        .argument
                        .as_ref()
                        .filter(|argument| syntax::is_static_key(&argument.value))
                        .map(|argument| argument.value.as_str()),
                    "csname" => syntax::literal_csname(source, &parsed.commands, index)
                        .map(|span| &source[span]),
                    name => Some(name),
                };
                matches!(
                    invoked,
                    None | Some("@input" | "@input@" | "@@input" | "scantokens")
                ) || matches!(
                    command.name.as_str(),
                    "let" | "futurelet" | "cslet" | "letcs" | "csletcs"
                ) || syntax::code_input_command(&command.name)
                    || invoked.is_some_and(|name| {
                        name != command.name
                            && (matches!(
                                name,
                                "input" | "include" | "subfile" | "InputIfFileExists"
                            ) || syntax::loader_extension(name).is_some())
                    })
                    || (matches!(
                        command.name.as_str(),
                        "input" | "include" | "subfile" | "InputIfFileExists"
                    ) || syntax::loader_extension(&command.name).is_some())
                        && if syntax::file_list(&command.name) {
                            syntax::complete_file_list_spans(source, command).is_none()
                        } else {
                            syntax::file_argument(source, command).is_none_or(|argument| {
                                !syntax::is_literal_file_path(&argument.value)
                            })
                        }
            })
        });
        let mut unverified_names: BTreeSet<_> = ambiguous.into_iter().map(Cow::Borrowed).collect();
        for name in environments {
            unverified_names.insert(Cow::Borrowed(name));
            unverified_names.insert(Cow::Owned(format!("end{name}")));
        }
        let mut local: BTreeMap<_, _> = unverified_names
            .iter()
            .cloned()
            .map(|name| (name, None))
            .collect();
        let mut tex_operands: BTreeMap<_, _> =
            local.keys().map(|name| (name.clone(), None)).collect();
        let mut aliases = BTreeMap::new();
        let mut literal_operands = BTreeMap::new();
        let facts: Vec<_> = documents
            .iter()
            .map(|(source, parsed)| Declarations::get(source, parsed))
            .collect();
        for (&(source, parsed), facts) in documents.iter().zip(&facts) {
            let positions = source_context
                .map(|signatures| signatures.source_command_positions(source, parsed));
            for ((entry, environment), fact) in parsed
                .definitions
                .iter()
                .map(|entry| (entry, false))
                .chain(parsed.environments.iter().map(|entry| (entry, true)))
                .zip(&facts.0)
                .filter(|(_, fact)| {
                    positions.as_ref().is_none_or(|positions| {
                        fact.position.is_some_and(|at| positions.contains(&at))
                    })
                })
            {
                // Interpret immutable declarations in this project's execution context.
                let mut signature = fact.signature.clone();
                let tex = fact.tex.clone();
                let copied = fact.copied.clone();
                let alias = fact.alias.clone();
                if environment {
                    let name = format!("end{}", entry.name);
                    let ending = fact.ending.clone();
                    let alias = fact.ending_alias.clone();
                    if let Some(target) = &copied {
                        literal_operands
                            .entry(Cow::Owned(name.clone()))
                            .or_insert_with(Vec::new)
                            .push(LiteralOperands::Copy(format!("end{target}")));
                    } else {
                        insert_literal_operands(
                            &mut literal_operands,
                            Cow::Owned(name.clone()),
                            ending.as_ref(),
                            None,
                            alias.as_deref(),
                        );
                    }
                    insert_command_signature(
                        &mut local,
                        &mut tex_operands,
                        &mut aliases,
                        Cow::Owned(name),
                        ending,
                        None,
                        alias,
                    );
                }
                // Expanded definitions capture a meaning at definition time;
                // their lexical body cannot establish a late-bound alias.
                let expanded = fact.expanded;
                if expanded && alias.is_some() {
                    signature = None;
                }
                if let Some(target) = copied {
                    literal_operands
                        .entry(Cow::Borrowed(entry.name.as_str()))
                        .or_insert_with(Vec::new)
                        .push(LiteralOperands::Copy(target));
                } else {
                    insert_literal_operands(
                        &mut literal_operands,
                        Cow::Borrowed(entry.name.as_str()),
                        signature.as_ref(),
                        tex.as_ref(),
                        alias.as_deref().filter(|_| !expanded),
                    );
                }
                insert_command_signature(
                    &mut local,
                    &mut tex_operands,
                    &mut aliases,
                    Cow::Borrowed(entry.name.as_str()),
                    signature,
                    tex,
                    alias.filter(|_| !expanded),
                );
            }
        }
        // An overridden target can discard, defer or reinterpret the text.
        // Keep an explicit definition of the forwarder, but never apply its
        // native executable-text shape to an unproved replacement target.
        for &(forwarder, targets) in BIBLATEX_TEXT_DEPENDENCIES {
            if targets.iter().any(|target| local.contains_key(*target))
                && !local.contains_key(forwarder)
            {
                local.insert(Cow::Borrowed(forwarder), None);
                tex_operands.insert(Cow::Borrowed(forwarder), None);
            }
        }
        local.extend(resolve_aliases(&local, &aliases, operand_builtin));
        tex_operands.extend(resolve_aliases(&tex_operands, &aliases, |_| None));
        // Only a unique, complete single-argument identity proves that a user
        // macro returns its argument as executable tokens. Inspect raw project
        // declarations too: source-context filtering must not hide a competing
        // definition. Delimiter controls are excluded because they remain data.
        let mut identities = BTreeMap::new();
        for (&(_, parsed), facts) in documents.iter().zip(&facts) {
            for (entry, fact) in parsed.definitions.iter().zip(&facts.0) {
                let ordinary = local.get(entry.name.as_str()).and_then(Option::as_ref);
                let tex = tex_operands
                    .get(entry.name.as_str())
                    .and_then(Option::as_ref);
                let one = tex.map_or_else(
                    || {
                        ordinary.is_some_and(|s| {
                            s.parameters.len() == 1
                                && matches!(
                                    s.parameters[0].kind,
                                    Kind::Required | Kind::RequiredGroup
                                )
                        })
                    },
                    |t| {
                        t.prefix.is_empty()
                            && t.delimiters.len() == 1
                            && t.delimiters[0].iter().all(|token| !token.starts_with('\\'))
                    },
                );
                let identity = (one && fact.identity_body).then_some(());
                identities
                    .entry(Cow::Borrowed(entry.name.as_str()))
                    .and_modify(|value| *value = None)
                    .or_insert(identity);
            }
            for entry in &parsed.environments {
                identities.insert(Cow::Borrowed(entry.name.as_str()), None);
                identities.insert(Cow::Owned(format!("end{}", entry.name)), None);
            }
        }
        identities.extend(resolve_aliases(&identities, &aliases, |_| None));
        let identity_parameters = identities
            .into_iter()
            .filter_map(|(name, identity)| identity.map(|_| name.into_owned()))
            .collect();
        let mut signatures = Self {
            local,
            tex_operands,
            literal_operands,
            unverified_names,
            detokenize_boundary_known,
            identity_parameters,
            hook_arities: crate::symbols::hook_arguments::Arities::default(),
            common_grammars: Default::default(),
            memo: Default::default(),
        };
        // Deferred or forwarded providers can install an unseen definition.
        // Proven displayed file commands cannot execute such a provider.
        if signatures.detokenize_boundary_known
            && documents.iter().any(|(source, parsed)| {
                let active = signatures.source_command_positions(source, parsed);
                let quoted = signatures.quoted_command_tokens(source, parsed);
                parsed.commands.iter().any(|command| {
                    (matches!(
                        command.name.as_str(),
                        "input" | "include" | "subfile" | "InputIfFileExists"
                    ) || syntax::loader_extension(&command.name).is_some())
                        && !active.contains(&command.span.start)
                        && !quoted.contains(&command.span.start)
                })
            })
        {
            signatures.detokenize_boundary_known = false;
        }
        signatures.hook_arities =
            crate::symbols::hook_arguments::Arities::collect(&documents, &signatures);
        signatures
    }

    fn invocation<'b>(
        &self,
        source: &'b str,
        parsed: &'b syntax::Syntax,
        index: usize,
        operands: &BTreeSet<usize>,
        declarations: &BTreeSet<(usize, usize)>,
    ) -> Option<(&'b str, usize, usize, bool)> {
        let command = parsed.commands.get(index)?;
        let local = &self.local;
        // A literal csname's opening command reads a name; its closing
        // command anchors the synthetic call below. Do not apply unknown
        // macro argument heuristics to a name such as `[`.
        if command.name == "csname"
            && !local.contains_key("csname")
            && !local.contains_key("endcsname")
            && literal_invocation_name(source, &parsed.commands, index).is_some()
        {
            return None;
        }

        // Grouped csuse names leave following spaces intact; a literal
        // endcsname, like an ordinary control word, discards lexical blanks.
        let named = command.argument.as_ref().filter(|argument| {
            command.name == "csuse"
                && !local.contains_key("csuse")
                && static_invocation_name(&argument.value)
        });
        let literal = (command.name == "endcsname"
            && !local.contains_key("csname")
            && !local.contains_key("endcsname"))
        .then(|| {
            index.checked_sub(1).and_then(|index| {
                (!operands.contains(&parsed.commands[index].span.start))
                    .then(|| literal_invocation_name(source, &parsed.commands, index))
                    .flatten()
            })
        })
        .flatten();
        if literal
            .as_ref()
            .is_some_and(|span| declarations.contains(&(span.start, span.end)))
        {
            return None;
        }
        let name = named
            .map(|argument| argument.value.as_str())
            .or_else(|| literal.as_ref().map(|span| &source[span.clone()]))
            .unwrap_or(command.name.as_str());
        let end = named.map_or(command.span.end, |argument| argument.outer_span.end);
        let start = literal.as_ref().map_or(command.span.start, |_| {
            parsed.commands[index - 1].span.start
        });
        Some((name, start, end, named.is_none()))
    }

    /// One static named call, anchored at its opening command. Reuse the
    /// operand/declaration rules of argument scanning without expanding aliases.
    pub(crate) fn named_invocation<'b>(
        &self,
        source: &'b str,
        parsed: &'b syntax::Syntax,
        index: usize,
        operands: &BTreeSet<usize>,
        declarations: &BTreeSet<(usize, usize)>,
    ) -> Option<(&'b str, std::ops::Range<usize>)> {
        let command = parsed.commands.get(index)?;
        let invocation_index = match command.name.as_str() {
            "csuse"
                if self.command_unchanged("csuse")
                    && command
                        .argument
                        .as_ref()
                        .is_some_and(|argument| static_invocation_name(&argument.value)) =>
            {
                index
            }
            "csname" if self.command_unchanged("csname") && self.command_unchanged("endcsname") => {
                literal_invocation_name(source, &parsed.commands, index)?;
                index + 1
            }
            _ => return None,
        };
        let (name, start, end, _) =
            self.invocation(source, parsed, invocation_index, operands, declarations)?;
        Some((name, start..end))
    }

    /// A unique zero-argument source alias whose current leaf is still builtin.
    /// Captured copies and expanded definitions do not enter this alias graph.
    pub(crate) fn forwarded_builtin(&self, name: &str) -> Option<&str> {
        let signature = self.local.get(name)?.as_ref()?;
        (signature.name != name && !self.local.contains_key(signature.name.as_str()))
            .then_some(signature.name.as_str())
    }

    pub(crate) fn invokes_builtin(&self, name: &str, builtin: &str) -> bool {
        self.local.get(name).map_or(name == builtin, |signature| {
            signature.as_ref().is_some_and(|s| s.name == builtin)
        }) && !self.local.contains_key(builtin)
    }

    /// A unique replacement, including a late-bound pure forwarder, has a
    /// parsed user grammar. Captured copies and ambiguous bindings stay unknown.
    pub(crate) fn has_custom_signature(&self, name: &str, builtin: &str) -> bool {
        self.local.contains_key(builtin) && self.local.get(name).is_some_and(Option::is_some)
    }

    /// Bound individual user parameters with the existing operand grammar.
    pub(crate) fn parameter_spans(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        name: &str,
        invocation: std::ops::Range<usize>,
        control_spaces: bool,
    ) -> Option<ParameterSpans> {
        let shape = parsed
            .contextual_command_shapes
            .get(&invocation.start)
            .map(|shape| shape.as_ref().clone())
            .unwrap_or_else(|| CommandShape {
                signature: self.local.get(name).cloned().flatten(),
                tex: self.tex_operands.get(name).cloned().flatten(),
            });
        shape.parameter_spans(source, parsed, invocation, control_spaces)
    }

    /// Preserve every grammar agreed by duplicate declarations, including
    /// differently documented definitions with identical parameter readers.
    pub(crate) fn completion_shapes(&self) -> BTreeMap<String, Option<CommandShape>> {
        let mut result: BTreeMap<_, _> = self
            .common_parameter_grammars()
            .iter()
            .map(|(name, operands)| {
                let shape = match operands {
                    Some(LiteralOperands::Signature(signature)) => Some(CommandShape {
                        signature: Some(signature.clone()),
                        tex: None,
                    }),
                    Some(LiteralOperands::Tex(tex)) => Some(CommandShape {
                        signature: None,
                        tex: Some(tex.clone()),
                    }),
                    _ => None,
                };
                (name.to_string(), shape)
            })
            .collect();
        // A class may disable a zero-argument command with `\let\name\relax`.
        // The meaning is ambiguous, but every declaration still reads zero
        // parameters. Certify only the completion boundary, never its meaning.
        // Captured user definitions and overridden builtins remain unknown.
        for (name, candidates) in &self.literal_operands {
            if result.get(name.as_ref()).is_some_and(Option::is_none)
                && !self.unverified_names.contains(name)
                && !candidates.is_empty()
                && candidates.iter().all(|candidate| match candidate {
                    LiteralOperands::Signature(signature) => signature.parameters.is_empty(),
                    LiteralOperands::Tex(tex) => tex.prefix.is_empty() && tex.delimiters.is_empty(),
                    LiteralOperands::Alias(target) | LiteralOperands::Copy(target) => {
                        target != "noexpand"
                            && !self.local.contains_key(target.as_str())
                            && operand_builtin(target)
                                .is_some_and(|signature| signature.parameters.is_empty())
                    }
                    LiteralOperands::Unknown => false,
                })
            {
                result.insert(name.to_string(), Some(CommandShape::no_arguments()));
            }
        }
        for name in COMMANDS
            .iter()
            .map(|entry| entry.0)
            .chain(crate::project::catalog::PRIMITIVES.iter().copied())
        {
            if !result.contains_key(name) {
                if let Some(signature) = operand_builtin(name) {
                    result.insert(
                        name.to_owned(),
                        Some(CommandShape {
                            signature: Some(signature),
                            tex: None,
                        }),
                    );
                }
            }
        }
        result
    }

    pub(crate) fn hook_signature(
        &self,
        name: &str,
        source: &str,
        after: usize,
        upper: bool,
    ) -> Option<Signature> {
        // The resolved leaf is already available. Ordinary commands should
        // not probe each hook interface while scanning every source token.
        let canonical = match self.local.get(name) {
            Some(signature) => signature.as_ref()?.name.as_str(),
            None => name,
        };
        if !crate::symbols::hook_arguments::USES.contains(&canonical)
            || !self.invokes_builtin(name, canonical)
        {
            return None;
        }
        let count = self.hook_arities.count(source, after, canonical, upper)?;
        let mut signature = builtin(canonical)?;
        signature.parameters.extend((1..=count).map(|n| Parameter {
            obey_spaces: false,
            kind: Kind::Required,
            label: format!("{{argument {n}}}"),
        }));
        Some(signature)
    }

    pub(crate) fn literal_integer_value(&self, source: &str, after: usize) -> Option<(i32, usize)> {
        syntax::literal_integer(source, after, |name| {
            [
                "relax",
                "global",
                "globaldefs",
                "DeclareGraphicsExtensions",
                "graphicspath",
                "begingroup",
                "endgroup",
                "bgroup",
                "egroup",
            ]
            .iter()
            .any(|builtin| self.invokes_builtin(name, builtin))
        })
    }

    pub(crate) fn builtin_alias_body_positions(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        builtins: &[&str],
    ) -> BTreeSet<usize> {
        parsed
            .definitions
            .iter()
            .filter(|entry| {
                builtins
                    .iter()
                    .any(|builtin| self.invokes_builtin(&entry.name, builtin))
            })
            .filter_map(|entry| source_alias_body(source, parsed, entry))
            .map(|body| syntax::skip_space(source, body.span.start))
            .collect()
    }

    pub(crate) fn environment_calls(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> Vec<(syntax::Argument, bool)> {
        self.environment_invocations(source, parsed)
            .into_iter()
            .filter(|call| !call.direct)
            .map(|call| (call.argument, call.operand))
            .collect()
    }

    pub(crate) fn environment_primitives_unchanged(&self) -> bool {
        !self.local.contains_key("begin") && !self.local.contains_key("end")
    }

    pub(crate) fn single_required_argument(&self, name: &str) -> bool {
        self.local
            .get(name)
            .and_then(Option::as_ref)
            .is_some_and(|signature| {
                !signature.starred
                    && signature.parameters.len() == 1
                    && matches!(
                        signature.parameters[0].kind,
                        Kind::Required | Kind::RequiredGroup
                    )
            })
            && self.tex_operands.get(name).is_none_or(Option::is_none)
    }

    pub(crate) fn command_unchanged(&self, name: &str) -> bool {
        !self.local.contains_key(name)
    }

    /// Possible patch/copy aliases only revoke string-reader meanings. A
    /// missing or expanded target can modify any tracked reader.
    pub(crate) fn macro_mutations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        possible: &StoredCodeAliases,
    ) -> Option<BTreeMap<usize, BTreeSet<Option<String>>>> {
        if !parsed.commands.iter().any(|command| {
            crate::symbols::macro_mutations::COMMANDS.contains(&command.name.as_str())
                || possible.get(&command.name).is_some_and(|targets| {
                    targets
                        .iter()
                        .any(|name| crate::symbols::macro_mutations::COMMANDS.contains(name))
                })
                || matches!(command.name.as_str(), "csuse" | "endcsname")
        }) {
            return Some(BTreeMap::new());
        }
        let operands = self.known_argument_tokens(source, parsed);
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        let mut result = BTreeMap::<_, BTreeSet<_>>::new();
        let mut work = 0usize;
        for (index, command) in parsed.commands.iter().enumerate() {
            work += 64;
            if work > 32 * 1024 * 1024 {
                return None;
            }
            if operands.contains(&command.span.start) {
                continue;
            }
            let Some((name, _, after, _)) =
                self.invocation(source, parsed, index, &operands, &declarations)
            else {
                continue;
            };
            let candidates = possible.get(name).into_iter().flatten().copied().chain(
                crate::symbols::macro_mutations::COMMANDS
                    .contains(&name)
                    .then_some(name),
            );
            for candidate in
                candidates.filter(|name| crate::symbols::macro_mutations::COMMANDS.contains(name))
            {
                let target =
                    crate::symbols::macro_mutations::target(source, after, candidate, &mut work);
                work += target.as_ref().map_or(64, |name| name.len() + 64);
                if work > 32 * 1024 * 1024 {
                    return None;
                }
                result.entry(command.span.start).or_default().insert(target);
            }
        }
        Some(result)
    }

    /// Locate stored or conditional code for conservative mutation guards.
    /// This does not determine whether, when, or how often code executes.
    pub(crate) fn stored_code(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        possible: &StoredCodeAliases,
    ) -> Option<Vec<(usize, std::ops::Range<usize>)>> {
        let code_parameter = |label: &str| {
            crate::symbols::macro_mutations::code(label)
                || matches!(label, "true code" | "false code")
        };
        let parameters = |name: &str| {
            if matches!(name, "mkbibfootnote" | "mkbibfootnotetext") {
                Some(&[("code", false)][..])
            } else {
                kernel_hook_parameters(name)
                    .or_else(|| crate::symbols::macro_mutations::parameters(name))
            }
        };
        let stores_code = |name: &str| {
            parameters(name)
                .is_some_and(|parameters| parameters.iter().any(|(label, _)| code_parameter(label)))
        };
        if !parsed.commands.iter().any(|command| {
            stores_code(&command.name)
                || possible.contains_key(&command.name)
                || matches!(command.name.as_str(), "csuse" | "endcsname")
                || self
                    .forwarded_builtin(&command.name)
                    .is_some_and(stores_code)
        }) {
            return Some(Vec::new());
        }
        let operands = self.argument_tokens(source, parsed);
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        let mut result = Vec::new();
        let mut work = 0;
        for (index, command) in parsed.commands.iter().enumerate() {
            if operands.contains(&command.span.start) {
                continue;
            }
            let Some((name, _, after, _)) =
                self.invocation(source, parsed, index, &operands, &declarations)
            else {
                continue;
            };
            let canonical = self.forwarded_builtin(name).unwrap_or(name);
            let mut candidates: BTreeSet<_> =
                possible.get(name).into_iter().flatten().copied().collect();
            if stores_code(canonical) && self.command_unchanged(canonical) {
                candidates.insert(canonical);
            }
            for candidate in candidates {
                let Some(parameters) = parameters(candidate) else {
                    continue;
                };
                let mut at = after;
                if !parameters.iter().any(|(label, _)| code_parameter(label)) {
                    continue;
                }
                for (label, optional) in parameters {
                    let before = at;
                    at = syntax::skip_space(source, at);
                    work += at - before + 64;
                    if work > 32 * 1024 * 1024 {
                        return None;
                    }
                    if *optional {
                        if source.as_bytes().get(at) == Some(&b'[') {
                            let Some((_, end)) = syntax::bracket_argument(source, at) else {
                                break;
                            };
                            work += end - at;
                            at = end;
                        }
                        continue;
                    }
                    let (span, end) = if source.as_bytes().get(at) == Some(&b'{') {
                        let Some((argument, end)) = syntax::group(source, at, b'{', b'}') else {
                            break;
                        };
                        (argument.span, end)
                    } else {
                        let Some(token) = syntax::required_token(source, at) else {
                            break;
                        };
                        (at..at + token.len(), at + token.len())
                    };
                    work += end - at;
                    if work > 32 * 1024 * 1024 {
                        return None;
                    }
                    at = end;
                    if code_parameter(label) {
                        result.push((command.span.start, span));
                    }
                }
            }
        }
        Some(result)
    }

    /// Literal filenames following static named calls and pure source aliases.
    /// Ordinary input/include commands retain their existing lexical handling.
    pub(crate) fn indirect_source_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(
            source,
            parsed,
            positions,
            &["input", "include", "InputIfFileExists"],
        )
    }

    /// Static package and class loaders share source-call token boundaries, including
    /// options and comma-separated names, without executing macro bodies.
    pub(crate) fn indirect_loader_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(
            source,
            parsed,
            positions,
            &[
                "usepackage",
                "RequirePackage",
                "RequirePackageWithOptions",
                "documentclass",
                "LoadClass",
                "LoadClassWithOptions",
            ],
        )
    }

    /// Option forwarding refers to a file without importing its declarations.
    pub(crate) fn indirect_option_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(
            source,
            parsed,
            positions,
            &["PassOptionsToPackage", "PassOptionsToClass"],
        )
    }

    /// A complete static selection scope, with unknown calls kept distinct
    /// from an absence of filters. Pure alias bodies are declarations, not calls.
    pub(crate) fn selection_filters(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> Option<Vec<BTreeSet<String>>> {
        if ["includeonly", "csuse", "csname", "endcsname"]
            .iter()
            .any(|name| self.local.contains_key(*name))
        {
            return None;
        }
        let positions = self.source_command_positions(source, parsed);
        let declarations: BTreeSet<_> = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        let alias_bodies: BTreeSet<_> = parsed
            .definitions
            .iter()
            .filter(|entry| self.invokes_builtin(&entry.name, "includeonly"))
            .filter_map(|entry| source_alias_body(source, parsed, entry))
            .map(|body| syntax::skip_space(source, body.span.start))
            .collect();
        let empty_operands = BTreeSet::new();
        for (index, command) in parsed.commands.iter().enumerate() {
            if declarations.contains(&(command.span.start, command.span.end)) {
                continue;
            }
            if positions.contains(&command.span.start) {
                if command.name == "csuse"
                    && command
                        .argument
                        .as_ref()
                        .is_none_or(|argument| !syntax::is_static_key(&argument.value))
                {
                    return None;
                }
                if command.name == "csname"
                    && syntax::literal_csname(source, &parsed.commands, index).is_none()
                {
                    return None;
                }
            }
            let Some((name, start, _, _)) =
                self.invocation(source, parsed, index, &empty_operands, &declarations)
            else {
                continue;
            };
            if self.invokes_builtin(name, "includeonly")
                && !positions.contains(&start)
                && !alias_bodies.contains(&start)
            {
                // An unsupported wrapper or operand can execute a selector.
                // Its presence cannot certify that the document has no filter.
                return None;
            }
        }
        let indirect = self.indirect_selection_invocations(source, parsed, &positions);
        if indirect.iter().any(|call| !call.certain) {
            return None;
        }
        parsed
            .commands
            .iter()
            .filter(|command| {
                command.name == "includeonly" && positions.contains(&command.span.start)
            })
            .chain(indirect.iter().map(|call| &call.command))
            .map(|command| {
                syntax::includeonly_names(source, command).map(|names| names.into_iter().collect())
            })
            .collect()
    }

    /// Existence checks reference files without executing their contents.
    pub(crate) fn indirect_conditional_checks(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(source, parsed, positions, &["IfFileExists"])
    }

    /// Selection references name chapters without loading their declarations.
    pub(crate) fn indirect_selection_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(source, parsed, positions, &["includeonly"])
    }

    /// Bibliography resources provide data rather than LaTeX declarations.
    pub(crate) fn indirect_bibliography_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(
            source,
            parsed,
            positions,
            &[
                "bibliography",
                "nobibliography",
                "bibliographystyle",
                "addbibresource",
                "addglobalbib",
                "addsectionbib",
            ],
        )
    }

    /// Static filenames in complete deferred code remain references, while
    /// their presence cannot establish that the resource has been loaded.
    pub(crate) fn indirect_bibliography_references(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        active: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        let bodies = merged_spans(definition_body_spans(source, parsed));
        let positions = parsed
            .commands
            .iter()
            .filter(|command| {
                active.contains(&command.span.start) || span_contains(&bodies, command.span.start)
            })
            .map(|command| command.span.start)
            .collect();
        let mut calls = self.indirect_bibliography_invocations(source, parsed, &positions);
        if !calls.is_empty() {
            let operands = self.editing_argument_tokens(source, parsed);
            calls.retain(|call| !operands.contains(&call.command.span.start));
        }
        calls
    }

    pub(crate) fn indirect_graphics_path_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(source, parsed, positions, &["graphicspath"])
    }

    pub(crate) fn indirect_graphics_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        self.indirect_file_invocations(source, parsed, positions, &["includegraphics"])
    }

    pub(crate) fn graphics_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
    ) -> Vec<SourceCall> {
        let mut calls = self.indirect_graphics_invocations(source, parsed, positions);
        calls.extend(
            parsed
                .commands
                .iter()
                .filter(|command| {
                    command.name == "includegraphics" && positions.contains(&command.span.start)
                })
                .map(|command| SourceCall {
                    command: command.clone(),
                    certain: self.command_unchanged("includegraphics"),
                }),
        );
        for call in &mut calls {
            call.command.argument = crate::project::graphics::argument(source, &call.command);
        }
        calls.sort_by_key(|call| call.command.span.start);
        calls
    }

    fn indirect_file_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
        builtins: &[&str],
    ) -> Vec<SourceCall> {
        let doc = crate::analysis::AnalysisContext::source_document(source, parsed);
        if let Some(calls) = doc
            .as_ref()
            .and_then(|doc| self.memo.get_calls(doc, builtins, positions))
        {
            return calls;
        }
        let calls = self.compute_indirect_file_invocations(source, parsed, positions, builtins);
        if let Some(doc) = doc.filter(|_| {
            !crate::analysis::AnalysisContext::current().is_some_and(|ctx| ctx.cancelled())
        }) {
            self.memo.insert_calls(doc, builtins, positions, &calls);
        }
        calls
    }

    fn compute_indirect_file_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        positions: &BTreeSet<usize>,
        builtins: &[&str],
    ) -> Vec<SourceCall> {
        let aliased = self
            .local
            .values()
            .flatten()
            .any(|signature| builtins.contains(&signature.name.as_str()));
        if !aliased
            && !parsed
                .commands
                .iter()
                .any(|command| matches!(command.name.as_str(), "csuse" | "endcsname"))
        {
            return vec![];
        }
        let operands = parsed
            .commands
            .iter()
            .filter(|command| !positions.contains(&command.span.start))
            .map(|command| command.span.start)
            .collect();
        let declarations: BTreeSet<_> = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        parsed
            .commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| {
                if !positions.contains(&command.span.start)
                    || builtins.contains(&command.name.as_str())
                    || declarations.contains(&(command.span.start, command.span.end))
                {
                    return None;
                }
                let (name, start, end, _) =
                    self.invocation(source, parsed, index, &operands, &declarations)?;
                if !positions.contains(&start) {
                    return None;
                }
                let builtin = builtins.iter().copied().find(|builtin| {
                    name == *builtin
                        || self
                            .local
                            .get(name)
                            .and_then(Option::as_ref)
                            .is_some_and(|signature| signature.name == *builtin)
                })?;
                Some(SourceCall {
                    command: syntax::Command {
                        name: builtin.into(),
                        span: start..end,
                        argument: if crate::bibliography::bib_resources::references_file(builtin) {
                            crate::bibliography::bib_resources::read_argument(
                                source,
                                end,
                                self.resource_is_tokenized(source, parsed, command.span.start),
                            )
                        } else if syntax::loader_extension(builtin).is_some()
                            || matches!(builtin, "bibliographystyle" | "includegraphics")
                        {
                            syntax::file_name_argument_start(source, builtin, end)
                                .and_then(|start| syntax::group(source, start, b'{', b'}'))
                                .map(|(argument, _)| argument)
                        } else {
                            syntax::source_input_argument(source, builtin, end)
                        },
                    },
                    certain: self.invokes_builtin(name, builtin),
                })
            })
            .collect()
    }

    pub(crate) fn source_command_positions(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> BTreeSet<usize> {
        self.initialized_source_command_positions(source, parsed, false)
    }

    /// A bibliography trace can independently prove immediate native notes.
    /// Other immediate analyses continue to treat their code as deferred.
    pub(crate) fn initialized_source_command_positions(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        immediate_notes: bool,
    ) -> BTreeSet<usize> {
        self.memoized_tokens(source, parsed, 16 | u8::from(immediate_notes), || {
            self.compute_initialized_source_command_positions(source, parsed, immediate_notes)
        })
    }

    fn compute_initialized_source_command_positions(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        immediate_notes: bool,
    ) -> BTreeSet<usize> {
        let mode = if immediate_notes {
            ParameterRead::InitializedNotes
        } else {
            ParameterRead::Source
        };
        let operands = self.argument_tokens_with_literal(source, parsed, None, false, false, mode);
        let bodies = merged_spans(definition_body_spans(source, parsed));
        let active = |start| !operands.contains(&start) && !span_contains(&bodies, start);
        // A redefined endinput also ends the region we can interpret safely.
        let stop = parsed
            .commands
            .iter()
            .filter(|command| command.name == "endinput" && active(command.span.start))
            .map(|command| {
                source[command.span.end..]
                    .find(['\r', '\n'])
                    .map_or(source.len(), |after| command.span.end + after)
            })
            .min()
            .unwrap_or(source.len());
        parsed
            .commands
            .iter()
            .filter(|command| command.span.start < stop && active(command.span.start))
            .map(|command| command.span.start)
            .collect()
    }

    pub(crate) fn environment_invocations(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> Vec<EnvironmentCall> {
        self.environment_boundaries(source, parsed)
            .into_iter()
            .filter_map(EnvironmentBoundary::into_call)
            .collect()
    }

    pub(crate) fn environment_boundaries(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> Vec<EnvironmentBoundary> {
        let operands = self.argument_tokens(source, parsed);
        let stored = self.source_argument_tokens(source, parsed);
        let bodies = definition_body_spans(source, parsed);
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        parsed
            .commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| {
                // Retain potential names inside data for rename validation;
                // they must never become editable environment references.
                let (name, start, end, _) =
                    self.invocation(source, parsed, index, &BTreeSet::new(), &declarations)?;
                if !self.invokes_builtin(name, "begin") && !self.invokes_builtin(name, "end") {
                    return None;
                }
                let argument = syntax::environment_name_argument(source, end);
                Some(EnvironmentBoundary {
                    span: start..argument.as_ref().map_or(end, |arg| arg.outer_span.end),
                    begin: self.invokes_builtin(name, "begin"),
                    direct: matches!(command.name.as_str(), "begin" | "end"),
                    operand: operands.contains(&start) || operands.contains(&command.span.start),
                    deferred: bodies.iter().any(|span| span.contains(&start))
                        || stored.contains(&command.span.start)
                            && !operands.contains(&command.span.start),
                    argument,
                })
            })
            .collect()
    }

    /// Tokens definitely rendered as characters by an unchanged TeX primitive.
    /// Unknown user macros can execute their operands, so their argument tokens
    /// are insufficient evidence for removing a build dependency.
    pub(crate) fn quoted_command_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> BTreeSet<usize> {
        let operands = self.source_argument_tokens(source, parsed);
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        let mut result = BTreeSet::new();
        for (index, command) in parsed.commands.iter().enumerate() {
            if operands.contains(&command.span.start) {
                continue;
            }
            let Some((name, _, end, control_spaces)) =
                self.invocation(source, parsed, index, &operands, &declarations)
            else {
                continue;
            };
            let at = optional_start(source, end, control_spaces);
            if self.invokes_builtin(name, "detokenize") {
                if let Some((argument, end)) = syntax::group(source, at, b'{', b'}') {
                    mark_operand_commands(parsed, argument.span.start, end, &mut result);
                }
            } else if self.invokes_builtin(name, "string") || self.invokes_builtin(name, "meaning")
            {
                // These primitives consume one token, including an opening
                // brace itself; they do not consume a following whole group.
                if required_token(source, at).is_some()
                    && syntax::control_word(source, at).is_some()
                {
                    result.insert(at);
                }
            }
        }
        result
    }

    /// Token arguments excluded from immediate execution, including stored
    /// hook code which remains available to structural navigation and edits.
    pub(crate) fn source_argument_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> BTreeSet<usize> {
        self.argument_tokens_with_literal(source, parsed, None, false, false, ParameterRead::Source)
    }

    pub(crate) fn argument_tokens(&self, source: &str, parsed: &syntax::Syntax) -> BTreeSet<usize> {
        self.argument_tokens_with_literal(
            source,
            parsed,
            None,
            false,
            false,
            ParameterRead::Ordinary,
        )
    }

    /// A stable local copy can bound its arguments without making that copy a
    /// confirmed reference/loader alias. Keep the copied shapes private to this
    /// operand query; callers still resolve command meanings through `self`.
    pub(crate) fn editing_argument_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> BTreeSet<usize> {
        self.editing_tokens(source, parsed, ParameterRead::Ordinary)
    }

    /// Ordered custom shapes bound string readers, but do not prove that a
    /// user macro executes argument tokens. Destructive declaration edits also
    /// require the ordinary project grammar to keep those tokens out of data.
    /// Unknown calls protect their grouped arguments, but only a known grammar
    /// establishes that an independent following control is an ungrouped operand.
    pub(crate) fn declaration_argument_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> BTreeSet<usize> {
        self.editing_tokens(source, parsed, ParameterRead::DeclarationData)
    }

    /// Unlike editing operands, these tokens are proved never to execute.
    /// Do not infer execution from a reader nested in an unknown user's data:
    /// that user may remove the reader and execute the remaining tokens.
    pub(crate) fn nonexecuting_argument_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        readers: &BTreeMap<String, usize>,
    ) -> BTreeSet<usize> {
        let operands = self.editing_tokens(source, parsed, ParameterRead::Source);
        // A caller may transform a replacement body or default value before
        // executing it. Prefix maxima preserve nested declaration data.
        let mut deferred = definition_data_spans(source, parsed);
        deferred.sort_unstable_by_key(|span| span.start);
        let mut end = 0;
        for span in &mut deferred {
            end = end.max(span.end);
            span.end = end;
        }
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        let mut result = BTreeSet::new();
        for (index, command) in parsed.commands.iter().enumerate() {
            if operands.contains(&command.span.start)
                || parsed.control_operands.contains(&command.span.start)
            {
                continue;
            }
            let Some((name, _, end, spaces)) =
                self.invocation(source, parsed, index, &operands, &declarations)
            else {
                continue;
            };
            if !readers
                .get(name)
                .is_some_and(|at| *at <= command.span.start)
            {
                continue;
            }
            if deferred
                .partition_point(|span| span.start <= command.span.start)
                .checked_sub(1)
                .is_some_and(|index| command.span.start < deferred[index].end)
            {
                continue;
            }
            if let Some(tex) = self.tex_operands.get(name).and_then(Option::as_ref) {
                if let Some(end) = tex.end_mode(source, end, spaces, true) {
                    mark_operand_commands(parsed, command.span.end, end, &mut result);
                }
                continue;
            }
            let builtin = (!self.local.contains_key(name))
                .then(|| operand_builtin(name))
                .flatten();
            if let Some(signature) = self
                .local
                .get(name)
                .and_then(Option::as_ref)
                .or(builtin.as_ref())
            {
                let read = argument_tokens(
                    (source, parsed),
                    end,
                    spaces,
                    signature,
                    true,
                    None,
                    &mut result,
                );
                if read.unfinished {
                    mark_operand_commands(parsed, read.end, source.len(), &mut result);
                }
            }
        }
        result
    }

    fn editing_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        parameters: ParameterRead,
    ) -> BTreeSet<usize> {
        self.memoized_tokens(source, parsed, parameters as u8 | 32, || {
            self.compute_editing_tokens(source, parsed, parameters)
        })
    }

    fn compute_editing_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        parameters: ParameterRead,
    ) -> BTreeSet<usize> {
        if !parsed
            .definitions
            .iter()
            .any(|entry| literal_copy_target(source, parsed, entry, false).is_some())
        {
            return self
                .argument_tokens_with_literal(source, parsed, None, false, false, parameters);
        }
        let mut shapes = Self {
            local: self.local.clone(),
            tex_operands: self.tex_operands.clone(),
            literal_operands: self.literal_operands.clone(),
            unverified_names: self.unverified_names.clone(),
            detokenize_boundary_known: self.detokenize_boundary_known,
            identity_parameters: self.identity_parameters.clone(),
            hook_arities: self.hook_arities.clone(),
            common_grammars: Default::default(),
            memo: Default::default(),
        };
        shapes.inline_copy_operands(parsed);
        shapes.argument_tokens_with_literal(source, parsed, None, false, false, parameters)
    }

    /// Prove only that a known caller discards all of the tokens it reads.
    /// The caller's replacement text and alias chain are checked separately.
    pub(crate) fn discarded_parameter_token(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        at: usize,
        discarded: &BTreeSet<String>,
    ) -> bool {
        if discarded.is_empty() {
            return false;
        }
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        for (index, lexical) in parsed
            .commands
            .iter()
            .enumerate()
            .take_while(|(_, command)| command.span.start < at)
        {
            if parsed.control_operands.contains(&lexical.span.start) {
                continue;
            }
            let Some((name, _, end, spaces)) = self.invocation(
                source,
                parsed,
                index,
                &parsed.control_operands,
                &declarations,
            ) else {
                continue;
            };
            if !discarded.contains(name)
                || !(self.local.get(name).and_then(Option::as_ref).is_some()
                    || self
                        .tex_operands
                        .get(name)
                        .and_then(Option::as_ref)
                        .is_some())
            {
                continue;
            }
            let shape = parsed.contextual_command_shapes.get(&lexical.span.start);
            let tex = shape
                .and_then(|shape| shape.tex.as_ref())
                .or_else(|| self.tex_operands.get(name).and_then(Option::as_ref));
            if let Some(tex) = tex {
                if tex
                    .end_mode(source, end, spaces, true)
                    .is_some_and(|end| (lexical.span.end..end).contains(&at))
                {
                    return true;
                }
                continue;
            }
            let signature = shape
                .and_then(|shape| shape.signature.as_ref())
                .or_else(|| self.local.get(name).and_then(Option::as_ref));
            if let Some(signature) = signature {
                let mut tokens = BTreeSet::new();
                argument_tokens(
                    (source, parsed),
                    end,
                    spaces,
                    signature,
                    true,
                    Some(at),
                    &mut tokens,
                );
                if tokens.contains(&at) {
                    return true;
                }
            }
        }
        false
    }

    /// Positions confirmed by a known parameter grammar, including repeated
    /// definitions with the same grammar. This does not prove their meaning.
    pub(crate) fn known_argument_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
    ) -> BTreeSet<usize> {
        self.argument_tokens_with_literal(source, parsed, None, false, true, ParameterRead::Source)
    }

    /// A stable copy can establish argument tokenization without certifying
    /// its expansion for navigation. Keep this inference in operand queries:
    /// unresolved, repeated or later source declarations cannot prove a copy's
    /// captured grammar. Pure forwarders retain their late-bound target.
    fn inline_copy_operands(&mut self, parsed: &syntax::Syntax) {
        self.common_grammars = Default::default();
        self.memo = Default::default();
        let mut entries = BTreeMap::new();
        for entry in parsed.definitions.iter().chain(&parsed.environments) {
            entries
                .entry(entry.name.as_str())
                .and_modify(|known| *known = None)
                .or_insert(Some(entry));
        }
        let pending: Vec<_> = self
            .literal_operands
            .iter()
            .filter_map(|(name, operands)| {
                let (target, copied) = match operands.as_slice() {
                    [LiteralOperands::Copy(target)] => (target, true),
                    [LiteralOperands::Alias(target)] => (target, false),
                    _ => return None,
                };
                let entry = entries.get(name.as_ref())?.as_ref()?;
                if copied {
                    let declaration = entry.detail.strip_prefix("Defined with \\")?;
                    if self.local.contains_key(declaration) {
                        return None;
                    }
                    if self.local.contains_key(target.as_str()) {
                        let original = entries.get(target.as_str())?.as_ref()?;
                        if original.span.start >= entry.span.start {
                            return None;
                        }
                    }
                }
                Some((name.clone(), target.as_str()))
            })
            .collect();
        for _ in 0..pending.len() {
            let mut changed = false;
            for (name, target) in &pending {
                let signature = self
                    .local
                    .get(*target)
                    .cloned()
                    .unwrap_or_else(|| operand_builtin(target));
                let tex = self.tex_operands.get(*target).cloned().flatten();
                if signature.is_none() && tex.is_none() {
                    continue;
                }
                if self.local.get(name) != Some(&signature)
                    || self.tex_operands.get(name) != Some(&tex)
                {
                    self.local.insert(name.clone(), signature);
                    self.tex_operands.insert(name.clone(), tex);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Catcode-changing inline commands see ordinary tokens when a known
    /// macro has already collected their code as an argument. Query the first
    /// body position: consuming only the control and delimiter does not tokenize
    /// the following source. Unknown arities cannot establish this context.
    fn pretokenized_literal_end(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        controls: &BTreeSet<usize>,
        at: usize,
        scanned: &mut usize,
    ) -> Option<usize> {
        let bodies: BTreeMap<_, _> = parsed
            .definitions
            .iter()
            .filter_map(|entry| {
                let (_, body) = TexOperands::declaration(source, parsed, entry)?;
                let declaration = entry.detail.strip_prefix("Defined with \\")?;
                if self.local.contains_key(declaration) {
                    return None;
                }
                let before = parsed
                    .commands
                    .partition_point(|command| command.span.start < entry.span.start);
                let command = parsed.commands[..before]
                    .iter()
                    .rev()
                    .find(|command| command.name == declaration)?;
                let end =
                    syntax::group(source, body, b'{', b'}').map_or(source.len(), |(_, end)| end);
                Some((command.span.start, end))
            })
            .collect();
        let mut operands = controls.clone();
        let declarations = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        for (index, command) in parsed.commands.iter().enumerate().skip(*scanned) {
            *scanned = index + 1;
            if operands.contains(&command.span.start) {
                continue;
            }
            if let Some(&end) = bodies.get(&command.span.start) {
                mark_operand_tokens(parsed, command.span.end, end, Some(at), &mut operands);
                if operands.contains(&at) {
                    return Some(end);
                }
                continue;
            }
            let Some((name, _, end, spaces)) =
                self.invocation(source, parsed, index, &operands, &declarations)
            else {
                continue;
            };
            let shape = parsed.contextual_command_shapes.get(&command.span.start);
            let tex = shape
                .and_then(|shape| shape.tex.as_ref())
                .or_else(|| self.tex_operands.get(name).and_then(Option::as_ref));
            let end = if let Some(end) = tex.and_then(|tex| tex.end_mode(source, end, spaces, true))
            {
                mark_operand_tokens(parsed, command.span.end, end, Some(at), &mut operands);
                end
            } else {
                let builtin = (!self.local.contains_key(name))
                    .then(|| operand_builtin(name))
                    .flatten();
                let signature = shape
                    .and_then(|shape| shape.signature.as_ref())
                    .or_else(|| self.local.get(name).and_then(Option::as_ref))
                    .or(builtin.as_ref());
                if let Some(signature) = signature {
                    let read = argument_tokens(
                        (source, parsed),
                        end,
                        spaces,
                        signature,
                        true,
                        Some(at),
                        &mut operands,
                    );
                    if read.unfinished && read.end <= at {
                        return Some(source.len());
                    }
                    read.end
                } else {
                    end
                }
            };
            if operands.contains(&at) {
                return Some(end);
            }
        }
        None
    }

    /// Structural refactors must not replace even executed argument tokens:
    /// adding a group or control word can change the caller's parameter boundary.
    pub(crate) fn is_parameter_token(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        at: usize,
    ) -> bool {
        self.argument_tokens_with_literal(
            source,
            parsed,
            Some(at),
            false,
            false,
            ParameterRead::All,
        )
        .contains(&at)
            || self.possible_literal_argument(source, parsed, at)
    }

    /// Literal tokens consumed by a known parameter grammar. Math scanning
    /// keeps legacy handling of unknown primitives such as sized TeX boxes.
    pub(crate) fn known_literal_is_argument(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        at: usize,
    ) -> bool {
        self.argument_tokens_with_literal(
            source,
            parsed,
            Some(at),
            false,
            true,
            ParameterRead::Source,
        )
        .contains(&at)
            || self.possible_literal_argument(source, parsed, at)
    }

    pub(crate) fn literal_is_argument(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        at: usize,
    ) -> bool {
        self.argument_tokens_with_literal(
            source,
            parsed,
            Some(at),
            false,
            false,
            ParameterRead::Ordinary,
        )
        .contains(&at)
            || self.possible_literal_argument(source, parsed, at)
    }

    /// A possible declaration can exclude a literal without assigning unknown
    /// commands in the consumer the declaration file's fallback arity.
    /// File-verb catcodes cannot alter tokens collected by any enclosing
    /// argument reader, including transparent text and callback code.
    pub(crate) fn resource_is_tokenized(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        at: usize,
    ) -> bool {
        self.pretokenized_literal_end(source, parsed, &parsed.control_operands, at, &mut 0)
            .is_some()
            || self.declared_literal_is_argument(source, parsed, at)
    }

    pub(crate) fn declared_literal_is_argument(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        at: usize,
    ) -> bool {
        self.argument_tokens_with_literal(
            source,
            parsed,
            Some(at),
            true,
            false,
            ParameterRead::Ordinary,
        )
        .contains(&at)
            || self.possible_literal_argument(source, parsed, at)
    }

    fn possible_literal_argument(&self, source: &str, parsed: &syntax::Syntax, at: usize) -> bool {
        if self.literal_operands.is_empty() {
            return false;
        }
        let declarations: BTreeSet<_> = parsed
            .definitions
            .iter()
            .chain(&parsed.environments)
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        let mut deferred = definition_body_spans(source, parsed);
        for entry in &parsed.definitions {
            if let Some((_, end)) = TexOperands::definition(source, parsed, entry) {
                deferred.push(entry.span.start..end);
            }
        }
        for (entry, environment) in parsed
            .definitions
            .iter()
            .map(|entry| (entry, false))
            .chain(parsed.environments.iter().map(|entry| (entry, true)))
        {
            if !self
                .local
                .contains_key(entry.detail.strip_prefix("Defined with \\").unwrap_or(""))
            {
                if let Some((_, end)) = literal_copy_target(source, parsed, entry, environment) {
                    deferred.push(entry.span.start..end);
                }
            }
        }
        let empty = BTreeSet::new();
        for (index, command) in parsed
            .commands
            .iter()
            .enumerate()
            .take_while(|(_, command)| command.span.start < at)
        {
            if parsed.is_ignored(command.span.start)
                || declarations.contains(&(command.span.start, command.span.end))
                || deferred
                    .iter()
                    .any(|span| span.contains(&command.span.start))
            {
                continue;
            }
            let Some((name, _, after, spaces)) =
                self.invocation(source, parsed, index, &empty, &declarations)
            else {
                continue;
            };
            if !self.literal_operands.contains_key(name)
                && (name != "begin" || self.local.contains_key("begin"))
            {
                continue;
            }
            let mut pending = vec![(name.to_owned(), after, spaces, false)];
            let mut visited = BTreeSet::new();
            // Do not skip calls merely consumed by another possible arity:
            // a different declaration may leave that call executable.
            while let Some((name, after, spaces, copied)) = pending.pop() {
                if !visited.insert((name.clone(), after, spaces, copied)) {
                    continue;
                }
                let declared = self.literal_operands.contains_key(name.as_str());
                if name == "begin" && (copied || !self.local.contains_key("begin")) {
                    if let Some(argument) = syntax::environment_name_argument(source, after) {
                        if self.literal_operands.contains_key(argument.value.as_str()) {
                            pending.push((argument.value, argument.outer_span.end, false, false));
                        }
                    }
                    if !declared {
                        continue;
                    }
                }
                let mut consumed = BTreeSet::new();
                if copied || !declared {
                    if name == "halign" && (copied || !self.local.contains_key("halign")) {
                        // A captured primitive alignment executes its body.
                        if !declared {
                            continue;
                        }
                    } else if let Some(signature) = operand_builtin(&name) {
                        let opaque = matches!(
                            name.as_str(),
                            "detokenize"
                                | "graphicspath"
                                | "DeclareGraphicsExtensions"
                                | "lstset"
                                | "fvset"
                                | "includeonly"
                        );
                        argument_tokens(
                            (source, parsed),
                            after,
                            spaces,
                            &signature,
                            opaque,
                            Some(at),
                            &mut consumed,
                        );
                    } else if !declared {
                        unknown_argument_token(
                            source,
                            after,
                            parsed,
                            Some(at),
                            true,
                            &mut consumed,
                        );
                    }
                    if consumed.contains(&at) {
                        return true;
                    }
                }
                for operands in self
                    .literal_operands
                    .get(name.as_str())
                    .into_iter()
                    .flatten()
                {
                    match operands {
                        LiteralOperands::Alias(target) => {
                            pending.push((target.clone(), after, spaces, false))
                        }
                        LiteralOperands::Copy(target) => {
                            pending.push((target.clone(), after, spaces, true))
                        }
                        LiteralOperands::Tex(operands) => {
                            if operands
                                .end(source, after, spaces)
                                .is_some_and(|end| (after..end).contains(&at))
                            {
                                return true;
                            }
                        }
                        LiteralOperands::Signature(signature) => {
                            argument_tokens(
                                (source, parsed),
                                after,
                                spaces,
                                signature,
                                true,
                                Some(at),
                                &mut consumed,
                            );
                            if consumed.contains(&at) {
                                return true;
                            }
                        }
                        LiteralOperands::Unknown => {
                            unknown_argument_token(
                                source,
                                after,
                                parsed,
                                Some(at),
                                true,
                                &mut consumed,
                            );
                            if consumed.contains(&at) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Preserve a boundary shared by every source declaration, without
    /// restoring a unique command meaning, identity body or help signature.
    /// Pure forwarders remain late-bound; captured copies do not participate.
    fn common_parameter_grammars(&self) -> &BTreeMap<String, Option<LiteralOperands>> {
        self.common_grammars.get_or_init(|| {
            self.build_common_parameter_grammars()
                .into_iter()
                .map(|(name, value)| (name.into_owned(), value))
                .collect()
        })
    }

    fn build_common_parameter_grammars(&self) -> BTreeMap<Cow<'a, str>, Option<LiteralOperands>> {
        let mut shapes: BTreeMap<_, _> = self
            .local
            .keys()
            .cloned()
            .map(|name| (name, None))
            .collect();
        let mut aliases = BTreeMap::new();
        for (name, candidates) in &self.literal_operands {
            if self.unverified_names.contains(name) {
                continue;
            }
            let Some(first) = candidates
                .first()
                .filter(|first| candidates.iter().all(|other| first.same_grammar(other)))
            else {
                continue;
            };
            if let LiteralOperands::Alias(target) = first {
                aliases.insert(name.clone(), target.clone());
            } else {
                shapes.insert(name.clone(), Some(first.clone()));
            }
        }
        shapes.extend(resolve_aliases(&shapes, &aliases, |name| {
            operand_builtin(name).map(LiteralOperands::Signature)
        }));
        shapes
    }

    fn memoized_tokens(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        mode: u8,
        compute: impl FnOnce() -> BTreeSet<usize>,
    ) -> BTreeSet<usize> {
        let doc = crate::analysis::AnalysisContext::source_document(source, parsed);
        if let Some(tokens) = doc.as_ref().and_then(|doc| self.memo.get(doc, mode)) {
            return tokens;
        }
        let result = compute();
        if let Some(doc) = doc.filter(|_| {
            !crate::analysis::AnalysisContext::current().is_some_and(|context| context.cancelled())
        }) {
            self.memo.insert(doc, mode, &result);
        }
        result
    }

    fn argument_tokens_with_literal(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        literal: Option<usize>,
        declared_only: bool,
        known_only: bool,
        parameters: ParameterRead,
    ) -> BTreeSet<usize> {
        let compute = || {
            self.compute_argument_tokens_with_literal(
                source,
                parsed,
                literal,
                declared_only,
                known_only,
                parameters,
            )
        };
        if literal.is_some() {
            return compute();
        }
        let mode = parameters as u8 | (u8::from(declared_only) << 6) | (u8::from(known_only) << 7);
        self.memoized_tokens(source, parsed, mode, compute)
    }

    fn compute_argument_tokens_with_literal(
        &self,
        source: &str,
        parsed: &syntax::Syntax,
        literal: Option<usize>,
        declared_only: bool,
        known_only: bool,
        parameters: ParameterRead,
    ) -> BTreeSet<usize> {
        let all_parameters = matches!(parameters, ParameterRead::All);
        let unknown_single = !matches!(parameters, ParameterRead::DeclarationData);
        let local = &self.local;
        let common_grammars = OnceCell::new();
        let deferred_bodies = OnceCell::new();
        let recovery_end = |at| {
            deferred_bodies
                .get_or_init(|| definition_body_spans(source, parsed))
                .iter()
                .filter(|span| span.contains(&at))
                .map(|span| span.end)
                .min()
                .unwrap_or(source.len())
        };
        let mut result = BTreeSet::new();
        let declarations: BTreeSet<_> = parsed
            .definitions
            .iter()
            .map(|entry| (entry.span.start, entry.span.end))
            .collect();
        for entry in &parsed.definitions {
            if let Some((_, end)) = TexOperands::definition(source, parsed, entry) {
                // Parameter-text delimiters are data at declaration time.
                // In particular a delimiter control must
                // not consume the first invocation following the definition.
                mark_operand_tokens(parsed, entry.span.start, end, literal, &mut result);
            }
        }
        for (entry, environment) in parsed
            .definitions
            .iter()
            .map(|entry| (entry, false))
            .chain(parsed.environments.iter().map(|entry| (entry, true)))
        {
            if !self
                .local
                .contains_key(entry.detail.strip_prefix("Defined with \\").unwrap_or(""))
            {
                if let Some((_, end)) = literal_copy_target(source, parsed, entry, environment) {
                    mark_operand_tokens(parsed, entry.span.start, end, literal, &mut result);
                }
            }
        }
        for (index, command) in parsed.commands.iter().enumerate() {
            if result.contains(&command.span.start) {
                continue;
            }
            if let Some(shape) = parsed
                .contextual_command_shapes
                .get(&command.span.start)
                .filter(|_| !matches!(parameters, ParameterRead::DeclarationData))
            {
                shape.argument_tokens(source, parsed, command, literal, &mut result);
                continue;
            }
            if let Some(arity) = parsed.string_test_arities.get(&command.span.start) {
                if let Some(count) = arity {
                    let name = if *count == 2 { "ifstrequal" } else { "ifblank" };
                    if let Some(signature) = operand_builtin(name) {
                        argument_tokens(
                            (source, parsed),
                            command.span.end,
                            true,
                            &signature,
                            false,
                            literal,
                            &mut result,
                        );
                    }
                } else if !known_only {
                    unknown_argument_token(
                        source,
                        command.span.end,
                        parsed,
                        literal,
                        unknown_single,
                        &mut result,
                    );
                }
                continue;
            }
            let Some((name, _, end, control_spaces)) =
                self.invocation(source, parsed, index, &result, &declarations)
            else {
                continue;
            };
            if declared_only && !local.contains_key(name) {
                continue;
            }
            // A primitive alignment executes its body. Treating that body as
            // opaque macro data would hide the literal row tabs we validate.
            if literal.is_some() && self.invokes_builtin(name, "halign") {
                continue;
            }
            if self.invokes_builtin(name, "globaldefs") {
                if let Some((_, after)) = self.literal_integer_value(source, end) {
                    // A backtick control is a character-code operand, not a
                    // macro invocation that can consume the following call.
                    mark_operand_tokens(parsed, end, after, literal, &mut result);
                }
                continue;
            }
            if self.invokes_builtin(name, "noexpand") {
                // Expansion suppresses one control token, including a control
                // symbol. A brace or ordinary math/alignment token keeps its
                // original meaning; it does not make a following group data.
                // A csuse name group preserves following space tokens, unlike
                // a literal control word or endcsname.
                let at = optional_start(source, end, control_spaces);
                // Refactors also preserve an ordinary single-token operand:
                // replacing it with a control would change expansion suppression.
                if (source.as_bytes().get(at) == Some(&b'\\') && token(source, at).is_some())
                    || (all_parameters && literal == Some(at))
                {
                    result.insert(at);
                }
                continue;
            }
            // This primitive consumes exactly one balanced general-text
            // group. The unknown-macro fallback would also consume the next
            // control token after that group, hiding an independent file call.
            if self.invokes_builtin(name, "detokenize") {
                if !self.detokenize_boundary_known {
                    if !known_only {
                        unknown_argument_token(
                            source,
                            end,
                            parsed,
                            literal,
                            unknown_single,
                            &mut result,
                        );
                    }
                    continue;
                }
                let at = syntax::skip_space(source, end);
                if let Some((_, after)) = syntax::group(source, at, b'{', b'}') {
                    mark_operand_tokens(parsed, at, after, literal, &mut result);
                } else if !known_only {
                    unknown_argument_token(
                        source,
                        end,
                        parsed,
                        literal,
                        unknown_single,
                        &mut result,
                    );
                }
                continue;
            }
            // A named call or a pure alias can invoke begin. Its environment
            // name follows the resolved invocation, rather than the lexical
            // command (whose first group may instead contain "begin").
            let begin = self
                .invokes_builtin(name, "begin")
                .then(|| syntax::environment_name_argument(source, end))
                .flatten();
            let environment = begin
                .as_ref()
                .filter(|argument| local.contains_key(argument.value.as_str()));
            let name = environment.map_or(name, |argument| argument.value.as_str());
            let end = environment.map_or(end, |argument| argument.outer_span.end);
            let control_spaces = environment.is_none() && control_spaces;
            if matches!(parameters, ParameterRead::DeclarationData)
                && environment.is_none()
                && self.identity_parameters.contains(name)
            {
                continue;
            }
            let common = local
                .get(name)
                .filter(|signature| signature.is_none())
                .and_then(|_| {
                    common_grammars
                        .get_or_init(|| self.common_parameter_grammars())
                        .get(name)
                        .and_then(Option::as_ref)
                });
            if let Some(operands) =
                self.tex_operands
                    .get(name)
                    .and_then(Option::as_ref)
                    .or(match common {
                        Some(LiteralOperands::Tex(operands)) => Some(operands),
                        _ => None,
                    })
            {
                // Delimited TeX arguments are read without executing any of
                // their tokens, including controls inside balanced groups.
                if let Some(end) = operands.end(source, end, control_spaces) {
                    mark_operand_tokens(parsed, command.span.end, end, literal, &mut result);
                    continue;
                }
                if common.is_some() {
                    mark_operand_tokens(
                        parsed,
                        end,
                        recovery_end(command.span.start),
                        literal,
                        &mut result,
                    );
                    continue;
                }
            }
            let signature = local.get(name).and_then(Option::as_ref).or(match common {
                Some(LiteralOperands::Signature(signature)) => Some(signature),
                _ => None,
            });
            let builtin = begin
                .as_ref()
                .filter(|_| environment.is_none())
                .and_then(|argument| builtin_environment(&argument.value))
                .or_else(|| self.hook_signature(name, source, end, true))
                .or_else(|| {
                    (!local.contains_key(name))
                        .then(|| operand_builtin(name))
                        .flatten()
                });
            // A builtin environment has additional parameters beyond begin's
            // name group, including when begin was reached through an alias.
            if let Some(signature) = builtin.as_ref().or(signature) {
                // Resolved aliases retain their leaf's name. Builtin
                // formatters and their pure aliases execute grouped contents;
                // a user macro can instead discard or detokenize its argument.
                // Message readers expand text without executing the resulting
                // nonexpandable tokens, including declarations and callbacks.
                let hook = kernel_hook_parameters(signature.name.as_str()).is_some();
                let note = matches!(
                    signature.name.as_str(),
                    "mkbibfootnote" | "mkbibfootnotetext"
                );
                let patch =
                    crate::symbols::macro_mutations::COMMANDS.contains(&signature.name.as_str());
                let opaque = all_parameters
                    || hook
                        && matches!(
                            parameters,
                            ParameterRead::Source | ParameterRead::InitializedNotes
                        )
                    || note && matches!(parameters, ParameterRead::Source)
                    || patch
                        && matches!(
                            parameters,
                            ParameterRead::Source | ParameterRead::InitializedNotes
                        )
                    || common.is_some()
                    || local.contains_key(signature.name.as_str())
                    || matches!(
                        signature.name.as_str(),
                        "graphicspath"
                            | "DeclareGraphicsExtensions"
                            | "lstset"
                            | "fvset"
                            | "includeonly"
                            | "typeout"
                            | "message"
                    );
                let read = argument_tokens(
                    (source, parsed),
                    end,
                    control_spaces,
                    signature,
                    opaque,
                    literal,
                    &mut result,
                );
                if (opaque || hook || note || patch)
                    && read.unfinished
                    && (hook
                        || note
                        || patch
                        || matches!(parameters, ParameterRead::DeclarationData)
                        || common.is_some()
                        || matches!(signature.name.as_str(), "typeout" | "message"))
                {
                    mark_operand_tokens(
                        parsed,
                        read.end,
                        recovery_end(command.span.start),
                        literal,
                        &mut result,
                    );
                }
            } else if !known_only
                && (literal.is_none() || !self.literal_operands.contains_key(name))
            {
                unknown_argument_token(source, end, parsed, literal, unknown_single, &mut result);
            }
        }
        result
    }
}

#[derive(Clone, Copy)]
enum ParameterRead {
    Ordinary,
    All,
    DeclarationData,
    // Stored hook code is structurally editable, but not immediate source.
    Source,
    InitializedNotes,
}

fn insert_command_signature<'a>(
    local: &mut BTreeMap<Cow<'a, str>, Option<Signature>>,
    tex_operands: &mut BTreeMap<Cow<'a, str>, Option<TexOperands>>,
    aliases: &mut BTreeMap<Cow<'a, str>, String>,
    name: Cow<'a, str>,
    signature: Option<Signature>,
    tex: Option<TexOperands>,
    alias: Option<String>,
) {
    match local.entry(name) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            if let Some(target) = alias {
                aliases.insert(entry.key().clone(), target);
            }
            tex_operands.insert(entry.key().clone(), tex);
            entry.insert(signature);
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            aliases.remove(entry.key());
            tex_operands.insert(entry.key().clone(), None);
            entry.insert(None);
        }
    }
}

fn argument_free(name: &str) -> bool {
    // Standard math symbols and operators emit an atom without consuming the
    // next control token. In particular, alpha/right and sin/right are two
    // independent calls. Local declarations still override these builtins.
    matches!(
        name,
        "relax"
            | "("
            | ")"
            | "["
            | "]"
            | "ExplSyntaxOn"
            | "ExplSyntaxOff"
            | "noindent"
            | "indent"
            | "maketitle"
            | "tableofcontents"
            | "listoffigures"
            | "listoftables"
            | "appendix"
            | "newpage"
            | "clearpage"
            | "cleardoublepage"
            | "newblock"
            | "newrefsegment"
            | "endrefsection"
            | "endrefcontext"
            | "endrefsegment"
            | "centering"
            | "raggedright"
            | "raggedleft"
            | "TeX"
            | "LaTeX"
            | "LaTeXe"
            | "quad"
            | "qquad"
            | "enspace"
            | "thinspace"
            | "normalfont"
            | "selectfont"
            | "bfseries"
            | "mdseries"
            | "itshape"
            | "upshape"
            | "slshape"
            | "scshape"
            | "rmfamily"
            | "sffamily"
            | "ttfamily"
            | "begingroup"
            | "endgroup"
            | "bgroup"
            | "egroup"
            | "global"
            | "globaldefs"
            | "long"
            | "outer"
            | "protected"
            | "hline"
            | "par"
            | "newline"
            | "hfill"
            | "vfill"
            | "hfil"
            | "vfil"
            | "iftrue"
            | "iffalse"
            | "else"
            | "fi"
            | "or"
            | "tiny"
            | "scriptsize"
            | "footnotesize"
            | "small"
            | "normalsize"
            | "large"
            | "Large"
            | "LARGE"
            | "huge"
            | "Huge"
            | "alpha"
            | "beta"
            | "gamma"
            | "delta"
            | "epsilon"
            | "varepsilon"
            | "zeta"
            | "eta"
            | "theta"
            | "vartheta"
            | "iota"
            | "kappa"
            | "lambda"
            | "mu"
            | "nu"
            | "xi"
            | "pi"
            | "varpi"
            | "rho"
            | "varrho"
            | "sigma"
            | "varsigma"
            | "tau"
            | "upsilon"
            | "phi"
            | "varphi"
            | "chi"
            | "psi"
            | "omega"
            | "Gamma"
            | "Delta"
            | "Theta"
            | "Lambda"
            | "Xi"
            | "Pi"
            | "Sigma"
            | "Upsilon"
            | "Phi"
            | "Psi"
            | "Omega"
            | "infty"
            | "partial"
            | "nabla"
            | "ell"
            | "hbar"
            | "imath"
            | "jmath"
            | "sum"
            | "prod"
            | "coprod"
            | "int"
            | "oint"
            | "bigcup"
            | "bigcap"
            | "bigsqcup"
            | "bigvee"
            | "bigwedge"
            | "bigodot"
            | "bigotimes"
            | "bigoplus"
            | "biguplus"
            | "log"
            | "lg"
            | "ln"
            | "lim"
            | "limsup"
            | "liminf"
            | "sin"
            | "arcsin"
            | "sinh"
            | "cos"
            | "arccos"
            | "cosh"
            | "tan"
            | "arctan"
            | "tanh"
            | "cot"
            | "coth"
            | "sec"
            | "csc"
            | "max"
            | "min"
            | "sup"
            | "inf"
            | "arg"
            | "ker"
            | "dim"
            | "hom"
            | "det"
            | "exp"
            | "Pr"
            | "gcd"
            | "deg"
    )
}

// Beamer headers and list markers have bounded operands. In particular, a
// completed title or an empty item must not consume the following begin token.
fn presentation_arguments(name: &str) -> Option<Vec<Parameter>> {
    let overlay = || Kind::Delimited {
        open: "<".into(),
        close: ">".into(),
        optional: true,
    };
    let group = || Kind::Delimited {
        open: "{".into(),
        close: "}".into(),
        optional: true,
    };
    let parameters = match name {
        "frametitle" | "framesubtitle" => vec![
            (Kind::Star, "*"),
            (overlay(), "<overlay>"),
            (Kind::Optional, "[short title]"),
            (Kind::RequiredGroup, "{title}"),
        ],
        "item" => vec![
            (overlay(), "<overlay>"),
            (Kind::Optional, "[label]"),
            (overlay(), "<overlay>"),
        ],
        "frame" => vec![
            (overlay(), "<overlay>"),
            (Kind::Optional, "[default overlay]"),
            (Kind::Optional, "[options]"),
            (group(), "{title}"),
            (group(), "{subtitle}"),
        ],
        "block" | "alertblock" | "exampleblock" => vec![
            (overlay(), "<overlay>"),
            (Kind::RequiredGroup, "{title}"),
            (overlay(), "<overlay>"),
        ],
        "itemize" | "enumerate" | "description" => {
            vec![(Kind::Optional, "[options]"), (Kind::Optional, "[label]")]
        }
        "columns" => vec![(Kind::Optional, "[options]")],
        "column" => vec![
            (Kind::Optional, "[position]"),
            (Kind::RequiredGroup, "{width}"),
        ],
        _ => return None,
    };
    Some(
        parameters
            .into_iter()
            .map(|(kind, label)| Parameter {
                obey_spaces: false,
                kind,
                label: label.into(),
            })
            .collect(),
    )
}

pub(crate) fn operand_builtin(name: &str) -> Option<Signature> {
    builtin(name).or_else(|| {
        let parameters = if matches!(name, "frametitle" | "framesubtitle" | "item") {
            presentation_arguments(name)?
        } else if argument_free(name) || name == "noexpand" {
            // noexpand's single-token rule is handled explicitly above; the
            // empty shape here lets static aliases retain its primitive name.
            vec![]
        } else if name == "detokenize" {
            // Give pure aliases the primitive's grouped operand boundary.
            vec![Parameter {
                obey_spaces: false,
                kind: Kind::RequiredGroup,
                label: "{tokens}".into(),
            }]
        } else if matches!(name, "refcontext" | "newrefcontext") {
            // Native contexts optionally read options and a braced context
            // name; absent arguments leave the following control executable.
            vec![
                Parameter {
                    obey_spaces: false,
                    kind: Kind::Optional,
                    label: "[options]".into(),
                },
                Parameter {
                    obey_spaces: false,
                    kind: Kind::Delimited {
                        open: "{".into(),
                        close: "}".into(),
                        optional: true,
                    },
                    label: "{context}".into(),
                },
            ]
        } else if name == "newtheorem" {
            // Both numbered forms end after the title and possible reset
            // counter; the starred form has no optional counter operands.
            [
                (Kind::Required, "{name}"),
                (Kind::UnstarredOptional, "[shared]"),
                (Kind::Required, "{title}"),
                (Kind::UnstarredOptional, "[within]"),
            ]
            .into_iter()
            .map(|(kind, label)| Parameter {
                obey_spaces: false,
                kind,
                label: label.into(),
            })
            .collect()
        } else if matches!(
            name,
            "graphicspath" | "DeclareGraphicsExtensions" | "lstset" | "fvset" | "includeonly"
        ) {
            // One configuration argument ends here; a following file call
            // is independent. Settings remain data, including through aliases.
            vec![Parameter {
                obey_spaces: false,
                kind: Kind::Required,
                label: "{configuration}".into(),
            }]
        } else if matches!(name, "csdef" | "csgdef" | "csedef" | "csxdef") {
            // Grouped destinations and replacement text do not consume the
            // following command. Parameter text stops this group-only reader.
            ["{name}", "{definition}"]
                .into_iter()
                .map(|label| Parameter {
                    obey_spaces: false,
                    kind: Kind::RequiredGroup,
                    label: label.into(),
                })
                .collect()
        } else {
            return None;
        };
        Some(Signature {
            name: name.into(),
            parameters,
            documentation: String::new(),
            starred: name == "newtheorem",
            nested_brackets: name != "newtheorem",
        })
    })
}

/// Braces in arguments and replacement text are balanced before their tokens
/// can execute. Include unfinished groups so diagnostics still report them.
pub(crate) fn reads_brace(source: &str, command: &syntax::Command, brace: usize) -> bool {
    fn operand_end(source: &str, mut at: usize) -> Option<usize> {
        let value = token(source, at)?;
        at += value.len();
        if value == r"\csname" {
            loop {
                at = syntax::skip_space(source, at);
                let value = token(source, at)?;
                at += value.len();
                if value == r"\endcsname" {
                    break;
                }
            }
        }
        Some(syntax::skip_space(source, at))
    }

    let mut at = syntax::skip_space(source, command.span.end);
    if matches!(
        command.name.as_str(),
        "def" | "gdef" | "edef" | "xdef" | "csdef" | "csgdef" | "csedef" | "csxdef"
    ) {
        if command.name.starts_with("cs") {
            if let Some((_, end)) = syntax::group(source, at, b'{', b'}') {
                if brace < end {
                    return true;
                }
                at = syntax::skip_space(source, end);
            }
        }
        while at <= brace {
            if source.as_bytes().get(at) == Some(&b'{') {
                return syntax::group(source, at, b'{', b'}').is_none_or(|(_, end)| brace < end);
            }
            let Some(end) = operand_end(source, at) else {
                return false;
            };
            at = end;
        }
        return false;
    }
    let Some(signature) = operand_builtin(&command.name) else {
        // Unknown arity: retain lexical pairing within a following group.
        while at < brace {
            let (open, close) = match source.as_bytes().get(at) {
                Some(b'{') => (b'{', b'}'),
                Some(b'[') => (b'[', b']'),
                _ => return false,
            };
            let Some((_, end)) = syntax::group(source, at, open, close) else {
                return true;
            };
            if brace < end {
                return true;
            }
            at = syntax::skip_space(source, end);
        }
        return false;
    };
    if signature.starred && source.as_bytes().get(at) == Some(&b'*') {
        at = syntax::skip_space(source, at + 1);
    }
    for parameter in &signature.parameters {
        if at >= brace {
            return false;
        }
        let bracket = match &parameter.kind {
            Kind::Optional | Kind::UnstarredOptional | Kind::RequiredBracket => {
                if source.as_bytes().get(at) != Some(&b'[') {
                    if matches!(parameter.kind, Kind::RequiredBracket) {
                        return false;
                    }
                    continue;
                }
                true
            }
            Kind::Star => {
                if source.as_bytes().get(at) == Some(&b'*') {
                    at = syntax::skip_space(source, at + 1);
                }
                continue;
            }
            Kind::Flag(_) | Kind::Delimited { .. } => return false,
            _ => false,
        };
        if bracket || source.as_bytes().get(at) == Some(&b'{') {
            let group = if bracket && !signature.nested_brackets {
                syntax::bracket_argument(source, at)
            } else {
                syntax::group(
                    source,
                    at,
                    if bracket { b'[' } else { b'{' },
                    if bracket { b']' } else { b'}' },
                )
            };
            let Some((_, end)) = group else {
                return true;
            };
            if brace < end {
                return true;
            }
            at = syntax::skip_space(source, end);
        } else {
            if matches!(parameter.kind, Kind::RequiredGroup) {
                return false;
            }
            let Some(end) = required_token(source, at).and_then(|_| operand_end(source, at)) else {
                return false;
            };
            at = end;
        }
    }
    false
}

/// Walk and memoize alias chains iteratively, including unknown targets and
/// cycles. The source graph may be arbitrarily deep without using Rust's stack.
fn resolve_aliases<'a, T: Clone>(
    local: &BTreeMap<Cow<'a, str>, Option<T>>,
    aliases: &BTreeMap<Cow<'a, str>, String>,
    builtin: impl Fn(&str) -> Option<T>,
) -> BTreeMap<Cow<'a, str>, Option<T>> {
    let mut resolved: BTreeMap<Cow<'a, str>, Option<T>> = BTreeMap::new();
    for name in aliases.keys() {
        let mut at = name.as_ref();
        let mut chain = vec![];
        let mut seen = BTreeSet::new();
        let signature = loop {
            if let Some(signature) = resolved.get(at) {
                break signature.clone();
            }
            if !seen.insert(at) {
                break None;
            }
            if let Some((key, target)) = aliases.get_key_value(at) {
                chain.push(key.clone());
                at = target;
            } else {
                break local.get(at).cloned().unwrap_or_else(|| builtin(at));
            }
        };
        for name in chain {
            resolved.insert(name, signature.clone());
        }
    }
    resolved
}

fn mark_operand_commands(
    parsed: &syntax::Syntax,
    start: usize,
    end: usize,
    result: &mut BTreeSet<usize>,
) {
    let first = parsed
        .commands
        .partition_point(|command| command.span.start < start);
    result.extend(
        parsed.commands[first..]
            .iter()
            .take_while(|command| command.span.start < end)
            .map(|command| command.span.start),
    );
}

fn mark_operand_tokens(
    parsed: &syntax::Syntax,
    start: usize,
    end: usize,
    literal: Option<usize>,
    result: &mut BTreeSet<usize>,
) {
    mark_operand_commands(parsed, start, end, result);
    if let Some(at) = literal.filter(|at| (start..end).contains(at)) {
        result.insert(at);
    }
}

fn unknown_argument_token(
    source: &str,
    mut at: usize,
    parsed: &syntax::Syntax,
    literal: Option<usize>,
    single: bool,
    result: &mut BTreeSet<usize>,
) {
    loop {
        at = syntax::skip_space(source, at);
        let grouped = match source.as_bytes().get(at) {
            Some(b'{') => syntax::group(source, at, b'{', b'}'),
            Some(b'[') => syntax::bracket_argument(source, at),
            Some(b'*') => {
                at += 1;
                continue;
            }
            _ => None,
        };
        if let Some((_, end)) = grouped {
            mark_operand_tokens(parsed, at, end, literal, result);
            at = end;
        } else {
            if !single && matches!(source.as_bytes().get(at), Some(b'{' | b'[')) {
                mark_operand_tokens(parsed, at, source.len(), literal, result);
            }
            if single && (syntax::control_word(source, at).is_some() || literal == Some(at)) {
                result.insert(at);
            }
            return;
        }
    }
}

fn operand_delimited_end(source: &str, at: usize, open: &str, close: &str) -> Option<usize> {
    let mut at = at + open.len();
    loop {
        at = syntax::skip_space(source, at);
        let value = token(source, at)?;
        if value == close {
            return Some(at + close.len());
        }
        if value == "{" {
            at = syntax::group(source, at, b'{', b'}')?.1;
        } else {
            at += value.len();
        }
    }
}

fn builtin_argument_data(signature: &Signature, parameter: usize) -> bool {
    match signature.name.as_str() {
        name if HOOK_CONDITIONS.contains(&name) => parameter == 0,
        name if crate::symbols::macro_mutations::COMMANDS.contains(&name) => {
            !crate::symbols::macro_mutations::code(
                signature.parameters[parameter]
                    .label
                    .trim_matches(['{', '}', '[', ']']),
            )
        }
        "AddToHook" | "AddToHookWithArguments" => parameter < 2,
        "AddToHookNext" | "AddToHookNextWithArguments" | "AtBeginDocument" | "AtEndDocument" => {
            parameter == 0
        }
        name if kernel_hook_parameters(name).is_some() => true,
        "newrefsection"
        | "refsection"
        | "newrefcontext"
        | "refcontext"
        | "assignrefcontextentries"
        | "assignrefcontextcats"
        | "assignrefcontextkeyws" => true,
        "begin" => {
            parameter == 1
                && signature
                    .parameters
                    .first()
                    .is_some_and(|name| name.label == "{refsection}")
        }
        "AtDataInput" | "AtBeginBiblist" | "AtEveryBiblistitem" => parameter == 0,
        "newenvironment" | "renewenvironment" | "provideenvironment" | "NewEnviron"
        | "RenewEnviron" => parameter < 3,
        "NewDocumentEnvironment"
        | "RenewDocumentEnvironment"
        | "ProvideDocumentEnvironment"
        | "DeclareDocumentEnvironment" => parameter < 2,
        "newtheorem" => parameter != 2,
        name if syntax::declares_environment_copy(name) => true,
        _ => false,
    }
}

/// The reader's progress is used only by lexical tokenization recovery.
/// Other consumers still require complete operands before proposing edits.
struct ArgumentRead {
    end: usize,
    unfinished: bool,
}

impl ArgumentRead {
    fn complete(end: usize) -> Self {
        Self {
            end,
            unfinished: false,
        }
    }
    fn unfinished(end: usize) -> Self {
        Self {
            end,
            unfinished: true,
        }
    }
}

fn argument_tokens(
    document: (&str, &syntax::Syntax),
    mut at: usize,
    mut control_spaces: bool,
    signature: &Signature,
    opaque: bool,
    literal: Option<usize>,
    result: &mut BTreeSet<usize>,
) -> ArgumentRead {
    let (source, parsed) = document;
    let mut starred = false;
    if signature.starred {
        at = optional_start(source, at, true);
        if source.as_bytes().get(at) == Some(&b'*') {
            if literal == Some(at) {
                result.insert(at);
            }
            at += 1;
            starred = true;
            control_spaces = false;
        }
    }
    for (index, parameter) in signature.parameters.iter().enumerate() {
        // Defaults and parameter specifications are data; start/end code is
        // indexed normally. A body may display its defaults without executing
        // their tokens, so color renames must not rewrite those tokens.
        let opaque = (opaque || builtin_argument_data(signature, index)).then_some(parsed);
        if starred && matches!(parameter.kind, Kind::UnstarredOptional) {
            continue;
        }
        let before = at;
        at = optional_start(source, at, !parameter.obey_spaces || control_spaces);
        let bracket = match &parameter.kind {
            Kind::Optional | Kind::UnstarredOptional | Kind::RequiredBracket => {
                if source.as_bytes().get(at) != Some(&b'[') {
                    if matches!(parameter.kind, Kind::RequiredBracket) {
                        return ArgumentRead::complete(at);
                    }
                    at = before;
                    continue;
                }
                true
            }
            Kind::Star => {
                if source.as_bytes().get(at) == Some(&b'*') {
                    if literal == Some(at) {
                        result.insert(at);
                    }
                    at += 1;
                    starred = true;
                    control_spaces = false;
                } else {
                    at = before;
                }
                continue;
            }
            Kind::Flag(flag) => {
                if token(source, at) == Some(flag.as_str()) {
                    if syntax::control_word(flag, 0).is_some() || literal == Some(at) {
                        result.insert(at);
                    }
                    at += flag.len();
                    control_spaces = syntax::control_word(flag, 0).is_some();
                } else {
                    at = before;
                }
                continue;
            }
            Kind::Delimited {
                open,
                close,
                optional,
            } => {
                if token(source, at) != Some(open.as_str()) {
                    if *optional {
                        at = before;
                        continue;
                    }
                    return ArgumentRead::complete(at);
                }
                let Some(end) = operand_delimited_end(source, at, open, close) else {
                    return ArgumentRead::unfinished(at);
                };
                if let Some(parsed) = opaque {
                    mark_operand_tokens(parsed, at, end, literal, result);
                }
                if syntax::control_word(open, 0).is_some() {
                    result.insert(at);
                }
                if syntax::control_word(close, 0).is_some() {
                    result.insert(end - close.len());
                }
                at = end;
                control_spaces = syntax::control_word(close, 0).is_some();
                continue;
            }
            _ => false,
        };
        if bracket || source.as_bytes().get(at) == Some(&b'{') {
            let group = if bracket && parsed.raw_resource_brackets.contains(&at) {
                syntax::verbatim_file_bracket(source, at)
            } else if bracket && !signature.nested_brackets {
                syntax::bracket_argument(source, at)
            } else {
                syntax::group(
                    source,
                    at,
                    if bracket { b'[' } else { b'{' },
                    if bracket { b']' } else { b'}' },
                )
            };
            let Some((_, end)) = group else {
                return ArgumentRead::unfinished(at);
            };
            if let Some(parsed) = opaque {
                mark_operand_tokens(parsed, at, end, literal, result);
            }
            at = end;
            control_spaces = false;
        } else {
            if matches!(parameter.kind, Kind::RequiredGroup) {
                return ArgumentRead::complete(at);
            }
            let Some(value) = required_token(source, at) else {
                return ArgumentRead::complete(at);
            };
            let word = syntax::control_word(source, at).is_some();
            if word || literal == Some(at) {
                result.insert(at);
            }
            at += value.len();
            control_spaces = word || value == "\\ ";
        }
    }
    ArgumentRead::complete(at)
}

/// Preserve an existing bare argument on this logical line. A normal newline
/// usually means the user is starting a new call, followed by unrelated source
/// such as end{document}; a commented-out newline continues the current call.
pub(crate) fn completion_required_token(source: &str) -> Option<&str> {
    let mut i = 0;
    loop {
        match source.as_bytes().get(i) {
            Some(b' ' | b'\t') => i += 1,
            Some(b'%') => i += source[i..].find('\n')? + 1,
            _ => {
                let value = required_token(source, i)?;
                // These tokens normally close a surrounding math, alignment,
                // group, or environment. Keep placeholders before them when
                // completing an unfinished call.
                return (!matches!(
                    value,
                    "$" | "&"
                        | "\\\\"
                        | "\\)"
                        | "\\]"
                        | "\\right"
                        | "\\end"
                        | "\\endgroup"
                        | "\\egroup"
                        | "\\fi"
                        | "\\else"
                        | "\\or"
                ))
                .then_some(value);
            }
        }
    }
}

fn delimiter_spec(source: &str, start: usize) -> Option<(String, usize)> {
    let start = syntax::skip_space(source, start);
    let (value, end) = if source.as_bytes().get(start) == Some(&b'{') {
        let (group, end) = syntax::group(source, start, b'{', b'}')?;
        (group.value, end)
    } else {
        let value = token(source, start)?;
        (value.to_owned(), start + value.len())
    };
    let at = syntax::skip_space(&value, 0);
    let delimiter = token(&value, at)?;
    if syntax::skip_space(&value, at + delimiter.len()) != value.len()
        || delimiter.chars().any(char::is_whitespace)
        || matches!(delimiter, "{" | "}" | "%" | "#")
    {
        return None;
    }
    Some((delimiter.to_owned(), end))
}

pub(crate) fn signature(
    workspace: &Workspace,
    name: &str,
    scope: &BTreeSet<Url>,
) -> Option<Signature> {
    lookup(workspace, name, false, scope, None)
}

pub(crate) fn uses_builtin(workspace: &Workspace, name: &str, scope: &BTreeSet<Url>) -> bool {
    builtin(name)
        .is_some_and(|builtin| signature(workspace, name, scope).as_ref() == Some(&builtin))
}

pub(crate) fn lookup<'a>(
    workspace: &'a Workspace,
    name: &str,
    environment: bool,
    scope: &BTreeSet<Url>,
    aliases: Option<&OnceCell<CommandSignatures<'a>>>,
) -> Option<Signature> {
    lookup_indexed(workspace, name, environment, scope, aliases, None)
}

type SourceDeclaration<'a> = (&'a Url, &'a Document, &'a Entry);
fn find_declaration<'a>(
    workspace: &'a Workspace,
    name: &str,
    environment: bool,
    scope: &BTreeSet<Url>,
    index: Option<&crate::symbols::command_docs::Index<'a>>,
) -> Result<Option<SourceDeclaration<'a>>, ()> {
    if let Some(index) = index {
        let namespace = if environment {
            crate::symbols::command_docs::Namespace::Environment
        } else {
            crate::symbols::command_docs::Namespace::Command
        };
        return index
            .declaration(name, namespace)
            .map(|declaration| declaration.map(|d| (d.uri, d.doc, d.entry)));
    }
    let mut declarations = workspace
        .documents
        .iter()
        .filter(|(uri, _)| scope.contains(*uri))
        .flat_map(|(uri, doc)| {
            (if environment {
                &doc.syntax.environments
            } else {
                &doc.syntax.definitions
            })
            .iter()
            .filter(move |entry| entry.name == name)
            .map(move |entry| (uri, doc, entry))
        });
    let first = declarations.next();
    if declarations.next().is_some() {
        Err(())
    } else {
        Ok(first)
    }
}

pub(crate) fn lookup_indexed<'a>(
    workspace: &'a Workspace,
    name: &str,
    environment: bool,
    scope: &BTreeSet<Url>,
    aliases: Option<&OnceCell<CommandSignatures<'a>>>,
    index: Option<&crate::symbols::command_docs::Index<'a>>,
) -> Option<Signature> {
    let declaration = find_declaration(workspace, name, environment, scope, index).ok()?;
    if let Some((uri, doc, entry)) = declaration {
        let mut signature = source_definition(&doc.text.source, &doc.syntax, entry, environment)?;
        if !environment
            && signature.parameters.is_empty()
            && source_alias(&doc.text.source, &doc.syntax, entry).is_some()
        {
            let local = OnceCell::new();
            let signatures = aliases.unwrap_or(&local).get_or_init(|| {
                CommandSignatures::new(
                    workspace
                        .documents
                        .iter()
                        .filter(|(uri, _)| scope.contains(*uri))
                        .map(|(_, doc)| (doc.text.source.as_str(), &doc.syntax)),
                )
            });
            signature = signatures.local.get(name)?.as_ref()?.clone();
            signature.name = name.into();
        }
        describe_definition(uri, doc, entry, environment, signature)
    } else if environment {
        builtin_environment(name)
    } else {
        if let Some((uri, doc, entry)) =
            find_declaration(workspace, name, true, scope, index).ok()?
        {
            let mut signature = definition(uri, doc, entry, true)?;
            signature.name = name.into();
            signature.parameters.remove(0);
            Some(signature)
        } else if matches!(
            name,
            "thefield"
                | "iffieldundef"
                | "iflistundef"
                | "ifnameundef"
                | "iffieldequalstr"
                | "iffieldequals"
                | "iffieldsequal"
                | "iflistequals"
                | "ifnameequals"
                | "ifentrytype"
        ) && workspace.documents.iter().any(|(uri, doc)| {
            scope.contains(uri) && crate::symbols::generated::generated_command_exists(doc, name)
        }) {
            // These data accessors can be replaced implicitly by counters,
            // theorems or newif; their builtin argument lists no longer apply.
            None
        } else {
            if BIBLATEX_TEXT_DEPENDENCIES
                .iter()
                .any(|(forwarder, _)| *forwarder == name)
            {
                let local = OnceCell::new();
                let signatures = aliases.unwrap_or(&local).get_or_init(|| {
                    CommandSignatures::new(
                        workspace
                            .documents
                            .iter()
                            .filter(|(uri, _)| scope.contains(*uri))
                            .map(|(_, doc)| (doc.text.source.as_str(), &doc.syntax)),
                    )
                });
                if !signatures.command_unchanged(name) {
                    return None;
                }
            }
            builtin(name).or_else(|| {
                let (package, command) = crate::project::packages::command(workspace, scope, name)?;
                Some(Signature {
                    name: name.into(),
                    parameters: command
                        .parameters
                        .iter()
                        .map(|parameter| {
                            let optional = parameter.starts_with('[');
                            Parameter {
                                obey_spaces: false,
                                kind: if parameter.starts_with('(') {
                                    Kind::Delimited {
                                        open: "(".into(),
                                        close: ")".into(),
                                        optional: true,
                                    }
                                } else if optional {
                                    Kind::Optional
                                } else {
                                    Kind::Required
                                },
                                label: if optional || parameter.starts_with(['{', '(']) {
                                    parameter.clone()
                                } else {
                                    format!("{{{parameter}}}")
                                },
                            }
                        })
                        .collect(),
                    documentation: crate::project::packages::documentation(package, command),
                    starred: command.starred,
                    nested_brackets: false,
                })
            })
        }
    }
}

pub(crate) fn builtin_environment(name: &str) -> Option<Signature> {
    if let Some(arguments) = presentation_arguments(name)
        .filter(|_| !matches!(name, "frametitle" | "framesubtitle" | "item"))
    {
        let mut parameters = vec![Parameter {
            obey_spaces: false,
            kind: Kind::Required,
            label: format!("{{{name}}}"),
        }];
        parameters.extend(arguments);
        return Some(Signature {
            name: "begin".into(),
            parameters,
            documentation: format!("Arguments of the {name} environment."),
            starred: false,
            nested_brackets: false,
        });
    }
    let arguments: &[(&str, bool)] = match name {
        "tabular" | "array" => &[("position", true), ("columns", false)],
        "tabular*" => &[("width", false), ("position", true), ("columns", false)],
        "minipage" => &[
            ("position", true),
            ("height", true),
            ("inner position", true),
            ("width", false),
        ],
        "figure" | "figure*" | "table" | "table*" => &[("placement", true)],
        "thebibliography" => &[("widest label", false)],
        "refsection" => &[("resources", true)],
        "refsegment" => &[],
        "list" => &[("label", false), ("spacing", false)],
        "alignat" | "alignat*" => &[("column pairs", false)],
        "alignedat" => &[("position", true), ("column pairs", false)],
        "minted" => &[("options", true), ("language", false)],
        "Verbatim" | "Verbatim*" | "LVerbatim" | "LVerbatim*" | "BVerbatim" | "BVerbatim*"
        | "VerbatimWrite" | "VerbatimBuffer" | "VerbEnv" | "lstlisting" => &[("options", true)],
        "SaveVerbatim" | "SaveVerbatim*" => &[("options", true), ("name", false)],
        "VerbatimOut" | "VerbatimOut*" => &[("options", true), ("file", false)],
        _ => return None,
    };
    let mut parameters = vec![Parameter {
        obey_spaces: false,
        kind: Kind::Required,
        label: format!("{{{name}}}"),
    }];
    parameters.extend(arguments.iter().map(|(label, optional)| Parameter {
        obey_spaces: false,
        kind: if *optional {
            Kind::Optional
        } else {
            Kind::Required
        },
        label: if *optional {
            format!("[{label}]")
        } else {
            format!("{{{label}}}")
        },
    }));
    Some(Signature {
        name: "begin".into(),
        parameters,
        documentation: format!("Arguments of the {name} environment."),
        starred: false,
        nested_brackets: !matches!(name, "minted" | "lstlisting" | "refsection")
            && !crate::parser::fancyvrb::is_environment(name),
    })
}

pub(crate) fn optional_start(source: &str, mut i: usize, ignore_spaces: bool) -> usize {
    let bytes = source.as_bytes();
    let mut skip_blanks = ignore_spaces;
    let mut new_line = false;
    loop {
        match bytes.get(i) {
            Some(b'%') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                skip_blanks = true;
                new_line = true;
            }
            Some(b' ' | b'\t') if skip_blanks => i += 1,
            Some(b'\r' | b'\n') if skip_blanks && !new_line => {
                if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                i += 1;
                new_line = true;
            }
            _ => return i,
        }
    }
}

pub(crate) fn citation_parameters(
    style: crate::bibliography::citations::Style,
    groups: usize,
) -> Vec<Parameter> {
    if style.context_entries() {
        return vec![
            Parameter {
                obey_spaces: false,
                kind: Kind::RequiredBracket,
                label: "[options]".into(),
            },
            Parameter {
                obey_spaces: false,
                kind: Kind::CitationKeys(style),
                label: "{keys}".into(),
            },
        ];
    }
    if style.single_key() {
        let labels: &[&str] = if matches!(
            style,
            crate::bibliography::citations::Style::EntryCategoryQuery
        ) {
            &["{key}", "{category}", "{true}", "{false}"]
        } else if style.context_codes() == 2 {
            &["{key}", "{true}", "{false}"]
        } else {
            &["{key}", "{code}"]
        };
        return labels
            .iter()
            .enumerate()
            .map(|(index, label)| Parameter {
                obey_spaces: false,
                kind: if index == 0 {
                    Kind::CitationKeys(style)
                } else {
                    Kind::Required
                },
                label: (*label).into(),
            })
            .collect();
    }
    if style.category_membership() || style.set_definition() {
        return vec![
            Parameter {
                obey_spaces: false,
                kind: Kind::Required,
                label: if style.set_definition() {
                    "{set key}"
                } else {
                    "{category}"
                }
                .into(),
            },
            Parameter {
                obey_spaces: false,
                kind: Kind::CitationKeys(style),
                label: "{keys}".into(),
            },
        ];
    }
    let mut parameters = Vec::new();
    if style.repeated() {
        parameters.extend(
            ["note / prenote", "postnote"]
                .into_iter()
                .map(|label| Parameter {
                    obey_spaces: false,
                    kind: Kind::Delimited {
                        open: "(".into(),
                        close: ")".into(),
                        optional: true,
                    },
                    label: format!("({label})"),
                }),
        );
    }
    for group in 1..=groups {
        let suffix = if style.repeated() {
            format!(" {group}")
        } else {
            String::new()
        };
        let fields = if style.volume() {
            vec![
                ("prenote", Kind::Optional),
                ("volume", Kind::Required),
                ("pages", Kind::Optional),
            ]
        } else {
            vec![
                ("note / prenote", Kind::Optional),
                ("postnote", Kind::Optional),
            ]
        };
        for (label, kind) in fields {
            let label = if matches!(kind, Kind::Optional) {
                format!("[{label}{suffix}]")
            } else {
                format!("{{{label}{suffix}}}")
            };
            parameters.push(Parameter {
                obey_spaces: false,
                kind,
                label,
            });
        }
        parameters.push(Parameter {
            obey_spaces: false,
            kind: Kind::CitationKeys(style),
            label: format!("{{keys{suffix}}}"),
        });
        if let Some(label) = style.selector() {
            parameters.push(Parameter {
                obey_spaces: false,
                kind: Kind::Optional,
                label: "[format]".into(),
            });
            parameters.push(Parameter {
                obey_spaces: false,
                kind: Kind::Required,
                label: format!("{{{label}}}"),
            });
        }
    }
    parameters
}

#[cfg(test)]
mod tests;
