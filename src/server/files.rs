//! Ordered file observations are prepared on snapshots, never under input locks.
use crate::{
    editing::file_operations::FileChanges,
    project::{state::ProjectState, workspace::Workspace},
};
use lsp_types::Url;
use std::{
    collections::{BTreeMap, BTreeSet},
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
    jobs: BTreeMap<u64, (FileChanges, bool)>,
    sources: BTreeSet<u64>,
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
}

impl Refreshes {
    #[cfg(test)]
    pub fn begin(&self, source_set: bool) -> Ticket {
        self.issue(source_set, None)
    }
    pub fn queue(&self, changes: FileChanges, watched: bool, source_set: bool) -> Ticket {
        self.issue(source_set, Some((changes, watched)))
    }
    fn issue(&self, source_set: bool, job: Option<(FileChanges, bool)>) -> Ticket {
        let mut state = self.state.lock().unwrap();
        state.issued += 1;
        let number = state.issued;
        if source_set {
            state.sources.insert(number);
            self.source_sets.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(job) = job {
            state.jobs.insert(number, job);
        }
        Ticket {
            owner: self.clone(),
            number,
        }
    }
    fn complete(&self, numbers: &[u64]) {
        let mut state = self.state.lock().unwrap();
        for number in numbers {
            if *number <= state.finished {
                continue;
            }
            state.jobs.remove(number);
            if state.sources.remove(number) {
                self.source_sets.fetch_sub(1, Ordering::SeqCst);
            }
            state.dropped.insert(*number);
        }
        loop {
            let next = state.finished + 1;
            if !state.dropped.remove(&next) {
                break;
            }
            state.finished = next;
        }
        self.updated.notify_waiters();
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

pub(super) struct Batch {
    owner: Refreshes,
    numbers: Vec<u64>,
    changes: FileChanges,
    pub watched: bool,
}
impl Batch {
    pub fn take_changes(&mut self) -> FileChanges {
        std::mem::take(&mut self.changes)
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        self.owner.complete(&self.numbers);
    }
}
impl Ticket {
    pub async fn batch(&self) -> Option<Batch> {
        loop {
            let changed = self.owner.updated.notified();
            {
                let mut state = self.owner.state.lock().unwrap();
                if state.finished >= self.number {
                    return None;
                }
                if state.finished + 1 == self.number && state.jobs.contains_key(&self.number) {
                    let (mut changes, watched) = state.jobs.remove(&self.number).unwrap();
                    let mut numbers = vec![self.number];
                    let mut next = self.number + 1;
                    // Never cross an explicit file operation. No timer delays
                    // the first event; only an existing backlog is coalesced.
                    while watched && state.jobs.get(&next).is_some_and(|(_, watched)| *watched) {
                        let (additional, _) = state.jobs.remove(&next).unwrap();
                        changes.extend(additional);
                        numbers.push(next);
                        next += 1;
                    }
                    return Some(Batch {
                        owner: self.owner.clone(),
                        numbers,
                        changes,
                        watched,
                    });
                }
            }
            changed.await;
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.owner.complete(&[self.number]);
    }
}

pub(super) struct Prepared {
    before: Workspace,
    after: Workspace,
    pub affected: BTreeSet<Url>,
    pub files: Option<Arc<crate::diagnostics::diagnostic_inputs::Files>>,
}
impl Prepared {
    #[cfg(test)]
    pub fn build(mut workspace: Workspace, changes: &FileChanges, watched: bool) -> Self {
        Self::prepare(&mut workspace, changes, watched)
    }

    #[cfg(test)]
    pub fn prepare(workspace: &mut Workspace, changes: &FileChanges, watched: bool) -> Self {
        Self::prepare_observed(workspace, changes, watched, false, &Default::default())
    }

    pub fn prepare_observed(
        workspace: &mut Workspace,
        changes: &FileChanges,
        watched: bool,
        observed_unchanged: bool,
        saved: &crate::project::file_inputs::Saved,
    ) -> Self {
        let before = workspace.clone();
        let graph_unchanged = watched
            && (observed_unchanged || changes.unchanged_saved_sources(workspace, saved).is_some());
        let (affected, files) = if graph_unchanged {
            let _timing = crate::performance::Span::new("files.graph_reused");
            // Pending editor propagation has its own coalesced owner. A late
            // save event must not force it just to discard an unrelated event.
            (changes.refresh_observed(workspace, watched, true), None)
        } else {
            let _timing = crate::performance::Span::new("files.graph_refresh");
            let (affected, files) = crate::diagnostics::diagnostic_inputs::Files::capture(|| {
                workspace.refresh_pending_dependencies();
                let affected = changes.refresh_observed(workspace, watched, false);
                crate::analysis::AnalysisContext::new()
                    .run(|_| crate::project::file_inputs::observe_graph(workspace));
                affected
            });
            (affected, Some(files))
        };
        Self {
            before,
            after: workspace.clone(),
            affected,
            files,
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
    async fn queued_watchers_coalesce_without_crossing_explicit_operations() {
        let refreshes = Refreshes::default();
        let barrier = refreshes.begin(false);
        let a = Url::parse("file:///tmp/a.tex").unwrap();
        let b = Url::parse("file:///tmp/b.tex").unwrap();
        let one = refreshes.queue(FileChanges::new([a.as_str()]).unwrap(), true, false);
        let two = refreshes.queue(FileChanges::new([b.as_str()]).unwrap(), true, true);
        let three = refreshes.queue(FileChanges::new([a.as_str()]).unwrap(), true, false);
        let explicit = refreshes.queue(FileChanges::new([a.as_str()]).unwrap(), false, false);
        let last = refreshes.queue(FileChanges::new([b.as_str()]).unwrap(), true, false);
        drop(barrier);
        let mut batch = one.batch().await.unwrap();
        assert_eq!(batch.numbers.len(), 3);
        assert!(refreshes.source_set_pending());
        let changes = batch.take_changes();
        assert!(changes.affects(&a) && changes.affects(&b));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), two.batch())
                .await
                .is_err()
        );
        drop(batch);
        assert!(!refreshes.source_set_pending());
        assert!(two.batch().await.is_none() && three.batch().await.is_none());
        let batch = explicit.batch().await.unwrap();
        assert!(!batch.watched);
        assert_eq!(batch.numbers.len(), 1);
        drop(batch);
        let batch = last.batch().await.unwrap();
        assert_eq!(batch.numbers.len(), 1);
        drop(batch);
        drop((one, two, three, explicit, last));
        assert!(refreshes.pending().is_none());
        assert!(refreshes.state.lock().unwrap().dropped.is_empty());
    }

    #[test]
    fn delayed_save_uses_saved_snapshot_without_forcing_pending_editor_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = Url::from_file_path(root.join("main.tex")).unwrap();
        let child = main.join("new.tex").unwrap();
        std::fs::write(root.join("main.tex"), "Saved").unwrap();
        let mut workspace = Workspace::default();
        workspace.open(main.clone(), "Saved".into(), 1);
        std::fs::write(root.join("new.tex"), "New source").unwrap();
        let mut saved = crate::project::file_inputs::Saved::default();
        saved.remember(main.clone(), &workspace.documents[&main]);
        workspace.documents.insert(
            main.clone(),
            Document::new(&main, "\\input{new}".into(), Some(2)),
        );
        workspace.defer_dependencies(main.clone());
        let changes = FileChanges::new([main.as_str()]).unwrap();
        let prepared =
            Prepared::prepare_observed(&mut workspace.clone(), &changes, true, false, &saved);
        assert!(prepared.files.is_none());
        prepared.merge(&mut workspace);
        assert!(workspace.pending_dependencies.contains(&main));
        assert!(!workspace.documents.contains_key(&child));
        workspace.refresh_pending_dependencies();
        assert!(workspace.documents.contains_key(&child));
    }

    #[test]
    fn mixed_save_artifacts_reuse_observed_graph_but_external_dependencies_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = Url::from_file_path(root.join("main.tex")).unwrap();
        let child = main.join("child.tex").unwrap();
        let source = "\\documentclass{article}\\input{child}";
        std::fs::write(root.join("main.tex"), source).unwrap();
        std::fs::write(root.join("child.tex"), "Old").unwrap();
        let mut workspace = Workspace::default();
        workspace.config.distribution.enabled = false;
        let (_, files) = crate::diagnostics::diagnostic_inputs::Files::capture(|| {
            workspace.open(main.clone(), source.into(), 1);
            crate::analysis::AnalysisContext::new()
                .run(|_| crate::project::file_inputs::observe_graph(&workspace));
        });
        let observed = crate::project::file_inputs::Observed::new(&workspace, 0, files);
        workspace.documents.insert(
            main.clone(),
            Document::new(&main, format!("{source}\n\\subsec"), Some(2)),
        );
        workspace.defer_dependencies(main.clone());
        let changes = FileChanges::new([
            main.as_str(),
            main.join("4913").unwrap().as_str(),
            main.join("main.tex~").unwrap().as_str(),
        ])
        .unwrap();
        let unchanged = observed.current(&workspace, 0);
        assert!(unchanged);
        let prepared = Prepared::prepare_observed(
            &mut workspace.clone(),
            &changes,
            true,
            unchanged,
            &Default::default(),
        );
        assert!(prepared.files.is_none());
        prepared.merge(&mut workspace);
        assert!(workspace.pending_dependencies.contains(&main));
        assert_eq!(workspace.documents[&child].text.source, "Old");

        std::fs::write(root.join("child.tex"), "External edit").unwrap();
        assert!(!observed.current(&workspace, 0));
        let external = FileChanges::new([child.as_str()]).unwrap();
        let prepared = Prepared::prepare_observed(
            &mut workspace.clone(),
            &external,
            true,
            false,
            &Default::default(),
        );
        assert!(prepared.files.is_some());
        prepared.merge(&mut workspace);
        assert_eq!(workspace.documents[&child].text.source, "External edit");
    }

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
