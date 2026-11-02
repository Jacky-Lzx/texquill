//! Prepared command candidates. No cached item owns a cursor-dependent edit.
use super::{completion_resolve, features, math_context, signature};
use crate::{
    parser::syntax,
    project::{
        catalog::{COMMANDS, PRIMITIVES},
        workspace::{Document, DocumentKind, Workspace},
    },
    symbols::{
        command_docs,
        commands::{CommandShape, CommandSignatures, Signature},
    },
};
use lsp_types::*;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    sync::{Arc, Weak},
};

const MAX_ITEMS: usize = 8192;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ORIGINS: usize = 4;
const MAX_SOURCE: usize = 32 * 1024;

struct Inputs {
    origin: Url,
    documents: Vec<(Url, Weak<Document>)>,
    epoch: u64,
    key: String,
    shapes: BTreeMap<String, Option<CommandShape>>,
    inert: BTreeSet<String>,
}

pub(crate) struct Prepared {
    inputs: Inputs,
    scope: BTreeSet<Url>,
    text: Vec<CompletionItem>,
    math: Vec<CompletionItem>,
    signatures: BTreeMap<String, Option<Signature>>,
    bytes: usize,
}

impl Inputs {
    fn matches(&self, ws: &Workspace, epoch: u64) -> bool {
        epoch == self.epoch
            && ws.index_truncated.is_empty()
            && ws
                .documents
                .values()
                .map(|doc| doc.text.source.len() as u64)
                .sum::<u64>()
                <= ws.config.indexing.max_total_bytes
            && ws
                .documents
                .get(&self.origin)
                .is_some_and(|doc| doc.text.source.len() as u64 <= ws.config.indexing.max_file_size)
            && self.documents.len() == ws.documents.len()
            && self.documents.iter().all(|(uri, revision)| {
                ws.documents
                    .get(uri)
                    .is_some_and(|doc| uri == &self.origin || std::ptr::eq(revision.as_ptr(), doc))
            })
            && ws
                .documents
                .get(&self.origin)
                .and_then(|doc| canonical(doc, &self.shapes, &self.inert))
                .as_ref()
                == Some(&self.key)
    }
}

impl Prepared {
    pub(crate) fn valid_for(&self, ws: &Workspace, epoch: u64) -> bool {
        self.inputs.matches(ws, epoch)
    }
    pub(crate) fn build(ws: &Workspace, origin: &Url, epoch: u64) -> Option<Self> {
        let doc = ws.documents.get(origin)?;
        if doc.kind != DocumentKind::Latex
            || doc.text.source.len() > MAX_SOURCE
            || !ws.index_truncated.is_empty()
            || ws
                .documents
                .values()
                .filter(|doc| doc.kind == DocumentKind::Latex)
                .map(|doc| doc.text.source.len())
                .sum::<usize>()
                > 4 * 1024 * 1024
            || ws
                .documents
                .values()
                .map(|doc| doc.syntax.definitions.len() + doc.syntax.environments.len())
                .sum::<usize>()
                > 4096
        {
            return None;
        }
        let scope = ws.symbol_scope(origin);
        let signatures = CommandSignatures::new(
            scope
                .iter()
                .filter_map(|uri| ws.documents.get(uri))
                .filter(|doc| doc.kind == DocumentKind::Latex)
                .map(|doc| (doc.text.source.as_str(), &doc.syntax)),
        );
        let grammar_bytes = signatures.cache_bytes().saturating_mul(2);
        if grammar_bytes >= MAX_ENTRY_BYTES {
            return None;
        }
        let shapes = signatures.completion_shapes();
        let declared: BTreeSet<_> = ws
            .documents
            .values()
            .flat_map(|doc| {
                doc.syntax
                    .definitions
                    .iter()
                    .chain(&doc.syntax.environments)
            })
            .map(|entry| entry.name.as_str())
            .collect();
        let literal = crate::symbols::commands::possible_builtin_aliases(
            scope
                .iter()
                .filter_map(|uri| ws.documents.get(uri))
                .filter(|doc| doc.kind == DocumentKind::Latex)
                .map(|doc| (doc.text.source.as_str(), &doc.syntax)),
            [
                "verb",
                "Verb",
                "EscVerb",
                "lstinline",
                "mintinline",
                "mint",
                "SaveVerb",
                "SaveGVerb",
            ],
            true,
        )?;
        for command in &doc.syntax.commands {
            if literal.contains_key(&command.name) {
                return None;
            }
            if command.name == "begin" {
                if let Some(argument) =
                    syntax::environment_name_argument(&doc.text.source, command.span.end)
                {
                    if !crate::project::catalog::ENVIRONMENTS.contains(&argument.value.as_str())
                        || syntax::is_verbatim_environment(&argument.value)
                        || declared.contains(argument.value.as_str())
                    {
                        return None;
                    }
                }
            }
        }
        let names: BTreeSet<_> = scope
            .iter()
            .filter_map(|uri| ws.documents.get(uri))
            .flat_map(|doc| {
                doc.syntax
                    .definitions
                    .iter()
                    .chain(&doc.syntax.environments)
            })
            .map(|entry| entry.name.as_str())
            .chain(COMMANDS.iter().map(|entry| entry.0))
            .collect();
        let mut inert = BTreeSet::from([String::new()]);
        for name in names {
            if !name.is_ascii() {
                continue;
            }
            for end in 1..=name.len() {
                let prefix = &name[..end];
                if !prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphabetic() || byte == b'@')
                    || declared.contains(prefix)
                    || PRIMITIVES.contains(&prefix)
                    || prefix.starts_with("if")
                    || matches!(
                        prefix,
                        "makeatletter" | "makeatother" | "ExplSyntaxOn" | "ExplSyntaxOff"
                    )
                {
                    continue;
                }
                let exact = COMMANDS.iter().find(|entry| entry.0 == prefix);
                let unknown_prefix =
                    exact.is_none_or(|entry| entry.1 == prefix) && !shapes.contains_key(prefix);
                let argument_free_builtin = exact.is_some_and(|entry| entry.1 == prefix)
                    && shapes
                        .get(prefix)
                        .and_then(Option::as_ref)
                        .is_some_and(CommandShape::takes_no_arguments);
                if unknown_prefix || argument_free_builtin {
                    inert.insert(prefix.to_owned());
                }
            }
        }
        let key = canonical(doc, &shapes, &inert)?;
        let mut templates = ws.clone();
        templates.client.snippets = true;
        let text = features::command_items(
            &templates,
            origin,
            &scope,
            "",
            "",
            lsp_types::Range::default(),
            false,
            true,
            MAX_ITEMS,
        );
        let math = features::command_items(
            &templates,
            origin,
            &scope,
            "",
            "",
            lsp_types::Range::default(),
            true,
            true,
            MAX_ITEMS,
        );
        if text.len() == MAX_ITEMS || math.len() == MAX_ITEMS {
            return None;
        }
        let lookup = signature::CompletionSignatures::new(ws, &scope);
        let signatures = text
            .iter()
            .chain(&math)
            .map(|item| item.label.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|name| {
                let signature = lookup.resolved(&name);
                (name, signature)
            })
            .collect();
        let bytes = serde_json::to_vec(&text).ok()?.len()
            + serde_json::to_vec(&math).ok()?.len()
            + key.len()
            + signatures_bytes(&signatures)
            + grammar_bytes
            + ws.documents
                .iter()
                .map(|(uri, _)| uri.as_str().len() + std::mem::size_of::<Document>() + 64)
                .sum::<usize>()
            + 2 * doc.text.source.len();
        if bytes > MAX_ENTRY_BYTES {
            return None;
        }
        Some(Self {
            inputs: Inputs {
                origin: origin.clone(),
                documents: ws.documents.weak_revisions(),
                epoch,
                key,
                shapes,
                inert,
            },
            scope,
            text,
            math,
            signatures,
            bytes,
        })
    }

    fn complete(&self, ws: &Workspace, position: Position) -> Option<Vec<CompletionItem>> {
        let doc = ws.documents.get(&self.inputs.origin)?;
        let offset = doc.text.offset(position)?;
        canonical_context(doc, &self.inputs.shapes, &self.inputs.inert, Some(offset))?;
        if doc
            .syntax
            .comments
            .iter()
            .any(|span| span.contains(&offset.saturating_sub(1)))
        {
            return Some(vec![]);
        }
        let start = doc.text.source[..offset].rfind('\n').map_or(0, |at| at + 1);
        let line_end = doc.text.source[offset..]
            .find('\n')
            .map_or(doc.text.source.len(), |at| offset + at);
        let line = &doc.text.source[start..line_end];
        let slash = start + line.len() - line.trim_start_matches([' ', '\t']).len();
        if doc.text.source.as_bytes().get(slash) != Some(&b'\\') || offset <= slash {
            return None;
        }
        let end = slash
            + 1
            + doc.text.source[slash + 1..line_end]
                .bytes()
                .take_while(|b| b.is_ascii_alphabetic() || *b == b'@')
                .count();
        if offset > end
            || !doc.text.source[end..line_end].trim().is_empty()
            || doc.syntax.is_ignored(slash)
            || doc.syntax.control_operands.contains(&slash)
            || crate::project::resource_context::grouped(&doc.text.source, &doc.syntax, slash)
        {
            return None;
        }
        let name = &doc.text.source[slash + 1..end];
        if !self.inputs.inert.contains(name) {
            return None;
        }
        let prefix = &doc.text.source[slash + 1..offset];
        let following = &doc.text.source[end..];
        let use_snippets = ws.client.snippets
            && !doc.text.source[syntax::skip_space(&doc.text.source, end)..]
                .starts_with(['{', '[', '*']);
        let math = math_context::is_math(&doc.text.source, &doc.syntax, slash);
        let fingerprint = ws
            .client
            .completion_resolve_documentation
            .then(|| completion_resolve::fingerprint(ws, &self.scope));
        let mut result = Vec::new();
        for template in (if math { &self.math } else { &self.text })
            .iter()
            .filter(|item| item.label.starts_with(prefix))
            .take(ws.completion_budget())
        {
            let mut item = template.clone();
            let existing = self
                .signatures
                .get(&item.label)
                .and_then(Option::as_ref)
                .is_some_and(|signature| signature::existing_arguments(signature, following));
            // Inline verbatim readers have extra delimiter rules. Keep their
            // established provider until that context has its own templates.
            if item.label == "lstinline"
                || crate::parser::fancyvrb::is_inline(&item.label)
                || crate::parser::fancyvrb::is_use(&item.label)
            {
                return None;
            }
            let CompletionTextEdit::Edit(edit) = item.text_edit.as_mut()? else {
                return None;
            };
            edit.range = doc.text.range(slash + 1, end);
            if !use_snippets || existing {
                edit.new_text = item.label.clone();
                item.insert_text_format = Some(InsertTextFormat::PLAIN_TEXT);
            }
            if let (Some(data), Some(fingerprint)) = (&mut item.data, &fingerprint) {
                data["fingerprint"] = fingerprint.clone().into();
            }
            result.push(item);
        }
        Some(result)
    }
}

fn signatures_bytes(signatures: &BTreeMap<String, Option<Signature>>) -> usize {
    signatures
        .iter()
        .map(|(name, signature)| {
            name.len()
                + 128
                + signature.as_ref().map_or(0, |signature| {
                    signature.documentation.len()
                        + signature
                            .parameters
                            .iter()
                            .map(|p| p.label.len() + 128)
                            .sum::<usize>()
                })
        })
        .sum()
}

/// Only whole, ungrouped prose/comment lines and inert command spellings may
/// vary. Parameter readers, declarations, literal regions and doc comments are
/// retained verbatim. Incomplete confirmed readers protect the remaining source.
fn canonical(
    doc: &Document,
    shapes: &BTreeMap<String, Option<CommandShape>>,
    inert: &BTreeSet<String>,
) -> Option<String> {
    canonical_context(doc, shapes, inert, None)
}

fn canonical_context(
    doc: &Document,
    shapes: &BTreeMap<String, Option<CommandShape>>,
    inert: &BTreeSet<String>,
    cursor: Option<usize>,
) -> Option<String> {
    if doc.kind != DocumentKind::Latex
        || doc.text.source.len() > MAX_SOURCE
        || doc.text.source.lines().count() > 512
        || doc
            .text
            .source
            .len()
            .saturating_mul(doc.text.source.lines().count())
            > 4 * 1024 * 1024
    {
        return None;
    }
    let source = &doc.text.source;
    let parsed = syntax::Syntax::latex(&doc.text);
    let mut protected: Vec<Range<usize>> = Vec::new();
    for command in &parsed.commands {
        if parsed.control_operands.contains(&command.span.start) {
            continue;
        }
        if let Some(shape) = shapes.get(&command.name) {
            let end = match shape {
                Some(shape) => shape
                    .parameter_spans(source, &parsed, command.span.clone(), false)
                    .map_or(source.len(), |read| read.end),
                // Ambiguous meanings retain the parser's indexed argument.
                // There is no confirmed parameter reader beyond that group.
                None => command.argument.as_ref().map_or_else(
                    || {
                        let next = syntax::skip_space(source, command.span.end);
                        if source
                            .as_bytes()
                            .get(next)
                            .is_some_and(|byte| !matches!(byte, b'\\' | b'}' | b']' | b'$'))
                        {
                            source[next..]
                                .find('\n')
                                .map_or(source.len(), |end| next + end)
                        } else {
                            command.span.end
                        }
                    },
                    |argument| argument.outer_span.end,
                ),
            };
            if end > command.span.end {
                protected.push(command.span.end..end);
            }
        } else if PRIMITIVES.contains(&command.name.as_str())
            && !crate::project::resource_context::grouped(source, &parsed, command.span.start)
        {
            // Native token/expression readers not described by the shared
            // parameter grammar cannot certify a nearby text edit as inert.
            return None;
        }
    }
    let mut result = String::new();
    let mut start = 0;
    for line in source.split_inclusive('\n') {
        let end = start + line.len();
        let value = line.trim();
        let grouped = crate::project::resource_context::grouped(source, &parsed, start);
        let blocked = grouped
            || protected
                .iter()
                .any(|span| span.start < end && start < span.end)
            || parsed.ignored.iter().any(|span| {
                span.start < end && start < span.end && !parsed.comments.contains(span)
            });
        let magic = value
            .strip_prefix('%')
            .is_some_and(|value| value.trim_start().starts_with(['!', '&']));
        let comment = !magic
            && value.starts_with('%')
            && parsed
                .comments
                .iter()
                .any(|span| start <= span.start && span.start < end);
        let prose = !value.contains(['\\', '{', '}', '$', '#', '&', '^', '~', '%']);
        let command = value
            .strip_prefix('\\')
            .is_some_and(|name| inert.contains(name));
        let retained = blocked || !(comment || prose || command);
        if retained
            && cursor.is_some_and(|offset| {
                start <= offset && (offset < end || offset == end && !line.ends_with('\n'))
            })
        {
            return None;
        }
        if retained {
            result.push_str(line);
        }
        start = end;
    }
    let mut raw = doc.clone();
    raw.syntax = parsed;
    for entry in raw
        .syntax
        .definitions
        .iter()
        .chain(&raw.syntax.environments)
    {
        if let Some(notes) = command_docs::comments(&raw, entry) {
            result.push_str(&notes);
        }
    }
    Some(result)
}

#[derive(Default)]
pub(crate) struct Cache {
    entries: VecDeque<Arc<Prepared>>,
}
impl Cache {
    pub(crate) fn covers(&self, ws: &Workspace, epoch: u64, uri: &Url) -> bool {
        self.entries
            .iter()
            .any(|entry| &entry.inputs.origin == uri && entry.inputs.matches(ws, epoch))
    }
    pub(crate) fn complete(
        &self,
        ws: &Workspace,
        epoch: u64,
        uri: &Url,
        position: Position,
    ) -> Option<Vec<CompletionItem>> {
        self.entries
            .iter()
            .rev()
            .find(|entry| &entry.inputs.origin == uri && entry.inputs.matches(ws, epoch))?
            .complete(ws, position)
    }
    pub(crate) fn insert(&mut self, prepared: Prepared) {
        self.entries
            .retain(|entry| entry.inputs.origin != prepared.inputs.origin);
        self.entries.push_back(Arc::new(prepared));
        while self.entries.len() > MAX_ORIGINS
            || self.entries.iter().map(|entry| entry.bytes).sum::<usize>() > MAX_BYTES
        {
            self.entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Workspace, Url, String) {
        let uri = Url::parse("untitled:prepared.tex").unwrap();
        let source = "\\documentclass{article}\n\\newcommand{\\projectmacro}[2]{#1#2}\n\\begin{document}\nReady\n\n\\end{document}\n".to_owned();
        let mut ws = Workspace::default();
        ws.client.snippets = true;
        ws.client.completion_resolve_documentation = true;
        ws.documents
            .insert(uri.clone(), Document::new(&uri, source.clone(), Some(1)));
        (ws, uri, source)
    }
    #[test]
    fn prepared_candidates_match_live_results_after_prose_and_prefix_edits() {
        let (mut ws, uri, source) = fixture();
        let mut cache = Cache::default();
        cache.insert(Prepared::build(&ws, &uri, 0).expect("eligible origin did not prepare"));
        for (index, prefix) in ["\\", "\\i", "\\in", "\\inp", "\\proj", "\\al"]
            .into_iter()
            .enumerate()
        {
            let current = source.replace(
                "Ready\n",
                &format!("Changed 😀 prose\n% typing more\n{prefix}\n"),
            );
            ws.documents.insert(
                uri.clone(),
                Document::new(&uri, current.clone(), Some(index as i32 + 2)),
            );
            let offset = current.find(&format!("{prefix}\n")).unwrap() + prefix.len();
            let position = ws.documents[&uri].text.position(offset);
            let items = cache
                .complete(&ws, 0, &uri, position)
                .unwrap_or_else(|| panic!("prefix {prefix:?} missed prepared candidates"));
            assert_eq!(
                serde_json::to_value(items).unwrap(),
                serde_json::to_value(features::completion(&ws, &uri, position)).unwrap(),
                "{prefix}"
            );
        }
    }
    #[test]
    fn structural_edits_metadata_changes_and_argument_contexts_invalidate_preparation() {
        let (mut ws, uri, source) = fixture();
        let mut cache = Cache::default();
        cache.insert(Prepared::build(&ws, &uri, 0).unwrap());
        for changed in [
            source.replace("[2]", "[3]"),
            source.replace("Ready", r"\input{new}"),
            source.replace("Ready", r"\let\input\projectmacro"),
            source.replace("Ready", r"\iftrue Text\fi"),
            source.replace("Ready", r"\section{\inp}"),
            source.replace("Ready", r"\verb|\inp|"),
            source.replace("Ready", r"\input{\inp}"),
            source.replace("Ready", "% !TeX root = other.tex"),
        ] {
            ws.documents
                .insert(uri.clone(), Document::new(&uri, changed, Some(2)));
            assert!(!cache.covers(&ws, 0, &uri));
        }
        ws.documents
            .insert(uri.clone(), Document::new(&uri, source, Some(2)));
        assert!(cache.covers(&ws, 0, &uri));
        assert!(!cache.covers(&ws, 1, &uri));
        let other = Url::parse("untitled:new.tex").unwrap();
        ws.documents.insert(
            other.clone(),
            Document::new(&other, r"\newcommand{\added}{}".into(), Some(1)),
        );
        assert!(!cache.covers(&ws, 0, &uri));
    }

    #[test]
    fn math_modes_snippet_negotiation_and_resolve_keep_the_original_contract() {
        for snippets in [false, true] {
            for resolve in [false, true] {
                for math in [false, true] {
                    let (mut ws, uri, source) = fixture();
                    ws.client.snippets = snippets;
                    ws.client.completion_resolve_documentation = resolve;
                    let source = if math {
                        source.replace("Ready", "$\nReady\n$")
                    } else {
                        source
                    };
                    ws.documents
                        .insert(uri.clone(), Document::new(&uri, source.clone(), Some(1)));
                    let mut cache = Cache::default();
                    cache.insert(Prepared::build(&ws, &uri, 0).unwrap());
                    for prefix in ["\\proj", "\\al"] {
                        let current =
                            source.replace("Ready", &format!("% same declarations\n{prefix}"));
                        ws.documents
                            .insert(uri.clone(), Document::new(&uri, current.clone(), Some(2)));
                        let position = ws.documents[&uri]
                            .text
                            .position(current.find(&format!("{prefix}\n")).unwrap() + prefix.len());
                        let items = cache.complete(&ws, 0, &uri, position).unwrap_or_else(|| panic!("math={math} snippets={snippets} resolve={resolve} prefix={prefix} covers={}",cache.covers(&ws,0,&uri)));
                        assert_eq!(
                            serde_json::to_value(&items).unwrap(),
                            serde_json::to_value(features::completion(&ws, &uri, position))
                                .unwrap()
                        );
                        if resolve && prefix == "\\proj" {
                            let item = items
                                .iter()
                                .find(|item| item.label == "projectmacro")
                                .unwrap()
                                .clone();
                            let edit = item.text_edit.clone();
                            let resolved = completion_resolve::resolve(&ws, item).unwrap();
                            assert_eq!(resolved.text_edit, edit);
                            assert!(resolved.documentation.is_some());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn documentation_comments_and_ungrouped_parameter_data_are_not_prose() {
        let (mut ws, uri, source) = fixture();
        for source in [
            source.replace("\\newcommand", "% macro documentation\n\\newcommand"),
            source.replace("Ready", "\\projectmacro\nReady\nnext"),
            source.replace(
                "Ready",
                "\\def\\delimited#1stop{#1}\n\\delimited\nReady\nstop",
            ),
        ] {
            ws.documents
                .insert(uri.clone(), Document::new(&uri, source.clone(), Some(1)));
            let mut cache = Cache::default();
            let prepared = Prepared::build(&ws, &uri, 0);
            if source.contains("\\def\\delimited") {
                assert!(
                    prepared.is_none(),
                    "native definition headers require full analysis"
                );
                continue;
            }
            cache.insert(prepared.unwrap());
            let changed = if source.contains("macro documentation") {
                source.replace("macro documentation", "changed documentation")
            } else {
                source.replace("Ready", "changed")
            };
            ws.documents
                .insert(uri.clone(), Document::new(&uri, changed, Some(2)));
            assert!(!cache.covers(&ws, 0, &uri));
        }
    }

    #[test]
    fn literal_aliases_and_declared_environments_use_contextual_completion() {
        let (mut ws, uri, source) = fixture();
        for source in [
            source.replace(
                "Ready",
                "\\newcommand{\\inline}{\\Verb}\n\\inline|literal text|",
            ),
            source.replace(
                "Ready",
                "\\newenvironment{custom}{}{}\n\\begin{custom}\n\\inp\n\\end{custom}",
            ),
        ] {
            ws.documents
                .insert(uri.clone(), Document::new(&uri, source, Some(1)));
            assert!(Prepared::build(&ws, &uri, 0).is_none());
        }
    }

    #[test]
    fn ordinary_comments_can_return_an_empty_ready_list() {
        let (mut ws, uri, source) = fixture();
        let mut cache = Cache::default();
        cache.insert(Prepared::build(&ws, &uri, 0).unwrap());
        let current = source.replace("Ready", "% editing comments");
        let position =
            crate::text::Text::new(current.clone()).position(current.find("comments").unwrap() + 3);
        ws.documents
            .insert(uri.clone(), Document::new(&uri, current, Some(2)));
        let items = cache.complete(&ws, 0, &uri, position).unwrap();
        assert!(items.is_empty());
        assert_eq!(items, features::completion(&ws, &uri, position));
    }
}
