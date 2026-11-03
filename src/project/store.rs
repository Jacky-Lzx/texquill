//! Runtime workspace mutation has one revision boundary for every consumer.
use super::workspace::Workspace;
use std::ops::{Deref, DerefMut};
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{Notify, OwnedRwLockReadGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub struct WorkspaceUpdate<'a> {
    workspace: RwLockWriteGuard<'a, Workspace>,
    revision: &'a AtomicU64,
    updated: &'a Notify,
    changed: bool,
    completion_epoch: Option<&'a AtomicU64>,
    source_revision: Option<&'a AtomicU64>,
    completion_inputs: &'a std::sync::RwLock<Arc<CompletionInputs>>,
    epoch: &'a AtomicU64,
}

impl Deref for WorkspaceUpdate<'_> {
    type Target = Workspace;
    fn deref(&self) -> &Workspace {
        &self.workspace
    }
}

impl DerefMut for WorkspaceUpdate<'_> {
    fn deref_mut(&mut self) -> &mut Workspace {
        if !self.changed {
            self.revision.fetch_add(1, Ordering::SeqCst);
            self.changed = true;
            if let Some(epoch) = self.completion_epoch {
                epoch.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(revision) = self.source_revision {
                revision.fetch_add(1, Ordering::SeqCst);
            }
            self.updated.notify_waiters();
        }
        &mut self.workspace
    }
}

#[derive(Default)]
pub(crate) struct CompletionInputs {
    pub workspace: Workspace,
    pub epoch: u64,
}

impl CompletionInputs {
    fn capture(workspace: &Workspace, epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            workspace: workspace.completion_snapshot(),
            epoch,
        })
    }

    fn publish(slot: &std::sync::RwLock<Arc<Self>>, next: Arc<Self>) {
        let old = std::mem::replace(&mut *slot.write().unwrap(), next);
        // Destruction of an obsolete syntax tree must not hold the reader slot.
        drop(old);
    }
}

impl Drop for WorkspaceUpdate<'_> {
    fn drop(&mut self) {
        if self.changed {
            let next =
                CompletionInputs::capture(&self.workspace, self.epoch.load(Ordering::SeqCst));
            CompletionInputs::publish(self.completion_inputs, next);
            // Document-bound waiters must also observe the published pointer,
            // after the early revision invalidation signal.
            self.updated.notify_waiters();
        }
    }
}

#[derive(Clone, Default)]
pub struct WorkspaceStore {
    workspace: Arc<RwLock<Workspace>>,
    revision: Arc<AtomicU64>,
    updated: Arc<Notify>,
    queued_changes: Arc<AtomicUsize>,
    completion_epoch: Arc<AtomicU64>,
    source_revision: Arc<AtomicU64>,
    completion_inputs: Arc<std::sync::RwLock<Arc<CompletionInputs>>>,
    file_inputs: Arc<std::sync::RwLock<Option<Arc<super::file_inputs::Observed>>>>,
    saved: Arc<std::sync::Mutex<super::file_inputs::Saved>>,
}

/// Count notifications from receipt until their ordered handler completes,
/// including malformed notifications and connection teardown.
pub(crate) struct QueuedChange(Arc<AtomicUsize>);

impl Drop for QueuedChange {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WorkspaceStore {
    pub fn new(workspace: Workspace) -> Self {
        let completion_inputs = Arc::new(std::sync::RwLock::new(CompletionInputs::capture(
            &workspace, 0,
        )));
        Self {
            completion_inputs,
            workspace: Arc::new(RwLock::new(workspace)),
            revision: Arc::default(),
            updated: Arc::default(),
            queued_changes: Arc::default(),
            completion_epoch: Arc::default(),
            source_revision: Arc::default(),
            file_inputs: Arc::default(),
            saved: Arc::default(),
        }
    }

    pub(crate) fn remember_save(&self, uri: lsp_types::Url, doc: &super::workspace::Document) {
        self.saved.lock().unwrap().remember(uri, doc);
    }
    pub(crate) fn saved_inputs(&self) -> super::file_inputs::Saved {
        self.saved.lock().unwrap().clone()
    }
    pub(crate) fn record_file_inputs(
        &self,
        workspace: &Workspace,
        files: Arc<crate::diagnostics::diagnostic_inputs::Files>,
    ) {
        let next = Arc::new(super::file_inputs::Observed::new(
            workspace,
            self.completion_epoch(),
            files,
        ));
        let old = self.file_inputs.write().unwrap().replace(next);
        drop(old);
    }
    pub(crate) fn file_inputs_unchanged(&self, workspace: &Workspace, epoch: u64) -> bool {
        let inputs = self.file_inputs.read().unwrap().clone();
        inputs.is_some_and(|inputs| inputs.current(workspace, epoch))
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, Workspace> {
        loop {
            let workspace = self.workspace.read().await;
            if !workspace.has_pending_dependencies() {
                return workspace;
            }
            drop(workspace);
            let mut workspace = self.workspace.write().await;
            if workspace.refresh_pending_dependencies() {
                self.publish_completion(&workspace);
                self.updated.notify_waiters();
            }
        }
    }

    pub async fn read_owned(self) -> OwnedRwLockReadGuard<Workspace> {
        loop {
            let workspace = self.workspace.clone().read_owned().await;
            if !workspace.has_pending_dependencies() {
                return workspace;
            }
            drop(workspace);
            let mut workspace = self.workspace.write().await;
            if workspace.refresh_pending_dependencies() {
                self.publish_completion(&workspace);
                self.updated.notify_waiters();
            }
        }
    }

    /// Invalidate old analyses on the first mutable access, including when
    /// reindexing replaces the entire Workspace value.
    pub async fn write(&self) -> WorkspaceUpdate<'_> {
        let mut workspace = self.workspace.write().await;
        if workspace.refresh_pending_dependencies() {
            self.publish_completion(&workspace);
            self.updated.notify_waiters();
        }
        self.update(workspace, true, true)
    }

    /// Apply sequential edit ranges without forcing intermediate project
    /// refreshes. All ordinary readers and writers still observe fresh inputs.
    pub(crate) async fn write_changes(&self) -> WorkspaceUpdate<'_> {
        self.update(self.workspace.write().await, false, true)
    }

    /// Derived data only. Snapshot workers validate their source revision
    /// before committing; propagation is not a new editor input or cache epoch.
    pub(crate) async fn write_prepared(&self) -> WorkspaceUpdate<'_> {
        self.update(self.workspace.write().await, false, false)
    }

    fn update<'a>(
        &'a self,
        workspace: RwLockWriteGuard<'a, Workspace>,
        invalidate: bool,
        source: bool,
    ) -> WorkspaceUpdate<'a> {
        WorkspaceUpdate {
            workspace,
            revision: &self.revision,
            updated: &self.updated,
            changed: false,
            completion_epoch: invalidate.then_some(&self.completion_epoch),
            source_revision: source.then_some(&self.source_revision),
            completion_inputs: &self.completion_inputs,
            epoch: &self.completion_epoch,
        }
    }

    /// Cache validation/preparation may inspect pending inputs. Ordinary
    /// language queries must use a fresh guard or a certified prepared result.
    pub(crate) async fn read_unindexed(&self) -> RwLockReadGuard<'_, Workspace> {
        self.workspace.read().await
    }

    fn publish_completion(&self, workspace: &Workspace) {
        let next = CompletionInputs::capture(workspace, self.completion_epoch());
        CompletionInputs::publish(&self.completion_inputs, next);
    }

    /// A coherent published input revision, independent of the async workspace
    /// lock. Ordered edits publish before yielding their dispatch turn.
    pub(crate) fn completion_inputs(&self) -> Arc<CompletionInputs> {
        self.completion_inputs.read().unwrap().clone()
    }

    pub(crate) fn invalidate_completion(&self) {
        self.completion_epoch.fetch_add(1, Ordering::SeqCst);
        self.updated.notify_waiters();
    }

    pub(crate) fn document_input_current(
        &self,
        uri: &lsp_types::Url,
        input: &Option<std::sync::Weak<super::workspace::Document>>,
    ) -> bool {
        let current = self.completion_inputs();
        match (input, current.workspace.documents.get(uri)) {
            (Some(input), Some(document)) => std::ptr::eq(input.as_ptr(), document),
            (None, None) => true,
            _ => false,
        }
    }

    pub(crate) async fn document_input_changed(
        &self,
        uri: &lsp_types::Url,
        input: &Option<std::sync::Weak<super::workspace::Document>>,
    ) {
        loop {
            let updated = self.updated.notified();
            if !self.document_input_current(uri, input) {
                return;
            }
            updated.await;
        }
    }

    pub(crate) fn completion_epoch(&self) -> u64 {
        self.completion_epoch.load(Ordering::SeqCst)
    }

    pub(crate) fn source_revision(&self) -> u64 {
        self.source_revision.load(Ordering::SeqCst)
    }

    /// A synchronous reader can finish propagation without advancing revision.
    pub(crate) fn update_signal(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.updated.notified()
    }

    pub(crate) fn queue_change(&self) -> QueuedChange {
        self.queued_changes.fetch_add(1, Ordering::SeqCst);
        QueuedChange(self.queued_changes.clone())
    }

    pub(crate) fn has_queued_changes(&self) -> bool {
        self.queued_changes.load(Ordering::SeqCst) != 0
    }

    pub(crate) async fn obsolete_background_inputs(&self) -> bool {
        if !self.has_queued_changes() {
            return false;
        }
        let inputs = self.completion_inputs();
        let workspace = &inputs.workspace;
        workspace.has_pending_dependencies() && self.has_queued_changes()
    }

    pub(crate) async fn obsolete_diagnostic_inputs(&self) -> bool {
        if !self.has_queued_changes() {
            return false;
        }
        // Inspect delivery metadata without flushing intermediate source
        // versions. Clients without refresh retain ordered snapshot reports.
        let inputs = self.completion_inputs();
        let workspace = &inputs.workspace;
        workspace.has_pending_dependencies()
            && workspace.diagnostic_delivery.supports_refresh()
            && self.has_queued_changes()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    pub(crate) async fn changed_since(&self, revision: u64) {
        loop {
            // Register before checking the revision so an update cannot land
            // between the check and sleeping, including before the first poll.
            let updated = self.updated.notified();
            if self.revision() != revision {
                return;
            }
            updated.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn all_snapshot_guards_flush_deferred_includes_and_cross_file_grammar() {
        use crate::project::workspace::Document;
        use lsp_types::Url;
        for mode in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let main = Url::from_file_path(root.join("main.tex")).unwrap();
            let provider = main.join("provider.tex").unwrap();
            let child = main.join("child.tex").unwrap();
            let new = main.join("new.tex").unwrap();
            let mut workspace = Workspace::default();
            for (uri, source) in [
                (
                    &main,
                    r"\documentclass{article}\input{provider}\input{child}",
                ),
                (&provider, r"\let\ifempty\ifstrempty"),
                (&child, r"\ifempty{\label{hidden}}{}{}"),
                (&new, r"\label{new}"),
            ] {
                std::fs::write(uri.to_file_path().unwrap(), source).unwrap();
                if uri != &new {
                    workspace
                        .documents
                        .insert(uri.clone(), Document::new(uri, source.into(), None));
                }
            }
            workspace.load_dependencies(&main, 0);
            assert!(workspace.documents[&child].syntax.labels.is_empty());
            let store = WorkspaceStore::new(workspace);
            {
                let mut workspace = store.write_changes().await;
                workspace.documents.insert(
                    provider.clone(),
                    Document::new(&provider, r"\def\ifempty#1{#1}\input{new}".into(), Some(2)),
                );
                workspace.defer_dependencies(provider.clone());
                assert!(!workspace.documents.contains_key(&new));
            }
            fn check(workspace: &Workspace, child: &Url, new: &Url) {
                assert!(!workspace.has_pending_dependencies());
                assert!(workspace.documents.contains_key(new));
                assert!(workspace.documents[child]
                    .syntax
                    .labels
                    .iter()
                    .any(|label| label.name == "hidden"));
            }
            match mode {
                0 => check(&*store.read().await, &child, &new),
                1 => check(&*store.read_owned().await, &child, &new),
                _ => check(&*store.write().await, &child, &new),
            }
        }
    }

    #[test]
    fn queued_change_guards_clear_admission_on_drop() {
        let store = WorkspaceStore::default();
        let first = store.queue_change();
        let second = store.queue_change();
        assert!(store.has_queued_changes());
        drop(first);
        assert!(store.has_queued_changes());
        drop(second);
        assert!(!store.has_queued_changes());
    }

    #[tokio::test]
    async fn revision_waiters_wake_on_mutation_and_detect_changes_before_their_first_poll() {
        let store = WorkspaceStore::default();
        let revision = store.revision();
        let mut changed = Box::pin(store.changed_since(revision));
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(changed.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(!store.write().await.client.snippets);
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(changed.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let unpolled = store.changed_since(revision);
        store.write().await.client.snippets = true;
        tokio::time::timeout(std::time::Duration::from_secs(1), changed)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), unpolled)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn document_waiters_ignore_unrelated_inputs_but_wake_after_target_publication() {
        for remove in [false, true] {
            let store = WorkspaceStore::default();
            let uri = lsp_types::Url::parse("untitled:document-view.tex").unwrap();
            store.write_changes().await.documents.insert(
                uri.clone(),
                super::super::workspace::Document::new(&uri, "Source".into(), Some(1)),
            );
            let current = store.completion_inputs();
            let input = current.workspace.documents.weak_revision(&uri);
            let mut changed = Box::pin(store.document_input_changed(&uri, &input));
            std::future::poll_fn(|context| {
                assert!(std::future::Future::poll(changed.as_mut(), context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            store.write().await.client.snippets = true;
            std::future::poll_fn(|context| {
                assert!(std::future::Future::poll(changed.as_mut(), context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            {
                let mut update = store.write_changes().await;
                if remove {
                    update.documents.remove(&uri);
                } else {
                    update.documents.insert(
                        uri.clone(),
                        super::super::workspace::Document::new(&uri, "Edited".into(), Some(2)),
                    );
                }
                assert!(store.document_input_current(&uri, &input));
                // Consume the early revision signal while the old pointer is
                // still published; dropping the guard must wake us again.
                std::future::poll_fn(|context| {
                    assert!(std::future::Future::poll(changed.as_mut(), context).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            }
            tokio::time::timeout(std::time::Duration::from_secs(1), changed)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn writes_invalidate_snapshots_even_when_replacing_the_workspace() {
        let store = WorkspaceStore::default();
        let revision = store.revision();
        let snapshot = store.read().await.clone();
        assert_eq!(store.revision(), revision);
        assert!(!store.write().await.client.snippets);
        assert_eq!(
            store.revision(),
            revision,
            "Reading through a write guard is not an input change"
        );
        {
            let mut workspace = store.write().await;
            workspace.client.snippets = true;
        }
        assert!(store.revision() > revision);
        assert!(!snapshot.client.snippets);
        let revision = store.revision();
        *store.write().await = Workspace::default();
        assert!(store.revision() > revision);
        assert!(!store.read().await.client.snippets);
    }
}
