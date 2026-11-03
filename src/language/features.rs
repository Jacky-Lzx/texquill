use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range as Span;

use lsp_types::*;

use crate::{
    parser::syntax::{self, Command, Entry},
    project::{
        catalog::{COMMANDS, ENVIRONMENTS},
        workspace::{Document, DocumentKind, Workspace},
    },
};

#[derive(Clone, Copy, PartialEq)]
enum TargetKind {
    Label,
    Anchor,
    Citation,
    BibString,
    Command,
    Environment,
    Counter,
    Conditional,
    Toggle,
    SavedText,
    MovingText,
    Color,
}

impl TargetKind {
    fn same_name(self, left: &str, right: &str) -> bool {
        if self == Self::BibString {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    }

    fn saved_kind(self) -> Option<crate::symbols::saved_text::Kind> {
        match self {
            Self::SavedText => Some(crate::symbols::saved_text::Kind::Saved),
            Self::MovingText => Some(crate::symbols::saved_text::Kind::Moving),
            _ => None,
        }
    }

    fn generated_kind(self) -> Option<crate::symbols::generated::Kind> {
        match self {
            Self::Environment => Some(crate::symbols::generated::Kind::Environment),
            Self::Counter => Some(crate::symbols::generated::Kind::Counter),
            Self::Conditional => Some(crate::symbols::generated::Kind::Conditional),
            Self::Toggle => Some(crate::symbols::generated::Kind::Toggle),
            _ => None,
        }
    }
}

fn static_target_name(kind: TargetKind, name: &str) -> bool {
    if kind == TargetKind::BibString {
        crate::bibliography::bib_strings::identifier(name)
    } else if kind == TargetKind::Citation {
        syntax::is_static_citation_key(name)
    } else if matches!(kind, TargetKind::Label | TargetKind::Anchor) {
        syntax::is_static_reference_key(name)
    } else {
        syntax::is_static_key(name)
    }
}

fn target_key_spans(source: &str, span: Span<usize>, kind: TargetKind) -> Vec<Span<usize>> {
    if kind == TargetKind::Citation {
        syntax::citation_list_spans(source, span)
    } else if kind == TargetKind::Label {
        syntax::reference_list_spans(source, span)
    } else {
        syntax::key_list_spans(source, span)
    }
}

struct Target<'a> {
    kind: TargetKind,
    name: &'a str,
    span: Span<usize>,
    control_word: bool,
}

fn raw_target(doc: &Document, offset: usize) -> Option<Target<'_>> {
    if let Some(entry) = doc
        .syntax
        .bibliography_strings
        .definitions
        .iter()
        .chain(&doc.syntax.bibliography_strings.references)
        .find(|entry| entry.span.start <= offset && offset <= entry.span.end)
    {
        return Some(Target {
            kind: TargetKind::BibString,
            name: &entry.name,
            span: entry.span.clone(),
            control_word: false,
        });
    }
    for kind in [TargetKind::SavedText, TargetKind::MovingText] {
        let saved_kind = kind.saved_kind().unwrap();
        if let Some(entry) = crate::symbols::saved_text::declarations(doc, saved_kind)
            .iter()
            .chain(crate::symbols::saved_text::references(doc, saved_kind))
            .find(|entry| entry.span.start <= offset && offset <= entry.span.end)
        {
            return Some(Target {
                kind,
                name: &entry.name,
                span: entry.span.clone(),
                control_word: false,
            });
        }
    }
    if let Some(entry) = doc
        .syntax
        .name_command_references
        .iter()
        .find(|entry| entry.span.start <= offset && offset <= entry.span.end)
    {
        return Some(Target {
            kind: TargetKind::Command,
            name: &entry.name,
            span: entry.span.clone(),
            control_word: doc.text.source[entry.span.clone()].starts_with('\\'),
        });
    }
    if doc.syntax.is_ignored(offset) {
        return None;
    }
    if let Some(entry) = doc
        .syntax
        .colors
        .iter()
        .chain(&doc.syntax.color_references)
        .find(|entry| entry.span.start <= offset && offset <= entry.span.end)
    {
        return Some(Target {
            kind: TargetKind::Color,
            name: &entry.name,
            span: entry.span.clone(),
            control_word: false,
        });
    }
    if let Some(entry) = doc
        .syntax
        .anchors
        .iter()
        .chain(&doc.syntax.anchor_references)
        .find(|entry| entry.span.start <= offset && offset <= entry.span.end)
    {
        return Some(Target {
            kind: TargetKind::Anchor,
            name: &entry.name,
            span: entry.span.clone(),
            control_word: false,
        });
    }
    for command in &doc.syntax.commands {
        if doc.syntax.control_operands.contains(&command.span.start) {
            if command.span.start <= offset && offset <= command.span.end {
                return Some(Target {
                    kind: TargetKind::Command,
                    name: &command.name,
                    span: command.span.clone(),
                    control_word: true,
                });
            }
            continue;
        }
        if let Some(argument) = syntax::environment_copy_source(&doc.text.source, command) {
            if argument.span.start <= offset && offset <= argument.span.end {
                if !syntax::is_static_key(&argument.value) {
                    return None;
                }
                return Some(Target {
                    kind: TargetKind::Environment,
                    name: &doc.text.source[argument.span.clone()],
                    span: argument.span,
                    control_word: false,
                });
            }
        }
        if let Some(span) = crate::symbols::named_commands::spans(doc, command)
            .into_iter()
            .find(|span| span.start <= offset && offset <= span.end)
        {
            return Some(Target {
                kind: TargetKind::Command,
                name: &doc.text.source[span.clone()],
                span,
                control_word: false,
            });
        }
        if let Some(argument) = syntax::separate_label_argument(&doc.text.source, command)
            .or_else(|| syntax::range_reference_end(&doc.text.source, command))
        {
            if argument.span.start <= offset && offset <= argument.span.end {
                if !syntax::is_static_reference_key(&argument.value) {
                    continue;
                }
                return Some(Target {
                    kind: TargetKind::Label,
                    name: &doc.text.source[argument.span.clone()],
                    span: argument.span,
                    control_word: false,
                });
            }
        }
        let environment_argument = matches!(command.name.as_str(), "begin" | "end")
            .then(|| syntax::environment_name_argument(&doc.text.source, command.span.end))
            .flatten();
        if let Some(arg) = environment_argument.as_ref().or(command.argument.as_ref()) {
            let kind = if syntax::is_reference(&command.name) || command.name == "label" {
                Some(TargetKind::Label)
            } else if (syntax::is_citation(&command.name)
                && !crate::bibliography::citations::is_structured(&command.name))
                || command.name == "bibitem"
            {
                Some(TargetKind::Citation)
            } else if matches!(command.name.as_str(), "begin" | "end")
                || syntax::declares_environment(&command.name)
            {
                Some(TargetKind::Environment)
            } else if matches!(command.name.as_str(), "newcounter" | "newaliascnt") {
                Some(TargetKind::Counter)
            } else {
                None
            };
            if let Some(kind) = kind {
                if arg.span.start <= offset && offset <= arg.span.end {
                    if syntax::is_citation(&command.name)
                        && !syntax::is_single_citation(&command.name)
                        || syntax::is_reference(&command.name)
                            && !syntax::is_single_reference(&command.name)
                    {
                        let Some(span) = target_key_spans(&doc.text.source, arg.span.clone(), kind)
                            .into_iter()
                            .find(|span| span.start <= offset && offset <= span.end)
                        else {
                            continue;
                        };
                        return Some(Target {
                            kind,
                            name: &doc.text.source[span.clone()],
                            span,
                            control_word: false,
                        });
                    }
                    if (matches!(kind, TargetKind::Environment | TargetKind::Counter)
                        || kind == TargetKind::Label
                            && (command.name == "label"
                                || syntax::is_single_reference(&command.name))
                        || kind == TargetKind::Citation
                            && (syntax::is_single_citation(&command.name)
                                || command.name == "bibitem"))
                        && !static_target_name(kind, &arg.value)
                    {
                        // Dynamic keys can still contain ordinary indexed macro calls.
                        if matches!(kind, TargetKind::Label | TargetKind::Citation) {
                            continue;
                        }
                        return None;
                    }
                    let local = offset - arg.span.start;
                    let start = arg.value[..local].rfind(',').map(|i| i + 1).unwrap_or(0);
                    let end = arg.value[local..]
                        .find(',')
                        .map(|i| local + i)
                        .unwrap_or(arg.value.len());
                    let span = syntax::trimmed_span(
                        &doc.text.source,
                        arg.span.start + start..arg.span.start + end,
                    );
                    return Some(Target {
                        kind,
                        name: &doc.text.source[span.clone()],
                        span,
                        control_word: false,
                    });
                }
            }
        }
        if command.span.start <= offset && offset <= command.span.end {
            return Some(Target {
                kind: TargetKind::Command,
                name: &command.name,
                span: command.span.clone(),
                control_word: true,
            });
        }
    }
    for entry in doc
        .syntax
        .citations
        .iter()
        .chain(&doc.syntax.bibliography_aliases)
        .chain(&doc.syntax.citation_references)
    {
        if entry.span.start <= offset && offset <= entry.span.end {
            return Some(Target {
                kind: TargetKind::Citation,
                name: &entry.name,
                span: entry.span.clone(),
                control_word: false,
            });
        }
    }
    None
}

fn target<'a>(workspace: &'a Workspace, uri: &Url, offset: usize) -> Option<Target<'a>> {
    let doc = workspace.documents.get(uri)?;
    if doc
        .syntax
        .ambiguous_string_tests
        .iter()
        .any(|span| span.start <= offset && offset <= span.end)
    {
        return None;
    }
    let raw = raw_target(doc, offset);
    if raw
        .as_ref()
        .is_some_and(|target| target.kind.saved_kind().is_some())
    {
        return raw;
    }
    let counter_operand = doc
        .syntax
        .name_counter_references
        .iter()
        .any(|entry| entry.span.start <= offset && offset <= entry.span.end);
    if doc.syntax.is_ignored(offset) && raw.is_none() && !counter_operand {
        return None;
    }
    if let Some(raw) = raw.as_ref().filter(|raw| {
        doc.kind == DocumentKind::Latex
            && matches!(raw.kind, TargetKind::Label | TargetKind::Citation)
            && !entries(doc, raw.kind).any(|entry| entry.span == raw.span)
    }) {
        let (citation, span) =
            crate::symbols::named_references::Index::new(workspace, uri).target(uri, offset)?;
        if citation != (raw.kind == TargetKind::Citation) {
            return None;
        }
        return Some(Target {
            kind: raw.kind,
            name: &doc.text.source[span.clone()],
            span,
            control_word: false,
        });
    }
    if raw
        .as_ref()
        .is_some_and(|target| target.kind != TargetKind::Command)
    {
        return raw;
    }
    if crate::symbols::named_references::may_contain_key(workspace, doc, offset) {
        if let Some((citation, span)) =
            crate::symbols::named_references::Index::new(workspace, uri).target(uri, offset)
        {
            return Some(Target {
                kind: if citation {
                    TargetKind::Citation
                } else {
                    TargetKind::Label
                },
                name: &doc.text.source[span.clone()],
                span,
                control_word: false,
            });
        }
    }
    if crate::symbols::environment_calls::may_contain_name(doc, offset) {
        if let Some(span) =
            crate::symbols::environment_calls::Index::new(workspace).target(uri, offset)
        {
            return Some(Target {
                kind: TargetKind::Environment,
                name: &doc.text.source[span.clone()],
                span,
                control_word: false,
            });
        }
    }
    for (target_kind, generated_kind) in [
        (
            TargetKind::Environment,
            crate::symbols::generated::Kind::Environment,
        ),
        (
            TargetKind::Counter,
            crate::symbols::generated::Kind::Counter,
        ),
        (
            TargetKind::Conditional,
            crate::symbols::generated::Kind::Conditional,
        ),
        (TargetKind::Toggle, crate::symbols::generated::Kind::Toggle),
    ] {
        if let Some((name, span)) = crate::symbols::generated::target(
            workspace,
            &workspace.symbol_scope(uri),
            doc,
            offset,
            generated_kind,
        ) {
            return Some(Target {
                kind: target_kind,
                name,
                span,
                control_word: false,
            });
        }
    }
    for index in 0..doc.syntax.commands.len() {
        if let Some(span) = syntax::literal_csname(&doc.text.source, &doc.syntax.commands, index) {
            if span.start <= offset && offset <= span.end {
                return Some(Target {
                    kind: TargetKind::Command,
                    name: &doc.text.source[span.clone()],
                    span,
                    control_word: false,
                });
            }
        }
    }
    raw
}

pub(crate) fn command_target<'a>(
    workspace: &'a Workspace,
    uri: &Url,
    position: Position,
) -> Option<&'a str> {
    let doc = workspace.documents.get(uri)?;
    let target = target(workspace, uri, doc.text.clamped_offset(position)?)?;
    (target.kind == TargetKind::Command).then_some(target.name)
}

fn entries(doc: &Document, kind: TargetKind) -> impl Iterator<Item = &Entry> {
    let primary: &[Entry] = match kind {
        TargetKind::Label => &doc.syntax.labels,
        TargetKind::Anchor => &doc.syntax.anchors,
        TargetKind::Citation => &doc.syntax.citations,
        TargetKind::BibString => &doc.syntax.bibliography_strings.definitions,
        TargetKind::Command => &doc.syntax.definitions,
        TargetKind::Environment => &doc.syntax.environments,
        TargetKind::Counter => &doc.syntax.counters,
        TargetKind::Conditional => &doc.syntax.conditionals,
        TargetKind::Toggle => &doc.syntax.toggles,
        TargetKind::SavedText => &doc.syntax.saved_text,
        TargetKind::MovingText => &doc.syntax.moving_text,
        TargetKind::Color => &doc.syntax.colors,
    };
    let aliases: &[Entry] = if kind == TargetKind::Citation {
        &doc.syntax.bibliography_aliases
    } else {
        &[]
    };
    primary.iter().chain(aliases)
}

fn rename_target<'a>(
    workspace: &'a Workspace,
    uri: &Url,
    position: Position,
) -> crate::rpc::Result<Option<(Target<'a>, BTreeSet<Url>)>> {
    let Some(doc) = workspace.documents.get(uri) else {
        return Ok(None);
    };
    let Some(target) = doc
        .text
        .offset(position)
        .and_then(|offset| target(workspace, uri, offset))
    else {
        return Ok(None);
    };
    let crate::project::workspace::SymbolProjects { mut scope, regions } =
        workspace.symbol_projects(uri);
    if target.kind == TargetKind::BibString
        && crate::bibliography::bib_strings::builtin(target.name).is_some()
    {
        return Ok(None);
    }
    if let Some(kind) = target.kind.saved_kind() {
        crate::symbols::saved_text::validate(workspace, &scope, kind)?;
    }
    if target.kind == TargetKind::Color {
        if crate::symbols::named_colors::builtin(target.name) {
            return Ok(None);
        }
        crate::symbols::named_colors::validate(workspace, &scope)?;
    }
    let declarations = |scope: &BTreeSet<Url>| -> Vec<Url> {
        workspace
            .documents
            .iter()
            .filter(|(uri, _)| scope.contains(*uri))
            .flat_map(|(uri, doc)| {
                entries(doc, target.kind)
                    .filter(|entry| target.kind.same_name(&entry.name, target.name))
                    .map(move |_| uri.clone())
            })
            .collect()
    };
    let definitions = declarations(&scope);
    if definitions.is_empty() {
        return Ok(None);
    }
    if definitions.len() != 1 {
        return Err(crate::rpc::Error::invalid_params(
            "Rename is ambiguous: the name has multiple definitions in this project",
        ));
    }
    // Shared occurrences must resolve to this declaration in every consumer.
    // Otherwise changing the shared file would silently break another paper.
    let occurrences = all_references(workspace, uri, position, true);
    loop {
        let used: BTreeSet<_> = occurrences
            .iter()
            .filter(|location| scope.contains(&location.uri))
            .map(|location| location.uri.clone())
            .collect();
        let before = scope.len();
        for project in regions
            .values()
            .filter(|project| !project.is_disjoint(&used))
        {
            if !project.contains(&definitions[0]) {
                return Err(crate::rpc::Error::invalid_params("Rename is ambiguous: a shared occurrence belongs to a project with a different or missing definition"));
            }
            scope.extend(project.iter().cloned());
        }
        if scope.len() == before {
            break;
        }
    }
    if declarations(&scope).len() != 1 {
        return Err(crate::rpc::Error::invalid_params(
            "Rename is ambiguous: projects sharing this definition contain multiple definitions",
        ));
    }
    if target.kind == TargetKind::Environment {
        crate::symbols::environment_calls::Index::new(workspace).validate(&scope, target.name)?;
    }
    if target.kind == TargetKind::Color {
        crate::symbols::named_colors::validate(workspace, &scope)?;
    }
    if target.kind == TargetKind::Anchor {
        crate::editing::anchor_edits::validate(workspace, &scope, target.name)?;
    }
    if let Some(kind) = target.kind.saved_kind() {
        crate::symbols::saved_text::validate(workspace, &scope, kind)?;
    }
    crate::project::resource_context::validate_rename(workspace, &scope, target.name)?;
    crate::editing::hook_argument_edits::validate(workspace, &scope, &occurrences)?;
    if matches!(target.kind, TargetKind::Label | TargetKind::Citation) {
        crate::symbols::named_references::Index::new(workspace, uri).validate(
            &scope,
            target.kind == TargetKind::Citation,
            target.name,
        )?;
    }
    Ok(Some((target, scope)))
}

pub fn prepare_rename(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
) -> crate::rpc::Result<Option<PrepareRenameResponse>> {
    crate::analysis::AnalysisContext::new().run(|_| prepare_rename_query(workspace, uri, position))
}

fn prepare_rename_query(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
) -> crate::rpc::Result<Option<PrepareRenameResponse>> {
    if let Some(offset) = workspace
        .documents
        .get(uri)
        .and_then(|doc| doc.text.offset(position))
    {
        if let Some(result) =
            crate::bibliography::bib_section_references::prepare(workspace, uri, offset)
        {
            return result;
        }
        if let Some(result) =
            crate::bibliography::bib_section_labels::prepare(workspace, uri, offset)
        {
            return result;
        }
        if let Some(result) = crate::bibliography::bib_categories::prepare(workspace, uri, offset) {
            return result;
        }
        if let Some(result) = crate::symbols::hook_names::prepare(workspace, uri, offset) {
            return result;
        }
    }
    if let Some(entry) = workspace.documents.get(uri).and_then(|doc| {
        doc.text
            .offset(position)
            .and_then(|offset| crate::symbols::extra_symbols::at(doc, offset))
    }) {
        return crate::symbols::extra_symbols::prepare(workspace, uri, entry);
    }
    let Some((target, _)) = rename_target(workspace, uri, position)? else {
        return Ok(None);
    };
    let doc = &workspace.documents[uri];
    let start = target.span.start + usize::from(target.control_word);
    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: doc.text.range(start, target.span.end),
        placeholder: target.name.into(),
    }))
}

pub fn rename(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    new_name: &str,
) -> crate::rpc::Result<Option<WorkspaceEdit>> {
    crate::analysis::AnalysisContext::new()
        .run(|_| rename_query(workspace, uri, position, new_name))
}

fn rename_query(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    new_name: &str,
) -> crate::rpc::Result<Option<WorkspaceEdit>> {
    let edit = rename_impl(workspace, uri, position, new_name)?;
    if let Some(edit) = &edit {
        crate::project::resource_context::validate_workspace_edit(workspace, edit)?;
    }
    Ok(edit)
}

fn rename_impl(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    new_name: &str,
) -> crate::rpc::Result<Option<WorkspaceEdit>> {
    if let Some(offset) = workspace
        .documents
        .get(uri)
        .and_then(|doc| doc.text.offset(position))
    {
        if let Some(result) =
            crate::bibliography::bib_section_references::rename(workspace, uri, offset, new_name)
        {
            return result;
        }
        if let Some(result) =
            crate::bibliography::bib_section_labels::rename(workspace, uri, offset, new_name)
        {
            return result;
        }
        if let Some(result) =
            crate::bibliography::bib_categories::rename(workspace, uri, offset, new_name)
        {
            return result;
        }
        if let Some(result) = crate::symbols::hook_names::rename(workspace, uri, offset, new_name) {
            return result;
        }
    }
    if let Some(entry) = workspace.documents.get(uri).and_then(|doc| {
        doc.text
            .offset(position)
            .and_then(|offset| crate::symbols::extra_symbols::at(doc, offset))
    }) {
        return crate::symbols::extra_symbols::rename(workspace, uri, entry, new_name);
    }
    let Some((target, scope)) = rename_target(workspace, uri, position)? else {
        return Ok(None);
    };
    let new_name = if target.kind == TargetKind::Command {
        new_name.strip_prefix('\\').unwrap_or(new_name)
    } else {
        new_name
    };
    let valid = !new_name.is_empty()
        && !(target.kind == TargetKind::Citation && new_name == "*")
        && if target.kind.saved_kind().is_some() {
            crate::symbols::saved_text::literal_name(new_name)
        } else if target.kind == TargetKind::Color {
            crate::symbols::named_colors::literal_name(new_name)
        } else {
            static_target_name(target.kind, new_name)
        };
    if !valid {
        return Err(crate::rpc::Error::invalid_params(
            if target.kind.saved_kind().is_some() {
                "Invalid saved-text name: use a nonempty literal name without TeX expansion or comments"
            } else if target.kind == TargetKind::Color {
                "Invalid color name: start with an ASCII letter, digit or underscore; use only letters, digits, underscore, hyphen and dot"
            } else if target.kind == TargetKind::BibString {
                "Invalid BibTeX abbreviation: use one identifier without an initial digit or value delimiters"
            } else {
                "Invalid name: use a single TeX command name or a reference key without separators"
            },
        ));
    }
    if new_name == target.name && target.kind != TargetKind::BibString {
        return Ok(Some(WorkspaceEdit::default()));
    }
    let conflicts = workspace
        .documents
        .iter()
        .filter(|(uri, _)| scope.contains(*uri))
        .any(|(_, doc)| {
            entries(doc, target.kind).any(|entry| {
                target.kind.same_name(&entry.name, new_name)
                    && !(target.kind == TargetKind::BibString
                        && target.kind.same_name(target.name, new_name))
            }) || target.kind == TargetKind::BibString
                && !target.kind.same_name(target.name, new_name)
                && doc
                    .syntax
                    .bibliography_strings
                    .references
                    .iter()
                    .any(|entry| entry.name.eq_ignore_ascii_case(new_name))
                || target.kind == TargetKind::Environment
                    && doc.syntax.definitions.iter().any(|entry| {
                        entry.name == new_name || entry.name == format!("end{new_name}")
                    })
                || target.kind == TargetKind::Command
                    && crate::symbols::generated::generated_command_exists(doc, new_name)
                || target.kind == TargetKind::Color
                    && doc
                        .syntax
                        .color_references
                        .iter()
                        .any(|entry| entry.name == new_name)
        });
    let builtin = match target.kind {
        TargetKind::Color => crate::symbols::named_colors::builtin(new_name),
        TargetKind::BibString => crate::bibliography::bib_strings::builtin(new_name).is_some(),
        TargetKind::Command | TargetKind::Environment => {
            crate::project::packages::command(workspace, &scope, new_name).is_some()
                || COMMANDS.iter().any(|(name, _, _)| *name == new_name)
                || syntax::declares_register(new_name)
                || crate::project::catalog::PRIMITIVES.contains(&new_name)
                || ENVIRONMENTS
                    .iter()
                    .any(|name| *name == new_name || format!("end{name}") == new_name)
        }
        _ => false,
    };
    if conflicts || builtin {
        return Err(crate::rpc::Error::invalid_params(format!(
            "The name `{new_name}` is already defined"
        )));
    }
    if let Some(kind) = target.kind.generated_kind() {
        if let Some(model) =
            crate::symbols::generated::in_scope(workspace, &scope, target.name, kind)
        {
            crate::symbols::generated::validate(workspace, &scope, target.name, new_name, model)?;
        }
    }
    let environment_calls = (target.kind == TargetKind::Environment)
        .then(|| crate::symbols::environment_calls::Index::new(workspace));
    let mut changes: std::collections::BTreeMap<Url, Vec<TextEdit>> =
        std::collections::BTreeMap::new();
    for location in all_references(workspace, uri, position, true)
        .into_iter()
        .filter(|location| scope.contains(&location.uri))
    {
        let document = &workspace.documents[&location.uri];
        let text = &document.text;
        if target.kind.saved_kind().is_some() {
            changes.entry(location.uri).or_default().push(
                crate::symbols::saved_text::replacement_edit(document, location.range, new_name),
            );
            continue;
        }
        if target.kind == TargetKind::Anchor {
            let edits = crate::symbols::anchors::rename_edits(document, location.range, new_name)?;
            changes.entry(location.uri).or_default().extend(edits);
            continue;
        }
        if matches!(target.kind, TargetKind::Counter | TargetKind::Environment) {
            let edit = if environment_calls
                .as_ref()
                .is_some_and(|calls| calls.is_bare_name(&location.uri, document, location.range))
            {
                TextEdit {
                    range: location.range,
                    new_text: format!("{{{new_name}}}"),
                }
            } else {
                crate::symbols::counters::replacement_edit(document, location.range, new_name)
            };
            changes.entry(location.uri).or_default().push(edit);
            continue;
        }
        let control_word = target.kind == TargetKind::Command
            && text
                .offset(location.range.start)
                .is_some_and(|offset| text.source[offset..].starts_with('\\'));
        if control_word
            && !new_name
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || byte == b'@')
        {
            return Err(crate::rpc::Error::invalid_params(
                "Names used in direct TeX commands must contain only command letters or @",
            ));
        }
        changes.entry(location.uri).or_default().push(TextEdit {
            range: location.range,
            new_text: if control_word {
                format!("\\{new_name}")
            } else if target.kind == TargetKind::Label {
                syntax::label_replacement_text(
                    text,
                    &document.syntax.commands,
                    location.range,
                    new_name,
                )
            } else {
                new_name.into()
            },
        });
    }
    for (uri, edits) in &changes {
        let doc = &workspace.documents[uri];
        if edits.iter().any(|edit| {
            let start = doc.text.offset(edit.range.start).unwrap_or(0);
            let end = doc.text.offset(edit.range.end).unwrap_or(start);
            doc.syntax
                .ambiguous_string_tests
                .iter()
                .any(|span| start < span.end && span.start < end)
        }) {
            return Err(crate::rpc::Error::invalid_params(
                "String-test arguments have different meanings across project or load contexts",
            ));
        }
    }
    for (uri, edits) in &mut changes {
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        edits.dedup_by(|a, b| a.range == b.range);
        if target.kind == TargetKind::Citation && new_name.contains('"') {
            brace_quoted_citation_edits(&workspace.documents[uri], edits);
        }
    }
    Ok(Some(workspace_edit(workspace, changes)))
}

/// Merge edits in a quoted BibTeX relationship or IDS value before changing its
/// delimiters. This keeps repeated keys in one list from producing overlapping
/// edits, and preserves the surrounding list members and whitespace.
pub(crate) fn brace_quoted_citation_edits(doc: &Document, edits: &mut Vec<TextEdit>) {
    let mut arguments: Vec<_> = doc
        .syntax
        .citation_fields
        .iter()
        .map(|field| &field.argument)
        .chain(&doc.syntax.bibliography_alias_values)
        .filter(|argument| doc.text.source.as_bytes().get(argument.outer_span.start) == Some(&b'"'))
        .collect();
    arguments.sort_by_key(|argument| argument.span.start);
    arguments.dedup_by_key(|argument| (argument.span.start, argument.span.end));
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    let mut pending = std::mem::take(edits).into_iter().peekable();
    let mut result = Vec::new();
    for argument in arguments {
        let inner = doc.text.range(argument.span.start, argument.span.end);
        while pending
            .peek()
            .is_some_and(|edit| edit.range.start < inner.start)
        {
            result.push(pending.next().unwrap());
        }
        if !pending
            .peek()
            .is_some_and(|edit| edit.range.end <= inner.end)
        {
            continue;
        }
        let mut value = String::from("{");
        let mut cursor = argument.span.start;
        while pending
            .peek()
            .is_some_and(|edit| edit.range.end <= inner.end)
        {
            let edit = pending.next().unwrap();
            let start = doc.text.offset(edit.range.start).unwrap();
            let end = doc.text.offset(edit.range.end).unwrap();
            value.push_str(&doc.text.source[cursor..start]);
            value.push_str(&edit.new_text);
            cursor = end;
        }
        value.push_str(&doc.text.source[cursor..argument.span.end]);
        value.push('}');
        result.push(TextEdit {
            range: doc
                .text
                .range(argument.outer_span.start, argument.outer_span.end),
            new_text: value,
        });
    }
    result.extend(pending);
    result.sort_by_key(|edit| (edit.range.start, edit.range.end));
    *edits = result;
}

pub fn workspace_edit(
    workspace: &Workspace,
    changes: std::collections::BTreeMap<Url, Vec<TextEdit>>,
) -> WorkspaceEdit {
    if workspace.client.versioned_edits {
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(
                changes
                    .into_iter()
                    .map(|(uri, edits)| {
                        let version = workspace.documents.get(&uri).and_then(|doc| doc.version);
                        TextDocumentEdit {
                            text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                            edits: edits.into_iter().map(OneOf::Left).collect(),
                        }
                    })
                    .collect(),
            )),
            ..Default::default()
        }
    } else {
        WorkspaceEdit {
            changes: Some(changes.into_iter().collect()),
            ..Default::default()
        }
    }
}

/// Works for incomplete groups too, so typing `\ref{` can offer completions.
pub(crate) fn completion_name_argument(
    source: &str,
    command: &Command,
    offset: usize,
) -> Option<(Span<usize>, Vec<Span<usize>>)> {
    if crate::bibliography::citations::style(&command.name).is_some_and(|style| style.single_key())
    {
        return crate::bibliography::citations::single_key_context(source, command, offset);
    }
    let (start, open) = if syntax::declares_environment_copy(&command.name) {
        (
            syntax::environment_copy_source_start(source, command)?,
            b'{',
        )
    } else if syntax::has_separate_label_argument(&command.name) {
        syntax::separate_label_argument_start(source, command)?
    } else if syntax::is_reference(&command.name) {
        (
            syntax::reference_argument_start(source, &command.name, command.span.end)?,
            b'{',
        )
    } else {
        (syntax::skip_space(source, command.span.end), b'{')
    };
    let reference = syntax::is_single_citation(&command.name)
        || syntax::is_reference(&command.name)
        || syntax::has_separate_label_argument(&command.name)
        || command.name == "hyperdef"
        || command.name == "hyperlink";
    let argument = |start, open| {
        let hyperlink = matches!(command.name.as_str(), "hyperref" | "hyperdef");
        if syntax::is_reference(&command.name)
            || syntax::has_separate_label_argument(&command.name)
            || command.name == "hyperdef"
            || syntax::is_single_citation(&command.name)
        {
            if let Some(context) = syntax::active_commented_name_argument(
                source,
                start,
                open,
                offset,
                !hyperlink || open != b'[',
                if syntax::is_single_citation(&command.name) {
                    syntax::is_static_citation_key
                } else if reference {
                    syntax::is_static_reference_key
                } else {
                    syntax::is_static_key
                },
            ) {
                return Some(context);
            }
        }
        let span = if syntax::is_single_citation(&command.name) {
            syntax::active_citation_argument(source, start, open, offset)
        } else if reference {
            syntax::active_reference_argument(source, start, open, offset)
        } else {
            syntax::active_name_argument(source, start, open, offset)
        }?;
        Some((span, vec![]))
    };
    argument(start, open)
        .or_else(|| argument(syntax::range_reference_end_start(source, command)?, b'{'))
}

fn argument_context(doc: &Document, offset: usize) -> Option<(&Command, Span<usize>)> {
    doc.syntax
        .commands
        .iter()
        .rev()
        .filter(|command| {
            command.span.end <= offset && !doc.syntax.control_operands.contains(&command.span.start)
        })
        .find_map(|command| argument_span(doc, command, offset).map(|span| (command, span)))
}

fn direct_file_context_allowed(workspace: &Workspace, uri: &Url, command: &Command) -> bool {
    let mut roots = workspace
        .root_document(uri)
        .map(|root| vec![root])
        .unwrap_or_else(|_| workspace.source_roots(uri));
    if roots.is_empty() {
        roots.push(uri.clone());
    }
    roots.iter().all(|root| {
        if command.name == "includegraphics" {
            return crate::project::graphics::calls(workspace, root)
                .get(uri)
                .into_iter()
                .flatten()
                .any(|call| call.certain && call.command.span == command.span);
        }
        let mut scope = workspace.project_documents(root);
        scope.insert(uri.clone());
        crate::project::source_inputs::editable_file_positions(workspace, &scope)
            .get(uri)
            .is_some_and(|positions| positions.contains(&command.span.start))
    })
}

fn indirect_source_context(
    workspace: &Workspace,
    uri: &Url,
    doc: &Document,
    offset: usize,
) -> Option<(Command, Span<usize>)> {
    let roots = workspace
        .root_document(uri)
        .map(|root| vec![root])
        .unwrap_or_else(|_| workspace.source_roots(uri));
    let mut selected: Option<(Command, Span<usize>)> = None;
    for root in roots {
        let inputs = crate::project::source_inputs::Index::new(workspace, &root);
        let images = crate::project::graphics::calls(workspace, &root);
        let calls = inputs
            .calls
            .get(uri)
            .into_iter()
            .flatten()
            .chain(inputs.loaders.get(uri).into_iter().flatten())
            .chain(inputs.forwarders.get(uri).into_iter().flatten())
            .chain(inputs.resource_references.get(uri).into_iter().flatten())
            .chain(inputs.selectors.get(uri).into_iter().flatten())
            .chain(inputs.checks.get(uri).into_iter().flatten())
            .chain(images.get(uri).into_iter().flatten());
        let (command, span) = calls.rev().find_map(|call| {
            (call.certain && call.command.span.end <= offset)
                .then(|| argument_span(doc, &call.command, offset))
                .flatten()
                .map(|span| (&call.command, span))
        })?;
        if selected.as_ref().is_some_and(|(previous, prefix)| {
            previous.name != command.name || previous.span != command.span || *prefix != span
        }) {
            return None;
        }
        selected = Some((command.clone(), span));
    }
    selected
}

fn argument_span(doc: &Document, command: &Command, offset: usize) -> Option<Span<usize>> {
    let source = &doc.text.source;
    if !(matches!(command.name.as_str(), "begin" | "end" | "hyperlink")
        || syntax::declares_environment_copy(&command.name)
        || syntax::is_reference(&command.name)
        || syntax::has_separate_label_argument(&command.name)
        || syntax::is_citation(&command.name)
        || syntax::file_extensions(&command.name).is_some())
    {
        return None;
    }
    if matches!(command.name.as_str(), "InputIfFileExists" | "IfFileExists") {
        return syntax::active_file_prefix(
            source,
            syntax::skip_space(source, command.span.end),
            offset,
            false,
            false,
        );
    }
    if matches!(command.name.as_str(), "input" | "include") {
        let start = syntax::skip_space(source, command.span.end);
        if source.as_bytes().get(start) == Some(&b'{') {
            return syntax::active_file_prefix(source, start, offset, false, false);
        }
        if command.name == "include" {
            return None;
        }
        return command.argument.as_ref().and_then(|argument| {
            (argument.span.start <= offset && offset <= argument.span.end)
                .then_some(argument.span.start..offset)
        });
    }
    if syntax::passes_options(&command.name) {
        let start = syntax::forwarded_file_argument_start(source, command)?;
        if syntax::group(source, start, b'{', b'}').is_some_and(|(argument, _)| {
            !argument.value.is_empty()
                && (!syntax::is_literal_file_path(&argument.value)
                    || argument.value.chars().any(char::is_whitespace))
        }) {
            return None;
        }
        let prefix = source.get(start..offset)?.strip_prefix('{')?;
        if !prefix.is_empty()
            && (!syntax::is_literal_file_path(prefix) || prefix.chars().any(char::is_whitespace))
        {
            return None;
        }
        return Some(start + 1..offset);
    }
    if crate::bibliography::bib_resources::references_file(&command.name) {
        return crate::bibliography::bib_resources::prefix(source, command, offset);
    }
    if command.name == "bibliographystyle" {
        return syntax::active_bibliography_style_prefix(
            source,
            syntax::file_name_argument_start(source, &command.name, command.span.end)?,
            offset,
        );
    }
    if crate::project::code_inputs::wrapped_filename(&command.name) {
        return crate::project::code_inputs::prefix(source, command, offset);
    }
    if command.name == "includegraphics" {
        return crate::project::graphics::prefix(source, command, offset);
    }
    if syntax::code_input_command(&command.name) {
        let start = syntax::file_name_argument_start(source, &command.name, command.span.end)?;
        let value = source.get(start..offset)?.strip_prefix('{')?;
        if value.is_empty() || syntax::is_literal_file_path(value) {
            return Some(start + 1..offset);
        }
        return None;
    }
    if syntax::file_list(&command.name) || syntax::loader_extension(&command.name) == Some("cls") {
        return syntax::active_file_prefix(
            source,
            syntax::file_name_argument_start(source, &command.name, command.span.end)?,
            offset,
            syntax::file_name_ignores_spaces(&command.name),
            syntax::file_list(&command.name),
        );
    }
    if crate::bibliography::citations::is_structured(&command.name) {
        return crate::bibliography::citations::prefix(source, command, offset);
    }
    // A completed inner call in an optional note must not hide the outer key.
    // Most calls also let us skip old, closed arguments without rescanning text.
    if !syntax::has_separate_label_argument(&command.name)
        && !syntax::is_range_reference(&command.name)
        && !syntax::declares_environment_copy(&command.name)
        && command
            .argument
            .as_ref()
            .is_some_and(|arg| arg.outer_span.end < offset)
    {
        return None;
    }
    if matches!(command.name.as_str(), "begin" | "end")
        || syntax::declares_environment_copy(&command.name)
        || syntax::is_single_reference(&command.name)
        || syntax::has_separate_label_argument(&command.name)
        || command.name == "hyperlink"
        || syntax::is_single_citation(&command.name)
    {
        let (span, _) = completion_name_argument(source, command, offset)?;
        let prefix = source[span.start..offset].trim_start();
        return Some(offset - prefix.len()..offset);
    }
    let at = if syntax::is_citation(&command.name) {
        syntax::citation_argument_start(source, &command.name, command.span.end)?
    } else if syntax::is_reference(&command.name) {
        syntax::reference_argument_start(source, &command.name, command.span.end)?
    } else {
        let mut at = syntax::skip_space(source, command.span.end);
        if source.as_bytes().get(at) == Some(&b'*') {
            if matches!(command.name.as_str(), "InputIfFileExists" | "IfFileExists")
                || syntax::loader_extension(&command.name).is_some()
            {
                return None;
            }
            at = syntax::skip_space(source, at + 1);
        }
        while source.as_bytes().get(at) == Some(&b'[') {
            if matches!(command.name.as_str(), "InputIfFileExists" | "IfFileExists")
                || syntax::loader_forwards_options(&command.name)
            {
                return None;
            }
            let (_, after) = syntax::group(source, at, b'[', b']')?;
            at = syntax::skip_space(source, after);
        }
        at
    };
    if syntax::is_reference(&command.name) || syntax::is_citation(&command.name) {
        return syntax::active_key_list_prefix(
            source,
            at,
            offset,
            if syntax::is_citation(&command.name) {
                syntax::is_static_citation_key
            } else {
                syntax::is_static_reference_key
            },
        );
    }
    let value = source.get(at..offset)?.strip_prefix('{')?;
    if value.contains(['{', '}', '%'])
        || value.contains('\n') && !syntax::file_name_ignores_spaces(&command.name)
    {
        return None;
    }
    // Lists have their own parser above. A comma in a single filename is
    // part of that filename, including directory names and image basenames.
    let key = value.trim_start();
    Some(offset - key.len()..offset)
}

pub fn completion(workspace: &Workspace, uri: &Url, position: Position) -> Vec<CompletionItem> {
    crate::analysis::AnalysisContext::new().run(|_| completion_in_query(workspace, uri, position))
}

pub(crate) fn completion_in_query(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
) -> Vec<CompletionItem> {
    completion_with_prefix(workspace, uri, position, false, &mut false)
}

pub(crate) fn completion_unfiltered(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
) -> Option<Vec<CompletionItem>> {
    let mut keys = false;
    let items = completion_with_prefix(workspace, uri, position, true, &mut keys);
    keys.then_some(items)
}

fn completion_with_prefix(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    all_keys: bool,
    keys: &mut bool,
) -> Vec<CompletionItem> {
    let Some(doc) = workspace.documents.get(uri) else {
        return vec![];
    };
    let Some(offset) = doc.text.offset(position) else {
        return vec![];
    };
    let scope = workspace.symbol_scope(uri);
    if doc.kind == DocumentKind::BibtexStyle {
        return vec![];
    }
    if doc.kind == DocumentKind::Bibtex {
        return bibliography_completion(workspace, uri, doc, offset, &scope);
    }
    if let Some(items) = crate::symbols::saved_text::completion(workspace, uri, offset, &scope) {
        return items;
    }
    if let Some(items) =
        crate::symbols::counters::saved_name_completion(workspace, uri, offset, &scope)
    {
        return items;
    }
    let name_command = doc.syntax.name_command_references.iter().any(|entry| {
        doc.text.source[entry.span.clone()].starts_with('\\')
            && entry.span.start < offset
            && offset <= entry.span.end
    });
    if doc.syntax.is_ignored(offset.saturating_sub(1)) && !name_command {
        return vec![];
    }
    if !name_command {
        if let Some(items) = crate::symbols::hook_names::completion(workspace, uri, offset) {
            return items;
        }
        if let Some(items) = crate::symbols::hook_labels::completion(workspace, uri, offset) {
            return items;
        }
        if let Some(items) = crate::symbols::expl3::completion(workspace, uri, offset) {
            return items;
        }
    }
    if !name_command {
        if let Some(items) = crate::bibliography::bib_categories::completion(workspace, uri, offset)
        {
            return items;
        }
    }
    if !name_command {
        if let Some(items) = crate::symbols::extra_symbols::completion(workspace, uri, offset) {
            return items;
        }
    }
    if !name_command {
        if let Some(items) = crate::project::imports::completion(workspace, uri, offset) {
            return items;
        }
    }
    if !name_command {
        if let Some(items) =
            crate::bibliography::bib_section_labels::completion(workspace, uri, offset)
        {
            return items;
        }
    }
    if !name_command {
        if let Some(items) =
            crate::bibliography::bib_section_keys::completion(workspace, uri, offset)
        {
            return items;
        }
    }
    if !name_command {
        let items = if all_keys {
            crate::bibliography::citation_search::completion_unfiltered(
                workspace, uri, offset, &scope,
            )
        } else {
            crate::bibliography::citation_search::completion(workspace, uri, offset, &scope)
        };
        if let Some(items) = items {
            *keys = true;
            return items;
        }
    }
    if !name_command {
        if let Some(items) = crate::project::options::completion(workspace, uri, offset) {
            return items;
        }
    }
    if !name_command {
        if let Some(items) =
            crate::bibliography::bib_accessors::completion(workspace, uri, offset, &scope)
        {
            return items;
        }
    }
    if let Some(items) = crate::symbols::counters::completion(workspace, uri, offset, &scope) {
        return items;
    }
    if let Some(items) = crate::symbols::named_colors::completion(workspace, uri, offset, &scope) {
        return items;
    }
    if let Some(items) = crate::symbols::conditionals::completion(workspace, uri, offset, &scope) {
        return items;
    }
    if let Some(items) = crate::symbols::named_commands::completion(workspace, uri, offset, &scope)
    {
        return items;
    }
    if !name_command {
        if let Some((command, span)) = indirect_source_context(workspace, uri, doc, offset) {
            return crate::language::path_completion::complete_source(
                workspace, uri, &command, span,
            );
        }
    }
    let context = (!name_command)
        .then(|| {
            argument_context(doc, offset)
                .filter(|(command, _)| {
                    !syntax::is_reference(&command.name)
                        && !syntax::has_separate_label_argument(&command.name)
                        && !syntax::is_citation(&command.name)
                })
                .map(|(command, span)| (std::borrow::Cow::Borrowed(command), span))
                .or_else(|| {
                    crate::symbols::named_references::completion_context(
                        workspace,
                        uri,
                        offset,
                        |command| argument_span(doc, command, offset),
                    )
                    .map(|(command, span)| (std::borrow::Cow::Owned(command), span))
                })
        })
        .flatten();
    if let Some((command, span)) = context {
        let command = command.as_ref();
        if syntax::file_extensions(&command.name).is_some() {
            if (command.name == "includegraphics"
                || crate::project::source_inputs::editable_file_command(&command.name))
                && !direct_file_context_allowed(workspace, uri, command)
            {
                return vec![];
            }
            return if crate::bibliography::bib_resources::references_file(&command.name) {
                crate::language::path_completion::complete_source(workspace, uri, command, span)
            } else {
                crate::language::path_completion::complete(workspace, uri, &command.name, span)
            };
        }
        let prefix = if all_keys {
            ""
        } else {
            &doc.text.source[span.clone()]
        };
        // Replace the rest of this key too when completing inside an existing
        // reference/environment; separators and following arguments stay intact.
        let suffix_length = doc.text.source[offset..]
            .chars()
            .take_while(|ch| !ch.is_whitespace() && !matches!(ch, '{' | '}' | ',' | '\\' | '%'))
            .map(char::len_utf8)
            .sum::<usize>();
        let (replacement_range, additional_edits) =
            if matches!(command.name.as_str(), "begin" | "end")
                || syntax::declares_environment_copy(&command.name)
                || syntax::is_single_reference(&command.name)
                || syntax::has_separate_label_argument(&command.name)
                || command.name == "hyperlink"
                || syntax::is_single_citation(&command.name)
            {
                let Some((name, removals)) =
                    completion_name_argument(&doc.text.source, command, offset)
                else {
                    return vec![];
                };
                let Some((main, mut additional)) =
                    syntax::name_completion_ranges(&doc.text, name, offset)
                else {
                    return vec![];
                };
                additional.extend(removals.into_iter().map(|span| TextEdit {
                    range: doc.text.range(span.start, span.end),
                    new_text: String::new(),
                }));
                (main, additional)
            } else {
                (doc.text.range(span.start, offset + suffix_length), vec![])
            };
        let kind = if command.name == "hyperlink" {
            Some(TargetKind::Anchor)
        } else if syntax::is_reference(&command.name)
            || syntax::has_separate_label_argument(&command.name)
        {
            Some(TargetKind::Label)
        } else if syntax::is_citation(&command.name) {
            Some(TargetKind::Citation)
        } else {
            None
        };
        let mut candidates = Vec::new();
        let mut bibliography_docs = BTreeMap::new();
        let limit = workspace.completion_budget();
        let mut seen = BTreeSet::new();
        let labels = (kind == Some(TargetKind::Label))
            .then(|| crate::symbols::label_declarations::Index::new(workspace, uri));
        if let Some(kind) = kind {
            'documents: for (entry_uri, entry_doc) in &workspace.documents {
                if !scope.contains(entry_uri) {
                    continue;
                }
                for entry in entries(entry_doc, kind) {
                    if labels
                        .as_ref()
                        .is_some_and(|labels| !labels.contains(entry_uri, entry))
                    {
                        continue;
                    }
                    if kind == TargetKind::Citation && !syntax::is_static_citation_key(&entry.name)
                    {
                        continue;
                    }
                    let before = candidates.len();
                    let full = completion_candidate(
                        &mut candidates,
                        &mut seen,
                        limit,
                        prefix,
                        &entry.name,
                        || {
                            if kind == TargetKind::Citation {
                                entry.detail.clone()
                            } else {
                                format!("{} — {}", entry.detail, super::display::path(entry_uri))
                            }
                        },
                    );
                    if kind == TargetKind::Citation && candidates.len() > before {
                        if let Some(docs) = crate::bibliography::reference_preview::documentation(
                            workspace,
                            entry_uri,
                            &entry.name,
                            &entry.detail,
                        ) {
                            bibliography_docs.insert(entry.name.clone(), docs);
                        }
                    }
                    if full {
                        break 'documents;
                    }
                }
            }
        } else if matches!(command.name.as_str(), "begin" | "end")
            || syntax::declares_environment_copy(&command.name)
        {
            for name in ENVIRONMENTS {
                if completion_candidate(&mut candidates, &mut seen, limit, prefix, name, || {
                    "LaTeX environment".into()
                }) {
                    break;
                }
            }
            if candidates.len() < limit {
                for argument in workspace
                    .documents
                    .iter()
                    .filter(|(uri, _)| scope.contains(*uri))
                    .flat_map(|(_, doc)| &doc.syntax.commands)
                    .filter(|cmd| cmd.name == "begin" || syntax::declares_environment(&cmd.name))
                    .filter_map(|cmd| cmd.argument.as_ref())
                    .filter(|argument| syntax::is_static_key(&argument.value))
                {
                    if completion_candidate(
                        &mut candidates,
                        &mut seen,
                        limit,
                        prefix,
                        &argument.value,
                        || "Document environment".into(),
                    ) {
                        break;
                    }
                }
            }
        } else {
            return vec![];
        }
        if kind == Some(TargetKind::Label) && candidates.len() < limit {
            for (name, info) in crate::compilation::auxiliary::labels(workspace, uri)
                .into_iter()
                .filter(|(_, info)| info.external)
            {
                if completion_candidate(&mut candidates, &mut seen, limit, prefix, &name, || {
                    crate::compilation::auxiliary::describe(&info)
                }) {
                    break;
                }
            }
        }
        *keys = matches!(kind, Some(TargetKind::Label | TargetKind::Citation));
        let environment_docs = (kind.is_none())
            .then(|| crate::language::completion_resolve::Commands::new(workspace, uri, &scope));
        return candidates
            .into_iter()
            .map(|(name, detail)| {
                let mut item = CompletionItem {
                    label: name.clone(),
                    detail: Some(detail),
                    documentation: bibliography_docs.remove(&name),
                    kind: Some(CompletionItemKind::REFERENCE),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: replacement_range,
                        new_text: syntax::label_name_text(&doc.text.source, command, &name),
                    })),
                    additional_text_edits: (!additional_edits.is_empty())
                        .then(|| additional_edits.clone()),
                    ..CompletionItem::default()
                };
                if let Some(docs) = &environment_docs {
                    docs.apply_environment(&mut item, &name);
                }
                if kind == Some(TargetKind::Citation) && item.documentation.is_some() {
                    item.detail = None;
                }
                item
            })
            .collect();
    }
    // Filename/name providers above can interpret literal conversion wrappers.
    // The remaining generic command provider must not complete quoted text.
    if doc
        .syntax
        .literal_conversions
        .iter()
        .any(|span| span.contains(&offset.saturating_sub(1)))
    {
        return vec![];
    }
    let line_start = doc.text.source[..offset]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let before = &doc.text.source[line_start..offset];
    let Some(slash) = before.rfind('\\') else {
        return vec![];
    };
    // An escaped backslash is a line break, not a command prefix.
    let slash_run = before[..=slash]
        .bytes()
        .rev()
        .take_while(|&b| b == b'\\')
        .count();
    if slash_run % 2 == 0 {
        return vec![];
    }
    let prefix = &before[slash + 1..];
    if !prefix.bytes().all(|b| b.is_ascii_alphabetic() || b == b'@') {
        return vec![];
    }
    let command_end = offset
        + doc.text.source[offset..]
            .bytes()
            .take_while(|b| b.is_ascii_alphabetic() || *b == b'@')
            .count();
    let range = doc.text.range(line_start + slash + 1, command_end);
    let allow_math = doc.syntax.control_operands.contains(&(line_start + slash))
        || crate::language::math_context::is_math(
            &doc.text.source,
            &doc.syntax,
            line_start + slash,
        );
    // Declaration/inspection operands name a control sequence without calling
    // it. Reuse the parser's operand boundaries rather than keyword lookbehind.
    let after_command = syntax::skip_space(&doc.text.source, command_end);
    let use_snippets = workspace.client.snippets
        && !doc.syntax.control_operands.contains(&(line_start + slash))
        && !doc.text.source[after_command..].starts_with(['{', '[', '*']);
    command_items(
        workspace,
        uri,
        &scope,
        prefix,
        &doc.text.source[command_end..],
        range,
        allow_math,
        use_snippets,
        workspace.completion_budget(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn command_items(
    workspace: &Workspace,
    uri: &Url,
    scope: &BTreeSet<Url>,
    prefix: &str,
    following: &str,
    range: lsp_types::Range,
    allow_math: bool,
    use_snippets: bool,
    limit: usize,
) -> Vec<CompletionItem> {
    super::command_index::complete(
        workspace,
        uri,
        scope,
        prefix,
        following,
        range,
        allow_math,
        use_snippets,
        limit,
    )
}

fn completion_candidate(
    candidates: &mut Vec<(String, String)>,
    seen: &mut BTreeSet<String>,
    limit: usize,
    prefix: &str,
    name: &str,
    detail: impl FnOnce() -> String,
) -> bool {
    if name.starts_with(prefix) && seen.insert(name.to_owned()) {
        candidates.push((name.to_owned(), detail()));
    }
    candidates.len() >= limit
}

fn bibliography_completion(
    workspace: &Workspace,
    uri: &Url,
    doc: &Document,
    offset: usize,
    scope: &BTreeSet<Url>,
) -> Vec<CompletionItem> {
    if let Some(items) =
        crate::bibliography::bib_completion::completion(workspace, doc, offset, scope)
    {
        return items;
    }
    if let Some(items) = crate::bibliography::bib_strings::completion(workspace, doc, offset, scope)
    {
        return items;
    }
    if let Some(items) = crate::bibliography::bib_source_catalog::completion(workspace, uri, offset)
    {
        return items;
    }
    if let Some(items) =
        crate::bibliography::bib_section_symbols::completion(workspace, uri, offset)
    {
        return items;
    }
    bibliography_relation_completions(workspace, doc, offset, scope, None)
}

pub(crate) fn bibliography_relation_completions(
    workspace: &Workspace,
    doc: &Document,
    offset: usize,
    scope: &BTreeSet<Url>,
    allowed: Option<&BTreeSet<String>>,
) -> Vec<CompletionItem> {
    let xdata = doc.syntax.citation_fields.iter().any(|field| {
        field.name == "xdata"
            && field.argument.span.start <= offset
            && offset <= field.argument.span.end
    });
    let typed =
        xdata.then(|| crate::bibliography::bib_completion::xdata_candidates(workspace, scope));
    let typed = &typed;
    let candidates = workspace
        .documents
        .iter()
        .filter(|(uri, _)| scope.contains(*uri))
        .flat_map(|(uri, doc)| {
            entries(doc, TargetKind::Citation)
                .filter(move |entry| {
                    typed.as_ref().is_none_or(|names| {
                        names.contains(entry.name.as_str())
                            && allowed.is_none_or(|allowed| {
                                crate::bibliography::bib_completion::xdata_owner(doc, &entry.span)
                                    .is_some_and(|owner| allowed.contains(owner))
                            })
                    })
                })
                .map(move |entry| (uri, entry.name.as_str(), entry.detail.as_str()))
        })
        .filter(|(_, name, _)| allowed.is_none_or(|allowed| allowed.contains(*name)));
    let limit = workspace.completion_budget();
    bibliography_relation_edits(workspace, doc, offset, candidates, limit)
}

pub(crate) fn bibliography_relation_edits<'a>(
    workspace: &Workspace,
    doc: &Document,
    offset: usize,
    candidates: impl Iterator<Item = (&'a Url, &'a str, &'a str)>,
    limit: usize,
) -> Vec<CompletionItem> {
    let Some(field) = doc
        .syntax
        .citation_fields
        .iter()
        .find(|field| field.argument.span.start <= offset && offset <= field.argument.span.end)
    else {
        return vec![];
    };
    let value = &doc.text.source[field.argument.span.start..offset];
    let key = if matches!(field.name.as_str(), "xdata" | "related" | "entryset") {
        value.rsplit(',').next().unwrap_or("")
    } else {
        value
    };
    let key = key.trim_start();
    let suffix = &doc.text.source[offset..field.argument.span.end];
    let remaining = suffix.find(',').unwrap_or(suffix.len());
    let remaining = suffix[..remaining].trim_end().len();
    let key_span = offset - key.len()..offset + remaining;
    let mut seen = BTreeSet::new();
    candidates
        .filter(|(_, name, _)| {
            syntax::is_static_citation_key(name)
                && name.starts_with(key)
                && seen.insert((*name).to_string())
        })
        .filter_map(|(uri, name, detail)| {
            let (span, new_text) = if name.contains('"')
                && doc
                    .text
                    .source
                    .as_bytes()
                    .get(field.argument.outer_span.start)
                    == Some(&b'"')
            {
                let before = &doc.text.source[field.argument.span.start..offset - key.len()];
                let after = &doc.text.source[offset + remaining..field.argument.span.end];
                (
                    field.argument.outer_span.clone(),
                    format!("{{{before}{}{after}}}", name),
                )
            } else {
                (key_span.clone(), name.to_string())
            };
            let (range, additional) = syntax::name_completion_ranges(&doc.text, span, offset)?;
            let documentation =
                crate::bibliography::reference_preview::documentation(workspace, uri, name, detail);
            Some(CompletionItem {
                label: name.to_string(),
                detail: documentation.is_none().then(|| detail.into()),
                documentation,
                kind: Some(CompletionItemKind::REFERENCE),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text })),
                additional_text_edits: (!additional.is_empty()).then_some(additional),
                ..Default::default()
            })
        })
        .take(limit)
        .collect()
}

pub fn definitions(workspace: &Workspace, uri: &Url, position: Position) -> Vec<Location> {
    crate::analysis::AnalysisContext::new().run(|_| definitions_query(workspace, uri, position))
}

fn definitions_query(workspace: &Workspace, uri: &Url, position: Position) -> Vec<Location> {
    let Some(doc) = workspace.documents.get(uri) else {
        return vec![];
    };
    let Some(offset) = doc.text.offset(position) else {
        return vec![];
    };

    if let Some(result) = crate::symbols::hook_names::definitions(workspace, uri, offset) {
        return result;
    }
    if let Some(result) = crate::symbols::hook_labels::definitions(workspace, uri, offset) {
        return result;
    }

    if let Some(result) = crate::bibliography::bib_categories::definitions(workspace, uri, offset) {
        return result;
    }
    if let Some(entry) = crate::symbols::extra_symbols::at(doc, offset) {
        return crate::symbols::extra_symbols::definitions(workspace, uri, entry);
    }
    if let Some(result) =
        crate::bibliography::bib_section_labels::definitions(workspace, uri, offset)
    {
        return result;
    }
    for link in crate::language::navigation::document_links(workspace, uri) {
        if link.range.start <= position && position <= link.range.end {
            if let Some(uri) = link.target.filter(|uri| uri.scheme() == "file") {
                return vec![Location {
                    uri,
                    range: Range::default(),
                }];
            }
        }
    }
    if let Some(result) =
        crate::bibliography::bib_source_sections::definitions(workspace, uri, offset)
    {
        return result;
    }
    if let Some(result) =
        crate::bibliography::bib_section_symbols::definitions(workspace, uri, offset)
    {
        return result;
    }
    if let Some(result) = crate::bibliography::bib_section_keys::definitions(workspace, uri, offset)
    {
        return result;
    }
    let Some(target) = target(workspace, uri, offset) else {
        return vec![];
    };
    let scope = workspace.symbol_scope(uri);
    let labels = (target.kind == TargetKind::Label)
        .then(|| crate::symbols::label_declarations::Index::new(workspace, uri));
    if labels
        .as_ref()
        .is_some_and(|labels| labels.is_data(uri, target.span.start))
    {
        return vec![];
    }
    let mut result: Vec<_> = workspace
        .documents
        .iter()
        .filter(|(uri, _)| scope.contains(*uri))
        .flat_map(|(uri, doc)| {
            entries(doc, target.kind)
                .filter(|entry| target.kind.same_name(&entry.name, target.name))
                .filter(|entry| {
                    labels
                        .as_ref()
                        .is_none_or(|labels| labels.contains(uri, entry))
                })
                .map(|entry| Location {
                    uri: uri.clone(),
                    range: doc.text.range(entry.span.start, entry.span.end),
                })
        })
        .collect();
    if result.is_empty() && target.kind == TargetKind::Label {
        if let Some(location) =
            crate::compilation::auxiliary::definition(workspace, uri, target.name)
        {
            result.push(location);
        }
    }
    result
}

pub fn references(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    crate::analysis::AnalysisContext::new()
        .run(|_| references_query(workspace, uri, position, include_declaration))
}

fn references_query(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let Some(doc) = workspace.documents.get(uri) else {
        return vec![];
    };
    if let Some(offset) = doc.text.offset(position) {
        if let Some(result) =
            crate::symbols::hook_names::references(workspace, uri, offset, include_declaration)
        {
            return result;
        }
        if let Some(result) =
            crate::symbols::hook_labels::references(workspace, uri, offset, include_declaration)
        {
            return result;
        }
        if let Some(result) = crate::bibliography::bib_categories::references(
            workspace,
            uri,
            offset,
            include_declaration,
        ) {
            return result;
        }
    }
    if let Some(offset) = doc.text.offset(position) {
        if let Some(result) = crate::bibliography::bib_section_labels::references(
            workspace,
            uri,
            offset,
            include_declaration,
        ) {
            return result;
        }
    }
    if let Some(entry) = doc
        .text
        .offset(position)
        .and_then(|offset| crate::symbols::extra_symbols::at(doc, offset))
    {
        return crate::symbols::extra_symbols::references(
            workspace,
            uri,
            entry,
            include_declaration,
        );
    }
    if let Some(offset) = doc.text.offset(position) {
        if let Some(result) = crate::bibliography::bib_section_references::references(
            workspace,
            uri,
            offset,
            include_declaration,
        ) {
            return result;
        }
        if let Some(result) =
            crate::bibliography::bib_section_symbols::references(workspace, uri, offset)
        {
            return result;
        }
    }
    let Some(target) = doc
        .text
        .offset(position)
        .and_then(|offset| target(workspace, uri, offset))
    else {
        return vec![];
    };
    let crate::project::workspace::SymbolProjects { mut scope, regions } =
        workspace.symbol_projects(uri);
    let labels = (target.kind == TargetKind::Label)
        .then(|| crate::symbols::label_declarations::Index::new(workspace, uri));
    if labels
        .as_ref()
        .is_some_and(|labels| labels.is_data(uri, target.span.start))
    {
        return vec![];
    }
    let definitions: Vec<_> = workspace
        .documents
        .iter()
        .filter(|(uri, _)| scope.contains(*uri))
        .flat_map(|(uri, doc)| {
            entries(doc, target.kind)
                .filter(|entry| target.kind.same_name(&entry.name, target.name))
                .filter(|entry| {
                    labels
                        .as_ref()
                        .is_none_or(|labels| labels.contains(uri, entry))
                })
                .map(move |_| uri)
        })
        .collect();
    if definitions.len() == 1 {
        for project in regions
            .values()
            .filter(|project| project.contains(definitions[0]))
        {
            scope.extend(project.iter().cloned());
        }
    }
    let mut result: Vec<_> = all_references(workspace, uri, position, include_declaration)
        .into_iter()
        .filter(|location| scope.contains(&location.uri))
        .filter(|location| {
            labels.as_ref().is_none_or(|labels| {
                workspace.documents.get(&location.uri).is_none_or(|doc| {
                    !doc.text
                        .offset(location.range.start)
                        .is_some_and(|at| labels.is_data(&location.uri, at))
                })
            })
        })
        .collect();
    if include_declaration && definitions.is_empty() && target.kind == TargetKind::Label {
        if let Some(location) =
            crate::compilation::auxiliary::definition(workspace, uri, target.name)
        {
            result.push(location);
        }
    }
    result
}

fn all_references(
    workspace: &Workspace,
    uri: &Url,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let Some(doc) = workspace.documents.get(uri) else {
        return vec![];
    };
    let Some(target) = doc
        .text
        .offset(position)
        .and_then(|offset| target(workspace, uri, offset))
    else {
        return vec![];
    };
    let generated = target.kind.generated_kind().and_then(|kind| {
        crate::symbols::generated::in_scope(
            workspace,
            &workspace.symbol_scope(uri),
            target.name,
            kind,
        )
    });
    let environment_calls = (target.kind == TargetKind::Environment)
        .then(|| crate::symbols::environment_calls::Index::new(workspace));
    let named_references = matches!(target.kind, TargetKind::Label | TargetKind::Citation)
        .then(|| crate::symbols::named_references::Index::new(workspace, uri));
    let mut result = Vec::new();
    for (uri, doc) in &workspace.documents {
        if let Some(index) = &named_references {
            result.extend(
                index
                    .references(uri, target.kind == TargetKind::Citation, target.name)
                    .map(|span| Location {
                        uri: uri.clone(),
                        range: doc.text.range(span.start, span.end),
                    }),
            );
        }
        if target.kind == TargetKind::BibString {
            result.extend(
                doc.syntax
                    .bibliography_strings
                    .references
                    .iter()
                    .chain(entries(doc, target.kind).filter(|_| include_declaration))
                    .filter(|entry| entry.name.eq_ignore_ascii_case(target.name))
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
            continue;
        }
        if let Some(calls) = &environment_calls {
            result.extend(calls.references(uri, target.name).map(|argument| Location {
                uri: uri.clone(),
                range: doc.text.range(argument.span.start, argument.span.end),
            }));
        }
        if let Some(model) = generated {
            result.extend(
                crate::symbols::generated::references(doc, target.name, model)
                    .into_iter()
                    .filter(|reference| {
                        !matches!(target.kind, TargetKind::Conditional | TargetKind::Toggle)
                            || !entries(doc, target.kind).any(|entry| entry.span == reference.span)
                    })
                    .map(|reference| Location {
                        uri: uri.clone(),
                        range: doc.text.range(reference.span.start, reference.span.end),
                    }),
            );
        }
        if target.kind == TargetKind::Color {
            result.extend(
                doc.syntax
                    .color_references
                    .iter()
                    .filter(|entry| entry.name == target.name)
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
        }
        if target.kind == TargetKind::Citation && doc.kind == DocumentKind::Bibtex {
            result.extend(
                doc.syntax
                    .citation_references
                    .iter()
                    .filter(|entry| entry.name == target.name)
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
        }
        if include_declaration {
            result.extend(
                entries(doc, target.kind)
                    .filter(|entry| entry.name == target.name)
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
        }
        if matches!(target.kind, TargetKind::Label | TargetKind::Citation) {
            continue;
        }
        if target.kind == TargetKind::Command {
            result.extend(
                doc.syntax
                    .name_command_references
                    .iter()
                    .filter(|entry| entry.name == target.name)
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
        }
        if let Some(kind) = target.kind.saved_kind() {
            result.extend(
                crate::symbols::saved_text::references(doc, kind)
                    .iter()
                    .filter(|entry| entry.name == target.name)
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
            continue;
        }
        if target.kind == TargetKind::Anchor {
            result.extend(
                doc.syntax
                    .anchor_references
                    .iter()
                    .filter(|entry| entry.name == target.name)
                    .map(|entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    }),
            );
            continue;
        }
        for (index, command) in doc.syntax.commands.iter().enumerate() {
            if target.kind != TargetKind::Command
                && doc.syntax.control_operands.contains(&command.span.start)
            {
                continue;
            }
            if target.kind == TargetKind::Environment && generated.is_none() {
                if let Some(argument) = syntax::environment_copy_source(&doc.text.source, command)
                    .filter(|argument| argument.value == target.name)
                {
                    result.push(Location {
                        uri: uri.clone(),
                        range: doc.text.range(argument.span.start, argument.span.end),
                    });
                }
            }
            if target.kind == TargetKind::Command {
                result.extend(
                    crate::symbols::named_commands::spans(doc, command)
                        .into_iter()
                        .filter(|span| doc.text.source[span.clone()] == *target.name)
                        .filter(|span| {
                            !doc.syntax
                                .definitions
                                .iter()
                                .any(|entry| entry.span == *span)
                        })
                        .map(|span| Location {
                            uri: uri.clone(),
                            range: doc.text.range(span.start, span.end),
                        }),
                );
                if let Some(span) =
                    syntax::literal_csname(&doc.text.source, &doc.syntax.commands, index)
                        .filter(|span| doc.text.source[span.clone()] == *target.name)
                        .filter(|span| {
                            !doc.syntax
                                .definitions
                                .iter()
                                .any(|entry| entry.span == *span)
                        })
                {
                    result.push(Location {
                        uri: uri.clone(),
                        range: doc.text.range(span.start, span.end),
                    });
                }
                if command.name == target.name
                    && !doc
                        .syntax
                        .definitions
                        .iter()
                        .any(|entry| entry.span == command.span)
                {
                    // Macro names inside their declaration group are indexed as commands too.
                    let declaration = doc.syntax.definitions.iter().any(|entry| {
                        entry.span.start == command.span.start && entry.name == command.name
                    });
                    if !declaration {
                        result.push(Location {
                            uri: uri.clone(),
                            range: doc.text.range(command.span.start, command.span.end),
                        });
                    }
                }
                continue;
            }
            if target.kind == TargetKind::Citation
                && crate::bibliography::citations::is_structured(&command.name)
            {
                continue;
            }
            let matches = match target.kind {
                TargetKind::Label => {
                    syntax::is_reference(&command.name)
                        || syntax::has_separate_label_argument(&command.name)
                }
                TargetKind::Citation => syntax::is_citation(&command.name),
                TargetKind::Command
                | TargetKind::Anchor
                | TargetKind::Color
                | TargetKind::BibString => false,
                TargetKind::Counter | TargetKind::Conditional | TargetKind::Toggle => false,
                TargetKind::SavedText | TargetKind::MovingText => false,
                TargetKind::Environment => matches!(command.name.as_str(), "begin" | "end"),
            };
            if !matches {
                continue;
            }
            if target.kind == TargetKind::Label {
                if let Some(argument) = syntax::range_reference_end(&doc.text.source, command)
                    .filter(|argument| argument.value == target.name)
                {
                    result.push(Location {
                        uri: uri.clone(),
                        range: doc.text.range(argument.span.start, argument.span.end),
                    });
                }
            }
            let separate_label = syntax::separate_label_argument(&doc.text.source, command);
            let environment_argument = (target.kind == TargetKind::Environment)
                .then(|| {
                    if matches!(command.name.as_str(), "begin" | "end") {
                        syntax::environment_name_argument(&doc.text.source, command.span.end)
                    } else {
                        command
                            .argument
                            .clone()
                            .map(|argument| syntax::single_key_argument(&doc.text.source, argument))
                    }
                })
                .flatten();
            let argument = if syntax::has_separate_label_argument(&command.name) {
                separate_label.as_ref()
            } else {
                environment_argument.as_ref().or(command.argument.as_ref())
            };
            if let Some(arg) = argument {
                if target.kind == TargetKind::Environment
                    || target.kind == TargetKind::Citation
                        && syntax::is_single_citation(&command.name)
                    || target.kind == TargetKind::Label
                        && (syntax::is_single_reference(&command.name)
                            || syntax::has_separate_label_argument(&command.name))
                {
                    if arg.value == target.name {
                        result.push(Location {
                            uri: uri.clone(),
                            range: doc.text.range(arg.span.start, arg.span.end),
                        });
                    }
                    continue;
                }
                for span in target_key_spans(&doc.text.source, arg.span.clone(), target.kind) {
                    if doc.text.source[span.clone()] == *target.name {
                        result.push(Location {
                            uri: uri.clone(),
                            range: doc.text.range(span.start, span.end),
                        });
                    }
                }
            }
        }
    }
    result
}

pub fn hover(workspace: &Workspace, uri: &Url, position: Position) -> Option<Hover> {
    crate::analysis::AnalysisContext::new().run(|_| hover_query(workspace, uri, position))
}

fn hover_query(workspace: &Workspace, uri: &Url, position: Position) -> Option<Hover> {
    let doc = workspace.documents.get(uri)?;
    let offset = doc.text.offset(position)?;
    if let Some(result) = crate::symbols::hook_names::hover(workspace, uri, offset) {
        return Some(result);
    }
    if let Some(result) = crate::symbols::hook_labels::hover(workspace, uri, offset) {
        return Some(result);
    }
    if let Some(result) = crate::bibliography::bib_categories::hover(workspace, uri, offset) {
        return Some(result);
    }
    if let Some(entry) = crate::symbols::extra_symbols::at(doc, offset) {
        return Some(crate::symbols::extra_symbols::hover(workspace, uri, entry));
    }

    if let Some(hover) = crate::bibliography::bib_section_labels::hover(workspace, uri, offset) {
        return Some(hover);
    }
    if let Some(hover) = crate::bibliography::bib_completion::hover(doc, offset) {
        return Some(hover);
    }
    if let Some(hover) = crate::bibliography::bib_accessors::hover(workspace, uri, offset) {
        return Some(hover);
    }
    if let Some(link) = crate::language::navigation::document_links(workspace, uri)
        .into_iter()
        .find(|link| link.range.start <= position && position <= link.range.end)
    {
        if let Some(target) = link.target {
            if let Some(file) = target
                .path_segments()
                .and_then(|mut segments| segments.next_back())
            {
                if let Some(package) = crate::project::packages::catalog()
                    .iter()
                    .find(|package| file == format!("{}.sty", package.name))
                {
                    return Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::PlainText,
                            value: format!(
                                "{}\n\n{}\nSource: {}\nDocumentation: https://texdoc.org/pkg/{}",
                                package.name,
                                package.description,
                                super::display::path(&target),
                                package.name
                            ),
                        }),
                        range: Some(link.range),
                    });
                }
            }
        }
    }
    if let Some(hover) = crate::bibliography::bib_source_sections::hover(workspace, uri, offset) {
        return hover;
    }
    if let Some(hover) = crate::bibliography::bib_section_symbols::hover(workspace, uri, offset) {
        return hover;
    }
    if let Some(hover) = crate::bibliography::bib_section_keys::hover(workspace, uri, offset) {
        return hover;
    }
    let target = target(workspace, uri, offset)?;
    let scope = workspace.symbol_scope(uri);
    let labels = (target.kind == TargetKind::Label)
        .then(|| crate::symbols::label_declarations::Index::new(workspace, uri));
    if labels
        .as_ref()
        .is_some_and(|labels| labels.is_data(uri, target.span.start))
    {
        return None;
    }
    let mut value = None;
    for (entry_uri, entry_doc) in &workspace.documents {
        if !scope.contains(entry_uri) {
            continue;
        }
        if let Some(entry) = entries(entry_doc, target.kind)
            .filter(|entry| {
                labels
                    .as_ref()
                    .is_none_or(|labels| labels.contains(entry_uri, entry))
            })
            .find(|entry| target.kind.same_name(&entry.name, target.name))
        {
            if target.kind == TargetKind::Citation {
                return Some(Hover {
                    contents: HoverContents::Markup(crate::bibliography::reference_preview::hover(
                        entry_doc, entry,
                    )),
                    range: Some(doc.text.range(target.span.start, target.span.end)),
                });
            }
            value = Some(format!(
                "{}\n\n{}\n{}:{}",
                entry.name,
                entry.detail,
                super::display::path(entry_uri),
                entry_doc.text.position(entry.span.start).line + 1
            ));
            break;
        }
    }
    if target.kind == TargetKind::Label {
        if let Some(info) = crate::compilation::auxiliary::labels(workspace, uri).get(target.name) {
            let extra = crate::compilation::auxiliary::describe(info);
            value =
                Some(value.map_or_else(|| extra.clone(), |value| format!("{value}\n\n{extra}")));
        }
    }
    if matches!(target.kind, TargetKind::Command | TargetKind::Environment) {
        let notes = if target.kind == TargetKind::Command {
            crate::symbols::command_docs::for_command(workspace, &scope, target.name)
        } else {
            crate::symbols::command_docs::for_environment(workspace, &scope, target.name)
        };
        if let (Some(value), Some(notes)) = (value.as_mut(), notes) {
            value.push_str("\n\n");
            value.push_str(&notes);
        }
    }
    if value.is_none() && target.kind == TargetKind::Command {
        value = crate::project::packages::command(workspace, &scope, target.name).map(
            |(package, command)| {
                format!(
                    "\\{}\n\n{}",
                    target.name,
                    crate::project::packages::documentation(package, command)
                )
            },
        );
    }
    if value.is_none() && target.kind == TargetKind::Command {
        value = COMMANDS
            .iter()
            .find(|(name, _, _)| *name == target.name)
            .map(|(name, _, docs)| format!("\\{name}\n\n{docs}"));
    }
    if value.is_none() && target.kind == TargetKind::BibString {
        value = crate::bibliography::bib_strings::builtin(target.name).map(|month| {
            format!(
                "{}\n\nCommon BibTeX month abbreviation: {month}",
                target.name
            )
        });
    }
    if value.is_none()
        && target.kind == TargetKind::Environment
        && ENVIRONMENTS.contains(&target.name)
    {
        value = Some(format!(
            "\\begin{{{}}} ... \\end{{{}}}\n\nLaTeX environment",
            target.name, target.name
        ));
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: value?,
        }),
        range: Some(doc.text.range(target.span.start, target.span.end)),
    })
}

pub fn diagnostics(doc: &Document) -> Vec<Diagnostic> {
    let inactive = crate::symbols::label_declarations::local(doc);
    diagnostics_for(doc, None, |entry| !inactive.contains(&entry.span.start))
}

pub(crate) fn diagnostics_at(
    doc: &Document,
    uri: &Url,
    labels: &crate::symbols::label_declarations::Index,
) -> Vec<Diagnostic> {
    diagnostics_for(doc, Some(uri), |entry| labels.contains(uri, entry))
}

fn diagnostics_for(
    doc: &Document,
    uri: Option<&Url>,
    label: impl Fn(&Entry) -> bool,
) -> Vec<Diagnostic> {
    let mut diagnostics = doc.syntax.diagnostics.clone();
    for (code, entries) in [
        ("duplicate-label", &doc.syntax.labels),
        ("duplicate-bibliography-key", &doc.syntax.citations),
        ("duplicate-pdf-destination", &doc.syntax.anchors),
    ] {
        let mut seen = std::collections::BTreeMap::<&str, &Entry>::new();
        for entry in entries {
            if code == "duplicate-label" && !label(entry) {
                continue;
            }
            if let Some(first) = seen.get(entry.name.as_str()) {
                let mut diagnostic = syntax::diagnostic(
                    &doc.text,
                    entry.span.clone(),
                    format!("Duplicate key `{}`", entry.name),
                );
                diagnostic.code = Some(NumberOrString::String(code.into()));
                diagnostic.related_information = uri.map(|uri| {
                    vec![DiagnosticRelatedInformation {
                        location: Location::new(
                            uri.clone(),
                            doc.text.range(first.span.start, first.span.end),
                        ),
                        message: format!("First declaration of `{}` in this document", entry.name),
                    }]
                });
                diagnostics.push(diagnostic);
            } else {
                seen.insert(&entry.name, entry);
            }
        }
    }
    diagnostics.sort_by_key(|d| (d.range.start.line, d.range.start.character));
    diagnostics
}

pub fn symbols(doc: &Document) -> Vec<DocumentSymbol> {
    // An initial hierarchical response already owns the whole tree. Avoid
    // cloning it just to populate a cache this client may never query again.
    doc.outline.get().map_or_else(
        || uncached_symbols(doc),
        |snapshot| snapshot.symbols.clone(),
    )
}

#[allow(deprecated)]
pub(crate) fn uncached_symbols(doc: &Document) -> Vec<DocumentSymbol> {
    if doc.kind == DocumentKind::Bibtex {
        return crate::bibliography::bib_symbols::symbols(doc);
    }
    let headings: Vec<_> = doc
        .syntax
        .commands
        .iter()
        .filter_map(|command| {
            Some((
                syntax::section_level(&command.name)?,
                command,
                command.argument.as_ref()?,
            ))
        })
        .collect();
    let mut result = Vec::new();
    for (index, (level, command, argument)) in headings.iter().enumerate() {
        let end = headings[index + 1..]
            .iter()
            .find(|(next_level, _, _)| next_level <= level)
            .map(|(_, command, _)| command.span.start)
            .unwrap_or(doc.text.source.len());
        result.push(DocumentSymbol {
            // Empty titles are common while editing, but clients require a
            // nonblank symbol name. Keep the heading and its children intact.
            name: if argument.value.trim().is_empty() {
                format!("\\{}", command.name)
            } else {
                argument.value.clone()
            },
            detail: Some(command.name.clone()),
            kind: SymbolKind::NAMESPACE,
            tags: None,
            deprecated: None,
            range: doc.text.range(command.span.start, end),
            selection_range: doc.text.range(argument.span.start, argument.span.end),
            children: None,
        });
    }
    for (entries, kind) in [
        (&doc.syntax.labels, SymbolKind::KEY),
        (&doc.syntax.citations, SymbolKind::KEY),
        (&doc.syntax.anchors, SymbolKind::KEY),
        (&doc.syntax.definitions, SymbolKind::FUNCTION),
        (&doc.syntax.environments, SymbolKind::CLASS),
        (&doc.syntax.counters, SymbolKind::VARIABLE),
        (&doc.syntax.conditionals, SymbolKind::BOOLEAN),
        (&doc.syntax.toggles, SymbolKind::BOOLEAN),
        (&doc.syntax.saved_text, SymbolKind::CONSTANT),
        (&doc.syntax.moving_text, SymbolKind::CONSTANT),
        (&doc.syntax.colors, SymbolKind::CONSTANT),
    ] {
        result.extend(entries.iter().map(|entry| DocumentSymbol {
            name: entry.name.clone(),
            detail: Some(entry.detail.clone()),
            kind: if entry.is_variable {
                SymbolKind::VARIABLE
            } else {
                kind
            },
            tags: None,
            deprecated: None,
            range: doc.text.range(entry.span.start, entry.span.end),
            selection_range: doc.text.range(entry.span.start, entry.span.end),
            children: None,
        }));
    }
    result.extend(crate::symbols::extra_symbols::symbols(doc));
    result.extend(super::structure::symbols(doc));
    // Outer scopes precede children when they start at the same position.
    result.sort_by_key(|symbol| (symbol.range.start, std::cmp::Reverse(symbol.range.end)));
    let mut nested = Vec::new();
    for symbol in result {
        insert_symbol(&mut nested, symbol);
    }
    nested
}

fn insert_symbol(symbols: &mut Vec<DocumentSymbol>, symbol: DocumentSymbol) {
    if let Some(parent) = symbols.last_mut() {
        if super::structure::container(parent)
            && parent.range.start <= symbol.range.start
            && symbol.range.end <= parent.range.end
        {
            insert_symbol(parent.children.get_or_insert_with(Vec::new), symbol);
            return;
        }
    }
    symbols.push(symbol);
}

/// Older clients require SymbolInformation instead of hierarchical DocumentSymbol.
pub fn flat_symbols(doc: &Document, uri: &Url) -> Vec<SymbolInformation> {
    let mut result = Vec::new();
    flatten_symbols(
        &super::symbol_cache::outline(doc).symbols,
        uri,
        None,
        &mut result,
    );
    result
}

#[allow(deprecated)]
fn flatten_symbols(
    symbols: &[DocumentSymbol],
    uri: &Url,
    container: Option<&str>,
    result: &mut Vec<SymbolInformation>,
) {
    for symbol in symbols {
        result.push(SymbolInformation {
            name: symbol.name.clone(),
            kind: symbol.kind,
            tags: symbol.tags.clone(),
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: symbol.selection_range,
            },
            container_name: container.map(str::to_owned),
        });
        if let Some(children) = &symbol.children {
            flatten_symbols(children, uri, Some(&symbol.name), result);
        }
    }
}
