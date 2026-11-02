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
            self.updated.notify_waiters();
        }
        &mut self.workspace
    }
}

#[derive(Clone, Default)]
pub struct WorkspaceStore {
    workspace: Arc<RwLock<Workspace>>,
    revision: Arc<AtomicU64>,
    updated: Arc<Notify>,
    queued_changes: Arc<AtomicUsize>,
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
        Self {
            workspace: Arc::new(RwLock::new(workspace)),
            revision: Arc::default(),
            updated: Arc::default(),
            queued_changes: Arc::default(),
        }
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, Workspace> {
        loop {
            let workspace = self.workspace.read().await;
            if !workspace.has_pending_dependencies() {
                return workspace;
            }
            drop(workspace);
            self.workspace.write().await.refresh_pending_dependencies();
        }
    }

    pub async fn read_owned(self) -> OwnedRwLockReadGuard<Workspace> {
        loop {
            let workspace = self.workspace.clone().read_owned().await;
            if !workspace.has_pending_dependencies() {
                return workspace;
            }
            drop(workspace);
            self.workspace.write().await.refresh_pending_dependencies();
        }
    }

    /// Invalidate old analyses on the first mutable access, including when
    /// reindexing replaces the entire Workspace value.
    pub async fn write(&self) -> WorkspaceUpdate<'_> {
        let mut workspace = self.workspace.write().await;
        workspace.refresh_pending_dependencies();
        self.update(workspace)
    }

    /// Apply sequential edit ranges without forcing intermediate project
    /// refreshes. All ordinary readers and writers still observe fresh inputs.
    pub(crate) async fn write_changes(&self) -> WorkspaceUpdate<'_> {
        self.update(self.workspace.write().await)
    }

    fn update<'a>(&'a self, workspace: RwLockWriteGuard<'a, Workspace>) -> WorkspaceUpdate<'a> {
        WorkspaceUpdate {
            workspace,
            revision: &self.revision,
            updated: &self.updated,
            changed: false,
        }
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
        let workspace = self.workspace.read().await;
        workspace.has_pending_dependencies() && self.has_queued_changes()
    }

    pub(crate) async fn obsolete_diagnostic_inputs(&self) -> bool {
        if !self.has_queued_changes() {
            return false;
        }
        // Inspect delivery metadata without flushing intermediate source
        // versions. Clients without refresh retain ordered snapshot reports.
        let workspace = self.workspace.read().await;
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
