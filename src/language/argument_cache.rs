//! Prefix-independent reference/citation candidates with exact cursor rebasing.
use super::completion_cache::{observe_closed, Dependency};
use crate::{
    diagnostics::diagnostic_inputs::Files,
    parser::syntax,
    project::workspace::{Document, DocumentKind, Workspace},
    symbols::commands::{CommandShape, CommandSignatures},
};
use lsp_types::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::Arc,
};

const MAX_SOURCE: usize = 32 * 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;

fn literal(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_:-./".contains(&byte))
}
fn slot(doc: &Document, position: Position) -> Option<Range<usize>> {
    if doc.kind != DocumentKind::Latex || doc.text.source.len() > MAX_SOURCE {
        return None;
    }
    let offset = doc.text.offset(position)?;
    doc.syntax.commands.iter().find_map(|command| {
        if !syntax::is_single_reference(&command.name)
            && !matches!(
                command.name.as_str(),
                "cite"
                    | "citep"
                    | "citet"
                    | "citealp"
                    | "citeauthor"
                    | "citeyear"
                    | "autocite"
                    | "parencite"
                    | "textcite"
                    | "footcite"
            )
        {
            return None;
        }
        let span = command.argument.as_ref()?.span.clone();
        (span.start <= offset && offset <= span.end && literal(&doc.text.source[span.clone()]))
            .then_some(span)
    })
}
pub(crate) fn candidate(doc: &Document, position: Position) -> bool {
    slot(doc, position).is_some()
}
pub(crate) fn changed_candidate(before: &Document, after: &Document) -> bool {
    if after.text.source.len() > MAX_SOURCE || before.text.source == after.text.source {
        return false;
    }
    let at = before
        .text
        .source
        .bytes()
        .zip(after.text.source.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    after.syntax.commands.iter().any(|command| {
        command.argument.as_ref().is_some_and(|argument| {
            command.span.start <= at
                && at <= argument.outer_span.end
                && candidate(after, after.text.position(argument.span.end))
        })
    })
}

pub(crate) fn positions(doc: &Document) -> Vec<Position> {
    doc.syntax
        .commands
        .iter()
        .filter_map(|command| {
            let position = doc.text.position(command.argument.as_ref()?.span.end);
            candidate(doc, position).then_some(position)
        })
        .take(8)
        .collect()
}

pub(crate) struct Prepared {
    pub origin: Url,
    epoch: u64,
    pub revision: u64,
    before: String,
    after: String,
    dependencies: Vec<Dependency>,
    shapes: BTreeMap<String, Option<CommandShape>>,
    inert: BTreeSet<String>,
    items: Vec<CompletionItem>,
    metadata: Vec<String>,
    citation: bool,
    files: Arc<Files>,
    pub bytes: usize,
}
impl Prepared {
    pub(crate) fn build(
        ws: &Workspace,
        origin: &Url,
        position: Position,
        epoch: u64,
    ) -> Option<Self> {
        let _timing = crate::performance::Span::new("completion.arguments.prepare");
        let doc = ws.documents.get(origin)?;
        let span = slot(doc, position)?;
        if !ws.index_truncated.is_empty()
            || ws
                .documents
                .values()
                .map(|doc| doc.text.source.len())
                .sum::<usize>()
                > 4 * 1024 * 1024
        {
            return None;
        }
        let (prepared, files) = Files::capture(|| {
            crate::analysis::AnalysisContext::new().run(|_| {
            observe_closed(ws);
            let items = super::features::completion_unfiltered(ws, origin, position)?;
            if items.len() >= ws.completion_budget() || items.iter().any(|item| {
                !matches!(&item.text_edit, Some(CompletionTextEdit::Edit(edit)) if edit.range == doc.text.range(span.start, span.end) && edit.new_text == item.label)
                    || item.additional_text_edits.as_ref().is_some_and(|edits| !edits.is_empty())
            }) { return None; }
            let scope = ws.symbol_scope(origin);
            let signatures = CommandSignatures::new(
                scope
                    .iter()
                    .filter_map(|uri| ws.documents.get(uri))
                    .filter(|doc| doc.kind == DocumentKind::Latex)
                    .map(|doc| (doc.text.source.as_str(), &doc.syntax)),
            );
            let command = doc.syntax.commands.iter().find(|command| {
                command
                    .argument
                    .as_ref()
                    .is_some_and(|argument| argument.span == span)
            })?;
            if !signatures.command_unchanged(&command.name) {
                return None;
            }
            let citation = syntax::is_citation(&command.name);
            let prefix = &doc.text.source[span.start..doc.text.offset(position)?];
            let metadata: Vec<_> = items
                .iter()
                .map(|item| {
                    if citation {
                        item.filter_text
                            .as_ref()?
                            .strip_prefix(&format!("{prefix} {} ", item.label))
                            .map(str::to_owned)
                    } else {
                        Some(String::new())
                    }
                })
                .collect::<Option<_>>()?;
            let shapes = signatures.completion_shapes();
            let inert = BTreeSet::new();
            let dependencies = Dependency::capture(ws, &shapes, &inert);
            let before = doc.text.source[..span.start].to_owned();
            let after = doc.text.source[span.end..].to_owned();
            let bytes = metadata.iter().map(String::capacity).sum::<usize>()
                + before.len()
                + after.len()
                + serde_json::to_vec(&items).ok()?.len()
                + dependencies.iter().map(Dependency::bytes).sum::<usize>()
                + shapes
                    .iter()
                    .map(|(name, shape)| {
                        name.len() + 128 + shape.as_ref().map_or(0, CommandShape::cache_bytes)
                    })
                    .sum::<usize>();
            Some(Self {
                origin: origin.clone(),
                epoch,
                revision: crate::analysis::AnalysisContext::current()
                    .and_then(|ctx| ctx.binding.as_ref().map(|binding| binding.revision))
                    .unwrap_or(0),
                before,
                after,
                dependencies,
                shapes,
                inert,
                items,
                metadata,
                citation,
                files: Arc::default(),
                bytes,
            })
        })
        });
        let mut prepared = prepared?;
        prepared.bytes += files.bytes;
        prepared.files = files;
        (prepared.bytes <= MAX_BYTES && prepared.valid_for(ws, epoch)).then_some(prepared)
    }
    pub(crate) fn same_slot(&self, other: &Self) -> bool {
        self.origin == other.origin && self.before == other.before && self.after == other.after
    }
    pub(crate) fn range(&self, ws: &Workspace) -> Option<Range<usize>> {
        let doc = ws.documents.get(&self.origin)?;
        let source = &doc.text.source;
        if doc.kind != DocumentKind::Latex
            || source.len() > MAX_SOURCE
            || source.len() < self.before.len() + self.after.len()
            || !source.starts_with(&self.before)
            || !source.ends_with(&self.after)
        {
            return None;
        }
        let range = self.before.len()..source.len() - self.after.len();
        literal(&source[range.clone()]).then_some(range)
    }
    pub(crate) fn valid_for(&self, ws: &Workspace, epoch: u64) -> bool {
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
            && ws.documents.len() == self.dependencies.len()
            && self.range(ws).is_some()
            && self
                .dependencies
                .iter()
                .all(|dependency| dependency.matches(ws, &self.origin, &self.shapes, &self.inert))
            && self.files.current_for_editor(ws)
    }
    pub(crate) fn complete(
        &self,
        ws: &Workspace,
        epoch: u64,
        position: Position,
    ) -> Option<Vec<CompletionItem>> {
        if !self.valid_for(ws, epoch) {
            return None;
        }
        let doc = &ws.documents[&self.origin];
        let range = self.range(ws)?;
        let offset = doc.text.offset(position)?;
        if offset < range.start || offset > range.end {
            return None;
        }
        let prefix = &doc.text.source[range.start..offset];
        if self.citation && prefix.chars().count() > 128 {
            return Some(vec![]);
        }
        let mut ranked: Vec<_> = self
            .items
            .iter()
            .zip(&self.metadata)
            .filter_map(|(item, metadata)| {
                let rank = if self.citation {
                    crate::bibliography::citation_search::score(&item.label, metadata, prefix)?
                } else if item.label.starts_with(prefix) {
                    0
                } else {
                    return None;
                };
                Some((rank, item, metadata))
            })
            .collect();
        if self.citation {
            ranked.sort_by(|a, b| (a.0, &a.1.label).cmp(&(b.0, &b.1.label)));
        }
        Some(
            ranked
                .into_iter()
                .take(ws.completion_budget())
                .map(|(rank, item, metadata)| {
                    let mut item = item.clone();
                    if let Some(CompletionTextEdit::Edit(edit)) = &mut item.text_edit {
                        edit.range = doc.text.range(range.start, range.end);
                    }
                    if self.citation {
                        item.filter_text = Some(format!("{prefix} {} {metadata}", item.label));
                        item.sort_text = Some(format!("{rank:02}{}", item.label));
                    }
                    item
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(
        command: &str,
        value: &str,
    ) -> (tempfile::TempDir, Workspace, Url, String, Position) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("refs.bib"),
            "@book{paper2026,title={One}}\n@article{paper2027,title={Two}}\n",
        )
        .unwrap();
        let uri = Url::from_file_path(root.join("main.tex")).unwrap();
        let source = format!("\\documentclass{{article}}\n\\begin{{document}}\n\\section{{Title}}\\label{{sec:one}}\n\\label{{sec:two}}\n\\bibliography{{refs}}\n😀 \\{command}{{{value}}}\n\\end{{document}}\n");
        std::fs::write(uri.to_file_path().unwrap(), &source).unwrap();
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.open(uri.clone(), source.clone(), 1);
        let at = source.rfind(&format!("{{{value}}}")).unwrap() + 1 + value.len();
        let position = ws.documents[&uri].text.position(at);
        (dir, ws, uri, source, position)
    }
    #[test]
    fn reference_and_citation_lists_filter_and_rebase_after_typing_backspace_and_mid_key_moves() {
        for (command, prefix, values) in [
            ("ref", "s", vec!["se", "sec:", "", "sec:two"]),
            (
                "cite",
                "p",
                vec!["paper", "", "paper2027", "One", "PAPER", "ppr", "2027"],
            ),
        ] {
            let (_dir, mut ws, uri, source, position) = fixture(command, prefix);
            let prepared = Prepared::build(&ws, &uri, position, 3).unwrap_or_else(|| {
                panic!(
                    "{command}: slot={:?}, items={:?}",
                    slot(&ws.documents[&uri], position),
                    super::super::features::completion_unfiltered(&ws, &uri, position)
                )
            });
            assert!(!prepared.items.is_empty());
            for (version, value) in values.into_iter().enumerate() {
                let current = source.replace(
                    &format!("\\{command}{{{prefix}}}"),
                    &format!("\\{command}{{{value}}}"),
                );
                ws.documents.insert(
                    uri.clone(),
                    Document::new(&uri, current.clone(), Some(version as i32 + 2)),
                );
                let range = prepared.range(&ws).unwrap();
                for offset in range.start..=range.end {
                    let position = ws.documents[&uri].text.position(offset);
                    assert_eq!(
                        serde_json::to_value(prepared.complete(&ws, 3, position).unwrap()).unwrap(),
                        serde_json::to_value(super::super::features::completion(
                            &ws, &uri, position
                        ))
                        .unwrap(),
                        "{command}: {value}, offset {offset}"
                    );
                }
            }
            assert!(!prepared.valid_for(&ws, 4));
        }
    }
    #[test]
    fn changed_labels_bibliography_and_context_reject_the_old_candidate_list() {
        for command in ["ref", "cite"] {
            let (dir, mut ws, uri, source, position) = fixture(command, "");
            let prepared = Prepared::build(&ws, &uri, position, 0).unwrap_or_else(|| {
                panic!(
                    "empty {command}: slot={:?}, items={:?}",
                    slot(&ws.documents[&uri], position),
                    super::super::features::completion_unfiltered(&ws, &uri, position)
                )
            });
            let current = source.replace("sec:one", "sec:changed");
            ws.documents
                .insert(uri.clone(), Document::new(&uri, current, Some(2)));
            assert!(!prepared.valid_for(&ws, 0));
            ws.documents
                .insert(uri.clone(), Document::new(&uri, source, Some(3)));
            assert!(prepared.valid_for(&ws, 0));
            std::fs::write(dir.path().join("refs.bib"), "@book{new,title={Changed}}").unwrap();
            assert!(!prepared.valid_for(&ws, 0));
        }
    }
    #[test]
    fn ordinary_other_buffer_prose_is_reusable_but_macro_arguments_and_definitions_are_not() {
        let (_dir, mut ws, uri, main_source, _) = fixture("ref", "s");
        let other = uri.join("other.tex").unwrap();
        let source = "\\newcommand{\\reader}[1]{#1}\nBody\n\\reader\nargument\n";
        ws.documents
            .insert(other.clone(), Document::new(&other, source.into(), Some(1)));
        std::fs::write(other.to_file_path().unwrap(), source).unwrap();
        let main_source =
            main_source.replace("\\begin{document}", "\\input{other}\n\\begin{document}");
        ws.documents.insert(
            uri.clone(),
            Document::new(&uri, main_source.clone(), Some(2)),
        );
        ws.load_dependencies(&uri, 0);
        let position = ws.documents[&uri]
            .text
            .position(main_source.rfind("{s}").unwrap() + 2);
        let prepared = Prepared::build(&ws, &uri, position, 0).unwrap();
        let original = ws.documents[&other].clone();
        ws.documents.insert(
            other.clone(),
            original.with_text(
                &other,
                source.replace("Body", "Writing more prose"),
                Some(2),
            ),
        );
        assert!(prepared.valid_for(&ws, 0));
        ws.documents.insert(
            other.clone(),
            original.with_text(
                &other,
                source.replace("argument", "changed argument"),
                Some(3),
            ),
        );
        assert!(!prepared.valid_for(&ws, 0));
        ws.documents.insert(
            other.clone(),
            original.with_text(&other, source.replace("[1]", "[2]"), Some(4)),
        );
        assert!(!prepared.valid_for(&ws, 0));
    }
    #[test]
    fn redefined_key_readers_and_new_project_configuration_use_the_semantic_provider() {
        let (dir, mut ws, uri, source, position) = fixture("ref", "s");
        let prepared = Prepared::build(&ws, &uri, position, 0).unwrap();
        std::fs::write(
            dir.path().join(".texquill.json"),
            "{\"build\":{\"rootFile\":\"other.tex\"}}",
        )
        .unwrap();
        assert!(!prepared.valid_for(&ws, 0));
        std::fs::remove_file(dir.path().join(".texquill.json")).unwrap();
        let source = source.replace(
            "\\begin{document}",
            "\\def\\ref#1{\\input{#1}}\n\\begin{document}",
        );
        ws.documents
            .insert(uri.clone(), Document::new(&uri, source.clone(), Some(2)));
        let position = ws.documents[&uri]
            .text
            .position(source.rfind("{s}").unwrap() + 2);
        assert!(Prepared::build(&ws, &uri, position, 0).is_none());
    }

    #[test]
    fn complex_key_lists_comments_and_low_candidate_budgets_use_the_semantic_provider() {
        let (_dir, mut ws, uri, _, position) = fixture("cite", "paper2026,p");
        assert!(Prepared::build(&ws, &uri, position, 0).is_none());
        let (_dir, ws2, uri2, _, position2) = fixture("ref", "s% comment");
        assert!(Prepared::build(&ws2, &uri2, position2, 0).is_none());
        ws.config.indexing.completion_limit = 1;
        let (_dir, mut limited, uri, _, position) = fixture("ref", "");
        limited.config.indexing.completion_limit = 1;
        assert!(Prepared::build(&limited, &uri, position, 0).is_none());
    }
}
