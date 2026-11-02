//! Literal kernel hook names, separate from code labels and reference keys.
use crate::{
    parser::syntax,
    project::workspace::{Document, Workspace},
    symbols::commands::CommandSignatures,
};
use lsp_types::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range as Span,
};

mod edits;
use super::hook_fields::Candidate;
pub(crate) use edits::{prepare, rename};

const COMMANDS: &[&str] = &[
    "NewHook",
    "NewReversedHook",
    "NewHookWithArguments",
    "NewReversedHookWithArguments",
    "NewMirroredHookPair",
    "NewMirroredHookPairWithArguments",
    "AddToHook",
    "AddToHookWithArguments",
    "AddToHookNext",
    "AddToHookNextWithArguments",
    "RemoveFromHook",
    "UseHook",
    "UseOneTimeHook",
    "UseHookWithArguments",
    "UseOneTimeHookWithArguments",
    "ClearHookNext",
    "ActivateGenericHook",
    "DisableGenericHook",
    "ShowHook",
    "LogHook",
    "DeclareHookRule",
    "ClearHookRule",
    "IfHookEmptyTF",
    "IfHookEmptyT",
    "IfHookEmptyF",
    "IfHookExistsTF",
];

fn valid(name: &str) -> bool {
    syntax::is_static_key(name) && name != "." && !name.starts_with("./")
}

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    span: Span<usize>,
    declaration: bool,
    complete: bool,
    grouped: bool,
    end_cursor: bool,
}

impl Entry {
    fn contains(&self, offset: usize, complete: bool) -> bool {
        (!complete || self.complete && valid(&self.name))
            && self.span.start <= offset
            && (offset < self.span.end || self.end_cursor && offset == self.span.end)
    }
}

pub(super) fn field(source: &str, after: usize) -> Option<(Span<usize>, usize, bool)> {
    let at = syntax::skip_space(source, after);
    if source.as_bytes().get(at) == Some(&b'{') {
        return Some(syntax::group(source, at, b'{', b'}').map_or_else(
            || {
                let end = source[at + 1..]
                    .find(['\r', '\n'])
                    .map_or(source.len(), |len| at + 1 + len);
                (at + 1..end, source.len(), false)
            },
            |(argument, end)| {
                let argument = syntax::single_key_argument(source, argument);
                (argument.span, end, true)
            },
        ));
    }
    // An ungrouped m argument is one token. Only ASCII characters have the
    // same token boundary in all supported engines.
    let byte = *source.as_bytes().get(at)?;
    let span = if byte == b'\\' {
        syntax::control_word(source, at)?
    } else if byte.is_ascii() && !matches!(byte, b'{' | b'}' | b'%') {
        at..at + 1
    } else {
        return None;
    };
    let end = span.end;
    Some((span, end, true))
}

fn name_field(source: &str, after: usize, command: &str) -> Option<(Entry, usize)> {
    let grouped = source.as_bytes().get(syntax::skip_space(source, after)) == Some(&b'{');
    let (mut span, end, complete) = field(source, after)?;
    // Registration normalizes names; UseHook and the conditions use the
    // internal control sequence with literal whitespace.
    if !matches!(command, "UseHook" | "UseHookWithArguments")
        && !crate::symbols::commands::HOOK_CONDITIONS.contains(&command)
    {
        let raw = &source[span.clone()];
        span.start += raw.len() - raw.trim_start_matches([' ', '\t', '\r', '\n']).len();
        span.end = span.start
            + source[span.clone()]
                .trim_end_matches([' ', '\t', '\r', '\n'])
                .len();
    }
    Some((
        Entry {
            name: source[span.clone()].into(),
            span,
            declaration: command.starts_with("New"),
            complete,
            grouped,
            end_cursor: grouped
                || source[end..].is_empty()
                || source[end..]
                    .starts_with(['\\', '{', '}', '[', ']', ' ', '\t', '\r', '\n', '%']),
        },
        end,
    ))
}

fn calls(doc: &Document, signatures: &CommandSignatures<'_>) -> BTreeMap<usize, Candidate<Entry>> {
    let source = &doc.text.source;
    let context = crate::symbols::named_references::LiteralContext::new(doc);
    let declarations: BTreeSet<_> = doc
        .syntax
        .definitions
        .iter()
        .chain(&doc.syntax.environments)
        .flat_map(|entry| [entry.span.start, entry.span.start.saturating_sub(1)])
        .collect();
    let empty = BTreeSet::new();
    let mut result = BTreeMap::new();
    for (index, command) in doc.syntax.commands.iter().enumerate() {
        if declarations.contains(&command.span.start) || context.literal(command.span.start) {
            continue;
        }
        let (name, invocation) = signatures
            .named_invocation(source, &doc.syntax, index, &empty, &context.declarations)
            .unwrap_or((&command.name, command.span.clone()));
        let name = if COMMANDS.contains(&name) && signatures.command_unchanged(name) {
            name
        } else if let Some(name) = signatures
            .forwarded_builtin(name)
            .filter(|name| COMMANDS.contains(name))
        {
            name
        } else {
            continue;
        };
        let count = if name.starts_with("NewMirroredHookPair") {
            2
        } else {
            1
        };
        let mut after = invocation.end;
        for _ in 0..count {
            let Some((entry, end)) = name_field(source, after, name) else {
                break;
            };
            after = end;
            let complete = entry.complete;
            if valid(&entry.name) || entry.name.is_empty() {
                result.insert(
                    entry.span.start,
                    Candidate {
                        entry,
                        invocation: command.span.start,
                        prerequisite: None,
                    },
                );
            }
            if !complete {
                break;
            }
        }
    }
    result
}

#[derive(Default)]
struct Index {
    entries: BTreeMap<Url, BTreeMap<usize, Entry>>,
}

impl Index {
    fn new(ws: &Workspace, origin: &Url, offset: usize, complete: bool) -> Self {
        Self {
            entries: super::hook_fields::index(ws, origin, offset, COMMANDS, calls, |entry| {
                entry.contains(offset, complete)
            }),
        }
    }

    fn target(&self, uri: &Url, offset: usize, complete: bool) -> Option<&Entry> {
        let entries = self.entries.get(uri)?;
        entries
            .values()
            .find(|entry| entry.span.contains(&offset) && entry.contains(offset, complete))
            .or_else(|| {
                entries
                    .values()
                    .find(|entry| entry.contains(offset, complete))
            })
    }

    fn entries<'a>(
        &'a self,
        scope: &'a BTreeSet<Url>,
    ) -> impl Iterator<Item = (&'a Url, &'a Entry)> {
        self.entries
            .iter()
            .filter(|(uri, _)| scope.contains(*uri))
            .flat_map(|(uri, entries)| {
                entries
                    .values()
                    .filter(|entry| entry.complete && valid(&entry.name))
                    .map(move |entry| (uri, entry))
            })
    }

    fn location(ws: &Workspace, uri: &Url, entry: &Entry) -> Location {
        Location {
            uri: uri.clone(),
            range: ws.documents[uri]
                .text
                .range(entry.span.start, entry.span.end),
        }
    }

    fn definitions(&self, ws: &Workspace, uri: &Url, name: &str) -> Vec<Location> {
        self.entries(&ws.symbol_scope(uri))
            .filter(|(_, entry)| entry.declaration && entry.name == name)
            .map(|(uri, entry)| Self::location(ws, uri, entry))
            .collect()
    }
}

pub(crate) fn definitions(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<Location>> {
    let index = Index::new(ws, uri, offset, true);
    let entry = index.target(uri, offset, true)?;
    Some(index.definitions(ws, uri, &entry.name))
}

pub(crate) fn references(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
    include: bool,
) -> Option<Vec<Location>> {
    let index = Index::new(ws, uri, offset, true);
    let entry = index.target(uri, offset, true)?;
    let mut scope = ws.symbol_scope(uri);
    let definitions = index.definitions(ws, uri, &entry.name);
    if definitions.len() == 1 {
        for project in ws
            .symbol_regions()
            .into_values()
            .filter(|project| project.contains(&definitions[0].uri))
        {
            scope.extend(project);
        }
    }
    Some(
        index
            .entries(&scope)
            .filter(|(_, other)| other.name == entry.name && (include || !other.declaration))
            .map(|(uri, entry)| Index::location(ws, uri, entry))
            .collect(),
    )
}

pub(crate) fn hover(ws: &Workspace, uri: &Url, offset: usize) -> Option<Hover> {
    let index = Index::new(ws, uri, offset, true);
    let entry = index.target(uri, offset, true)?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: format!("{} (LaTeX kernel hook)", entry.name),
        }),
        range: Some(Index::location(ws, uri, entry).range),
    })
}

pub(crate) fn completion(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<CompletionItem>> {
    let index = Index::new(ws, uri, offset, false);
    let target = index.target(uri, offset, false)?;
    if target.declaration {
        return Some(vec![]);
    }
    let doc = ws.documents.get(uri)?;
    let prefix = &doc.text.source[target.span.start..offset];
    let names: BTreeSet<_> = index
        .entries(&ws.symbol_scope(uri))
        .filter(|(_, entry)| entry.declaration && entry.name.starts_with(prefix))
        .map(|(_, entry)| entry.name.clone())
        .collect();
    Some(
        names
            .into_iter()
            .take(256)
            .take(ws.completion_budget())
            .map(|name| CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::EVENT),
                detail: Some("LaTeX kernel hook".into()),
                filter_text: Some(name.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: Index::location(ws, uri, target).range,
                    new_text: name,
                })),
                ..Default::default()
            })
            .collect(),
    )
}
