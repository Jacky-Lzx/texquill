//! Literal code labels in registrations, default settings and ordering rules.
use super::hook_fields::Candidate;
use crate::{
    parser::syntax,
    project::workspace::{Document, Workspace},
    symbols::commands::CommandSignatures,
};
use lsp_types::*;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    ops::Range as Span,
};

const COMMANDS: &[&str] = &[
    "AddToHook",
    "AddToHookWithArguments",
    "AtBeginDocument",
    "AtEndDocument",
    "RemoveFromHook",
    "DeclareHookRule",
    "DeclareDefaultHookRule",
    "ClearHookRule",
    "PushDefaultHookLabel",
    "SetDefaultHookLabel",
];

fn literal(value: &str) -> bool {
    syntax::is_static_key(value) && value != "." && !value.starts_with("./")
}

fn label_name(value: &str) -> bool {
    literal(value) && !matches!(value, "*" | "top-level")
}

fn trimmed(source: &str, mut span: Span<usize>) -> Span<usize> {
    let raw = &source[span.clone()];
    span.start += raw.len() - raw.trim_start_matches([' ', '\t', '\r', '\n']).len();
    span.end = span.start
        + source[span.clone()]
            .trim_end_matches([' ', '\t', '\r', '\n'])
            .len();
    span
}

fn normalized(source: &str, span: Span<usize>) -> Span<usize> {
    let mut pieces = syntax::name_literal_pieces(source, span.clone())
        .into_iter()
        .map(|piece| trimmed(source, piece))
        .filter(|piece| !piece.is_empty());
    match (pieces.next(), pieces.next()) {
        (Some(piece), None) => piece,
        _ => trimmed(source, span),
    }
}

fn optional(source: &str, after: usize) -> Option<(Span<usize>, usize, bool)> {
    let at = syntax::skip_space(source, after);
    if source.as_bytes().get(at) != Some(&b'[') {
        return None;
    }
    Some(syntax::optional_note_end(source, at).map_or_else(
        || {
            let end = source[at + 1..]
                .find(['\r', '\n'])
                .map_or(source.len(), |len| at + 1 + len);
            (at + 1..end, source.len(), false)
        },
        |end| {
            let argument = syntax::single_key_argument(
                source,
                syntax::Argument {
                    value: source[at + 1..end - 1].into(),
                    span: at + 1..end - 1,
                    outer_span: at..end,
                },
            );
            (argument.span, end, true)
        },
    ))
}

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    hook: Option<String>,
    default_setting: bool,
    // Only registration/removal fields address the kernel's separate
    // document-level slot; rules and default settings use ordinary labels.
    allows_document_slot: bool,
    relative_base: Option<String>,
    name: String,
    span: Span<usize>,
    declaration: bool,
    complete: bool,
    grouped: bool,
}

impl Entry {
    fn valid_label(&self) -> bool {
        label_name(&self.name) || self.allows_document_slot && self.name == "top-level"
    }

    fn contains(&self, at: usize, complete: bool) -> bool {
        (!complete || self.complete && self.valid_label())
            && self.span.start <= at
            && at <= self.span.end
    }
}

fn calls(doc: &Document, signatures: &CommandSignatures<'_>) -> BTreeMap<usize, Candidate<Entry>> {
    let source = &doc.text.source;
    let context = super::named_references::LiteralContext::new(doc);
    let declarations: BTreeSet<_> = doc
        .syntax
        .definitions
        .iter()
        .chain(&doc.syntax.environments)
        .flat_map(|entry| [entry.span.start, entry.span.start.saturating_sub(1)])
        .collect();
    let empty = BTreeSet::new();
    let mut result = BTreeMap::new();
    let mut last_push: Option<(usize, &str, usize)> = None;
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
        let lifecycle = match name {
            "AtBeginDocument" => Some("begindocument"),
            "AtEndDocument" => Some("enddocument"),
            _ => None,
        };
        let default_setting = matches!(name, "PushDefaultHookLabel" | "SetDefaultHookLabel");
        let push_setting = name == "PushDefaultHookLabel";
        let (hook, mut after) = if let Some(hook) = lifecycle {
            // The public lifecycle wrappers invoke AddToHook at registration
            // time; their own unchanged spelling does not certify its meaning.
            if !signatures.command_unchanged("AddToHook") {
                continue;
            }
            (Some(hook), invocation.end)
        } else if name == "DeclareDefaultHookRule" || default_setting {
            (None, invocation.end)
        } else {
            let Some((hook, after, true)) = super::hook_names::field(source, invocation.end) else {
                continue;
            };
            (Some(&source[normalized(source, hook)]), after)
        };
        if hook.is_some_and(|hook| !literal(hook)) {
            continue;
        }
        // The kernel stores default rules in this reserved hook. Public
        // rule/clear calls can address the same table explicitly.
        let hook = match (name, hook) {
            ("DeclareHookRule" | "ClearHookRule", Some("??")) => None,
            _ => hook,
        };
        let declaration = lifecycle.is_some() || name.starts_with("AddToHook");
        let optional_label = declaration || name == "RemoveFromHook";
        let count = if optional_label || default_setting {
            1
        } else {
            2
        };
        for slot in 0..count {
            if slot == 1 && matches!(name, "DeclareHookRule" | "DeclareDefaultHookRule") {
                let Some((_, end, true)) = super::hook_names::field(source, after) else {
                    break;
                };
                after = end;
            }
            let grouped = optional_label
                || source.as_bytes().get(syntax::skip_space(source, after)) == Some(&b'{');
            let field = if optional_label {
                optional(source, after)
            } else {
                super::hook_names::field(source, after)
            };
            let Some((span, end, complete)) = field else {
                break;
            };
            after = end;
            let span = normalized(source, span);
            let raw = &source[span.clone()];
            // Resolve only an immediately adjacent literal Push. No intervening
            // command, closing group, file boundary or presumed execution order
            // is used to infer the default stack. Operand validation below must
            // certify both invocations in every consumer interpretation.
            let relative = last_push.filter(|(end, _, _)| {
                optional_label
                    && syntax::skip_space(source, *end) == command.span.start
                    && (raw == "." || raw.starts_with("./"))
            });
            let name = relative.map_or(Cow::Borrowed(raw), |(_, base, _)| {
                Cow::Owned(if raw == "." {
                    base.into()
                } else {
                    format!("{base}/{}", &raw[2..])
                })
            });
            if label_name(&name) || optional_label && name == "top-level" || name.is_empty() {
                if push_setting && complete && label_name(raw) {
                    last_push = Some((end, raw, command.span.start));
                }
                result.insert(
                    span.start,
                    Candidate {
                        entry: Entry {
                            hook: hook.map(str::to_owned),
                            // A setting names the default across hooks; it
                            // does not itself register code or certify the
                            // execution context of any omitted label.
                            default_setting,
                            allows_document_slot: optional_label,
                            relative_base: relative.map(|(_, base, _)| base.into()),
                            name: name.into_owned(),
                            span,
                            declaration,
                            complete,
                            grouped,
                        },
                        invocation: command.span.start,
                        prerequisite: relative.map(|(_, _, at)| at),
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
    fn new(ws: &Workspace, uri: &Url, offset: usize, complete: bool) -> Self {
        Self {
            entries: super::hook_fields::index(ws, uri, offset, COMMANDS, calls, |entry| {
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
        target: &'a Entry,
    ) -> impl Iterator<Item = (&'a Url, &'a Entry)> {
        self.entries
            .iter()
            .filter(|(uri, _)| scope.contains(*uri))
            .flat_map(|(uri, entries)| entries.values().map(move |entry| (uri, entry)))
            .filter(move |(_, entry)| {
                entry.complete
                    && entry.valid_label()
                    && (entry.name != "top-level" || target.allows_document_slot)
                    && (target.hook.is_none() || entry.hook.is_none() || entry.hook == target.hook)
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
    fn definitions(&self, ws: &Workspace, uri: &Url, target: &Entry) -> Vec<Location> {
        self.entries(&ws.symbol_scope(uri), target)
            .filter(|(_, entry)| entry.declaration && entry.name == target.name)
            .map(|(uri, entry)| Self::location(ws, uri, entry))
            .collect()
    }
}

pub(crate) fn definitions(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<Location>> {
    let index = Index::new(ws, uri, offset, true);
    let target = index.target(uri, offset, true)?;
    Some(index.definitions(ws, uri, target))
}

pub(crate) fn references(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
    include: bool,
) -> Option<Vec<Location>> {
    let index = Index::new(ws, uri, offset, true);
    let target = index.target(uri, offset, true)?;
    let mut scope = ws.symbol_scope(uri);
    let definitions: BTreeSet<_> = index
        .definitions(ws, uri, target)
        .into_iter()
        .map(|location| location.uri)
        .collect();
    for project in ws
        .symbol_regions()
        .into_values()
        .filter(|project| !project.is_disjoint(&definitions))
    {
        scope.extend(project);
    }
    Some(
        index
            .entries(&scope, target)
            .filter(|(_, entry)| entry.name == target.name && (include || !entry.declaration))
            .map(|(uri, entry)| Index::location(ws, uri, entry))
            .collect(),
    )
}

pub(crate) fn hover(ws: &Workspace, uri: &Url, offset: usize) -> Option<Hover> {
    let index = Index::new(ws, uri, offset, true);
    let target = index.target(uri, offset, true)?;
    let scope = target.hook.as_ref().map_or_else(
        || {
            if target.default_setting {
                "All hooks (default code label)"
            } else {
                "All hooks (default ordering rule)"
            }
            .into()
        },
        |hook| format!("Hook: {hook}"),
    );
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: if target.name == "top-level" {
                format!("top-level (Document-level code slot)\n\n{scope}")
            } else {
                format!("{} (LaTeX hook code label)\n\n{}", target.name, scope)
            },
        }),
        range: Some(Index::location(ws, uri, target).range),
    })
}

pub(crate) fn completion(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<CompletionItem>> {
    let index = Index::new(ws, uri, offset, false);
    let target = index.target(uri, offset, false)?;
    let doc = ws.documents.get(uri)?;
    let raw_prefix = &doc.text.source[target.span.start..offset];
    let prefix = target
        .relative_base
        .as_ref()
        .map_or(Cow::Borrowed(raw_prefix), |base| {
            Cow::Owned(
                raw_prefix
                    .strip_prefix("./")
                    .map_or_else(|| base.clone(), |suffix| format!("{base}/{suffix}")),
            )
        });
    let scope = ws.symbol_scope(uri);
    let names: BTreeSet<_> = index
        .entries(&scope, target)
        .filter(|(other_uri, entry)| {
            entry.declaration
                && entry.name.starts_with(prefix.as_ref())
                && target.relative_base.as_ref().is_none_or(|base| {
                    entry
                        .name
                        .strip_prefix(base)
                        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                })
                && (*other_uri != uri || entry.span != target.span)
        })
        .map(|(_, entry)| entry.name.clone())
        .collect();
    Some(
        names
            .into_iter()
            .take(256)
            .take(ws.completion_budget())
            .map(|name| {
                let label = target
                    .relative_base
                    .as_ref()
                    .map_or_else(|| name.clone(), |base| format!(".{}", &name[base.len()..]));
                CompletionItem {
                    label: label.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(if name == "top-level" {
                        target.hook.as_ref().map_or_else(
                            || "Document-level code slot".into(),
                            |hook| format!("Document-level code slot in hook {hook}"),
                        )
                    } else {
                        target.hook.as_ref().map_or_else(
                            || "Code label across hooks".into(),
                            |hook| format!("Code label in hook {hook}"),
                        )
                    }),
                    filter_text: Some(label.clone()),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: doc.text.range(target.span.start, target.span.end),
                        new_text: if target.grouped || label.len() == 1 {
                            label
                        } else {
                            format!("{{{label}}}")
                        },
                    })),
                    ..CompletionItem::default()
                }
            })
            .collect(),
    )
}
