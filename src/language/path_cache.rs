//! Native, unresolved input filenames may vary without changing project inputs.
//! Each certificate retains exact surrounding text and filesystem observations.
use crate::{
    diagnostics::diagnostic_inputs::Files,
    parser::syntax,
    project::workspace::{resolve_file_path, Document, DocumentKind, Workspace},
};
use lsp_types::{CompletionItem, Position, Url};
use std::{
    ops::Range,
    sync::{Arc, Weak},
};

const MAX_SOURCE: usize = 32 * 1024;
const MAX_SLOTS: usize = 8;
const MAX_BYTES: usize = 2 * 1024 * 1024;

struct Slot {
    command: usize,
    before: String,
    after: String,
}
pub(crate) struct Prepared {
    origin: Url,
    epoch: u64,
    pub(crate) revision: u64,
    documents: Vec<(Url, Weak<Document>)>,
    slots: Vec<Slot>,
    files: Arc<Files>,
}

fn literal(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.".contains(&byte))
}
fn unresolved(uri: &Url, value: &str) -> bool {
    value.is_empty() || resolve_file_path(uri, "input", value, &["tex"]).is_none()
}

/// A cheap preparation hint only; publication still requires the full certificate.
pub(crate) fn candidate(doc: &Document, position: Position) -> bool {
    if doc.kind != DocumentKind::Latex || doc.text.source.len() > MAX_SOURCE {
        return false;
    }
    let Some(offset) = doc.text.offset(position) else {
        return false;
    };
    let source = &doc.text.source[..offset];
    source
        .rsplit('\n')
        .next()
        .unwrap_or(source)
        .trim_start()
        .strip_prefix("\\input{")
        .is_some_and(literal)
}

/// Completion requests can be cancelled before dispatch during rapid input.
/// Start preparing from the changed line instead of waiting for such a request.
pub(crate) fn changed_candidate(before: &Document, after: &Document) -> bool {
    if after.text.source.len() > MAX_SOURCE || before.text.source == after.text.source {
        return false;
    }
    let source = &after.text.source;
    let mut at = before
        .text
        .source
        .bytes()
        .zip(source.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !source.is_char_boundary(at) {
        at -= 1;
    }
    let end = source[at..].find('\n').map_or(source.len(), |end| at + end);
    let line = source[..end].trim_end_matches([' ', '\t', '\r']);
    let cursor = line.strip_suffix('}').map_or(line.len(), str::len);
    candidate(after, after.text.position(cursor))
}

impl Prepared {
    pub(crate) fn build(ws: &Workspace, origin: &Url, epoch: u64) -> Option<Self> {
        let (slots, files) = Files::capture(|| {
            crate::analysis::AnalysisContext::new().run(|_| {
                super::completion_cache::observe_closed(ws);
                Self::slots(ws, origin)
            })
        });
        Self::retain(ws, origin, epoch, slots, files)
    }

    pub(crate) fn complete_and_prepare(
        ws: &Workspace,
        origin: &Url,
        position: Position,
        epoch: u64,
    ) -> (Vec<CompletionItem>, Option<Self>) {
        // One observed immutable query supplies both the response and its proof.
        // Keep its local indexes alive while certifying the input context.
        let ((items, slots), files) = Files::capture(|| {
            super::completion_cache::observe_closed(ws);
            crate::analysis::AnalysisContext::new().run(|_| {
                let items = super::features::completion_in_query(ws, origin, position);
                (items, Self::slots(ws, origin))
            })
        });
        (items, Self::retain(ws, origin, epoch, slots, files))
    }

    fn slots(ws: &Workspace, origin: &Url) -> Vec<Slot> {
        let Some(doc) = ws.documents.get(origin) else {
            return vec![];
        };
        if doc.kind != DocumentKind::Latex
            || doc.text.source.len() > MAX_SOURCE
            || !ws.index_truncated.is_empty()
            || ws.has_pending_dependencies()
        {
            return vec![];
        }
        let mut slots = Vec::new();
        for command in &doc.syntax.commands {
            if command.name != "input" {
                continue;
            }
            let source = &doc.text.source;
            let start = command.span.start;
            let line_start = source[..start].rfind('\n').map_or(0, |at| at + 1);
            if !source[line_start..start].trim().is_empty()
                || crate::project::resource_context::grouped(source, &doc.syntax, start)
            {
                continue;
            }
            let at = syntax::skip_space(source, command.span.end);
            if source.as_bytes().get(at) != Some(&b'{') {
                continue;
            }
            let end = source[at + 1..]
                .find(['\r', '\n'])
                .map_or(source.len(), |end| at + 1 + end);
            let line = &source[at + 1..end];
            let value = line.trim_end().strip_suffix('}').unwrap_or(line);
            if !literal(value)
                || !unresolved(origin, value)
                || syntax::active_file_prefix(source, at, at + 1 + value.len(), false, false)
                    != Some(at + 1..at + 1 + value.len())
            {
                continue;
            }
            slots.push(Slot {
                command: start,
                before: source[..at + 1].into(),
                after: source[at + 1 + value.len()..].into(),
            });
            if slots.len() == MAX_SLOTS {
                break;
            }
        }
        if slots.is_empty() || ws.root_document(origin).ok().as_ref() != Some(origin) {
            return vec![];
        }
        let scope = ws.project_documents(origin);
        let positions = crate::project::source_inputs::active_direct_file_positions(ws, &scope);
        slots.retain(|slot| {
            positions
                .get(origin)
                .is_some_and(|set| set.contains(&slot.command))
        });
        slots
    }

    fn retain(
        ws: &Workspace,
        origin: &Url,
        epoch: u64,
        slots: Vec<Slot>,
        files: Arc<Files>,
    ) -> Option<Self> {
        if slots.is_empty() || !files.current_for_editor(ws) {
            return None;
        }
        let result = Self {
            origin: origin.clone(),
            epoch,
            revision: crate::analysis::AnalysisContext::current()
                .and_then(|context| context.binding.as_ref().map(|binding| binding.revision))
                .unwrap_or(0),
            documents: ws.documents.weak_revisions(),
            slots,
            files,
        };
        (result.bytes() <= MAX_BYTES).then_some(result)
    }

    pub(crate) fn origin(&self) -> &Url {
        &self.origin
    }
    pub(crate) fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.origin.as_str().len()
            + self.files.bytes
            + self
                .slots
                .iter()
                .map(|slot| slot.before.capacity() + slot.after.capacity() + 128)
                .sum::<usize>()
            + self
                .documents
                .iter()
                .map(|(uri, _)| uri.as_str().len() + std::mem::size_of::<Document>() + 128)
                .sum::<usize>()
    }
    fn spans(&self, ws: &Workspace, epoch: u64) -> Option<Vec<Range<usize>>> {
        if epoch != self.epoch
            || !ws.index_truncated.is_empty()
            || self.documents.len() != ws.documents.len()
            || ws
                .documents
                .values()
                .map(|doc| doc.text.source.len() as u64)
                .sum::<u64>()
                > ws.config.indexing.max_total_bytes
            || !self.documents.iter().all(|(uri, weak)| {
                ws.documents
                    .get(uri)
                    .is_some_and(|doc| uri == &self.origin || std::ptr::eq(weak.as_ptr(), doc))
            })
        {
            return None;
        }
        let doc = ws.documents.get(&self.origin)?;
        if doc.kind != DocumentKind::Latex
            || doc.text.source.len() > MAX_SOURCE
            || doc.text.source.len() as u64 > ws.config.indexing.max_file_size
        {
            return None;
        }
        let source = &doc.text.source;
        let spans: Vec<_> = self
            .slots
            .iter()
            .filter_map(|slot| {
                if source.len() < slot.before.len() + slot.after.len()
                    || !source.starts_with(&slot.before)
                    || !source.ends_with(&slot.after)
                {
                    return None;
                }
                let span = slot.before.len()..source.len() - slot.after.len();
                let value = &source[span.clone()];
                (literal(value) && unresolved(&self.origin, value)).then_some(span)
            })
            .collect();
        (!spans.is_empty() && self.files.current_for_editor(ws)).then_some(spans)
    }
    pub(crate) fn valid_for(&self, ws: &Workspace, epoch: u64) -> bool {
        self.spans(ws, epoch).is_some()
    }
    pub(crate) fn complete(
        &self,
        ws: &Workspace,
        epoch: u64,
        position: Position,
    ) -> Option<Vec<CompletionItem>> {
        let doc = ws.documents.get(&self.origin)?;
        let offset = doc.text.offset(position)?;
        let span = self
            .spans(ws, epoch)?
            .into_iter()
            .find(|span| span.start <= offset && offset <= span.end)?;
        Some(super::path_completion::complete_prepared_input(
            ws,
            &self.origin,
            span.start..offset,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(line: &str) -> (tempfile::TempDir, Workspace, Url, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Tex")).unwrap();
        std::fs::write(dir.path().join("Tex/0_Abstract.tex"), "Text").unwrap();
        let source = if line == "\\input{Tex/" {
            format!("\\documentclass{{article}}\n\\begin{{document}}\n{line}")
        } else {
            format!("\\documentclass{{article}}\n\\begin{{document}}\n{line}\n\\end{{document}}\n")
        };
        let path = dir.path().join("main.tex");
        std::fs::write(&path, &source).unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        let mut ws = Workspace::default();
        ws.open(uri.clone(), source.clone(), 1);
        (dir, ws, uri, source)
    }
    fn change(ws: &mut Workspace, uri: &Url, text: String) {
        let doc = ws.documents[uri].with_text(uri, text, Some(2));
        ws.documents.insert(uri.clone(), doc);
        ws.defer_dependencies(uri.clone());
    }

    #[test]
    fn preparation_follows_filename_edits_even_without_a_completion_request() {
        let (_dir, ws, uri, source) = fixture("\\input{Tex/}");
        let before = &ws.documents[&uri];
        for (source, expected) in [
            (source.replace("Tex/", "Tex/0"), true),
            (source.replace("Tex/", ""), true),
            (source.replace("Tex/", "\\macro"), false),
            (
                source.replace("\\end{document}", "% new comment\n\\end{document}"),
                false,
            ),
            (
                source.replace("\\end{document}", "\n\\end{document}"),
                false,
            ),
            (
                source.replace("\\begin{document}", "% 中文🙂\n\\begin{document}"),
                false,
            ),
            (source.clone(), false),
        ] {
            let after = before.with_text(&uri, source.clone(), Some(2));
            assert_eq!(changed_candidate(before, &after), expected, "{source}");
        }
    }

    #[test]
    fn the_first_full_response_prepares_a_reusable_observed_context() {
        let (dir, mut ws, uri, source) = fixture("\\input{Tex/}");
        let offset = source.find("Tex/").unwrap() + 4;
        let position = ws.documents[&uri].text.position(offset);
        assert!(candidate(&ws.documents[&uri], position));
        let (items, prepared) = Prepared::complete_and_prepare(&ws, &uri, position, 0);
        let prepared = prepared.unwrap();
        let expected = crate::language::features::completion(&ws, &uri, position);
        assert_eq!(
            serde_json::to_value(items).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        change(&mut ws, &uri, source.replace("Tex/", "Tex/0"));
        assert!(prepared.valid_for(&ws, 0));
        std::fs::write(
            dir.path().join(".texquill.json"),
            r#"{"rootDocument":"other.tex"}"#,
        )
        .unwrap();
        assert!(
            !prepared.valid_for(&ws, 0),
            "configuration probes made during completion must be retained"
        );
    }

    #[test]
    fn late_old_preparation_cannot_replace_a_newer_context() {
        let (_dir, mut ws, uri, source) = fixture("\\input{Tex/}");
        let mut older = Prepared::build(&ws, &uri, 0).unwrap();
        older.revision = 1;
        change(
            &mut ws,
            &uri,
            source.replace("\\begin{document}", "% new context\n\\begin{document}"),
        );
        ws.refresh_pending_dependencies();
        let mut newer = Prepared::build(&ws, &uri, 0).unwrap();
        newer.revision = 2;
        let mut cache = super::super::completion_cache::Cache::default();
        cache.insert_paths(newer);
        assert!(cache.covers_paths(&ws, 0, &uri));
        cache.insert_paths(older);
        assert!(cache.covers_paths(&ws, 0, &uri));
    }

    #[test]
    fn input_prefixes_reuse_certificates_with_fresh_edits_and_live_directory_candidates() {
        for initial in ["\\input{Tex/}", "\\input{Tex/"] {
            let (_dir, mut ws, uri, source) = fixture(initial);
            let prepared = Prepared::build(&ws, &uri, 1).expect(initial);
            for prefix in ["Tex/0", "Tex/0_Ab", "Tex/"] {
                let source = source.replace("Tex/", prefix);
                change(&mut ws, &uri, source.clone());
                assert!(prepared.valid_for(&ws, 1), "{source}");
                let offset = source.find(prefix).unwrap() + prefix.len();
                let position = ws.documents[&uri].text.position(offset);
                let cached = prepared.complete(&ws, 1, position).unwrap();
                assert!(cached.iter().any(|item| item.label == "Tex/0_Abstract.tex"));
                let mut fresh = ws.clone();
                fresh.refresh_pending_dependencies();
                let expected = crate::language::features::completion(&fresh, &uri, position);
                assert_eq!(
                    serde_json::to_value(cached).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{initial}: {prefix}"
                );
                assert!(prepared.complete(&ws, 1, Position::new(0, 0)).is_none());
            }
        }
    }

    #[test]
    fn real_inputs_and_semantic_changes_leave_the_fast_path() {
        let (_dir, mut ws, uri, source) = fixture("\\input{Tex/}");
        let prepared = Prepared::build(&ws, &uri, 7).unwrap();
        for changed in [
            source.replace("Tex/", "Tex/0_Abstract"),
            source.replace("Tex/", "Tex/0_Abstract.tex"),
            source.replace("Tex/", "Tex/\\macro"),
            source.replace("Tex/", "Tex/%comment"),
            source.replace("Tex/", "Tex/\n0"),
            source.replace("\\input", "\\include"),
            source.replace("\\begin{document}", "\\def\\input#1{}\n\\begin{document}"),
            source.replace(
                "\\end{document}",
                "\\newcommand\\changed{}\n\\end{document}",
            ),
        ] {
            change(&mut ws, &uri, changed.clone());
            assert!(!prepared.valid_for(&ws, 7), "{changed}");
        }
        change(&mut ws, &uri, source.clone());
        assert!(prepared.valid_for(&ws, 7));
        assert!(!prepared.valid_for(&ws, 8));
        ws.index_truncated.insert("maxFiles".into());
        assert!(!prepared.valid_for(&ws, 7));
        ws.index_truncated.clear();
        let other = Url::parse("untitled:other.tex").unwrap();
        ws.documents.insert(
            other.clone(),
            Document::new(&other, "\\def\\input#1{}".into(), Some(1)),
        );
        assert!(!prepared.valid_for(&ws, 7));
    }

    #[test]
    fn dependency_versions_and_filesystem_changes_invalidate_certificates() {
        let (dir, mut ws, uri, source) = fixture("\\input{Tex/}");
        let other = Url::parse("untitled:other.tex").unwrap();
        ws.documents
            .insert(other.clone(), Document::new(&other, "Text".into(), Some(1)));
        let prepared = Prepared::build(&ws, &uri, 0).unwrap();
        ws.documents.get_mut(&other).unwrap().text = crate::text::Text::new("Changed".into());
        assert!(!prepared.valid_for(&ws, 0));
        let prepared = Prepared::build(&ws, &uri, 0).unwrap();
        change(&mut ws, &uri, source.replace("Tex/", "Tex/new"));
        assert!(prepared.valid_for(&ws, 0));
        std::fs::write(dir.path().join("Tex/new.tex"), "\\newcommand\\newmacro{}").unwrap();
        assert!(!prepared.valid_for(&ws, 0));
    }

    #[test]
    fn quoted_grouped_overridden_and_nonliteral_inputs_are_not_certified() {
        for line in [
            "{\\input{Tex/}}",
            "\\detokenize{\\input{Tex/}}",
            "\\let\\input\\relax\n\\input{Tex/}",
            "\\input{Tex/\\name}",
            "\\input{Tex/}\\newcommand\\later{}",
            "\\input{Tex/0_Abstract}",
            "\\input{Tex/\n\\nextcommand",
        ] {
            let (_dir, ws, uri, _) = fixture(line);
            assert!(Prepared::build(&ws, &uri, 0).is_none(), "{line}");
        }
    }
}
