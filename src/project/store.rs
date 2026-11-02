//! Runtime workspace mutation has one revision boundary for every consumer.
use super::workspace::Workspace;
use std::ops::{Deref, DerefMut};
use std::sync::{
    atomic::{AtomicU64, Ordering},
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
}

impl WorkspaceStore {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace: Arc::new(RwLock::new(workspace)),
            revision: Arc::default(),
            updated: Arc::default(),
        }
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, Workspace> {
        self.workspace.read().await
    }

    pub async fn read_owned(self) -> OwnedRwLockReadGuard<Workspace> {
        self.workspace.read_owned().await
    }

    /// Invalidate old analyses on the first mutable access, including when
    /// reindexing replaces the entire Workspace value.
    pub async fn write(&self) -> WorkspaceUpdate<'_> {
        let workspace = self.workspace.write().await;
        WorkspaceUpdate {
            workspace,
            revision: &self.revision,
            updated: &self.updated,
            changed: false,
        }
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
