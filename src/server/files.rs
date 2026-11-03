//! Ordered file observations are prepared on snapshots, never under input locks.
use crate::{
    editing::file_operations::FileChanges,
    project::{state::ProjectState, workspace::Workspace},
};
use lsp_types::Url;
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    issued: u64,
    finished: u64,
    dropped: BTreeSet<u64>,
}

#[derive(Clone, Default)]
pub(super) struct Refreshes {
    state: Arc<Mutex<State>>,
    updated: Arc<Notify>,
    source_sets: Arc<AtomicUsize>,
}

pub(super) struct Ticket {
    owner: Refreshes,
    number: u64,
    source_set: bool,
}

impl Refreshes {
    pub fn begin(&self, source_set: bool) -> Ticket {
        let mut state = self.state.lock().unwrap();
        state.issued += 1;
        if source_set {
            self.source_sets.fetch_add(1, Ordering::SeqCst);
        }
        Ticket {
            owner: self.clone(),
            number: state.issued,
            source_set,
        }
    }
    pub fn source_set_pending(&self) -> bool {
        self.source_sets.load(Ordering::SeqCst) != 0
    }
    pub fn pending(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        (state.issued > state.finished).then_some(state.issued)
    }
    pub async fn wait(&self, number: u64) {
        loop {
            let changed = self.updated.notified();
            if self.state.lock().unwrap().finished >= number {
                return;
            }
            changed.await;
        }
    }
}

impl Ticket {
    pub async fn previous(&self) {
        self.owner.wait(self.number - 1).await;
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap();
        if self.source_set {
            self.owner.source_sets.fetch_sub(1, Ordering::SeqCst);
        }
        state.dropped.insert(self.number);
        loop {
            let next = state.finished + 1;
            if !state.dropped.remove(&next) {
                break;
            }
            state.finished = next;
        }
        self.owner.updated.notify_waiters();
    }
}

pub(super) struct Prepared {
    before: Workspace,
    after: Workspace,
    pub affected: BTreeSet<Url>,
}
impl Prepared {
    #[cfg(test)]
    pub fn build(mut workspace: Workspace, changes: &FileChanges, watched: bool) -> Self {
        Self::prepare(&mut workspace, changes, watched)
    }

    pub fn prepare(workspace: &mut Workspace, changes: &FileChanges, watched: bool) -> Self {
        let before = workspace.clone();
        workspace.refresh_pending_dependencies();
        let affected = if watched {
            changes.refresh_watched(workspace)
        } else {
            changes.refresh(workspace)
        };
        Self {
            before,
            after: workspace.clone(),
            affected,
        }
    }

    pub fn current(&self, workspace: &Workspace) -> bool {
        self.before.documents.same_revisions(&workspace.documents)
            && self.before.pending_dependencies == workspace.pending_dependencies
            && self.before.roots == workspace.roots
            && serde_json::to_value(&self.before.config).unwrap()
                == serde_json::to_value(&workspace.config).unwrap()
            && same_projects(&self.before.project, &workspace.project)
    }

    /// Call only after checking the source generation and `current`. No file
    /// reads, project traversal, or parsing belongs in this publication step.
    pub fn merge(self, workspace: &mut Workspace) -> bool {
        let updates = self.after.documents.updates_since(&self.before.documents);
        let changed = !updates.is_empty();
        workspace.documents.apply_updates(updates);
        workspace.pending_dependencies = self.after.pending_dependencies;
        workspace.index_truncated = self.after.index_truncated;
        // A build may publish while the refresh computes. Only discard a
        // diagnostic observation that is still exactly the one we checked.
        for (root, old) in &self.before.project.build_diagnostics {
            if !self.after.project.build_diagnostics.contains_key(root)
                && workspace.project.build_diagnostics.get(root) == Some(old)
                && same_checks(
                    self.before.project.build_checks.get(root),
                    workspace.project.build_checks.get(root),
                )
            {
                workspace.project.build_diagnostics.remove(root);
                workspace.project.build_diagnostic_sources.remove(root);
            }
        }
        for (uri, old) in &self.before.project.lint_diagnostics {
            if !self.after.project.lint_diagnostics.contains_key(uri)
                && workspace.project.lint_diagnostics.get(uri) == Some(old)
            {
                workspace.project.lint_diagnostics.remove(uri);
            }
        }
        changed
    }
}
fn same_checks<T>(left: Option<&Arc<T>>, right: Option<&Arc<T>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}
fn same_projects(left: &ProjectState, right: &ProjectState) -> bool {
    left.recorded_inputs == right.recorded_inputs
        && left.recorded_binary_inputs == right.recorded_binary_inputs
        && left.recorded_bindings == right.recorded_bindings
        && left.bibliography_inputs == right.bibliography_inputs
        && left.bibliography_databases == right.bibliography_databases
        && left.bibliography_bindings == right.bibliography_bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::workspace::Document;

    #[tokio::test]
    async fn observation_tickets_finish_in_order_even_when_a_waiter_is_cancelled() {
        let refreshes = Refreshes::default();
        let first = refreshes.begin(true);
        let second = refreshes.begin(true);
        let last = refreshes.pending().unwrap();
        drop(second);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), refreshes.wait(last))
                .await
                .is_err()
        );
        drop(first);
        refreshes.wait(last).await;
        assert!(refreshes.pending().is_none());
    }

    #[test]
    fn prepared_refresh_rejects_editor_changes_and_preserves_new_build_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let uri = Url::from_file_path(dir.path().join("child.tex")).unwrap();
        std::fs::write(uri.to_file_path().unwrap(), "New").unwrap();
        let mut workspace = Workspace::default();
        workspace
            .documents
            .insert(uri.clone(), Document::new(&uri, "Old".into(), None));
        let old = vec![lsp_types::Diagnostic::new_simple(
            lsp_types::Range::default(),
            "Old error".into(),
        )];
        workspace
            .project
            .build_diagnostics
            .insert(uri.clone(), [(uri.clone(), old)].into());
        let changes = FileChanges::new([uri.as_str()]).unwrap();
        let prepared = Prepared::build(workspace.clone(), &changes, false);
        let mut edited = workspace.clone();
        edited
            .documents
            .insert(uri.clone(), Document::new(&uri, "Unsaved".into(), Some(2)));
        assert!(!prepared.current(&edited));
        let fresh = [(
            uri.clone(),
            vec![lsp_types::Diagnostic::new_simple(
                lsp_types::Range::default(),
                "Fresh error".into(),
            )],
        )]
        .into();
        workspace
            .project
            .build_diagnostics
            .insert(uri.clone(), fresh);
        assert!(prepared.current(&workspace));
        assert!(prepared.merge(&mut workspace));
        assert_eq!(workspace.documents[&uri].text.source, "New");
        assert_eq!(
            workspace.project.build_diagnostics[&uri][&uri][0].message,
            "Fresh error"
        );
    }

    #[test]
    fn unchanged_watcher_preserves_open_and_closed_document_identities() {
        let dir = tempfile::tempdir().unwrap();
        let main = Url::from_file_path(dir.path().join("main.tex")).unwrap();
        let child = main.join("child.tex").unwrap();
        std::fs::write(main.to_file_path().unwrap(), "\\input{child}").unwrap();
        std::fs::write(child.to_file_path().unwrap(), "Body").unwrap();
        let mut workspace = Workspace::default();
        workspace.open(main.clone(), "\\input{child}".into(), 1);
        let before = workspace.documents.clone();
        let changes = FileChanges::new([main.as_str(), child.as_str()]).unwrap();
        let prepared = Prepared::build(workspace.clone(), &changes, true);
        assert!(prepared.current(&workspace));
        assert!(!prepared.merge(&mut workspace));
        assert!(before.same_revisions(&workspace.documents));
    }
}
