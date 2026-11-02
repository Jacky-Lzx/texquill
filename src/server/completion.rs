//! Coalesced preparation and source refreshes never hold the input lock while
//! computing. A newer source revision discards the speculative result.
use super::{client::Client, queries::Queries};
use crate::{
    language::completion_cache::{Cache, Prepared},
    project::store::WorkspaceStore,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::task::AbortHandle;

#[derive(Default)]
struct State {
    generation: u64,
    task: Option<AbortHandle>,
    closed: bool,
    eager: bool,
    origins: VecDeque<lsp_types::Url>,
}
#[derive(Clone, Default)]
pub(crate) struct Completion {
    cache: Arc<Mutex<Cache>>,
    state: Arc<Mutex<State>>,
    requested: Arc<tokio::sync::Notify>,
}
impl Completion {
    pub(crate) fn covers(&self, ws: &crate::project::workspace::Workspace, epoch: u64) -> bool {
        let cache = self.cache.lock().unwrap();
        ws.pending_dependencies
            .iter()
            .all(|uri| cache.covers(ws, epoch, uri))
    }
    pub(crate) fn complete(
        &self,
        ws: &crate::project::workspace::Workspace,
        epoch: u64,
        uri: &lsp_types::Url,
        position: lsp_types::Position,
    ) -> Option<Vec<lsp_types::CompletionItem>> {
        self.cache
            .lock()
            .unwrap()
            .complete(ws, epoch, uri, position)
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        if let Some(task) = state.task.take() {
            task.abort();
        }
    }
    pub(crate) fn schedule(
        &self,
        workspace: WorkspaceStore,
        queries: Queries,
        client: Client,
        pending: Arc<AtomicBool>,
        eager: bool,
        origin: Option<lsp_types::Url>,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        state.eager |= eager;
        self.requested.notify_waiters();
        if let Some(origin) = origin {
            state.origins.retain(|uri| uri != &origin);
            state.origins.push_front(origin);
            state.origins.truncate(4);
        }
        if state.task.is_some() {
            return;
        }
        let this = self.clone();
        let task = tokio::spawn(async move {
            this.run(workspace, queries, client, pending).await;
        });
        state.task = Some(task.abort_handle());
    }
    async fn run(
        self,
        workspace: WorkspaceStore,
        queries: Queries,
        client: Client,
        pending: Arc<AtomicBool>,
    ) {
        loop {
            let (generation, eager, origins) = {
                let mut state = self.state.lock().unwrap();
                (
                    state.generation,
                    std::mem::take(&mut state.eager),
                    state.origins.clone(),
                )
            };
            if !eager {
                loop {
                    let revision = workspace.revision();
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(120)) => break,
                        _ = workspace.changed_since(revision) => {}
                    }
                }
            }
            let (snapshot, revision, epoch) = {
                let inputs = workspace.read_unindexed().await;
                (
                    inputs.clone(),
                    workspace.revision(),
                    workspace.completion_epoch(),
                )
            };
            let cache = self.cache.clone();
            let analysis = queries.analyses.clone();
            let result = queries
                .run_background(move |cancel| {
                    crate::analysis::AnalysisContext::new().run(|context| {
                        let mut snapshot = snapshot;
                        let updates = crate::analysis::AnalysisContext::for_workspace(
                            &snapshot,
                            epoch,
                            revision,
                            analysis.clone(),
                        );
                        let changed = updates.run(|_| snapshot.refresh_pending_dependencies());
                        let shared = crate::analysis::AnalysisContext::for_workspace(
                            &snapshot, epoch, revision, analysis,
                        );
                        let mut prepared = Vec::new();
                        for uri in &origins {
                            let Some(doc) = snapshot.documents.get(uri) else {
                                continue;
                            };
                            context.check()?;
                            if doc.version.is_some()
                                && !cache.lock().unwrap().covers(&snapshot, epoch, uri)
                            {
                                if let Some(entry) =
                                    shared.run(|_| Prepared::build(&snapshot, uri, epoch))
                                {
                                    prepared.push(entry);
                                }
                            }
                        }
                        context.check()?;
                        cancel.check()?;
                        Ok((snapshot, changed, prepared))
                    })
                })
                .await;
            let mut retry;
            let observed;
            if let Ok((snapshot, changed, prepared)) = result {
                let delivery = snapshot.diagnostic_delivery.clone();
                let mut inputs = workspace.write_prepared().await;
                let current =
                    workspace.revision() == revision && workspace.completion_epoch() == epoch;
                if current && changed && inputs.has_pending_dependencies() {
                    *inputs = snapshot;
                }
                for entry in prepared {
                    if entry.valid_for(&inputs, workspace.completion_epoch()) {
                        self.cache.lock().unwrap().insert(entry);
                    }
                }
                observed = workspace.revision();
                drop(inputs);
                if current && pending.swap(false, Ordering::SeqCst) {
                    delivery.publish(&client, vec![]).await;
                }
                retry = !current;
            } else {
                retry = workspace.revision() != revision;
                observed = workspace.revision();
            }
            let updated = workspace.update_signal();
            let requested = self.requested.notified();
            {
                let mut state = self.state.lock().unwrap();
                if state.closed {
                    state.task = None;
                    return;
                }
                retry |= state.generation != generation || workspace.revision() != observed;
            }
            if !retry {
                tokio::select! { _ = updated => {}, _ = requested => {} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::diagnostic::Delivery;
    use crate::project::workspace::{Document, Workspace};
    use lsp_types::Url;
    #[tokio::test]
    async fn prepared_replies_leave_project_refresh_to_the_coalesced_worker() {
        let uri = Url::parse("untitled:worker.tex").unwrap();
        let source = "\\documentclass{article}\n\\begin{document}\nReady\n\\end{document}\n";
        let mut inputs = Workspace::default();
        inputs.client.snippets = true;
        inputs.diagnostic_delivery = Delivery::new(true, true);
        inputs
            .documents
            .insert(uri.clone(), Document::new(&uri, source.into(), Some(1)));
        let workspace = WorkspaceStore::new(inputs);
        let queries = Queries::default();
        let completion = Completion::default();
        let (client, _outgoing) = Client::new();
        let pending = Arc::new(AtomicBool::new(false));
        completion.schedule(
            workspace.clone(),
            queries.clone(),
            client.clone(),
            pending.clone(),
            true,
            Some(uri.clone()),
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let inputs = workspace.read_unindexed().await;
                if completion.cache.lock().unwrap().covers(
                    &inputs,
                    workspace.completion_epoch(),
                    &uri,
                ) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("preparation did not finish");
        let current = source.replace("Ready", "% typing fast\n\\inp");
        {
            let mut inputs = workspace.write_changes().await;
            inputs
                .documents
                .insert(uri.clone(), Document::new(&uri, current.clone(), Some(2)));
            inputs.defer_dependencies(uri.clone());
        }
        pending.store(true, Ordering::SeqCst);
        {
            let inputs = workspace.read_unindexed().await;
            let position = inputs.documents[&uri]
                .text
                .position(current.find("\\inp").unwrap() + 4);
            let items = completion
                .complete(&inputs, workspace.completion_epoch(), &uri, position)
                .unwrap();
            assert!(items.iter().any(|item| item.label == "input"));
            assert!(
                inputs.has_pending_dependencies(),
                "completion synchronously refreshed the project"
            );
        }
        completion.schedule(
            workspace.clone(),
            queries.clone(),
            client,
            pending.clone(),
            false,
            None,
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if !workspace.read_unindexed().await.has_pending_dependencies()
                    && !pending.load(Ordering::SeqCst)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("deferred analysis never caught up");
        assert_eq!(workspace.read().await.documents[&uri].version, Some(2));
        {
            let mut inputs = workspace.write().await;
            inputs.client.snippets = false;
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let inputs = workspace.read_unindexed().await;
                if completion.cache.lock().unwrap().covers(
                    &inputs,
                    workspace.completion_epoch(),
                    &uri,
                ) {
                    break;
                }
                drop(inputs);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("metadata changes did not rewarm the idle service");
        completion.close();
        queries.shutdown().await;
    }
    #[tokio::test]
    async fn closing_preparation_cancels_queued_work_without_leaking_capacity() {
        let completion = Completion::default();
        let queries = Queries::default();
        let (client, _outgoing) = Client::new();
        completion.schedule(
            WorkspaceStore::default(),
            queries.clone(),
            client,
            Arc::new(AtomicBool::new(false)),
            true,
            None,
        );
        completion.close();
        tokio::time::timeout(std::time::Duration::from_secs(1), queries.shutdown())
            .await
            .unwrap();
        assert!(completion.state.lock().unwrap().closed);
    }
}
