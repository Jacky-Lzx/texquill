//! LSP signature help and completion snippets over shared command semantics.
use crate::{
    parser::syntax::{self, required_token, token},
    project::workspace::{Document, DocumentKind, Workspace},
    symbols::commands::{
        builtin_environment, citation_parameters, completion_required_token, lookup,
        operand_builtin, optional_start, signature, starts_letter, uses_builtin, CommandSignatures,
        Kind, Parameter, Signature,
    },
};
use lsp_types::*;
use std::{cell::OnceCell, collections::BTreeSet};

impl Signature {
    fn information(self, active: usize) -> SignatureHelp {
        let label = self.label();
        SignatureHelp {
            signatures: vec![SignatureInformation {
                label,
                documentation: Some(Documentation::String(self.documentation)),
                parameters: Some(
                    self.parameters
                        .into_iter()
                        .map(|parameter| ParameterInformation {
                            label: ParameterLabel::Simple(parameter.label),
                            documentation: parameter.obey_spaces.then(|| Documentation::String(
                                "An intervening TeX space prevents this optional argument from being read (xparse !).".into(),
                            )),
                        })
                        .collect(),
                ),
                active_parameter: Some(active as u32),
            }],
            active_signature: Some(0),
            active_parameter: Some(active as u32),
        }
    }
}

/// Omit optional arguments and flags so accepting a completion preserves the
/// macro's defaults. Tabstops follow the order of its required arguments.
pub fn completion(
    workspace: &Workspace,
    name: &str,
    scope: &BTreeSet<Url>,
) -> Option<(String, Option<String>)> {
    CompletionSignatures::new(workspace, scope).at(name, "")
}

/// Reuse lazy alias resolution across all candidates of one completion request.
pub(crate) struct CompletionSignatures<'a> {
    workspace: &'a Workspace,
    scope: &'a BTreeSet<Url>,
    aliases: OnceCell<CommandSignatures<'a>>,
    definitions: OnceCell<crate::symbols::command_docs::Index<'a>>,
}

impl<'a> CompletionSignatures<'a> {
    pub(crate) fn new(workspace: &'a Workspace, scope: &'a BTreeSet<Url>) -> Self {
        Self {
            workspace,
            scope,
            aliases: OnceCell::new(),
            definitions: OnceCell::new(),
        }
    }

    pub(super) fn resolved(&self, name: &str) -> Option<Signature> {
        crate::symbols::commands::lookup_indexed(
            self.workspace,
            name,
            false,
            self.scope,
            Some(&self.aliases),
            Some(self.definitions.get_or_init(|| {
                crate::symbols::command_docs::Index::new(self.workspace, self.scope)
            })),
        )
    }

    pub(crate) fn at(&self, name: &str, following: &str) -> Option<(String, Option<String>)> {
        let signature = self.resolved(name)?;
        if existing_arguments(&signature, following) {
            return Some((signature.label(), None));
        }
        let mut snippet = name.to_owned();
        let mut tabstop = 0;
        for (index, parameter) in signature.parameters.iter().enumerate() {
            let (open, close) = match &parameter.kind {
                Kind::Required
                | Kind::RequiredGroup
                | Kind::CitationKeys(_)
                | Kind::CommandToken => ("{", "}"),
                Kind::RequiredBracket => ("[", "]"),
                Kind::Delimited {
                    open,
                    close,
                    optional: false,
                } => (open.as_str(), close.as_str()),
                _ => continue,
            };
            let number = parameter
                .label
                .strip_prefix("{#")
                .and_then(|label| label.strip_suffix('}'))
                .and_then(|number| number.parse::<usize>().ok())
                .unwrap_or(index + 1);
            tabstop += 1;
            if tabstop == 1 && starts_letter(open) {
                snippet.push(' ');
            }
            if matches!(parameter.kind, Kind::Delimited { .. }) {
                push_delimiter(&mut snippet, open);
            } else {
                snippet.push_str(open);
            }
            snippet.push_str(&format!("${{{tabstop}:arg{number}}}"));
            if matches!(parameter.kind, Kind::Delimited { .. }) {
                push_delimiter(&mut snippet, close);
            } else {
                snippet.push_str(close);
            }
        }
        Some((signature.label(), (tabstop > 0).then_some(snippet)))
    }

    pub(crate) fn has_existing_arguments(&self, name: &str, following: &str) -> bool {
        // A short builtin-only result need not allocate a project-wide lookup
        // table. Reuse it when user/package candidates have already needed it.
        if self.definitions.get().is_none()
            || name == "lstinline"
            || name == "Verb"
            || crate::parser::fancyvrb::is_inline_save(name)
            || crate::parser::fancyvrb::is_use(name)
        {
            return has_existing_arguments(self.workspace, name, self.scope, following);
        }
        self.resolved(name)
            .is_some_and(|signature| existing_arguments(&signature, following))
    }
}

pub(crate) fn has_existing_arguments(
    workspace: &Workspace,
    name: &str,
    scope: &BTreeSet<Url>,
    following: &str,
) -> bool {
    if name == "lstinline"
        && uses_builtin(workspace, name, scope)
        && crate::parser::listings::inline_span(following, 0).is_some()
    {
        return true;
    }
    if name == "Verb"
        && uses_builtin(workspace, name, scope)
        && [false, true].into_iter().any(|ordinary| {
            crate::parser::fancyvrb::inline_start(following, 0, ordinary).is_some_and(|(at, _)| {
                following[at..]
                    .chars()
                    .next()
                    .is_some_and(|delimiter| delimiter != '}')
            })
        })
    {
        return true;
    }
    if (crate::parser::fancyvrb::is_inline_save(name) || crate::parser::fancyvrb::is_use(name))
        && uses_builtin(workspace, name, scope)
        && crate::parser::fancyvrb::inline_start(following, 0, true)
            .is_some_and(|(at, _)| crate::parser::fancyvrb::name_argument(following, at).is_some())
    {
        return true;
    }
    signature(workspace, name, scope)
        .is_some_and(|signature| existing_arguments(&signature, following))
}

pub(super) fn existing_arguments(signature: &Signature, following: &str) -> bool {
    let ungrouped_argument = completion_required_token(following);
    let following = &following[syntax::skip_space(following, 0)..];
    // The same argument reader decides whether existing custom delimiters must
    // be preserved. Stop at the first required argument: later delimiters cannot
    // start a valid call here.
    for parameter in &signature.parameters {
        match &parameter.kind {
            Kind::Delimited { open, optional, .. } => {
                if token(following, 0) == Some(open.as_str()) {
                    return true;
                }
                if !optional {
                    break;
                }
            }
            Kind::Optional | Kind::UnstarredOptional | Kind::Star => {}
            Kind::Flag(flag) => {
                if token(following, 0) == Some(flag.as_str()) {
                    return true;
                }
            }
            Kind::Required if ungrouped_argument.is_some() => return true,
            Kind::CommandToken
                if ungrouped_argument
                    .is_some_and(|value| syntax::control_word(value, 0).is_some()) =>
            {
                return true;
            }
            _ => break,
        }
    }
    false
}

fn push_delimiter(snippet: &mut String, delimiter: &str) {
    for ch in delimiter.chars() {
        if matches!(ch, '\\' | '$' | '}') {
            snippet.push('\\');
        }
        snippet.push(ch);
    }
    // A control word followed by an argument placeholder (or another letter
    // delimiter) needs a separator; TeX discards this space when tokenizing.
    if delimiter.starts_with('\\') && syntax::control_word(delimiter, 0).is_some() {
        snippet.push(' ');
    }
}

fn group_end(
    doc: &Document,
    start: usize,
    cursor: usize,
    open: u8,
    close: u8,
    raw_header: Option<&std::ops::Range<usize>>,
) -> Option<usize> {
    let source = &doc.text.source;
    let bytes = source.as_bytes();
    let mut depth = 1usize;
    let mut braces = 0usize;
    let mut i = start + 1;
    // Parser-emitted ignored spans are ordered and do not overlap.
    let mut ignored = doc.syntax.ignored.partition_point(|span| span.end <= i);
    while i < cursor {
        while doc
            .syntax
            .ignored
            .get(ignored)
            .is_some_and(|span| span.end <= i)
        {
            ignored += 1;
        }
        if let Some(span) = doc.syntax.ignored.get(ignored) {
            if span.start <= i
                && !raw_header.is_some_and(|header| span.start == header.start && i < header.end)
            {
                i = span.end;
                continue;
            }
        }
        match bytes[i] {
            b'%' => {
                while i < cursor && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'\\' => {
                i += 1;
                if i < source.len() {
                    i += source[i..].chars().next().unwrap().len_utf8();
                }
                continue;
            }
            b'{' if open != b'{' => braces += 1,
            b'}' if open != b'{' => braces = braces.saturating_sub(1),
            ch if ch == open && braces == 0 => depth += 1,
            ch if ch == close && braces == 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn delimited_end(
    doc: &Document,
    start: usize,
    cursor: usize,
    open: &str,
    close: &str,
) -> Option<usize> {
    let source = &doc.text.source;
    let mut i = start + open.len();
    let mut depth = 1usize;
    let mut braces = 0usize;
    let mut ignored = doc.syntax.ignored.partition_point(|span| span.end <= i);
    while i < cursor {
        while doc
            .syntax
            .ignored
            .get(ignored)
            .is_some_and(|span| span.end <= i)
        {
            ignored += 1;
        }
        if let Some(span) = doc.syntax.ignored.get(ignored) {
            if span.start <= i {
                i = span.end;
                continue;
            }
        }
        let current = token(source, i)?;
        i += current.len();
        if i > cursor {
            return None;
        }
        match current {
            "{" => braces += 1,
            "}" => braces = braces.saturating_sub(1),
            current if current == close && braces == 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            current if current == open && braces == 0 => depth += 1,
            _ => {}
        }
    }
    None
}

/// Readers may ignore spaces, including lexical blanks after a control word.
/// A comment removes its endline and the next line's indentation. A genuinely
/// blank line contributes \par even when ordinary space tokens are ignored.
fn active_parameter(
    doc: &Document,
    command: &syntax::Command,
    end: usize,
    cursor: usize,
    signature: &Signature,
    raw_header: Option<&std::ops::Range<usize>>,
) -> Option<usize> {
    let source = &doc.text.source;
    let mut i = end;
    // Literal control words discard following lexical blanks; named calls
    // through a grouped csuse argument do not. endcsname is a control word too.
    let mut control_word_spaces = end == command.span.end;
    let mut starred = false;
    if signature.starred {
        let at = optional_start(source, i, true);
        if source.as_bytes().get(at) == Some(&b'*') {
            i = at + 1;
            starred = true;
            control_word_spaces = false;
        }
    }
    for (index, parameter) in signature.parameters.iter().enumerate() {
        if starred && matches!(parameter.kind, Kind::UnstarredOptional) {
            continue;
        }
        let before_space = i;
        i = if parameter.obey_spaces {
            optional_start(source, i, control_word_spaces)
        } else {
            optional_start(source, i, true)
        };
        let missing_optional = match &parameter.kind {
            Kind::Optional | Kind::UnstarredOptional => source.as_bytes().get(i) != Some(&b'['),
            Kind::Star => source.as_bytes().get(i) != Some(&b'*'),
            Kind::Flag(flag) => token(source, i) != Some(flag.as_str()),
            Kind::Delimited {
                open,
                optional: true,
                ..
            } => token(source, i) != Some(open.as_str()),
            _ => false,
        };
        if missing_optional && i <= cursor && i < source.len() {
            // Optional lookahead preserves skipped space tokens on a miss.
            // A later ! parameter must see them even if this reader ignored them.
            i = before_space;
            continue;
        }
        if i >= cursor {
            return Some(index);
        }
        let (open, close) = match &parameter.kind {
            Kind::Flag(flag) => {
                if token(source, i) == Some(flag.as_str()) {
                    let end = i + flag.len();
                    if cursor < end {
                        return Some(index);
                    }
                    i = end;
                    control_word_spaces = syntax::control_word(flag, 0).is_some();
                }
                continue;
            }
            Kind::Delimited {
                open,
                close,
                optional,
            } => {
                if token(source, i) != Some(open.as_str()) {
                    if *optional {
                        continue;
                    }
                    return None;
                }
                match delimited_end(doc, i, cursor, open, close) {
                    Some(end) => {
                        i = end;
                        control_word_spaces = syntax::control_word(close, 0).is_some();
                    }
                    None => return Some(index),
                }
                continue;
            }
            Kind::CommandToken if source.as_bytes().get(i) != Some(&b'{') => {
                let end = crate::symbols::named_commands::control_argument_end(doc, command, i)?;
                if cursor <= end {
                    return Some(index);
                }
                i = end;
                control_word_spaces = false;
                continue;
            }
            Kind::Star => {
                if source.as_bytes().get(i) == Some(&b'*') {
                    i += 1;
                    starred = true;
                    control_word_spaces = false;
                }
                continue;
            }
            Kind::Optional | Kind::UnstarredOptional if source.as_bytes().get(i) != Some(&b'[') => {
                continue
            }
            Kind::RequiredBracket if source.as_bytes().get(i) != Some(&b'[') => return None,
            Kind::Optional | Kind::UnstarredOptional | Kind::RequiredBracket => (b'[', b']'),
            Kind::Required if source.as_bytes().get(i) != Some(&b'{') => {
                if &source[i..] == "\\" {
                    return Some(index);
                }
                let argument = required_token(source, i)?;
                let end = i + argument.len();
                let word = syntax::control_word(argument, 0).is_some();
                // At EOF the user may still be typing this control word. A
                // lexical separator confirms that the argument has finished.
                if cursor < end || (word && cursor == end && end == source.len()) {
                    return Some(index);
                }
                i = end;
                // Both control words and the control-space token discard
                // following lexical blanks; other control symbols do not.
                control_word_spaces = word || argument == "\\ ";
                continue;
            }
            Kind::RequiredGroup | Kind::CitationKeys(_)
                if source.as_bytes().get(i) != Some(&b'{') =>
            {
                return None
            }
            Kind::Required | Kind::RequiredGroup | Kind::CitationKeys(_) | Kind::CommandToken => {
                (b'{', b'}')
            }
        };
        let end = if open == b'[' && doc.syntax.raw_resource_brackets.contains(&i) {
            syntax::verbatim_file_bracket(&source[..cursor], i).map(|(_, end)| end)
        } else if open == b'[' && !signature.nested_brackets {
            syntax::optional_note_end(&source[..cursor], i)
        } else {
            group_end(doc, i, cursor, open, close, raw_header)
        };
        match end {
            Some(end) => {
                i = end;
                control_word_spaces = false;
            }
            None => return Some(index),
        }
    }
    None
}

fn citation_parameter(
    style: crate::bibliography::citations::Style,
    group: usize,
    role: crate::bibliography::citations::Role,
) -> usize {
    use crate::bibliography::citations::Role;
    if style.context_entries() {
        return match role {
            Role::Note(0) => 0,
            Role::Keys => 1,
            _ => unreachable!(),
        };
    }
    if style.single_key() {
        return match role {
            Role::Keys => 0,
            Role::Category => 1,
            Role::Code(index) => {
                index
                    + 1
                    + usize::from(matches!(
                        style,
                        crate::bibliography::citations::Style::EntryCategoryQuery
                    ))
            }
            _ => unreachable!(),
        };
    }
    if style.category_membership() || style.set_definition() {
        return match role {
            Role::Category | Role::SetKey => 0,
            Role::Keys => 1,
            _ => unreachable!(),
        };
    }
    if let Role::Overall(index) = role {
        return index;
    }
    let base =
        if style.repeated() { 2 } else { 0 } + (group - 1) * if style.volume() { 4 } else { 3 };
    base + match role {
        Role::Note(index) => index,
        Role::Volume => 1,
        Role::Pages => 2,
        Role::Keys => {
            if style.volume() {
                3
            } else {
                2
            }
        }
        Role::Format => 3,
        Role::Selector => 4,
        Role::Overall(_) | Role::Category | Role::SetKey | Role::Code(_) => unreachable!(),
    }
}

fn citation_active(
    source: &str,
    end: usize,
    cursor: usize,
    style: crate::bibliography::citations::Style,
    signature: &mut Signature,
) -> Option<usize> {
    let parsed = crate::bibliography::citations::arguments(source, end, style);
    let groups = parsed.fields.last().map_or(1, |field| field.group.max(1));
    signature.parameters = citation_parameters(style, groups);
    for field in &parsed.fields {
        if cursor < field.argument.outer_span.end
            || (!field.complete && cursor == field.argument.outer_span.end)
        {
            return Some(citation_parameter(style, field.group, field.role));
        }
    }
    parsed
        .waiting
        .filter(|waiting| waiting.span.start <= cursor && cursor <= waiting.span.end)
        .map(|waiting| citation_parameter(style, waiting.group, waiting.role))
}

/// Only the parser's actual raw environment region can open this header reader.
/// Keep its range intact for folding and linked editing; the ordinary argument
/// matcher temporarily reads through it for a header-only signature query.
fn verbatim_environment_header(
    doc: &Document,
    cursor: usize,
) -> Option<(&syntax::Command, String, std::ops::Range<usize>)> {
    let command = doc
        .syntax
        .commands
        .iter()
        .rev()
        .find(|command| command.span.end <= cursor)?;
    if command.name != "begin" || doc.syntax.control_operands.contains(&command.span.start) {
        return None;
    }
    let argument = syntax::environment_name_argument(&doc.text.source, command.span.end)?;
    if (!matches!(argument.value.as_str(), "minted" | "lstlisting")
        && !crate::parser::fancyvrb::is_environment(&argument.value))
        || cursor < argument.outer_span.end
    {
        return None;
    }
    let raw = doc
        .syntax
        .ignored
        .iter()
        .find(|span| span.start == argument.outer_span.end)?;
    let end = if argument.value == "minted" {
        crate::parser::minted::environment_header_end(&doc.text.source, raw.start)
    } else if argument.value == "lstlisting" {
        crate::parser::listings::environment_options_end(&doc.text.source, raw.start)
    } else {
        crate::parser::fancyvrb::environment_header_end(
            &doc.text.source,
            raw.start,
            &argument.value,
        )
        .map(|(boundary, _)| boundary)
    }
    .unwrap_or(raw.end);
    (cursor <= end).then_some((command, argument.value, raw.start..end))
}

pub fn help(workspace: &Workspace, uri: &Url, position: Position) -> Option<SignatureHelp> {
    crate::analysis::AnalysisContext::new().run(|_| help_query(workspace, uri, position))
}

fn help_query(workspace: &Workspace, uri: &Url, position: Position) -> Option<SignatureHelp> {
    let doc = workspace.documents.get(uri)?;
    if doc.kind != DocumentKind::Latex {
        return None;
    }
    let cursor = doc.text.offset(position)?;
    if let Some(help) = crate::symbols::expl3::help(workspace, uri, cursor) {
        return Some(help);
    }
    if let Some((command, name, header)) = verbatim_environment_header(doc, cursor) {
        if crate::parser::fancyvrb::header_comment(&doc.text.source, header.start, cursor) {
            return None;
        }
        let scope = workspace.symbol_scope(uri);
        let changes_header = |target: &str| {
            target == "begin" || target == name || name == "lstlisting" && target == "lstlisting@"
        };
        if workspace
            .documents
            .iter()
            .filter(|(uri, _)| scope.contains(*uri))
            .any(|(_, doc)| {
                doc.syntax
                    .definitions
                    .iter()
                    .chain(&doc.syntax.environments)
                    .any(|entry| changes_header(&entry.name))
                    || doc.syntax.command_undefinitions.values().any(|target| {
                        target
                            .as_ref()
                            .is_none_or(|target| changes_header(&target.name))
                    })
                    || name == "lstlisting"
                        && doc.syntax.commands.iter().any(|command| {
                            command.name == "lstnewenvironment"
                                && command.argument.as_ref().is_none_or(|argument| {
                                    !syntax::is_static_key(&argument.value)
                                        || argument.value == name
                                })
                        })
                    || crate::parser::fancyvrb::is_environment(&name)
                        && doc.syntax.commands.iter().any(|command| {
                            matches!(
                                command.name.as_str(),
                                "DefineVerbatimEnvironment"
                                    | "CustomVerbatimEnvironment"
                                    | "RecustomVerbatimEnvironment"
                            ) && command.argument.as_ref().is_none_or(|argument| {
                                !syntax::is_static_key(&argument.value)
                                    || argument.value == name.trim_end_matches('*')
                                    || argument.value == name
                            })
                        })
            })
        {
            return None;
        }
        let signature = builtin_environment(&name)?;
        return active_parameter(
            doc,
            command,
            command.span.end,
            cursor,
            &signature,
            Some(&header),
        )
        .map(|active| signature.information(active));
    }
    if doc.syntax.ignored.iter().any(|span| {
        span.start <= cursor.saturating_sub(1)
            && cursor.saturating_sub(1) < span.end
            && !doc.syntax.literal_tests.contains(span)
            && !doc.syntax.commands.iter().any(|command| {
                command.name == "inputminted"
                    && crate::parser::minted::language(&doc.text.source, command.span.end)
                        .is_some_and(|argument| argument.span == *span)
            })
            && !doc.syntax.commands.iter().any(|command| {
                command.argument.as_ref().is_some_and(|argument| {
                    argument.span == *span
                        && (matches!(command.name.as_str(), "url" | "href" | "hyperref")
                            || crate::parser::minted::is_inline(&command.name)
                            || (crate::parser::fancyvrb::is_inline_save(&command.name)
                                || crate::parser::fancyvrb::is_use(&command.name))
                                && !crate::parser::fancyvrb::header_comment(
                                    &doc.text.source,
                                    argument.span.start,
                                    cursor,
                                ))
                })
            })
    }) {
        return None;
    }
    let scope = workspace.symbol_scope(uri);
    let hook_signatures = OnceCell::new();
    for (index, command) in doc
        .syntax
        .commands
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, command)| {
            command.span.end <= cursor && !doc.syntax.control_operands.contains(&command.span.start)
        })
        .take(128)
    {
        let literal = if command.name == "endcsname" {
            index.checked_sub(1).and_then(|index| {
                syntax::literal_csname(&doc.text.source, &doc.syntax.commands, index)
            })
        } else {
            None
        };
        if doc.syntax.definitions.iter().any(|entry| {
            entry.span == command.span || literal.as_ref().is_some_and(|span| entry.span == *span)
        }) {
            continue;
        }
        let named = command.argument.as_ref().filter(|argument| {
            command.name == "csuse"
                && cursor >= argument.outer_span.end
                && syntax::is_static_key(&argument.value)
        });
        let end = named.map_or(command.span.end, |argument| argument.outer_span.end);
        let name = named
            .map(|argument| argument.value.as_str())
            .or_else(|| literal.as_ref().map(|span| &doc.text.source[span.clone()]))
            .unwrap_or(command.name.as_str());
        if doc
            .syntax
            .contextual_command_shapes
            .get(&command.span.start)
            .is_some_and(|shape| shape.takes_no_arguments())
        {
            continue;
        }
        let signature = if let Some(arity) = doc.syntax.string_test_arities.get(&command.span.start)
        {
            let Some(count) = arity else {
                continue;
            };
            let mut signature =
                operand_builtin(if *count == 2 { "ifstrequal" } else { "ifblank" })?;
            signature.name = name.into();
            Some(signature)
        } else if name == "hyperref"
            && doc
                .text
                .source
                .as_bytes()
                .get(syntax::skip_space(&doc.text.source, end))
                == Some(&b'{')
            && !workspace.documents.iter().any(|(uri, doc)| {
                scope.contains(uri)
                    && doc
                        .syntax
                        .definitions
                        .iter()
                        .any(|entry| entry.name == name)
            })
        {
            Some(Signature {
                name: name.into(),
                starred: false,
                nested_brackets: true,
                documentation: "Link to a URL using a category and target name. Requires hyperref."
                    .into(),
                parameters: ["url", "category", "name", "text"]
                    .into_iter()
                    .map(|label| Parameter {
                        obey_spaces: false,
                        kind: Kind::Required,
                        label: format!("{{{label}}}"),
                    })
                    .collect(),
            })
        } else if name == "begin" {
            syntax::group(
                &doc.text.source,
                syntax::skip_space(&doc.text.source, end),
                b'{',
                b'}',
            )
            .map(|(argument, _)| argument)
            .filter(|argument| cursor > argument.span.end)
            .and_then(|argument| lookup(workspace, &argument.value, true, &scope, None))
            .or_else(|| signature(workspace, name, &scope))
        } else {
            signature(workspace, name, &scope)
        };
        if let Some(mut signature) = signature {
            if signature
                .parameters
                .first()
                .is_some_and(|parameter| parameter.label == "{hook}")
            {
                let signatures = hook_signatures.get_or_init(|| {
                    CommandSignatures::new(
                        workspace
                            .documents
                            .iter()
                            .filter(|(uri, _)| scope.contains(*uri))
                            .map(|(_, doc)| (doc.text.source.as_str(), &doc.syntax)),
                    )
                });
                if let Some(hook) = signatures.hook_signature(name, &doc.text.source, end, false) {
                    signature.parameters = hook.parameters;
                }
            }
            if literal.is_some() {
                signature.name = format!("csname {name}\\endcsname");
            } else if named.is_some() {
                signature.name = format!("csuse{{{name}}}");
            }
            if let Some(style) =
                signature
                    .parameters
                    .iter()
                    .find_map(|parameter| match parameter.kind {
                        Kind::CitationKeys(style) => Some(style),
                        _ => None,
                    })
            {
                if let Some(active) =
                    citation_active(&doc.text.source, end, cursor, style, &mut signature)
                {
                    return Some(signature.information(active));
                }
                continue;
            }
            if let Some(active) = active_parameter(doc, command, end, cursor, &signature, None) {
                return Some(signature.information(active));
            }
        }
    }
    None
}

#[cfg(test)]
mod documentation_tests {
    use super::*;

    fn setup(source: &str) -> (Workspace, Url) {
        let uri = Url::parse("untitled:signature-docs.tex").unwrap();
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.open(uri.clone(), source.into(), 1);
        (ws, uri)
    }

    fn at_end(ws: &Workspace, uri: &Url) -> Option<SignatureHelp> {
        let doc = &ws.documents[uri];
        help(ws, uri, doc.text.position(doc.text.source.len()))
    }

    fn documentation(help: &SignatureHelp) -> &str {
        let Some(Documentation::String(text)) = &help.signatures[0].documentation else {
            panic!("Expected plain documentation")
        };
        text
    }

    #[test]
    fn direct_and_static_named_signatures_append_plain_comments_without_changing_parameters() {
        for invocation in [
            r"\example",
            r"\csuse{example}",
            r"\csname example\endcsname",
        ] {
            let source = format!("% 中文😀 **plain** <tag>\r\n%\r\n% The required text.\r\n\\newcommand{{\\example}}[2][DEF]{{#1#2}}\r\n{invocation}{{value");
            let (ws, uri) = setup(&source);
            let signature = at_end(&ws, &uri).unwrap();
            assert_eq!(signature.active_parameter, Some(1));
            assert_eq!(
                signature.signatures[0].parameters.as_ref().unwrap().len(),
                2
            );
            let doc = documentation(&signature);
            assert!(doc.contains("Defined with \\newcommand"), "{doc}");
            assert!(
                doc.ends_with("中文😀 **plain** <tag>\n\nThe required text."),
                "{doc}"
            );
        }
    }

    #[test]
    fn aliases_use_their_own_documentation_and_keep_the_resolved_signature() {
        for definition in [r"\newcommand{\alias}{\example}", r"\def\alias{\example}"] {
            let source = format!("% Original documentation\n\\newcommand{{\\example}}[1]{{#1}}\n% Alias documentation\n{definition}\n\\alias{{value");
            let (ws, uri) = setup(&source);
            let signature = at_end(&ws, &uri).unwrap();
            assert_eq!(signature.signatures[0].label, "\\alias{#1}");
            let doc = documentation(&signature);
            assert!(doc.ends_with("Alias documentation"), "{doc}");
            assert!(!doc.contains("Original documentation"));
        }
    }

    #[test]
    fn incremental_comments_update_and_ambiguous_definitions_do_not_offer_a_signature() {
        let original = "% Original\n\\newcommand{\\example}[1]{#1}\n\\example{value";
        let (mut ws, uri) = setup(original);
        assert!(documentation(&at_end(&ws, &uri).unwrap()).ends_with("Original"));
        ws.open(uri.clone(), original.replace("Original", "Updated"), 2);
        assert!(documentation(&at_end(&ws, &uri).unwrap()).ends_with("Updated"));
        ws.open(
            uri.clone(),
            format!("\\newcommand{{\\example}}[2]{{#1#2}}\n{original}"),
            3,
        );
        assert!(at_end(&ws, &uri).is_none());
    }

    #[test]
    fn environment_and_macro_signatures_keep_their_own_declaration_comments() {
        for declaration in [
            r"\newenvironment{example}[2][DEF]{}{}",
            r"\NewDocumentEnvironment{example}{O{DEF}m}{}{}",
        ] {
            let definitions = format!("% Macro documentation\n\\newcommand{{\\example}}[1]{{#1}}\n% Environment documentation\n{declaration}\n");
            let (ws, uri) = setup(&format!("{definitions}\\example{{value"));
            let signature = at_end(&ws, &uri).unwrap();
            assert!(documentation(&signature).ends_with("Macro documentation"));
            assert_eq!(signature.active_parameter, Some(0));
            let (ws, uri) = setup(&format!("{definitions}\\begin{{example}}{{value"));
            let signature = at_end(&ws, &uri).unwrap();
            assert!(documentation(&signature).ends_with("Environment documentation"));
            assert!(!documentation(&signature).contains("Macro documentation"));
            assert_eq!(signature.active_parameter, Some(2));
        }
    }

    #[test]
    fn unrelated_projects_do_not_supply_signature_comments() {
        let dir = tempfile::tempdir().unwrap();
        let main = Url::from_file_path(dir.path().join("main.tex")).unwrap();
        let definitions = Url::from_file_path(dir.path().join("macros.tex")).unwrap();
        let source = "\\documentclass{article}\n\\input{macros}\n\\example{value";
        for (name, contents) in [
            ("main.tex", source),
            (
                "macros.tex",
                "% Selected project docs\n\\newcommand{\\example}[1]{#1}",
            ),
            (
                "other.tex",
                "\\documentclass{article}\n% Other docs\n\\newcommand{\\example}[2]{#1#2}",
            ),
        ] {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.add_root(dir.path().into());
        ws.open(main.clone(), source.into(), 1);
        let signature = at_end(&ws, &main).unwrap();
        assert!(documentation(&signature).ends_with("Selected project docs"));
        ws.open(
            definitions,
            "% Changed project docs\n\\newcommand{\\example}[1]{#1}".into(),
            2,
        );
        assert!(documentation(&at_end(&ws, &main).unwrap()).ends_with("Changed project docs"));
    }
}
