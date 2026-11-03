//! Build queueing and connection-owned coordination.
use crate::project::store::WorkspaceStore;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use crate::rpc::{Error, Result};
use crate::server::client::Client;
use crate::{
    compilation::{build_log, file_checks::InputSnapshot},
    config::BuildConfig,
};
use lsp_types::*;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::compilation::build::{reject_outdated_pdf, run_with_mode, Outcome};
mod completion;
mod preparation;
mod publication;

pub enum BuildStarted {}
impl notification::Notification for BuildStarted {
    type Params = Value;
    const METHOD: &'static str = "latex/buildStarted";
}
pub enum BuildFinished {}
impl notification::Notification for BuildFinished {
    type Params = Value;
    const METHOD: &'static str = "latex/buildFinished";
}

#[derive(Default)]
struct Record {
    active: Option<(u64, Arc<AtomicBool>)>,
    result: Option<Value>,
    queued: Option<QueuedBuild>,
    rebuild: bool,
    preparing: bool,
    // Saves and passive file events recheck the new inputs after preparation.
    preparing_saved: Option<Url>,
    cancel_generation: u64,
    inputs: BTreeMap<Url, InputSnapshot>,
    configuration: Option<Value>,
    checks: Arc<crate::compilation::file_checks::Snapshot>,
    artifact: Option<PathBuf>,
    // Preserve the path passed to the compiler as well as initial physical
    // artifact names: a directory alias may be retargeted while it runs.
    output_directory: Option<PathBuf>,
    auxiliary_directory: Option<PathBuf>,
    auxiliary_artifact: Option<PathBuf>,
    index_artifacts: BTreeSet<PathBuf>,
    bibliography_output_directory: Option<PathBuf>,
}

fn artifact_binding(path: PathBuf) -> PathBuf {
    path.parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(path.file_name())
        .map_or_else(
            || crate::project::workspace::normalize_path(&path),
            |(parent, name)| parent.join(name),
        )
}

impl Record {
    fn current_artifact(&self, path: &Path) -> Option<PathBuf> {
        Some(
            self.output_directory
                .as_ref()?
                .canonicalize()
                .ok()?
                .join(path.file_name()?),
        )
    }

    fn shares_pdf(&self, artifact: &Path) -> bool {
        self.artifact.as_ref().is_some_and(|previous| {
            previous == artifact || self.current_artifact(previous).as_deref() == Some(artifact)
        })
    }

    fn shared_index(&self, candidates: &BTreeSet<PathBuf>) -> Option<PathBuf> {
        self.index_artifacts.iter().find_map(|path| {
            [
                Some(path.clone()),
                self.auxiliary_directory.as_ref().and_then(|directory| {
                    Some(directory.canonicalize().ok()?.join(path.file_name()?))
                }),
            ]
            .into_iter()
            .flatten()
            .find(|path| candidates.contains(path))
        })
    }
}

struct QueuedBuild {
    uri: Url,
    rebuild: bool,
}

pub(super) struct WatchedBuild {
    root: Url,
    queued: QueuedBuild,
    generation: u64,
}

#[derive(Clone)]
pub struct BuildManager {
    client: Client,
    workspace: WorkspaceStore,
    state: Arc<Mutex<BTreeMap<Url, Record>>>,
    sequence: Arc<AtomicU64>,
    idle: Arc<Notify>,
    closing: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    tasks: Arc<AtomicUsize>,
}

struct BuildInput {
    root: Url,
    root_path: PathBuf,
    prepared_documents: crate::project::documents::Updates,
    snapshots: BTreeMap<Url, (Option<i32>, String)>,
    file_snapshots: BTreeMap<Url, InputSnapshot>,
    open_aliases: BTreeMap<Url, Url>,
    config: BuildConfig,
    progress_supported: bool,
    project: BTreeSet<Url>,
    static_code_inputs: BTreeSet<Url>,
    checks: Arc<crate::compilation::file_checks::Snapshot>,
    rebuild: bool,
}

struct TaskGuard {
    tasks: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.tasks.fetch_sub(1, Ordering::SeqCst);
        self.idle.notify_waiters();
    }
}

struct PreparationGuard {
    manager: BuildManager,
    root: Url,
    build_id: u64,
    cancel: Arc<AtomicBool>,
    launched: bool,
}

impl Drop for PreparationGuard {
    fn drop(&mut self) {
        if self.launched {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(record) = self.manager.state.lock().unwrap().get_mut(&self.root) {
            if record
                .active
                .as_ref()
                .is_some_and(|(id, _)| *id == self.build_id)
            {
                record.active = None;
                record.preparing = false;
                record.preparing_saved = None;
                record.queued = None;
            }
        }
        self.manager.tasks.fetch_sub(1, Ordering::SeqCst);
        self.manager.idle.notify_waiters();
    }
}

impl BuildManager {
    pub fn new(client: Client, workspace: WorkspaceStore) -> Self {
        Self {
            client,
            workspace,
            state: Arc::default(),
            sequence: Arc::new(AtomicU64::new(1)),
            idle: Arc::default(),
            closing: Arc::default(),
            disconnected: Arc::default(),
            tasks: Arc::default(),
        }
    }

    pub async fn start(&self, uri: Url) -> Result<Value> {
        self.start_request(uri, None, false, None).await
    }

    pub async fn rebuild(&self, uri: Url) -> Result<Value> {
        self.start_request(uri, None, true, None).await
    }

    pub(crate) async fn clean(&self, uri: Url, artifacts: bool, dry_run: bool) -> Result<Value> {
        let mut workspace = self.workspace.write().await;
        let root = workspace
            .root_document(&uri)
            .map_err(Error::invalid_params)?;
        workspace.refresh_project(&root);
        let config = workspace.build_config(&root);
        config
            .validate_resolution()
            .map_err(Error::invalid_params)?;
        let root_path = root
            .to_file_path()
            .map_err(|_| Error::invalid_params("Cleanup requires a local file"))?;
        let physical = |path: PathBuf| {
            path.canonicalize()
                .ok()
                .or_else(|| {
                    path.parent()?
                        .canonicalize()
                        .ok()
                        .map(|parent| parent.join(path.file_name().unwrap()))
                })
                .unwrap_or_else(|| crate::project::workspace::normalize_path(&path))
        };
        let artifact = config
            .artifact_path(&root_path, "pdf")
            .map(artifact_binding);
        let index_artifacts: BTreeSet<_> =
            crate::compilation::index_processing::outputs(&root_path, &config)
                .into_iter()
                .map(artifact_binding)
                .collect();
        let directories: Vec<_> = [
            config.output_path(&root_path),
            config.auxiliary_path(&root_path),
            config.final_output_path(&root_path),
        ]
        .into_iter()
        .flatten()
        .map(&physical)
        .collect();
        let directory = config.output_path(&root_path).map(physical);
        let (result, affected) = {
            // Serialize with build reservation and queued generation checks.
            // No process is cancelled implicitly by a cleanup request.
            let mut state = self.state.lock().unwrap();
            if self.closing.load(Ordering::SeqCst) {
                return Err(Error::invalid_request());
            }
            for (owner, record) in state.iter() {
                let shared = artifact
                    .as_ref()
                    .is_some_and(|path| record.shares_pdf(path))
                    || record.shared_index(&index_artifacts).is_some();
                let same_directory = record
                    .artifact
                    .as_ref()
                    .and_then(|path| path.parent())
                    .zip(directory.as_deref())
                    .is_some_and(|(a, b)| a == b)
                    || record.output_directory.as_ref().is_some_and(|path| {
                        directory
                            .as_ref()
                            .is_some_and(|directory| physical(path.clone()) == *directory)
                    })
                    || record
                        .index_artifacts
                        .iter()
                        .any(|path| directory.as_ref().is_some_and(|dir| path.starts_with(dir)));
                if record.active.is_some()
                    && (owner == &root
                        || shared
                        || same_directory
                        || [&record.output_directory, &record.auxiliary_directory]
                            .into_iter()
                            .flatten()
                            .any(|path| directories.contains(&physical(path.clone()))))
                {
                    return Err(Error::invalid_params(format!(
                        "Wait for the active build of {owner} before cleaning its output directory"
                    )));
                }
                if owner != &root && shared {
                    return Err(Error::invalid_params(format!("Build artifacts are shared with {owner}; choose a separate jobName or outputDirectory before cleaning")));
                }
            }
            let plan = crate::compilation::build_clean::Plan::prepare(
                &workspace, &root, &config, artifacts,
            )
            .map_err(Error::invalid_params)?;
            let mut result = plan.execute(dry_run).map_err(Error::invalid_params)?;
            result["rootUri"] = json!(root);
            result["outputDirectory"] = json!(Url::from_directory_path(&plan.directory).ok());
            result["artifacts"] = json!(artifacts);
            let mut affected = BTreeSet::new();
            if !dry_run {
                let record = state.entry(root.clone()).or_default();
                record.queued = None;
                record.cancel_generation = record.cancel_generation.wrapping_add(1);
                record.result = None;
                record.inputs.clear();
                record.checks = Arc::default();
                if let Some(old) = workspace.project.build_diagnostics.remove(&root) {
                    affected.extend(old.into_keys());
                }
                workspace.project.build_diagnostic_sources.remove(&root);
                workspace.project.build_checks.remove(&root);
                if artifacts {
                    workspace.project.pdf_checks.remove(&root);
                }
                affected.insert(root.clone());
                affected.extend(
                    workspace
                        .documents
                        .iter()
                        .filter_map(|(uri, doc)| doc.version.is_some().then_some(uri.clone())),
                );
            }
            (result, affected)
        };
        let delivery = workspace.diagnostic_delivery.clone();
        let notifications = if delivery.is_pull() {
            vec![]
        } else {
            workspace.diagnostic_snapshots(affected)
        };
        drop(workspace);
        if !dry_run {
            delivery.publish(&self.client, notifications).await;
        }
        Ok(result)
    }

    pub(crate) async fn rebuild_guarded(
        &self,
        uri: Url,
        guard: crate::editing::RebuildGuard,
    ) -> Result<Value> {
        self.start_request(uri, None, true, Some(guard)).await
    }

    async fn while_open<F: Future>(&self, action: F) -> Option<F::Output> {
        if self.disconnected.load(Ordering::SeqCst) {
            return None;
        }
        let closed = async {
            loop {
                let changed = self.idle.notified();
                if self.disconnected.load(Ordering::SeqCst) {
                    return;
                }
                changed.await;
            }
        };
        // EOF can stop the transport from consuming outgoing notifications.
        // Cancellation must still finish when that channel is backpressured.
        tokio::select! {
            output = action => Some(output),
            _ = closed => None,
        }
    }

    async fn start_request(
        &self,
        uri: Url,
        queued_generation: Option<(Url, u64)>,
        rebuild: bool,
        guard: Option<crate::editing::RebuildGuard>,
    ) -> Result<Value> {
        // Capture the ordered editor state without propagating dependencies on
        // the live workspace. The compiler reads saved bytes; subsequent edits
        // are checked when results are published, not copied over this snapshot.
        let workspace = self.workspace.read_unindexed().await.clone();
        let root = workspace
            .root_document(&uri)
            .map_err(Error::invalid_params)?;
        if queued_generation
            .as_ref()
            .is_some_and(|(owner, _)| owner != &root)
        {
            return Ok(json!({"status":"cancelled", "rootUri":queued_generation.unwrap().0}));
        }
        if guard.as_ref().is_some_and(|guard| {
            root != uri || !crate::editing::valid_rebuild_guard(&workspace, &root, guard)
        }) {
            return Err(Error::invalid_params(
                "Rebuild action is outdated; request code actions again",
            ));
        }
        let build_id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.state.lock().unwrap();
            if self.closing.load(Ordering::SeqCst) {
                return Err(Error::invalid_request());
            }
            if queued_generation
                .as_ref()
                .is_some_and(|(owner, generation)| {
                    state
                        .get(owner)
                        .is_none_or(|record| record.cancel_generation != *generation)
                })
            {
                return Ok(json!({"status":"cancelled", "rootUri":root}));
            }
            let record = state.entry(root.clone()).or_default();
            if let Some((id, _)) = &record.active {
                if rebuild && (queued_generation.is_none() || !record.rebuild) {
                    record.queued = Some(QueuedBuild { uri, rebuild: true });
                }
                return Ok(json!({"status":"running", "buildId":id, "rootUri":root,
                    "queued":record.queued.is_some(), "rebuild":record.rebuild,
                    "queuedRebuild":record.queued.as_ref().is_some_and(|queued| queued.rebuild)}));
            }
            if rebuild {
                record.queued = None;
                if queued_generation.is_none() {
                    record.cancel_generation = record.cancel_generation.wrapping_add(1);
                }
            }
            record.active = Some((build_id, cancel.clone()));
            record.preparing = true;
            record.preparing_saved = None;
            record.rebuild = rebuild;
            self.tasks.fetch_add(1, Ordering::SeqCst);
        }
        let original_documents = workspace.documents.clone();
        let mut preparation = PreparationGuard {
            manager: self.clone(),
            root: root.clone(),
            build_id,
            cancel: cancel.clone(),
            launched: false,
        };
        crate::server::dispatch::release();
        let worker_root = root.clone();
        let worker_cancel = cancel.clone();
        self.tasks.fetch_add(1, Ordering::SeqCst);
        let worker_task = TaskGuard {
            tasks: self.tasks.clone(),
            idle: self.idle.clone(),
        };
        let mut input = tokio::task::spawn_blocking(move || {
            let _task = worker_task;
            preparation::prepare(
                workspace,
                original_documents,
                worker_root,
                rebuild,
                &worker_cancel,
            )
        })
        .await
        .map_err(|error| Error::invalid_params(error.to_string()))??;
        // Root/config changes can invalidate a pending launch. Ordinary typing
        // may proceed: the build retains its original saved-input snapshots.
        if !input.prepared_documents.is_empty() {
            let mut current = self.workspace.write_prepared().await;
            current
                .documents
                .apply_updates(std::mem::take(&mut input.prepared_documents));
        }
        let current = self.workspace.read_unindexed().await.clone();
        let expected = serde_json::to_value(input.config.unresolved()).unwrap();
        let current_root = root.clone();
        self.tasks.fetch_add(1, Ordering::SeqCst);
        let worker_task = TaskGuard {
            tasks: self.tasks.clone(),
            idle: self.idle.clone(),
        };
        tokio::task::spawn_blocking(move || {
            let _task = worker_task;
            if current.root_document(&uri).ok().as_ref() != Some(&current_root) {
                return Err(Error::invalid_params(
                    "Root changed during build preparation; build again",
                ));
            }
            if serde_json::to_value(current.build_config(&current_root).unresolved()).unwrap()
                != expected
            {
                return Err(Error::invalid_params(
                    "Build settings changed during build preparation; build again",
                ));
            }
            Ok(())
        })
        .await
        .map_err(|error| Error::invalid_params(error.to_string()))??;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::request_cancelled());
        }
        let BuildInput {
            config,
            root_path,
            project,
            file_snapshots,
            checks,
            ..
        } = &input;
        let artifact = config.artifact_path(root_path, "pdf").map(artifact_binding);
        let index_artifacts: BTreeSet<_> =
            crate::compilation::index_processing::outputs(root_path, config)
                .into_iter()
                .map(artifact_binding)
                .collect();
        {
            let mut state = self.state.lock().unwrap();
            if self.closing.load(Ordering::SeqCst) {
                return Err(Error::invalid_request());
            }
            if let Some((owner, _)) = state.iter().find(|(owner, record)| {
                *owner != &root
                    && record.active.is_some()
                    && (artifact
                        .as_ref()
                        .is_some_and(|path| record.shares_pdf(path))
                        || config.artifact_path(root_path, "log").is_some_and(|path| {
                            record.auxiliary_artifact.as_ref().is_some_and(|previous| {
                                artifact_binding(previous.clone()) == artifact_binding(path)
                            })
                        }))
            }) {
                return Err(Error::invalid_params(format!(
                    "Another active build writes the same output file: {}; wait for {} to finish or choose a different jobName or outputDirectory",
                    artifact.as_ref().unwrap().display(), owner
                )));
            }
            if let Some((owner, path)) = state.iter().find_map(|(owner, record)| {
                if owner == &root || record.active.is_none() {
                    return None;
                }
                record
                    .shared_index(&index_artifacts)
                    .map(|path| (owner, path))
            }) {
                return Err(Error::invalid_params(format!("Another active build writes the same index output file: {}; wait for {} to finish or choose a different index jobName or outputDirectory",
                    path.display(), owner)));
            }
            let record = state.entry(root.clone()).or_default();
            if cancel.load(Ordering::Relaxed)
                || !record
                    .active
                    .as_ref()
                    .is_some_and(|(id, _)| *id == build_id)
            {
                return Err(Error::request_cancelled());
            }
            record.preparing = false;
            record.configuration = Some(serde_json::to_value(config.unresolved()).unwrap());
            record.inputs = project
                .iter()
                .map(|uri| {
                    let snapshot = file_snapshots
                        .get(uri)
                        .cloned()
                        .unwrap_or(InputSnapshot::File(None));
                    (uri.clone(), snapshot)
                })
                .collect();
            record.checks = Arc::new(checks.with_inputs(file_snapshots.clone()));
            record.artifact = artifact;
            record.output_directory = config.final_output_path(root_path);
            record.auxiliary_directory = config.auxiliary_path(root_path);
            record.auxiliary_artifact = config.artifact_path(root_path, "log");
            record.index_artifacts = index_artifacts;
            record.bibliography_output_directory =
                config.bibliography_tool.as_ref().and_then(|_| {
                    config
                        .auxiliary_path(root_path)
                        .map(|path| crate::project::workspace::normalize_path(&path))
                });
        }
        let saved = self
            .state
            .lock()
            .unwrap()
            .get_mut(&root)
            .and_then(|record| record.preparing_saved.take());
        self.launch(input, build_id, cancel);
        preparation.launched = true;
        if let Some(uri) = saved {
            Box::pin(self.on_save(uri)).await?;
        }
        Ok(json!({"status":"started", "buildId":build_id, "rootUri":root, "rebuild":rebuild}))
    }

    /// Queue changed saved inputs once; identical saves reuse the active build.
    pub async fn on_save(&self, uri: Url) -> Result<()> {
        let workspace = self.workspace.read_unindexed().await;
        let root = workspace
            .root_document(&uri)
            .map_err(Error::invalid_params)?;
        {
            let mut state = self.state.lock().unwrap();
            if let Some(record) = state.get_mut(&root) {
                if let Some((_, cancel)) = &record.active {
                    if record.preparing {
                        record.preparing_saved = Some(uri);
                        return Ok(());
                    }
                    if !cancel.load(Ordering::Relaxed)
                        // Compare compiler input bytes, not editor versions or
                        // modification times, which can change on a plain save.
                        && (!record.inputs.contains_key(&uri)
                            || record.inputs.iter().any(|(uri, input)| input.changed(uri))
                            || record.checks.changed_path().is_some()
                            || record.checks.image_changed(|_| true)
                            || record.configuration.as_ref()
                                != Some(
                                    &serde_json::to_value(
                                        workspace.build_config(&root).unresolved(),
                                    )
                                    .unwrap(),
                                ))
                    {
                        let rebuild = record.queued.as_ref().is_some_and(|queued| queued.rebuild);
                        record.queued = Some(QueuedBuild { uri, rebuild });
                    }
                    return Ok(());
                }
            }
        }
        drop(workspace);
        self.start(uri).await.map(|_| ())
    }

    pub fn cancel(&self, root: Option<&Url>) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut count = 0;
        for (uri, record) in state.iter_mut() {
            if root.is_none_or(|root| root == uri) {
                record.queued = None;
                record.cancel_generation = record.cancel_generation.wrapping_add(1);
                if let Some((_, cancel)) = &record.active {
                    cancel.store(true, Ordering::Relaxed);
                    count += 1;
                }
            }
        }
        count
    }

    pub fn cancel_progress(&self, token: &ProgressToken) {
        let NumberOrString::String(token) = token else {
            return;
        };
        let Some(id) = token
            .strip_prefix("latex-build-")
            .and_then(|id| id.parse::<u64>().ok())
        else {
            return;
        };
        for record in self.state.lock().unwrap().values_mut() {
            if let Some((active, cancel)) = &record.active {
                if *active == id {
                    record.queued = None;
                    record.cancel_generation = record.cancel_generation.wrapping_add(1);
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn status(&self, root: &Url) -> Value {
        let state = self.state.lock().unwrap();
        match state.get(root) {
            Some(Record {
                active: Some((id, _)),
                queued,
                rebuild,
                ..
            }) => {
                json!({"status":"running", "buildId":id, "rootUri":root, "queued":queued.is_some(), "rebuild":rebuild, "queuedRebuild":queued.as_ref().is_some_and(|queued| queued.rebuild)})
            }
            Some(Record {
                result: Some(result),
                ..
            }) => result.clone(),
            _ => json!({"status":"idle", "rootUri":root}),
        }
    }

    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.idle.notify_waiters();
        self.cancel(None);
        loop {
            let finished = self.idle.notified();
            if self.tasks.load(Ordering::SeqCst) == 0 {
                break;
            }
            finished.await;
        }
    }

    pub fn disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        self.idle.notify_waiters();
    }

    pub async fn files_renamed(&self, renames: &crate::editing::file_operations::Renames) {
        self.cancel_changed_inputs(|uri| renames.affects(uri), false)
            .await;
    }

    pub async fn files_changed(&self, changes: &crate::editing::file_operations::FileChanges) {
        self.cancel_changed_inputs(|uri| changes.affects(uri), false)
            .await;
    }

    pub(super) async fn watched_files_changed(
        &self,
        changes: &crate::editing::file_operations::FileChanges,
    ) -> Vec<WatchedBuild> {
        self.cancel_changed_inputs(|uri| changes.affects(uri), true)
            .await
    }

    pub(super) async fn resume_watched(&self, builds: Vec<WatchedBuild>) {
        for WatchedBuild {
            root,
            queued,
            generation,
        } in builds
        {
            if !self.closing.load(Ordering::SeqCst)
                && (queued.rebuild
                    || self
                        .workspace
                        .read()
                        .await
                        .settings(&queued.uri)
                        .build
                        .on_save)
            {
                if let Err(error) = self
                    .start_request(queued.uri, Some((root, generation)), queued.rebuild, None)
                    .await
                {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("Queued compilation skipped: {}", error.message),
                        )
                        .await;
                }
            }
        }
    }

    async fn cancel_changed_inputs(
        &self,
        affects: impl Fn(&Url) -> bool,
        verify_inputs: bool,
    ) -> Vec<WatchedBuild> {
        let mut builds = BTreeSet::new();
        let mut followups = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            for (root, record) in state.iter_mut() {
                if verify_inputs && record.preparing {
                    // These checks still describe the previous compilation.
                    // A delayed save notification must not cancel a new run
                    // before its own input snapshots have been installed.
                    record.preparing_saved.get_or_insert_with(|| root.clone());
                    continue;
                }
                // TeX may rewrite a database itself (e.g. filecontents).
                // Watch notifications cannot identify the writer. For explicit
                // pipelines, validate databases inside the output directory at
                // completion using recorder/backend snapshots instead.
                let defer_generated_database = |uri: &Url| {
                    verify_inputs
                        && record.active.is_some()
                        && uri.to_file_path().is_ok_and(|path| {
                            path.extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("bib"))
                                && record.bibliography_output_directory.as_ref().is_some_and(
                                    |directory| {
                                        crate::project::workspace::normalize_path(&path)
                                            .starts_with(directory)
                                    },
                                )
                        })
                };
                let image_changed = record.checks.image_changed(&affects);
                if (!verify_inputs && affects(root))
                    || image_changed
                    || record.inputs.iter().any(|(uri, snapshot)| {
                        !defer_generated_database(uri)
                            && affects(uri)
                            && (!verify_inputs || snapshot.changed(uri))
                    })
                    || (record
                        .checks
                        .watched()
                        .iter()
                        .any(|uri| !defer_generated_database(uri) && affects(uri))
                        && record.checks.changed_path().is_some())
                {
                    let queued = record.queued.take();
                    record.cancel_generation = record.cancel_generation.wrapping_add(1);
                    // A watcher invalidates this run, not the user's request
                    // for newer saved inputs. Resume only after file refresh,
                    // with a generation guard against explicit cancellation.
                    if verify_inputs && (queued.is_some() || image_changed) {
                        followups.push(WatchedBuild {
                            root: root.clone(),
                            queued: queued.unwrap_or_else(|| QueuedBuild {
                                uri: root.clone(),
                                rebuild: false,
                            }),
                            generation: record.cancel_generation,
                        });
                    }
                    if let Some((id, cancel)) = &record.active {
                        cancel.store(true, Ordering::Relaxed);
                        builds.insert(*id);
                    }
                }
            }
        }
        loop {
            let finished = self.idle.notified();
            if !self.state.lock().unwrap().values().any(|record| {
                record
                    .active
                    .as_ref()
                    .is_some_and(|(id, _)| builds.contains(id))
            }) {
                break;
            }
            finished.await;
        }
        followups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::workspace::Workspace;

    #[cfg(unix)]
    fn gated_preparation() -> (BuildManager, tempfile::TempDir, Url, String) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap();
        let main = path.join("main.tex");
        let source =
            "\\documentclass{article}\n\\begin{document}\n\\input{missing}\n\\end{document}\n"
                .to_string();
        std::fs::write(&main, &source).unwrap();
        std::fs::write(path.join(".latexmkrc"), "$out_dir = dynamic_output();\n").unwrap();
        let compiler = path.join("latexmk");
        std::fs::write(&compiler, r#"#!/bin/sh
if [ "$1" = '-dir-report-only' ]; then
  touch preparing
  while [ ! -e release ]; do sleep 0.01; done
  printf '\nTEXQUILL_CONFIGURATION {"output":"build","auxiliary":"build","final_output":"build","mode":1,"sources":[]}\n'
  exit 0
fi
touch compiled
for arg do case "$arg" in -outdir=*) out=${arg#-outdir=} ;; esac; done
mkdir -p "$out"
printf '%%PDF-1.4\n' > "$out/main.pdf"
printf 'This is a controlled build.\n' > "$out/main.log"
printf 'INPUT main.tex\n' > "$out/main.fls"
"#).unwrap();
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755)).unwrap();
        let uri = Url::from_file_path(main).unwrap();
        let mut workspace = Workspace::default();
        workspace.config.distribution.enabled = false;
        workspace.config.build.executable = compiler.to_string_lossy().into();
        workspace.open(uri.clone(), source.clone(), 1);
        let (client, mut outgoing) = Client::new();
        // These tests inspect state, rather than backpressuring publication.
        tokio::spawn(async move { while outgoing.recv().await.is_some() {} });
        (
            BuildManager::new(client, WorkspaceStore::new(workspace)),
            directory,
            uri,
            source,
        )
    }

    #[cfg(unix)]
    async fn wait_preparing(directory: &Path) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !directory.join("preparing").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    async fn wait_build(manager: &BuildManager) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while manager.tasks.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn preparation_releases_ordered_edits_and_preserves_the_saved_snapshot() {
        let (manager, directory, uri, source) = gated_preparation();
        let mut queue = crate::server::dispatch::RequestQueue::default();
        let worker = manager.clone();
        let main = uri.clone();
        let pending = tokio::spawn(queue.ordered(async move { worker.start(main).await }));
        wait_preparing(directory.path()).await;
        let edited = source.replace("missing", "other");
        let workspace = manager.workspace.clone();
        let edit_uri = uri.clone();
        let edit = edited.clone();
        tokio::time::timeout(
            Duration::from_secs(1),
            queue.ordered(async move {
                let mut workspace = workspace.write_changes().await;
                let doc =
                    workspace
                        .documents
                        .get(&edit_uri)
                        .unwrap()
                        .with_text(&edit_uri, edit, Some(2));
                workspace.documents.insert(edit_uri, doc);
            }),
        )
        .await
        .expect("build preparation held the ordered turn or workspace lock");
        std::fs::write(directory.path().join("release"), "").unwrap();
        assert_eq!(pending.await.unwrap().unwrap()["status"], "started");
        wait_build(&manager).await;
        assert_eq!(
            manager
                .workspace
                .read_unindexed()
                .await
                .documents
                .get(&uri)
                .unwrap()
                .text
                .source,
            edited
        );
        assert_eq!(manager.status(&uri)["stale"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn cancelling_preparation_reaps_workers_without_launching_the_compiler() {
        let (manager, directory, uri, _) = gated_preparation();
        let worker = manager.clone();
        let main = uri.clone();
        let pending = tokio::spawn(async move { worker.start(main).await });
        wait_preparing(directory.path()).await;
        assert_eq!(manager.cancel(Some(&uri)), 1);
        assert!(pending.await.unwrap().is_err());
        manager.shutdown().await;
        assert_eq!(manager.tasks.load(Ordering::SeqCst), 0);
        assert!(!directory.path().join("compiled").exists());
        assert_ne!(manager.status(&uri)["status"], "running");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn identical_saves_during_preparation_do_not_queue_a_followup() {
        let (manager, directory, uri, _) = gated_preparation();
        let worker = manager.clone();
        let main = uri.clone();
        let pending = tokio::spawn(async move { worker.start(main).await });
        wait_preparing(directory.path()).await;
        manager.on_save(uri.clone()).await.unwrap();
        std::fs::write(directory.path().join("release"), "").unwrap();
        assert_eq!(pending.await.unwrap().unwrap()["status"], "started");
        assert!(manager.state.lock().unwrap()[&uri].queued.is_none());
        wait_build(&manager).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn save_watchers_during_preparation_do_not_compare_previous_build_inputs() {
        let (manager, directory, uri, source) = gated_preparation();
        let before = InputSnapshot::file(&uri.to_file_path().unwrap());
        let saved = source.replace("missing", "new-missing");
        std::fs::write(uri.to_file_path().unwrap(), &saved).unwrap();
        manager.workspace.write_changes().await.documents.insert(
            uri.clone(),
            crate::project::workspace::Document::new(&uri, saved, Some(2)),
        );
        manager.state.lock().unwrap().insert(
            uri.clone(),
            Record {
                inputs: [(uri.clone(), before)].into(),
                result: Some(json!({"status":"success", "buildId":0})),
                ..Default::default()
            },
        );
        let worker = manager.clone();
        let main = uri.clone();
        let pending = tokio::spawn(async move { worker.on_save(main).await });
        wait_preparing(directory.path()).await;
        let changes = crate::editing::file_operations::FileChanges::new([
            uri.as_str(),
            uri.join("4913").unwrap().as_str(),
            uri.join("main.tex~").unwrap().as_str(),
        ])
        .unwrap();
        let followups = manager.watched_files_changed(&changes).await;
        assert!(followups.is_empty());
        assert!(!manager.state.lock().unwrap()[&uri]
            .active
            .as_ref()
            .unwrap()
            .1
            .load(Ordering::Relaxed));
        std::fs::write(directory.path().join("release"), "").unwrap();
        pending.await.unwrap().unwrap();
        wait_build(&manager).await;
        assert!(directory.path().join("compiled").exists());
        assert_eq!(manager.status(&uri)["status"], "success");
        assert!(manager.state.lock().unwrap()[&uri].queued.is_none());
    }

    #[tokio::test]
    async fn explicit_cancellation_invalidates_a_detached_watcher_followup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.tex");
        std::fs::write(&path, "\\documentclass{article}\n").unwrap();
        let path = path.canonicalize().unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        let before = InputSnapshot::file(&path);
        let source = "\\documentclass{article}\nChanged.\n";
        std::fs::write(&path, source).unwrap();
        let mut workspace = Workspace::default();
        workspace.open(uri.clone(), source.into(), 2);
        workspace.config.build.executable = "unavailable-custom-compiler".into();
        workspace.config.build.rebuild_arguments = Some(vec![]);
        let (client, _outgoing) = Client::new();
        let manager = BuildManager::new(client, WorkspaceStore::new(workspace));
        manager.state.lock().unwrap().insert(
            uri.clone(),
            Record {
                inputs: [(uri.clone(), before)].into(),
                queued: Some(QueuedBuild {
                    uri: uri.clone(),
                    rebuild: true,
                }),
                ..Default::default()
            },
        );
        let changes = crate::editing::file_operations::FileChanges::new([uri.as_str()]).unwrap();
        let pending = manager.watched_files_changed(&changes).await;
        assert_eq!(pending.len(), 1);
        manager.cancel(Some(&uri));
        manager.resume_watched(pending).await;
        assert_eq!(manager.tasks.load(Ordering::SeqCst), 0);
        assert!(manager.state.lock().unwrap()[&uri].queued.is_none());
        assert!(!directory.path().join("build").exists());
    }

    #[tokio::test]
    async fn queued_rebuilds_do_not_follow_a_changed_root_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.tex");
        let second = directory.path().join("second.tex");
        let source = "\\documentclass{article}\n";
        std::fs::write(&first, source).unwrap();
        std::fs::write(&second, source).unwrap();
        let first = Url::from_file_path(first).unwrap();
        let second = Url::from_file_path(second).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(directory.path().into());
        workspace.config.build.root_file = Some("second.tex".into());
        workspace.config.build.executable = "unavailable-custom-compiler".into();
        workspace.config.build.rebuild_arguments = Some(vec![]);
        let workspace = WorkspaceStore::new(workspace);
        let (client, _outgoing) = Client::new();
        let manager = BuildManager::new(client, workspace);
        manager
            .state
            .lock()
            .unwrap()
            .insert(first.clone(), Record::default());
        for rebuild in [false, true] {
            let delayed = manager
                .start_request(first.clone(), Some((first.clone(), 0)), rebuild, None)
                .await
                .unwrap();
            assert_eq!(delayed["status"], "cancelled", "{delayed}");
            assert_eq!(delayed["rootUri"], first.as_str());
            assert_eq!(manager.status(&second)["status"], "idle");
            assert_eq!(manager.tasks.load(Ordering::SeqCst), 0);
            assert!(!directory.path().join("build").exists());
        }
    }

    #[tokio::test]
    async fn delayed_force_callbacks_preserve_newer_builds_saves_and_cancellations() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("main.tex");
        let source = "\\documentclass{article}\n";
        std::fs::write(&main, source).unwrap();
        let uri = Url::from_file_path(main).unwrap();
        let mut workspace = Workspace::default();
        workspace.open(uri.clone(), source.into(), 1);
        let workspace = WorkspaceStore::new(workspace);
        let (client, _outgoing) = Client::new();
        let manager = BuildManager::new(client, workspace);
        manager.state.lock().unwrap().insert(
            uri.clone(),
            Record {
                active: Some((99, Arc::default())),
                rebuild: true,
                queued: Some(QueuedBuild {
                    uri: uri.clone(),
                    rebuild: false,
                }),
                ..Default::default()
            },
        );
        // A force callback from an earlier completion is satisfied by a newer
        // forced build, and must leave that build's ordinary save queue intact.
        let delayed = manager
            .start_request(uri.clone(), Some((uri.clone(), 0)), true, None)
            .await
            .unwrap();
        assert_eq!(delayed["queuedRebuild"], false);
        assert_eq!(delayed["queued"], true);
        // A new user request still asks for another force after the active one.
        let manual = manager.rebuild(uri.clone()).await.unwrap();
        assert_eq!(manual["queuedRebuild"], true);
        // If an ordinary build overtook the old callback, retain the force.
        {
            let mut state = manager.state.lock().unwrap();
            let record = state.get_mut(&uri).unwrap();
            record.rebuild = false;
            record.queued = None;
        }
        let delayed = manager
            .start_request(uri.clone(), Some((uri.clone(), 0)), true, None)
            .await
            .unwrap();
        assert_eq!(delayed["queuedRebuild"], true);
        assert_eq!(delayed["rebuild"], false);
        manager.cancel(Some(&uri));
        let delayed = manager
            .start_request(uri.clone(), Some((uri.clone(), 0)), true, None)
            .await
            .unwrap();
        assert_eq!(delayed["status"], "cancelled");
        assert!(manager.state.lock().unwrap()[&uri].queued.is_none());
        assert_eq!(manager.tasks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_new_manual_rebuild_invalidates_a_detached_force_callback() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("main.tex");
        let source = "\\documentclass{article}\n";
        std::fs::write(&main, source).unwrap();
        let uri = Url::from_file_path(main).unwrap();
        let mut workspace = Workspace::default();
        workspace.open(uri.clone(), source.into(), 1);
        workspace.config.build.executable = "unavailable-custom-compiler".into();
        workspace.config.build.rebuild_arguments = Some(vec![]);
        let workspace = WorkspaceStore::new(workspace);
        let (client, _outgoing) = Client::new();
        let manager = BuildManager::new(client, workspace);
        manager
            .state
            .lock()
            .unwrap()
            .insert(uri.clone(), Record::default());
        let manual = manager.rebuild(uri.clone()).await.unwrap();
        assert_eq!(manual["status"], "started");
        let delayed = manager
            .start_request(uri.clone(), Some((uri.clone(), 0)), true, None)
            .await
            .unwrap();
        assert_eq!(delayed["status"], "cancelled");
        assert!(manager.state.lock().unwrap()[&uri].queued.is_none());
        manager.disconnect();
        manager.shutdown().await;
        assert_eq!(manager.tasks.load(Ordering::SeqCst), 0);
    }
}
