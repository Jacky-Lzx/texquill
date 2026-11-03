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
    pub(crate) fn ensure_scheduled(
        &self,
        workspace: WorkspaceStore,
        queries: Queries,
        client: Client,
        pending: Arc<AtomicBool>,
    ) {
        if self.state.lock().unwrap().task.is_none() {
            self.schedule(workspace, queries, client, pending, false, None);
        }
    }

    pub(crate) fn remember_argument(&self, prepared: crate::language::argument_cache::Prepared) {
        self.cache.lock().unwrap().insert_argument(prepared);
    }
    pub(crate) fn remember_path(&self, prepared: crate::language::path_cache::Prepared) {
        self.cache.lock().unwrap().insert_paths(prepared);
    }
    #[cfg(test)]
    fn covers(&self, ws: &crate::project::workspace::Workspace, epoch: u64) -> bool {
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
            let eager = if eager {
                true
            } else {
                loop {
                    let requested = self.requested.notified();
                    if std::mem::take(&mut self.state.lock().unwrap().eager) {
                        break true;
                    }
                    let revision = workspace.revision();
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(120)) => break false,
                        _ = workspace.changed_since(revision) => {}
                        _ = requested => {}
                    }
                }
            };
            let (snapshot, revision, epoch, source_revision) = {
                let inputs = workspace.read_unindexed().await;
                (
                    inputs.clone(),
                    workspace.revision(),
                    workspace.completion_epoch(),
                    workspace.source_revision(),
                )
            };
            let before = snapshot.documents.revisions();
            let source_inputs = workspace.clone();
            let cache = self.cache.clone();
            let analysis = queries.analyses.clone();
            let work = move |cancel: &crate::analysis::AnalysisContext| {
                let epoch_inputs = source_inputs.clone();
                crate::analysis::AnalysisContext::cancellable(move || {
                    epoch_inputs.completion_epoch() != epoch
                })
                .run(|context| {
                    context.check()?;
                    let mut snapshot = snapshot;
                    if snapshot.has_pending_dependencies()
                        && source_inputs.source_revision() != source_revision
                    {
                        return Err(crate::rpc::Error::request_cancelled());
                    }
                    analysis.command_candidates().prepare(&snapshot);
                    let changed = if snapshot.has_pending_dependencies() {
                        let updates = crate::analysis::AnalysisContext::for_workspace(
                            &snapshot,
                            epoch,
                            revision,
                            analysis.clone(),
                        );
                        let inputs = source_inputs.clone();
                        crate::analysis::AnalysisContext::cancellable(move || {
                            inputs.source_revision() != source_revision
                                || inputs.completion_epoch() != epoch
                        })
                        .run(|context| {
                            context.check()?;
                            let changed = updates.run(|_| snapshot.refresh_pending_dependencies());
                            context.check()?;
                            Ok(changed)
                        })?
                    } else {
                        false
                    };
                    // Candidate preparation for an already propagated snapshot
                    // may survive ordinary typing. Its canonical-input proof is
                    // checked against the current text before insertion below.
                    let shared = crate::analysis::AnalysisContext::for_workspace(
                        &snapshot, epoch, revision, analysis,
                    );
                    let mut prepared = Vec::new();
                    let mut paths = Vec::new();
                    let mut arguments = Vec::new();
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
                        if doc.version.is_some() {
                            for position in crate::language::argument_cache::positions(doc) {
                                context.check()?;
                                if !cache
                                    .lock()
                                    .unwrap()
                                    .covers_argument(&snapshot, epoch, uri, position)
                                {
                                    if let Some(entry) = shared.run(|_| {
                                        crate::language::argument_cache::Prepared::build(
                                            &snapshot, uri, position, epoch,
                                        )
                                    }) {
                                        arguments.push(entry);
                                    }
                                }
                            }
                        }
                        if doc.version.is_some()
                            && !cache.lock().unwrap().covers_paths(&snapshot, epoch, uri)
                        {
                            if let Some(entry) = shared.run(|_| {
                                crate::language::path_cache::Prepared::build(&snapshot, uri, epoch)
                            }) {
                                paths.push(entry);
                            }
                        }
                    }
                    context.check()?;
                    cancel.check()?;
                    Ok((snapshot, changed, prepared, paths, arguments))
                })
            };
            // A native input request needs its certificate during typing.
            // Automatic views must not keep that single coalesced job behind
            // the background semaphore. Ordinary preparation remains idle work.
            let result = if eager {
                queries.run(work).await
            } else {
                let requested = self.requested.notified();
                if self.state.lock().unwrap().eager {
                    continue;
                }
                tokio::select! {
                    value = queries.run_background(work) => value,
                    _ = requested => continue,
                }
            };
            let mut retry;
            let observed;
            if let Ok((snapshot, changed, prepared, paths, arguments)) = result {
                let delivery = snapshot.diagnostic_delivery.clone();
                let mut inputs = workspace.write_prepared().await;
                let current = workspace.source_revision() == source_revision
                    && workspace.completion_epoch() == epoch
                    && before.current(&inputs.documents);
                if current && changed && inputs.has_pending_dependencies() {
                    // Preparation owns source propagation, not concurrent build
                    // diagnostics/PDF history or independent project metadata.
                    inputs.documents = snapshot.documents;
                    inputs.pending_dependencies = snapshot.pending_dependencies;
                    inputs.index_truncated = snapshot.index_truncated;
                }
                for entry in prepared {
                    if entry.valid_for(&inputs, workspace.completion_epoch()) {
                        self.cache.lock().unwrap().insert(entry);
                    }
                }
                for entry in paths {
                    if entry.valid_for(&inputs, workspace.completion_epoch()) {
                        self.cache.lock().unwrap().insert_paths(entry);
                    }
                }
                for entry in arguments {
                    if entry.valid_for(&inputs, workspace.completion_epoch()) {
                        self.cache.lock().unwrap().insert_argument(entry);
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
    async fn requested_path_preparation_does_not_wait_for_automatic_view_capacity() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() = true;
                self.0 .1.notify_all();
            }
        }
        let release = Release(gate.clone());
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queries = Queries::default();
        let mut holders = Vec::new();
        for _ in 0..3 {
            let queries = queries.clone();
            let gate = gate.clone();
            let started = started.clone();
            holders.push(tokio::spawn(async move {
                queries
                    .run_background(move |_| {
                        started.fetch_add(1, Ordering::SeqCst);
                        let (mutex, condition) = &*gate;
                        let _guard = condition
                            .wait_while(mutex.lock().unwrap(), |open| !*open)
                            .unwrap();
                        Ok(())
                    })
                    .await
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while started.load(Ordering::SeqCst) != 3 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.tex");
        let uri = Url::from_file_path(&path).unwrap();
        let source =
            "\\documentclass{article}\n\\begin{document}\n\\input{missing/}\n\\end{document}\n";
        std::fs::write(path, source).unwrap();
        let mut inputs = Workspace::default();
        inputs.open(uri.clone(), source.into(), 1);
        let workspace = WorkspaceStore::new(inputs);
        let completion = Completion::default();
        let (client, _outgoing) = Client::new();
        completion.schedule(
            workspace.clone(),
            queries,
            client,
            Arc::new(AtomicBool::new(false)),
            true,
            Some(uri.clone()),
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let inputs = workspace.read_unindexed().await;
                if completion.cache.lock().unwrap().covers_paths(
                    &inputs,
                    workspace.completion_epoch(),
                    &uri,
                ) {
                    break;
                }
                drop(inputs);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("automatic views blocked requested path preparation");
        completion.close();
        drop(release);
        for holder in holders {
            holder.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn native_path_preparation_keeps_prefix_edits_unindexed_and_rejects_new_source_edges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Tex")).unwrap();
        std::fs::write(
            dir.path().join("Tex/0_Abstract.tex"),
            r"\newcommand\frominput[2]{#1#2}",
        )
        .unwrap();
        let source =
            "\\documentclass{article}\n\\begin{document}\n\\input{Tex/}\n\\end{document}\n";
        let path = dir.path().join("main.tex");
        std::fs::write(&path, source).unwrap();
        let uri = Url::from_file_path(path).unwrap();
        let mut inputs = Workspace::default();
        inputs.client.snippets = true;
        inputs.diagnostic_delivery = Delivery::new(true, true);
        inputs.open(uri.clone(), source.into(), 1);
        let workspace = WorkspaceStore::new(inputs);
        let completion = Completion::default();
        let (client, _outgoing) = Client::new();
        completion.schedule(
            workspace.clone(),
            Queries::default(),
            client,
            Arc::new(AtomicBool::new(false)),
            true,
            Some(uri.clone()),
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let inputs = workspace.read_unindexed().await;
                if completion.cache.lock().unwrap().covers_paths(
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
        .expect("path preparation did not finish");
        for (index, prefix) in ["Tex/0", "Tex/0_Ab", "Tex/0_Abstract.tex"]
            .into_iter()
            .enumerate()
        {
            let current = source.replace("Tex/", prefix);
            {
                let mut inputs = workspace.write_changes().await;
                let doc =
                    inputs.documents[&uri].with_text(&uri, current.clone(), Some(index as i32 + 2));
                inputs.documents.insert(uri.clone(), doc);
                inputs.defer_dependencies(uri.clone());
            }
            let inputs = workspace.read_unindexed().await;
            let position = inputs.documents[&uri]
                .text
                .position(current.find(prefix).unwrap() + prefix.len());
            let reply = completion.complete(&inputs, workspace.completion_epoch(), &uri, position);
            if prefix.ends_with(".tex") {
                assert!(
                    reply.is_none(),
                    "a resolved input must refresh project semantics"
                );
                assert!(!completion.covers(&inputs, workspace.completion_epoch()));
            } else {
                assert!(reply
                    .unwrap()
                    .iter()
                    .any(|item| item.label == "Tex/0_Abstract.tex"));
                assert!(completion.covers(&inputs, workspace.completion_epoch()));
                assert!(
                    inputs.has_pending_dependencies(),
                    "completion flushed dependency propagation"
                );
            }
        }
        let inputs = workspace.read().await;
        assert!(inputs.documents.values().any(|doc| doc
            .syntax
            .definitions
            .iter()
            .any(|entry| entry.name == "frominput")));
        drop(inputs);
        completion.close();
    }

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
