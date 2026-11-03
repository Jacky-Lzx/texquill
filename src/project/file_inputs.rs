//! Source-graph observations and bounded last-save text, independent of typing.
use super::{
    documents::Revisions,
    workspace::{Document, DocumentKind, Workspace},
};
use crate::diagnostics::diagnostic_inputs::Files;
use lsp_types::Url;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
};

#[derive(Clone, Default)]
pub(crate) struct Saved(VecDeque<(Url, DocumentKind, Arc<crate::text::Text>)>);
impl Saved {
    pub fn remember(&mut self, uri: Url, doc: &Document) {
        self.0.retain(|(old, _, _)| old != &uri);
        if doc.text.source.len() <= 1024 * 1024 {
            self.0
                .push_back((uri, doc.kind, Arc::new(doc.text.clone())));
        }
        while self.0.len() > 64
            || self
                .0
                .iter()
                .map(|(_, _, text)| text.source.len())
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            self.0.pop_front();
        }
    }
    pub fn matches(&self, uri: &Url, doc: &Document, disk: &str) -> bool {
        doc.version.is_some()
            && self.0.iter().any(|(saved, kind, text)| {
                saved == uri && *kind == doc.kind && text.matches_saved(disk)
            })
    }
}

pub(crate) struct Observed {
    documents: Revisions,
    epoch: u64,
    pub files: Arc<Files>,
}
impl Observed {
    pub fn new(workspace: &Workspace, epoch: u64, files: Arc<Files>) -> Self {
        Self {
            documents: workspace.documents.revisions(),
            epoch,
            files,
        }
    }
    pub fn current(&self, workspace: &Workspace, epoch: u64) -> bool {
        self.epoch == epoch
            && workspace.index_truncated.is_empty()
            && self
                .documents
                .current_except(&workspace.documents, &workspace.pending_dependencies)
            && self.files.current_for_editor(workspace)
    }
}

/// Called inside one observing immutable analysis after propagation. Cover all
/// root graphs and loose open sources, including failed filename lookups.
pub(crate) fn observe_graph(workspace: &Workspace) {
    crate::language::completion_cache::observe_closed(workspace);
    let projects = workspace.saved_projects();
    let covered: BTreeSet<_> = projects.values().flatten().collect();
    for (uri, doc) in &workspace.documents {
        if doc.version.is_some() && doc.kind == DocumentKind::Latex && !covered.contains(uri) {
            workspace.project_documents(uri);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn save_proofs_follow_saved_bytes_not_later_edits_and_are_bounded() {
        let uri = Url::parse("untitled:save.tex").unwrap();
        let saved_doc = Document::new(&uri, "Saved".into(), Some(1));
        let current = Document::new(&uri, "Typing after save".into(), Some(2));
        let mut saved = Saved::default();
        saved.remember(uri.clone(), &saved_doc);
        assert!(saved.matches(&uri, &current, "Saved\n"));
        assert!(!saved.matches(&uri, &current, "External edit"));
        saved.remember(uri.clone(), &current);
        assert!(!saved.matches(&uri, &current, "Saved"));
        for i in 0..70 {
            let uri = Url::parse(&format!("untitled:{i}.tex")).unwrap();
            saved.remember(uri, &saved_doc);
        }
        assert_eq!(saved.0.len(), 64);
    }
    #[test]
    fn graph_observations_cover_missing_inputs_but_allow_unpropagated_editor_text() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let uri = Url::from_file_path(root.join("main.tex")).unwrap();
        let source = "\\documentclass{article}\\InputIfFileExists{main.tex~}{}{}";
        std::fs::write(uri.to_file_path().unwrap(), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.config.distribution.enabled = false;
        let (_, files) = Files::capture(|| {
            workspace.open(uri.clone(), source.into(), 1);
            crate::analysis::AnalysisContext::new().run(|_| observe_graph(&workspace));
        });
        let observed = Observed::new(&workspace, 4, files);
        workspace.documents.insert(
            uri.clone(),
            Document::new(&uri, format!("{source}\n\\subsec"), Some(2)),
        );
        workspace.defer_dependencies(uri.clone());
        assert!(observed.current(&workspace, 4));
        assert!(!observed.current(&workspace, 5));
        std::fs::write(root.join("4913"), "Unrelated").unwrap();
        assert!(observed.current(&workspace, 4));
        std::fs::write(root.join("main.tex~"), "\\newcommand{\\loaded}{}").unwrap();
        assert!(!observed.current(&workspace, 4));
    }
}
