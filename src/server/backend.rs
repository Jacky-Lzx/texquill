//! LSP backend and protocol handlers.
use crate::project::store::WorkspaceStore;
use crate::{
    compilation::{auxiliary, recorder, synctex},
    config::{self, project_config},
    diagnostics::{chktex, diagnostic, diagnostic_inputs},
    editing::{self, code_action_resolve, file_operations, refactor},
    formatting::{self, bib_formatting, on_type_formatting, range_formatting},
    language::{
        call_hierarchy, code_lens, colors, completion_resolve, features, navigation, semantic,
        semantic_cache, signature,
    },
    parser::syntax,
    process::tasks,
    project::distribution,
    server::{builds, dispatch, queries},
};

use crate::project::workspace::{self, document_uri, Workspace};
use crate::rpc::Result;
use crate::server::client::Client;
use lsp_types::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Mutex;

fn file_operation_options() -> FileOperationRegistrationOptions {
    FileOperationRegistrationOptions {
        filters: vec![FileOperationFilter {
            scheme: Some("file".into()),
            pattern: FileOperationPattern {
                glob: "**/*".into(),
                matches: None,
                options: None,
            },
        }],
    }
}

fn validate_diagnostic_identifier(identifier: Option<&str>) -> Result<()> {
    if identifier.is_some_and(|identifier| identifier != diagnostic::IDENTIFIER) {
        Err(crate::rpc::Error::invalid_params(
            "Unknown diagnostic provider",
        ))
    } else {
        Ok(())
    }
}

fn content_modified() -> crate::rpc::Error {
    crate::rpc::Error {
        code: crate::rpc::ErrorCode::ServerError(-32801),
        message: "Analysis inputs changed".into(),
        data: None,
    }
}

fn diagnostic_modified() -> crate::rpc::Error {
    crate::rpc::Error {
        code: crate::rpc::ErrorCode::ServerError(-32802),
        message: "Diagnostic inputs changed".into(),
        data: Some(serde_json::json!({"retriggerRequest":false})),
    }
}

pub struct Backend {
    client: Client,
    workspace: WorkspaceStore,
    builds: builds::BuildManager,
    sync_queries: synctex::Queries,
    external_tasks: tasks::Tasks,
    queries: queries::Queries,
    shutdown_requested: AtomicBool,
    pending_change_publish: Arc<AtomicBool>,
    completion: super::completion::Completion,
    semantic_cache: Mutex<semantic_cache::Cache>,
    diagnostic_cache: Arc<Mutex<diagnostic::Cache>>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.completion.close();
    }
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let workspace = WorkspaceStore::default();
        Self {
            builds: builds::BuildManager::new(client.clone(), workspace.clone()),
            sync_queries: synctex::Queries::default(),
            external_tasks: tasks::Tasks::default(),
            queries: queries::Queries::default(),
            client,
            workspace,
            shutdown_requested: AtomicBool::new(false),
            pending_change_publish: Arc::new(AtomicBool::new(false)),
            completion: Default::default(),
            semantic_cache: Mutex::new(semantic_cache::Cache::default()),
            diagnostic_cache: Arc::new(Mutex::new(diagnostic::Cache::default())),
        }
    }

    /// Cancel and reap external work, retaining final notifications for shutdown.
    pub fn cleanup(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let builds = self.builds.clone();
        let queries = self.sync_queries.clone();
        let external_tasks = self.external_tasks.clone();
        let workspace = self.workspace.clone();
        let analysis = self.queries.clone();
        self.completion.close();
        async move {
            tokio::join!(
                queries.shutdown(),
                external_tasks.shutdown(),
                analysis.shutdown()
            );
            builds.shutdown().await;
            workspace.read().await.diagnostic_delivery.close();
        }
    }

    /// Stop outbound build messages before reaping work on a closed transport.
    pub fn disconnect(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let builds = self.builds.clone();
        let cleanup = self.cleanup();
        async move {
            builds.disconnect();
            cleanup.await;
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown_requested.load(Ordering::Relaxed)
    }

    pub async fn cancel_progress(&self, params: WorkDoneProgressCancelParams) {
        self.builds.cancel_progress(&params.token);
    }

    /// Capture inputs in wire order, then analyse the immutable snapshot without
    /// keeping subsequent editor updates behind a long-running read lock.
    async fn read_query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Workspace) -> T + Send + 'static,
    ) -> Result<T> {
        let (workspace, epoch, revision) = {
            let inputs = self.workspace.read().await;
            (
                inputs.clone(),
                self.workspace.completion_epoch(),
                self.workspace.revision(),
            )
        };
        let analysis = self.queries.analyses.clone();
        dispatch::release();
        self.queries
            .run(move |_| {
                Ok(crate::analysis::AnalysisContext::for_workspace(
                    &workspace, epoch, revision, analysis,
                )
                .run(|_| query(&workspace)))
            })
            .await
    }

    /// Automatically refreshed views have no use after their input revision
    /// changes. Drop obsolete queued work and propagate cancellation to nested
    /// readers; interactive requests retain their ordered snapshot semantics.
    async fn read_background_query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Workspace) -> T + Send + 'static,
    ) -> Result<T> {
        if self.workspace.obsolete_background_inputs().await {
            return Err(content_modified());
        }
        self.wait_prepared_inputs().await?;
        let (workspace, revision, epoch) = {
            let workspace = self.workspace.read().await;
            (
                workspace.clone(),
                self.workspace.revision(),
                self.workspace.completion_epoch(),
            )
        };
        let revisions = self.workspace.clone();
        let analysis = self.queries.analyses.clone();
        dispatch::release();
        let work = self.queries.run_background(move |_| {
            let context_revisions = revisions.clone();
            let context = crate::analysis::AnalysisContext::cancellable(move || {
                context_revisions.revision() != revision
            });
            if revisions.revision() != revision {
                return Err(content_modified());
            }
            let result = context.run(|_| {
                crate::analysis::AnalysisContext::for_workspace(
                    &workspace, epoch, revision, analysis,
                )
                .run(|_| query(&workspace))
            });
            if revisions.revision() != revision {
                return Err(content_modified());
            }
            Ok(result)
        });
        tokio::select! {
            biased;
            _ = self.workspace.changed_since(revision) => Err(content_modified()),
            result = work => result,
        }
    }

    async fn read_edit_query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Workspace) -> Result<T> + Clone + Send + 'static,
    ) -> Result<T> {
        let (mut workspace, mut revision) = {
            let workspace = self.workspace.read().await;
            (workspace.clone(), self.workspace.revision())
        };
        let documents = workspace.documents.clone();
        dispatch::release();
        for attempt in 0..2 {
            let operation = query.clone();
            let result = self
                .queries
                .run(move |_| {
                    crate::analysis::AnalysisContext::new().run(|_| operation(&workspace))
                })
                .await?;
            let current = self.workspace.read().await;
            if self.workspace.revision() == revision {
                return Ok(result);
            }
            // Startup discovery or build metadata can finish during a query.
            // Recompute once with the new metadata only if every source revision
            // is identical; changed source positions must never be reinterpreted.
            if attempt == 1 || !documents.same_revisions(&current.documents) {
                return Err(crate::rpc::Error {
                    code: crate::rpc::ErrorCode::ServerError(-32801),
                    message: "Analysis inputs changed; request edits again".into(),
                    data: None,
                });
            }
            workspace = current.clone();
            revision = self.workspace.revision();
        }
        unreachable!("An edit query returns after at most two analyses")
    }

    async fn reload_distribution(&self) {
        let config = self.workspace.read().await.config.distribution.clone();
        let discovery_config = config.clone();
        if let Ok(mut task) = self.external_tasks.start(move |cancel| {
            let distribution =
                distribution::Distribution::discover_cancelled(&discovery_config, || {
                    cancel.cancelled()
                });
            if cancel.cancelled() {
                return Err(crate::rpc::Error::request_cancelled());
            }
            Ok(distribution)
        }) {
            if let Ok(distribution) = task.wait().await {
                let mut workspace = self.workspace.write().await;
                if workspace.config.distribution == config && workspace.distribution != distribution
                {
                    workspace.distribution = distribution;
                }
            }
        }
    }

    async fn format_document(
        &self,
        uri: Url,
        options: FormattingOptions,
        range: Option<Range>,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = document_uri(uri);
        let workspace = self.workspace.read().await;
        let Some(doc) = workspace.documents.get(&uri) else {
            return Ok(None);
        };
        let kind = doc.kind;
        if !matches!(
            kind,
            workspace::DocumentKind::Latex | workspace::DocumentKind::Bibtex
        ) {
            return Ok(None);
        }
        let range = range
            .map(|range| range_formatting::normalize(&doc.text, range))
            .transpose()?;
        if let Some(range) = range {
            formatting::validate_options(&options)?;
            if range.start == range.end {
                return Ok(Some(vec![]));
            }
        }
        let text = doc.text.clone();
        let version = doc.version;
        let config = workspace.settings(&uri).formatting.clone();
        let directory = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .filter(|path| path.is_dir())
            .or_else(|| workspace.roots.iter().next().cloned());
        let mut task = if kind == workspace::DocumentKind::Bibtex {
            bib_formatting::start(&self.external_tasks, options, text.source.clone(), range)?
        } else {
            formatting::start(
                &self.external_tasks,
                config.clone(),
                options,
                text.source.clone(),
                directory,
            )?
        };
        drop(workspace);
        dispatch::release();
        let formatted = task.wait().await?;
        let workspace = self.workspace.read().await;
        if workspace.settings(&uri).formatting != config
            || !workspace.documents.get(&uri).is_some_and(|doc| {
                doc.version == version && doc.text.source == text.source && doc.kind == kind
            })
        {
            return Err(crate::rpc::Error {
                code: crate::rpc::ErrorCode::ServerError(-32801),
                message: "Document or formatting configuration changed; request formatting again"
                    .into(),
                data: None,
            });
        }
        drop(workspace);
        Ok(Some(match range {
            _ if kind == workspace::DocumentKind::Bibtex => {
                formatting::replacement(&text, 0..text.source.len(), &formatted)
                    .into_iter()
                    .collect()
            }
            Some(range) if kind == workspace::DocumentKind::Latex => {
                range_formatting::edits(&text, formatted, range)
            }
            _ => formatting::edits(&text, formatted),
        }))
    }

    async fn reindex(&self) -> Result<serde_json::Value> {
        let mut workspace = self.workspace.write().await;
        let state = Arc::new(std::sync::Mutex::new(Some(std::mem::take(&mut *workspace))));
        let input = state.clone();
        let task = self.external_tasks.start(move |cancel| {
            let mut indexed = input.lock().unwrap().take().unwrap();
            let roots = std::mem::take(&mut indexed.roots);
            indexed.documents.retain(|_, doc| doc.version.is_some());
            indexed.index_truncated.clear();
            for root in roots {
                indexed.add_root_in_background(root, &|| cancel.cancelled());
            }
            Ok(indexed)
        });
        let result = match task {
            Ok(mut task) => task.wait().await,
            Err(error) => Err(error),
        };
        match result {
            Ok(indexed) => *workspace = indexed,
            Err(error) => {
                if let Some(original) = state.lock().unwrap().take() {
                    *workspace = original;
                }
                return Err(error);
            }
        }
        Ok(
            serde_json::json!({"files":workspace.documents.len(),"truncated":workspace.index_truncated,"limits":workspace.config.indexing}),
        )
    }

    async fn lint(&self, uri: &Url) {
        let workspace = self.workspace.read().await;
        let config = workspace.settings(uri).diagnostics.chktex.clone();
        let Some(doc) = workspace
            .documents
            .get(uri)
            .filter(|doc| config.enabled && doc.kind == workspace::DocumentKind::Latex)
        else {
            return;
        };
        let (source, version) = (doc.text.source.clone(), doc.version);
        let directory = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf));
        let task = chktex::start(
            &self.external_tasks,
            config.clone(),
            source.clone(),
            directory,
        );
        drop(workspace);
        dispatch::release();
        let result = match task {
            Ok(mut task) => task.wait().await,
            Err(error) => Err(error),
        };
        let mut workspace = self.workspace.write().await;
        if workspace.settings(uri).diagnostics.chktex != config
            || !workspace.documents.get(uri).is_some_and(|doc| {
                doc.version == version && doc.version.is_some() && doc.text.source == source
            })
        {
            return;
        }
        match result {
            Ok(diagnostics) => {
                workspace
                    .project
                    .lint_diagnostics
                    .insert(uri.clone(), diagnostics);
            }
            Err(error) => {
                workspace.project.lint_diagnostics.remove(uri);
                drop(workspace);
                self.client
                    .log_message(MessageType::WARNING, error.message)
                    .await;
                self.publish_open().await;
                return;
            }
        }
        drop(workspace);
        self.publish_open().await;
    }

    fn prepare_completion(&self) {
        self.completion.schedule(
            self.workspace.clone(),
            self.queries.clone(),
            self.client.clone(),
            self.pending_change_publish.clone(),
            false,
            None,
        );
    }

    async fn wait_prepared_inputs(&self) -> Result<()> {
        let source = self.workspace.source_revision();
        loop {
            let changed = self.workspace.update_signal();
            let inputs = self.workspace.read_unindexed().await;
            if self.workspace.source_revision() != source {
                return Err(content_modified());
            }
            if !inputs.has_pending_dependencies()
                || !inputs.diagnostic_delivery.supports_refresh()
                || !self
                    .completion
                    .covers(&inputs, self.workspace.completion_epoch())
            {
                return Ok(());
            }
            drop(inputs);
            dispatch::release();
            changed.await;
        }
    }

    async fn publish_open(&self) {
        self.publish_affected(Default::default()).await;
        self.prepare_completion();
    }

    async fn diagnostic_reports(
        &self,
        workspace: tokio::sync::OwnedRwLockReadGuard<Workspace>,
        uris: std::collections::BTreeSet<Url>,
        previous: std::collections::BTreeMap<Url, String>,
    ) -> Result<Vec<(Url, Option<i32>, DocumentDiagnosticReportKind)>> {
        let revision = self.workspace.revision();
        let snapshot = (*workspace).clone();
        drop(workspace);
        let workspace = snapshot;
        let delivery = workspace.diagnostic_delivery.clone();
        let revisions = self.workspace.clone();
        let refresh_supported = delivery.supports_refresh();
        dispatch::release();
        let work = async {
            let mut cache = self.diagnostic_cache.clone().lock_owned().await;
            self.queries
                .run_background(move |cancel| {
                    let inputs = revisions.clone();
                    crate::analysis::AnalysisContext::cancellable(move || {
                        refresh_supported && inputs.revision() != revision
                    })
                    .run(|context| {
                        let superseded = || refresh_supported && revisions.revision() != revision;
                        if superseded() {
                            return Err(diagnostic_modified());
                        }
                        let mut reports = std::collections::BTreeMap::new();
                        let missing: Vec<_> = uris
                            .into_iter()
                            .filter(|uri| {
                                if let Some(report) = cache.lookup(
                                    uri,
                                    previous.get(uri).map(String::as_str),
                                    revision,
                                ) {
                                    reports.insert(uri.clone(), report);
                                    false
                                } else {
                                    true
                                }
                            })
                            .collect();
                        if !missing.is_empty() {
                            let (snapshots, files) = diagnostic_inputs::Files::capture(|| {
                                workspace
                                    .diagnostic_snapshots_cancelled(missing, || context.cancelled())
                            });
                            if superseded() {
                                return Err(diagnostic_modified());
                            }
                            cancel.check()?;
                            for snapshot in snapshots {
                                let uri = snapshot.0.clone();
                                let report = cache.computed(
                                    snapshot,
                                    previous.get(&uri).map(String::as_str),
                                    Some(diagnostic::Inputs {
                                        revision,
                                        files: files.clone(),
                                    }),
                                );
                                reports.insert(uri, report);
                            }
                        }
                        if superseded() {
                            return Err(diagnostic_modified());
                        }
                        Ok(reports
                            .into_iter()
                            .map(|(uri, report)| {
                                let version =
                                    workspace.documents.get(&uri).and_then(|doc| doc.version);
                                (uri, version, report)
                            })
                            .collect())
                    })
                })
                .await
        };
        let result = if refresh_supported {
            tokio::select! {
                biased;
                _ = self.workspace.changed_since(revision) => Err(diagnostic_modified()),
                result = work => result,
            }
        } else {
            work.await
        };
        if result
            .as_ref()
            .is_err_and(|error| i64::from(error.code) == -32802)
        {
            // One refresh replaces obsolete queued reports, including a
            // revision invalidated by background distribution discovery.
            delivery.publish(&self.client, vec![]).await;
        }
        result
    }

    async fn publish_affected(&self, affected: std::collections::BTreeSet<Url>) {
        let workspace = self.workspace.read().await;
        let delivery = workspace.diagnostic_delivery.clone();
        if delivery.is_pull() {
            drop(workspace);
            delivery.publish(&self.client, vec![]).await;
        } else {
            delivery.schedule(&self.client, self.workspace.clone(), affected);
        }
    }

    async fn refresh_file_events(
        &self,
        changes: Result<file_operations::FileChanges>,
        from_watcher: bool,
    ) {
        let changes = match changes {
            Ok(changes) => changes,
            Err(error) => {
                self.client
                    .log_message(MessageType::WARNING, error.message)
                    .await;
                return;
            }
        };
        let watched_builds = if from_watcher {
            self.builds.watched_files_changed(&changes).await
        } else {
            self.builds.files_changed(&changes).await;
            Default::default()
        };
        let affected = {
            let mut workspace = self.workspace.write().await;
            if from_watcher {
                changes.refresh_watched(&mut workspace)
            } else {
                changes.refresh(&mut workspace)
            }
        };
        {
            let mut cache = self.semantic_cache.lock().await;
            for uri in &affected {
                cache.remove(uri);
            }
        }
        if !from_watcher {
            let mut cache = self.diagnostic_cache.lock().await;
            for uri in &affected {
                cache.remove(uri);
            }
        }
        // Revalidate cached analysis after a watcher event, preserving result
        // IDs when a delayed notification changes neither payload nor version.
        self.publish_affected(affected).await;
        self.builds.resume_watched(watched_builds).await;
    }
}

impl Backend {
    pub(crate) async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut workspace = self.workspace.write().await;
        let pull_diagnostics = params
            .capabilities
            .text_document
            .as_ref()
            .is_some_and(|document| document.diagnostic.is_some());
        let diagnostic_refresh = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.diagnostic.as_ref())
            .and_then(|diagnostic| diagnostic.refresh_support)
            .unwrap_or(false);
        workspace.diagnostic_delivery =
            diagnostic::Delivery::new(pull_diagnostics, diagnostic_refresh);
        workspace.client.build_progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);
        if let Some(options) = params.initialization_options {
            workspace.config =
                config::Config::parse(options).map_err(crate::rpc::Error::invalid_params)?;
        }
        workspace.client.inlay_refresh = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|cap| cap.inlay_hint.as_ref())
            .and_then(|cap| cap.refresh_support)
            .unwrap_or(false);
        workspace.client.versioned_edits = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.workspace_edit.as_ref())
            .and_then(|edit| edit.document_changes)
            .unwrap_or(false);
        workspace.client.apply_edits = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.apply_edit)
            .unwrap_or(false);
        workspace.client.watch_files = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|watch| watch.dynamic_registration)
            .unwrap_or(false);
        workspace.client.snippets = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|doc| doc.completion.as_ref())
            .and_then(|completion| completion.completion_item.as_ref())
            .and_then(|item| item.snippet_support)
            .unwrap_or(false);
        workspace.client.completion_resolve_documentation = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|doc| doc.completion.as_ref())
            .and_then(|completion| completion.completion_item.as_ref())
            .and_then(|item| item.resolve_support.as_ref())
            .is_some_and(|support| {
                support
                    .properties
                    .iter()
                    .any(|property| property == "documentation")
            });
        workspace.client.hierarchical_symbols = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|doc| doc.document_symbol.as_ref())
            .and_then(|symbols| symbols.hierarchical_document_symbol_support)
            .unwrap_or(false);
        workspace.client.code_action_literals = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|doc| doc.code_action.as_ref())
            .is_some_and(|action| action.code_action_literal_support.is_some());
        workspace.client.code_action_resolve_edits = workspace.client.code_action_literals
            && params
                .capabilities
                .text_document
                .as_ref()
                .and_then(|doc| doc.code_action.as_ref())
                .is_some_and(|action| {
                    action.data_support == Some(true)
                        && action.resolve_support.as_ref().is_some_and(|support| {
                            support.properties.iter().any(|property| property == "edit")
                        })
                });
        workspace.client.folding_capabilities = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|doc| doc.folding_range.as_ref())
            .cloned()
            .unwrap_or_default();
        let mut scan_roots = Vec::new();
        if let Some(folders) = params.workspace_folders {
            for folder in folders {
                if let Ok(root) = folder.uri.to_file_path() {
                    scan_roots.push(root);
                }
            }
        }
        if scan_roots.is_empty() {
            if let Some(uri) = params.root_uri {
                if let Ok(root) = uri.to_file_path() {
                    scan_roots.push(root);
                }
            }
        }
        let mut indexed = std::mem::take(&mut *workspace);
        let mut task = self.external_tasks.start(move |cancel| {
            for root in scan_roots {
                if cancel.cancelled() {
                    return Err(crate::rpc::Error::request_cancelled());
                }
                indexed.add_root_in_background(root, &|| cancel.cancelled());
            }
            Ok(indexed)
        })?;
        *workspace = task.wait().await?;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: Some(vec![
                        "\\".into(),
                        "{".into(),
                        ",".into(),
                        "/".into(),
                        "@".into(),
                        "[".into(),
                        "=".into(),
                    ]),
                    ..Default::default()
                }),
                diagnostic_provider: pull_diagnostics.then(|| {
                    DiagnosticServerCapabilities::Options(DiagnosticOptions {
                        identifier: Some(diagnostic::IDENTIFIER.into()),
                        inter_file_dependencies: true,
                        workspace_diagnostics: true,
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    })
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".into(),
                    more_trigger_character: Some(vec!["}".into()]),
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(
                        ["{", "[", ",", "<", "(", "|", "*", "+"]
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                    ),
                    retrigger_characters: Some(
                        ["}", "]", ">", ")", "|"]
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                    ),
                    ..Default::default()
                }),
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                linked_editing_range_provider: Some(LinkedEditingRangeServerCapabilities::Simple(
                    true,
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::REFACTOR_REWRITE,
                            CodeActionKind::REFACTOR_EXTRACT,
                            CodeActionKind::SOURCE_FIX_ALL,
                        ]),
                        resolve_provider: Some(true),
                        ..Default::default()
                    },
                )),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "latex.indexingStatus".into(),
                        "latex.reindex".into(),
                        "latex.build".into(),
                        "latex.rebuild".into(),
                        "latex.cleanAuxiliary".into(),
                        "latex.cleanArtifacts".into(),
                        "latex.cancelBuild".into(),
                        "latex.buildStatus".into(),
                        "latex.forwardSearch".into(),
                        "latex.inverseSearch".into(),
                        "latex.applyQuickFix".into(),
                        "latex.applyCodeAction".into(),
                    ],
                    ..Default::default()
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                color_provider: Some(ColorProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(semantic::options()),
                ),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                        did_create: Some(file_operation_options()),
                        did_delete: Some(file_operation_options()),
                        will_rename: Some(file_operation_options()),
                        did_rename: Some(file_operation_options()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "texquill".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    pub(crate) async fn initialized(&self, _: InitializedParams) {
        // Distribution discovery and watcher registration do not alter source
        // text. Let document notifications and interactive requests proceed
        // while waiting for the external tool or the client's registration ACK.
        dispatch::release();
        self.reload_distribution().await;
        if self.workspace.read().await.client.watch_files {
            let binary_inputs = syntax::file_extensions("includegraphics")
                .unwrap()
                .iter()
                .chain(recorder::DATA_EXTENSIONS)
                .map(|extension| {
                    extension
                        .chars()
                        .flat_map(|ch| ['[', ch, ch.to_ascii_uppercase(), ']'])
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(",");
            let registration = Registration {
                id: "texquill-watch-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(serde_json::json!({
                    "watchers": [
                        {"globPattern": format!("**/*.{{{}}}",workspace::SOURCE_EXTENSIONS.join(",")), "kind": 7},
                        {"globPattern": format!("**/*.{{{binary_inputs}}}"), "kind": 7},
                        {"globPattern": "**/{.latexmkrc,latexmkrc,.texquill.json}", "kind": 7},
                        {"globPattern": "**/*", "kind": 5}
                    ]
                })),
            };
            if let Err(error) = self.client.register_capability(vec![registration]).await {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("File watcher registration failed: {error}"),
                    )
                    .await;
            }
        }
        self.client
            .log_message(MessageType::INFO, "texquill ready")
            .await;
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.cleanup().await;
        self.shutdown_requested.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let item = params.text_document;
        let origin = item.uri.clone();
        let mut workspace = self.workspace.write().await;
        workspace.open_with_language(item.uri, item.text, item.version, &item.language_id);
        drop(workspace);
        self.publish_open().await;
        self.completion.schedule(
            self.workspace.clone(),
            self.queries.clone(),
            self.client.clone(),
            self.pending_change_publish.clone(),
            true,
            Some(origin),
        );
    }

    pub(crate) async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = document_uri(params.text_document.uri);
        let mut workspace = self.workspace.write_changes().await;
        let Some(doc) = workspace.documents.get(&uri) else {
            return;
        };
        if !doc
            .version
            .is_some_and(|version| params.text_document.version > version)
        {
            return;
        }
        let Some(text) = doc.text.changed(params.content_changes) else {
            drop(workspace);
            self.client
                .log_message(
                    MessageType::WARNING,
                    "Ignored invalid edit range; document content was preserved",
                )
                .await;
            return;
        };
        let updated = doc.with_text(&uri, text.source, Some(params.text_document.version));
        let prepare_path = crate::language::path_cache::changed_candidate(doc, &updated);
        let prepare_commands = crate::language::command_index::changed_declarations(doc, &updated);
        workspace.documents.insert(uri.clone(), updated);
        workspace.clear_build_diagnostics(&uri);
        workspace.defer_dependencies(uri.clone());
        self.pending_change_publish.store(true, Ordering::SeqCst);
        drop(workspace);
        if prepare_path || prepare_commands {
            self.completion.schedule(
                self.workspace.clone(),
                self.queries.clone(),
                self.client.clone(),
                self.pending_change_publish.clone(),
                true,
                Some(uri),
            );
        }
        if !self.workspace.has_queued_changes() {
            self.finish_changes().await;
        }
    }

    pub(crate) fn queue_change(&self) -> crate::project::store::QueuedChange {
        self.workspace.queue_change()
    }

    pub(crate) async fn finish_changes(&self) {
        if self.workspace.has_queued_changes() {
            return;
        }
        let mut workspace = self.workspace.write_prepared().await;
        if workspace.has_pending_dependencies()
            && workspace.diagnostic_delivery.supports_refresh()
            && self
                .completion
                .covers(&workspace, self.workspace.completion_epoch())
        {
            drop(workspace);
            self.prepare_completion();
            return;
        }
        if workspace.has_pending_dependencies() {
            let context = crate::analysis::AnalysisContext::for_workspace(
                &workspace,
                self.workspace.completion_epoch(),
                self.workspace.revision(),
                self.queries.analyses.clone(),
            );
            context.run(|_| workspace.refresh_pending_dependencies());
        }
        drop(workspace);
        if self.pending_change_publish.swap(false, Ordering::SeqCst) {
            self.publish_open().await;
        }
    }

    pub(crate) async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = document_uri(params.text_document.uri);
        if let Some(source) = params.text {
            let mut workspace = self.workspace.write().await;
            if let Some(doc) = workspace
                .documents
                .get(&uri)
                .filter(|doc| doc.text.source != source)
            {
                let updated = doc.with_text(&uri, source, doc.version);

                workspace.documents.insert(uri.clone(), updated);
                workspace.clear_build_diagnostics(&uri);
                workspace.load_dependencies(&uri, 0);
            }
        }
        self.publish_open().await;
        let on_save = self.workspace.read().await.settings(&uri).build.on_save;
        if on_save {
            if let Err(error) = self.builds.on_save(uri.clone()).await {
                self.client
                    .log_message(MessageType::WARNING, error.message)
                    .await;
            }
        }
        self.lint(&uri).await;
    }

    pub(crate) async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = document_uri(params.text_document.uri);
        let mut workspace = self.workspace.write().await;
        workspace.close(&uri);
        workspace.project.lint_diagnostics.remove(&uri);
        self.semantic_cache.lock().await.remove(&uri);
        self.diagnostic_cache.lock().await.remove(&uri);
        let delivery = workspace.diagnostic_delivery.clone();
        drop(workspace);
        if !delivery.is_pull() {
            delivery
                .publish(&self.client, vec![(uri, vec![], None)])
                .await;
        }
        self.publish_open().await;
    }

    pub(crate) async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        self.refresh_file_events(
            file_operations::FileChanges::new(
                params.changes.iter().map(|change| change.uri.as_str()),
            ),
            true,
        )
        .await;
    }

    pub(crate) async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        match config::Config::parse(params.settings) {
            Ok(config) => {
                let indexing_changed =
                    self.workspace.read().await.config.indexing != config.indexing;
                let distribution_changed =
                    self.workspace.read().await.config.distribution != config.distribution;
                {
                    let mut workspace = self.workspace.write().await;
                    workspace.project.lint_diagnostics.clear();
                    project_config::invalidate();
                    workspace.config = config;
                }
                if indexing_changed {
                    if let Err(error) = self.reindex().await {
                        self.client
                            .log_message(MessageType::WARNING, error.message)
                            .await;
                    }
                }
                if distribution_changed {
                    self.reload_distribution().await;
                }
                self.publish_open().await;
            }
            Err(error) => self.client.log_message(MessageType::WARNING, error).await,
        }
    }

    pub(crate) async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command == "latex.reindex" {
            let status = self.reindex().await?;
            self.publish_open().await;
            return Ok(Some(status));
        }
        if params.command == "latex.indexingStatus" {
            let workspace = self.workspace.read().await;
            return Ok(Some(
                serde_json::json!({"files":workspace.documents.len(),"truncated":workspace.index_truncated,"limits":workspace.config.indexing}),
            ));
        }
        let argument = params.arguments.first();
        if params.command == "latex.cancelBuild" && argument.is_none() {
            return Ok(Some(
                serde_json::json!({"cancelled":self.builds.cancel(None)}),
            ));
        }
        let uri = argument
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("uri").and_then(serde_json::Value::as_str))
            })
            .and_then(|value| Url::parse(value).ok())
            .map(document_uri)
            .ok_or_else(|| {
                crate::rpc::Error::invalid_params(
                    "Expected a document URI as the first command argument",
                )
            })?;
        match params.command.as_str() {
            // Keep the ordered turn until the editor acknowledges command edits.
            "latex.applyCodeAction" => {
                let mut request: refactor::Request = serde_json::from_value(
                    argument.cloned().unwrap_or_default(),
                )
                .map_err(|_| crate::rpc::Error::invalid_params("Invalid code action request"))?;
                request.uri = document_uri(request.uri);
                let edit =
                    refactor::resolve(&*self.workspace.read().await, request).ok_or_else(|| {
                        crate::rpc::Error::invalid_params(
                            "Code action is outdated; request code actions again",
                        )
                    })?;
                let response = self.client.apply_edit(edit).await?;
                Ok(Some(serde_json::to_value(response).unwrap()))
            }
            "latex.applyQuickFix" => {
                let mut request: editing::FixRequest = serde_json::from_value(
                    argument.cloned().unwrap_or_default(),
                )
                .map_err(|_| crate::rpc::Error::invalid_params("Invalid quick fix request"))?;
                request.uri = document_uri(request.uri);
                let edit = editing::resolve_fix(&*self.workspace.read().await, request)
                    .ok_or_else(|| {
                        crate::rpc::Error::invalid_params(
                            "Quick fix is outdated; request code actions again",
                        )
                    })?;
                let response = self.client.apply_edit(edit).await?;
                Ok(Some(serde_json::to_value(response).unwrap()))
            }
            "latex.build" => self.builds.start(uri).await.map(Some),
            "latex.cleanAuxiliary" | "latex.cleanArtifacts" => {
                let dry_run = match argument.and_then(|value| value.get("dryRun")) {
                    None => false,
                    Some(value) => value.as_bool().ok_or_else(|| {
                        crate::rpc::Error::invalid_params("dryRun must be a boolean")
                    })?,
                };
                self.builds
                    .clean(uri, params.command == "latex.cleanArtifacts", dry_run)
                    .await
                    .map(Some)
            }
            "latex.rebuild" => {
                if let Some(value) = argument.and_then(|value| value.get("rebuildGuard")) {
                    let mut guard: editing::RebuildGuard = serde_json::from_value(value.clone())
                        .map_err(|_| {
                            crate::rpc::Error::invalid_params(
                                "Invalid rebuild action; request code actions again",
                            )
                        })?;
                    guard.uri = document_uri(guard.uri);
                    self.builds.rebuild_guarded(uri, guard).await.map(Some)
                } else {
                    self.builds.rebuild(uri).await.map(Some)
                }
            }
            "latex.forwardSearch" | "latex.inverseSearch" => {
                let project = synctex::prepare(&*self.workspace.read().await, &uri)?;
                let available = |root: &Url| {
                    let status = self.builds.status(root);
                    status["status"] == "idle"
                        || (status["status"] == "success" && status["stale"] != true)
                };
                if !available(&project.root) {
                    return Err(crate::rpc::Error::invalid_params(
                        "Finish a successful build before querying SyncTeX",
                    ));
                }
                let value = argument.cloned().unwrap_or_default();
                let inverse = params.command == "latex.inverseSearch";
                let mut task = self.sync_queries.start(project.clone(), value, inverse)?;
                dispatch::release();
                let result = task.wait().await?;
                let current = synctex::prepare(&*self.workspace.read().await, &uri)?;
                if current != project || !available(&current.root) {
                    return Err(crate::rpc::Error::invalid_params(
                        "Build artifacts changed during the SyncTeX query; retry after compiling",
                    ));
                }
                Ok(Some(result))
            }
            "latex.cancelBuild" | "latex.buildStatus" => {
                let root = self
                    .workspace
                    .read()
                    .await
                    .root_document(&uri)
                    .map_err(crate::rpc::Error::invalid_params)?;
                if params.command == "latex.cancelBuild" {
                    Ok(Some(
                        serde_json::json!({"cancelled":self.builds.cancel(Some(&root))}),
                    ))
                } else {
                    Ok(Some(self.builds.status(&root)))
                }
            }
            _ => Err(crate::rpc::Error::invalid_params(format!(
                "Unknown command: {}",
                params.command
            ))),
        }
    }

    pub(crate) async fn will_rename_files(
        &self,
        params: RenameFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        self.read_edit_query(move |workspace| {
            file_operations::will_rename(workspace, &params.files)
        })
        .await
    }

    pub(crate) async fn did_create_files(&self, params: CreateFilesParams) {
        self.refresh_file_events(
            file_operations::FileChanges::new(params.files.iter().map(|file| file.uri.as_str())),
            false,
        )
        .await;
    }

    pub(crate) async fn did_delete_files(&self, params: DeleteFilesParams) {
        self.refresh_file_events(
            file_operations::FileChanges::new(params.files.iter().map(|file| file.uri.as_str())),
            false,
        )
        .await;
    }

    pub(crate) async fn did_rename_files(&self, params: RenameFilesParams) {
        match file_operations::Renames::new(&params.files) {
            Ok(renames) => self.builds.files_renamed(&renames).await,
            Err(error) => {
                self.client
                    .log_message(MessageType::WARNING, error.message)
                    .await;
                return;
            }
        }
        let mut workspace = self.workspace.write().await;
        match file_operations::did_rename(&mut workspace, &params.files) {
            Ok(affected) => {
                drop(workspace);
                self.publish_affected(affected).await;
            }
            Err(error) => {
                drop(workspace);
                self.client
                    .log_message(MessageType::WARNING, error.message)
                    .await;
            }
        }
    }

    pub(crate) async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        let mut workspace = self.workspace.write().await;
        for folder in params.event.removed {
            if let Ok(root) = folder.uri.to_file_path() {
                workspace.roots.remove(&root);
            }
        }
        let roots = workspace.roots.clone();
        workspace.documents.retain(|uri, doc| {
            doc.version.is_some()
                || uri
                    .to_file_path()
                    .ok()
                    .is_some_and(|path| roots.iter().any(|root| path.starts_with(root)))
        });
        for folder in params.event.added {
            if let Ok(root) = folder.uri.to_file_path() {
                workspace.add_root(root);
            }
        }
        drop(workspace);
        self.publish_open().await;
    }

    pub(crate) async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        validate_diagnostic_identifier(params.identifier.as_deref())?;
        if self.workspace.obsolete_diagnostic_inputs().await {
            return Err(diagnostic_modified());
        }
        self.wait_prepared_inputs()
            .await
            .map_err(|_| diagnostic_modified())?;
        let uri = document_uri(params.text_document.uri);
        let workspace = self.workspace.clone().read_owned().await;
        let previous = params
            .previous_result_id
            .map(|previous| (uri.clone(), previous))
            .into_iter()
            .collect();
        let (_, _, report) = self
            .diagnostic_reports(workspace, [uri].into_iter().collect(), previous)
            .await?
            .pop()
            .unwrap();
        let report = match report {
            DocumentDiagnosticReportKind::Full(full_document_diagnostic_report) => {
                DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                    related_documents: None,
                    full_document_diagnostic_report,
                })
            }
            DocumentDiagnosticReportKind::Unchanged(unchanged_document_diagnostic_report) => {
                DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
                    related_documents: None,
                    unchanged_document_diagnostic_report,
                })
            }
        };
        Ok(report.into())
    }

    pub(crate) async fn workspace_diagnostic(
        &self,
        params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        validate_diagnostic_identifier(params.identifier.as_deref())?;
        if self.workspace.obsolete_diagnostic_inputs().await {
            return Err(diagnostic_modified());
        }
        self.wait_prepared_inputs()
            .await
            .map_err(|_| diagnostic_modified())?;
        let previous: std::collections::BTreeMap<_, _> = params
            .previous_result_ids
            .into_iter()
            .map(|previous| (document_uri(previous.uri), previous.value))
            .collect();
        let workspace = self.workspace.clone().read_owned().await;
        let uris = workspace
            .documents
            .keys()
            .cloned()
            .chain(
                workspace
                    .project
                    .build_diagnostics
                    .values()
                    .flat_map(|project| project.keys().cloned()),
            )
            // Clear previously reported files which were removed.
            .chain(previous.keys().cloned())
            .collect();
        let reports = self.diagnostic_reports(workspace, uris, previous).await?;
        let items = reports
            .into_iter()
            .map(|(uri, version, report)| {
                let version = version.map(i64::from);
                match report {
                    DocumentDiagnosticReportKind::Full(full_document_diagnostic_report) => {
                        WorkspaceDocumentDiagnosticReport::Full(
                            WorkspaceFullDocumentDiagnosticReport {
                                uri,
                                version,
                                full_document_diagnostic_report,
                            },
                        )
                    }
                    DocumentDiagnosticReportKind::Unchanged(
                        unchanged_document_diagnostic_report,
                    ) => WorkspaceDocumentDiagnosticReport::Unchanged(
                        WorkspaceUnchangedDocumentDiagnosticReport {
                            uri,
                            version,
                            unchanged_document_diagnostic_report,
                        },
                    ),
                }
            })
            .collect();
        Ok(WorkspaceDiagnosticReport { items }.into())
    }

    pub(crate) async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.format_document(params.text_document.uri, params.options, None)
            .await
    }

    pub(crate) async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.format_document(params.text_document.uri, params.options, Some(params.range))
            .await
    }

    pub(crate) async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = document_uri(params.text_document_position.text_document.uri);
        self.read_edit_query(move |workspace| {
            let Some(doc) = workspace.documents.get(&uri) else {
                return Ok(None);
            };
            if doc.kind != workspace::DocumentKind::Latex {
                return Ok(None);
            }
            if !workspace.settings(&uri).formatting.on_type {
                return Ok(Some(vec![]));
            }
            on_type_formatting::edits(
                workspace,
                &uri,
                params.text_document_position.position,
                &params.ch,
                &params.options,
            )
            .map(Some)
        })
        .await
    }

    pub(crate) async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let params = params.text_document_position_params;
        self.read_query(move |workspace| {
            call_hierarchy::prepare(
                workspace,
                &document_uri(params.text_document.uri),
                params.position,
            )
        })
        .await?
    }

    pub(crate) async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.read_query(move |workspace| {
            call_hierarchy::incoming(workspace, &params.item).map(Some)
        })
        .await?
    }

    pub(crate) async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.read_query(move |workspace| {
            call_hierarchy::outgoing(workspace, &params.item).map(Some)
        })
        .await?
    }

    pub(crate) async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        if params.range.start > params.range.end {
            return Err(crate::rpc::Error::invalid_params(
                "Invalid inlay hint range",
            ));
        }
        self.read_background_query(move |workspace| {
            Some(auxiliary::hints(
                workspace,
                &document_uri(params.text_document.uri),
                params.range,
            ))
        })
        .await
    }

    pub(crate) async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let (prepare_path, epoch) = {
            let position = &params.text_document_position;
            let inputs = self.workspace.completion_inputs();
            let workspace = &inputs.workspace;
            if let Some(items) = self.completion.complete(
                workspace,
                inputs.epoch,
                &document_uri(position.text_document.uri.clone()),
                position.position,
            ) {
                let limit = workspace.config.indexing.completion_limit;
                let incomplete = items.len() >= limit;
                let items = items.into_iter().take(limit).collect();
                return Ok(Some(if incomplete {
                    CompletionResponse::List(CompletionList {
                        is_incomplete: true,
                        items,
                    })
                } else {
                    CompletionResponse::Array(items)
                }));
            }
            let prepare = workspace
                .documents
                .get(&document_uri(position.text_document.uri.clone()))
                .is_some_and(|doc| crate::language::path_cache::candidate(doc, position.position));
            (prepare, inputs.epoch)
        };
        self.completion.schedule(
            self.workspace.clone(),
            self.queries.clone(),
            self.client.clone(),
            self.pending_change_publish.clone(),
            prepare_path,
            Some(document_uri(
                params.text_document_position.text_document.uri.clone(),
            )),
        );
        let completion = self.completion.clone();
        self.read_query(move |workspace| {
            let position = params.text_document_position;
            let uri = document_uri(position.text_document.uri);
            let mut items = if prepare_path {
                let (items, prepared) = crate::language::path_cache::Prepared::complete_and_prepare(
                    workspace,
                    &uri,
                    position.position,
                    epoch,
                );
                if let Some(prepared) = prepared.filter(|_| {
                    !crate::analysis::AnalysisContext::current()
                        .is_some_and(|context| context.cancelled())
                }) {
                    completion.remember_path(prepared);
                }
                items
            } else {
                features::completion(workspace, &uri, position.position)
            };
            let limit = workspace.config.indexing.completion_limit;
            let incomplete = items.len() >= limit || !workspace.index_truncated.is_empty();
            items.truncate(limit);
            Some(if incomplete {
                CompletionResponse::List(CompletionList {
                    is_incomplete: true,
                    items,
                })
            } else {
                CompletionResponse::Array(items)
            })
        })
        .await
    }

    pub(crate) async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        self.read_query(move |workspace| completion_resolve::resolve(workspace, item))
            .await?
    }

    pub(crate) async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params;
        self.read_query(move |workspace| {
            features::hover(
                workspace,
                &document_uri(position.text_document.uri),
                position.position,
            )
        })
        .await
    }

    pub(crate) async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let params = params.text_document_position_params;
        self.read_query(move |workspace| {
            signature::help(
                workspace,
                &document_uri(params.text_document.uri),
                params.position,
            )
        })
        .await
    }

    pub(crate) async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        self.read_query(move |workspace| {
            let locations = features::definitions(
                workspace,
                &document_uri(position.text_document.uri),
                position.position,
            );
            Some(GotoDefinitionResponse::Array(locations))
        })
        .await
    }

    pub(crate) async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let position = params.text_document_position;
        self.read_query(move |workspace| {
            Some(features::references(
                workspace,
                &document_uri(position.text_document.uri),
                position.position,
                params.context.include_declaration,
            ))
        })
        .await
    }

    pub(crate) async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.read_query(move |workspace| {
            features::prepare_rename(
                workspace,
                &document_uri(params.text_document.uri),
                params.position,
            )
        })
        .await?
    }

    pub(crate) async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let position = params.text_document_position;
        self.read_edit_query(move |workspace| {
            features::rename(
                workspace,
                &document_uri(position.text_document.uri),
                position.position,
                &params.new_name,
            )
        })
        .await
    }

    pub(crate) async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let params = params.text_document_position_params;
        self.read_background_query(move |workspace| {
            editing::highlights(
                workspace,
                &document_uri(params.text_document.uri),
                params.position,
            )
        })
        .await
        .map(Some)
    }

    pub(crate) async fn linked_editing_range(
        &self,
        params: LinkedEditingRangeParams,
    ) -> Result<Option<LinkedEditingRanges>> {
        let params = params.text_document_position_params;
        self.read_background_query(move |workspace| {
            let uri = document_uri(params.text_document.uri);
            let Some(doc) = workspace.documents.get(&uri) else {
                return Ok(None);
            };
            if doc.text.offset(params.position).is_none() {
                return Err(crate::rpc::Error::invalid_params(
                    "Invalid linked editing position",
                ));
            }
            Ok(editing::linked_editing_ranges_in_workspace(
                workspace,
                &uri,
                params.position,
            ))
        })
        .await?
    }

    pub(crate) async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        self.read_query(move |workspace| {
            let Some(doc) = workspace
                .documents
                .get(&document_uri(params.text_document.uri))
            else {
                return Ok(None);
            };
            editing::selection_ranges(doc, &params.positions)
                .map(Some)
                .ok_or_else(|| crate::rpc::Error::invalid_params("Invalid selection position"))
        })
        .await?
    }

    pub(crate) async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        if params.context.trigger_kind == Some(CodeActionTriggerKind::AUTOMATIC) {
            return self
                .read_background_query(move |workspace| {
                    code_action_resolve::actions(
                        workspace,
                        &document_uri(params.text_document.uri),
                        params.range,
                        &params.context,
                    )
                })
                .await
                .map(Some);
        }
        self.read_edit_query(move |workspace| {
            Ok(code_action_resolve::actions(
                workspace,
                &document_uri(params.text_document.uri),
                params.range,
                &params.context,
            ))
        })
        .await
        .map(Some)
    }

    pub(crate) async fn code_action_resolve(&self, action: CodeAction) -> Result<CodeAction> {
        self.read_edit_query(move |workspace| code_action_resolve::resolve(workspace, action))
            .await
    }

    pub(crate) async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        self.read_background_query(move |workspace| {
            Some(code_lens::lenses(
                workspace,
                &document_uri(params.text_document.uri),
            ))
        })
        .await
    }

    pub(crate) async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.read_background_query(move |workspace| {
            let uri = document_uri(params.text_document.uri);
            workspace.documents.get(&uri).map(|doc| {
                if workspace.client.hierarchical_symbols {
                    DocumentSymbolResponse::Nested(features::symbols(doc))
                } else {
                    DocumentSymbolResponse::Flat(features::flat_symbols(doc, &uri))
                }
            })
        })
        .await
    }

    pub(crate) async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.read_query(move |workspace| {
            Some(navigation::workspace_symbols(workspace, &params.query))
        })
        .await
    }

    pub(crate) async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        self.read_background_query(move |workspace| {
            workspace
                .documents
                .get(&document_uri(params.text_document.uri))
                .map(|doc| {
                    navigation::folding_ranges_for_client(
                        doc,
                        &workspace.client.folding_capabilities,
                    )
                })
        })
        .await
    }

    pub(crate) async fn document_color(
        &self,
        params: DocumentColorParams,
    ) -> Result<Vec<ColorInformation>> {
        self.read_background_query(move |workspace| {
            workspace
                .documents
                .get(&document_uri(params.text_document.uri))
                .map(colors::document_colors)
                .unwrap_or_default()
        })
        .await
    }

    pub(crate) async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        self.read_query(move |workspace| {
            let Some(doc) = workspace
                .documents
                .get(&document_uri(params.text_document.uri))
            else {
                return Ok(vec![]);
            };
            if params.range.start > params.range.end
                || doc.text.offset(params.range.start).is_none()
                || doc.text.offset(params.range.end).is_none()
            {
                return Err(crate::rpc::Error::invalid_params(
                    "Invalid color presentation range",
                ));
            }
            Ok(colors::presentations(doc, params.range, params.color))
        })
        .await?
    }

    pub(crate) async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = document_uri(params.text_document.uri);
        let query_uri = uri.clone();
        let tokens = self
            .read_background_query(move |workspace| {
                workspace
                    .documents
                    .get(&query_uri)
                    .map(|doc| semantic::tokens(doc, None))
                    .transpose()
            })
            .await??;
        let Some(tokens) = tokens else {
            return Ok(None);
        };
        Ok(Some(SemanticTokensResult::Tokens(
            self.semantic_cache.lock().await.full(uri, tokens),
        )))
    }

    pub(crate) async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri = document_uri(params.text_document.uri);
        let query_uri = uri.clone();
        let tokens = self
            .read_background_query(move |workspace| {
                workspace
                    .documents
                    .get(&query_uri)
                    .map(|doc| semantic::tokens(doc, None))
                    .transpose()
            })
            .await??;
        let Some(tokens) = tokens else {
            return Ok(None);
        };
        Ok(Some(self.semantic_cache.lock().await.delta(
            uri,
            &params.previous_result_id,
            tokens,
        )))
    }

    pub(crate) async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        self.read_background_query(move |workspace| {
            workspace
                .documents
                .get(&document_uri(params.text_document.uri))
                .map(|doc| {
                    semantic::tokens(doc, Some(params.range)).map(SemanticTokensRangeResult::Tokens)
                })
                .transpose()
        })
        .await?
    }

    pub(crate) async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        self.read_background_query(move |workspace| {
            Some(navigation::document_links(
                workspace,
                &document_uri(params.text_document.uri),
            ))
        })
        .await
    }
}

#[cfg(test)]
mod read_query_tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    async fn warm_completion(backend: &Backend, uri: &Url, source: &str) -> Position {
        {
            let mut inputs = backend.workspace.write().await;
            inputs.client.snippets = true;
            inputs.documents.insert(
                uri.clone(),
                workspace::Document::new(uri, source.into(), Some(1)),
            );
        }
        backend.completion.schedule(
            backend.workspace.clone(),
            backend.queries.clone(),
            backend.client.clone(),
            backend.pending_change_publish.clone(),
            true,
            Some(uri.clone()),
        );
        let position =
            crate::text::Text::new(source.into()).position(source.find("\\inp").unwrap() + 4);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let inputs = backend.workspace.completion_inputs();
                if backend
                    .completion
                    .complete(&inputs.workspace, inputs.epoch, uri, position)
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        position
    }

    #[tokio::test]
    async fn prepared_completion_does_not_acquire_the_build_workspace_lock() {
        let (client, _outgoing) = Client::new();
        let backend = Backend::new(client);
        let uri = Url::parse("untitled:build-lock.tex").unwrap();
        let source = "\\documentclass{article}\n\\begin{document}\n\\inp\n\\end{document}\n";
        let position = warm_completion(&backend, &uri, source).await;
        let epoch = backend.workspace.completion_epoch();
        let mut build = backend.workspace.write_prepared().await;
        build
            .project
            .build_diagnostics
            .insert(uri.clone(), Default::default());
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            backend.completion(
                serde_json::from_value(
                    serde_json::json!({"textDocument":{"uri":uri},"position":position}),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("cached completion waited for a build writer")
        .unwrap()
        .unwrap();
        let items = match response {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };
        let item = items.iter().find(|item| item.label == "input").unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
            panic!("missing edit");
        };
        assert_eq!(edit.range.end, position);
        assert_eq!(edit.new_text, "input{${1:file}}");
        drop(build);
        assert_eq!(backend.workspace.completion_inputs().epoch, epoch);
        backend.completion.close();
    }

    #[tokio::test]
    async fn published_completion_inputs_reject_a_changed_macro_before_repreparation() {
        let (client, _outgoing) = Client::new();
        let backend = Backend::new(client);
        let uri = Url::parse("untitled:published-macro.tex").unwrap();
        let source =
            "\\newcommand{\\projectmacro}[2]{#1#2}\n\\begin{document}\n\\inp\n\\end{document}\n";
        let position = warm_completion(&backend, &uri, source).await;
        let old = backend.workspace.completion_inputs();
        {
            let mut inputs = backend.workspace.write_changes().await;
            let edited = inputs.documents[&uri].with_text(
                &uri,
                source.replace("[2]{#1#2}", "[1]{#1}"),
                Some(2),
            );
            inputs.documents.insert(uri.clone(), edited);
        }
        let current = backend.workspace.completion_inputs();
        assert_eq!(old.workspace.documents[&uri].version, Some(1));
        assert_eq!(current.workspace.documents[&uri].version, Some(2));
        assert!(backend
            .completion
            .complete(&current.workspace, current.epoch, &uri, position)
            .is_none());
        backend.completion.close();
    }

    #[tokio::test]
    async fn prepared_completion_overtakes_waiting_views_without_serving_old_source_positions() {
        let (client, _outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client));
        let uri = Url::parse("untitled:prepared-dispatch.tex").unwrap();
        let source = "\\documentclass{article}\n\\begin{document}\n\\inp\n\\end{document}\n";
        {
            let mut inputs = backend.workspace.write().await;
            inputs.client.snippets = true;
            inputs.diagnostic_delivery = diagnostic::Delivery::new(true, true);
            inputs.documents.insert(
                uri.clone(),
                workspace::Document::new(&uri, source.into(), Some(1)),
            );
        }
        backend.completion.schedule(
            backend.workspace.clone(),
            backend.queries.clone(),
            backend.client.clone(),
            backend.pending_change_publish.clone(),
            true,
            Some(uri.clone()),
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let inputs = backend.workspace.read_unindexed().await;
                let position = inputs.documents[&uri]
                    .text
                    .position(source.find("\\inp").unwrap() + 4);
                if backend
                    .completion
                    .complete(
                        &inputs,
                        backend.workspace.completion_epoch(),
                        &uri,
                        position,
                    )
                    .is_some()
                {
                    break;
                }
                drop(inputs);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("preparation did not finish");
        let mut workers = Vec::new();
        let mut finishes = Vec::new();
        for _ in 0..3 {
            let queries = backend.queries.clone();
            let (started, start) = oneshot::channel();
            let (finish, finished) = oneshot::channel();
            workers.push(tokio::spawn(async move {
                queries
                    .run_background(move |_| {
                        let _ = started.send(());
                        let _ = finished.blocking_recv();
                        Ok(())
                    })
                    .await
            }));
            start.await.unwrap();
            finishes.push(finish);
        }
        let current = source.replace("\\inp", "% moved\n正文 😀 text \\inp");
        backend
            .did_change(
                serde_json::from_value(serde_json::json!({
                    "textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":current}]
                }))
                .unwrap(),
            )
            .await;
        let mut queue = dispatch::RequestQueue::default();
        let background = backend.clone();
        let view_uri = uri.clone();
        let view = tokio::spawn(queue.ordered(async move {
            background
                .read_background_query(move |ws| ws.documents[&view_uri].version)
                .await
        }));
        let completion = backend.clone();
        let position =
            crate::text::Text::new(current.clone()).position(current.find("\\inp").unwrap() + 4);
        let reply = tokio::time::timeout(
            Duration::from_secs(1),
            queue.ordered(async move {
                completion
                    .completion(
                        serde_json::from_value(
                            serde_json::json!({"textDocument":{"uri":uri},"position":position}),
                        )
                        .unwrap(),
                    )
                    .await
            }),
        )
        .await;
        let still_pending = backend
            .workspace
            .read_unindexed()
            .await
            .has_pending_dependencies();
        assert_eq!(
            backend
                .read_query(|ws| ws.documents.values().find_map(|doc| doc.version))
                .await
                .unwrap(),
            Some(2)
        );
        for finish in finishes {
            let _ = finish.send(());
        }
        for worker in workers {
            worker.await.unwrap().unwrap();
        }
        let reply = serde_json::to_value(
            reply
                .expect("completion waited for background refresh")
                .unwrap(),
        )
        .unwrap();
        let items = reply.as_array().unwrap();
        let input = items.iter().find(|item| item["label"] == "input").unwrap();
        assert_eq!(
            input["textEdit"]["range"]["end"],
            serde_json::to_value(position).unwrap()
        );
        assert!(
            still_pending,
            "prepared reply forced synchronous project refresh"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), view)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Some(2)
        );
        backend.cleanup().await;
    }

    #[tokio::test]
    async fn intermediate_pull_reports_only_skip_refreshable_clients() {
        for refresh in [false, true] {
            let (client, _outgoing) = Client::new();
            let backend = Backend::new(client);
            let uri = Url::parse("untitled:batched-diagnostics.tex").unwrap();
            {
                let mut workspace = backend.workspace.write().await;
                workspace.diagnostic_delivery = diagnostic::Delivery::new(true, refresh);
                workspace.documents.insert(
                    uri.clone(),
                    workspace::Document::new(&uri, "{broken".into(), Some(1)),
                );
            }
            let queued = backend.queue_change();
            backend.did_change(serde_json::from_value(serde_json::json!({
                "textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"balanced"}]
            })).unwrap()).await;
            let params =
                || serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri}})).unwrap();
            let result = backend.diagnostic(params()).await;
            if refresh {
                assert_eq!(i64::from(result.unwrap_err().code), -32802);
                assert!(backend
                    .workspace
                    .write_changes()
                    .await
                    .has_pending_dependencies());
            } else {
                assert_eq!(
                    serde_json::to_value(result.unwrap()).unwrap()["items"],
                    serde_json::json!([])
                );
                assert!(!backend
                    .workspace
                    .write_changes()
                    .await
                    .has_pending_dependencies());
            }
            drop(queued);
            backend.finish_changes().await;
            assert_eq!(
                serde_json::to_value(backend.diagnostic(params()).await.unwrap()).unwrap()["items"],
                serde_json::json!([])
            );
            assert!(!backend.pending_change_publish.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn queued_incremental_changes_skip_intermediate_includes_but_keep_ordered_queries_fresh()
    {
        let directory = tempfile::tempdir().unwrap();
        let uri = Url::from_file_path(directory.path().join("main.tex")).unwrap();
        let skipped = uri.join("skipped.tex").unwrap();
        let required = uri.join("required.tex").unwrap();
        std::fs::write(skipped.to_file_path().unwrap(), r"\label{skipped}").unwrap();
        std::fs::write(required.to_file_path().unwrap(), r"\label{required}").unwrap();
        let (client, _outgoing) = Client::new();
        let backend = Backend::new(client);
        backend.workspace.write().await.documents.insert(
            uri.clone(),
            workspace::Document::new(&uri, "😀".into(), Some(1)),
        );
        let queued = backend.queue_change();
        let change = |version, changes| {
            serde_json::from_value(serde_json::json!({
                "textDocument":{"uri":uri,"version":version},"contentChanges":changes
            }))
            .unwrap()
        };
        backend
            .did_change(change(
                2,
                serde_json::json!([{"range":{
            "start":{"line":0,"character":2},"end":{"line":0,"character":2}
        },"text":r"\input{skipped}"}]),
            ))
            .await;
        {
            let inputs = backend.workspace.write_changes().await;
            assert!(inputs.has_pending_dependencies());
            assert!(!inputs.documents.contains_key(&skipped));
        }
        assert_eq!(
            i64::from(
                backend
                    .read_background_query(|_| panic!("obsolete view executed"))
                    .await
                    .unwrap_err()
                    .code
            ),
            -32801
        );
        // A genuine query between edits must still see its wire-ordered graph.
        assert!(backend
            .read_query({
                let uri = skipped.clone();
                move |ws| ws.documents.contains_key(&uri)
            })
            .await
            .unwrap());
        backend
            .did_change(change(
                3,
                serde_json::json!([{"range":{
            "start":{"line":0,"character":9},"end":{"line":0,"character":16}
        },"text":"required"}]),
            ))
            .await;
        assert!(!backend
            .workspace
            .write_changes()
            .await
            .documents
            .contains_key(&required));
        // Invalid ranges preserve the last valid incremental revision. Dropping
        // the final receipt must still flush/publish that earlier valid edit.
        backend
            .did_change(change(
                4,
                serde_json::json!([{"range":{
            "start":{"line":99,"character":0},"end":{"line":99,"character":0}
        },"text":"invalid"}]),
            ))
            .await;
        drop(queued);
        backend.finish_changes().await;
        let inputs = backend.workspace.read().await;
        assert_eq!(inputs.documents[&uri].text.source, "😀\\input{required}");
        assert_eq!(inputs.documents[&uri].version, Some(3));
        assert!(inputs.documents.contains_key(&required));
        assert!(!inputs.has_pending_dependencies());
        assert!(!backend.pending_change_publish.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn obsolete_background_queues_do_not_block_completion_or_execute_old_snapshots() {
        let (client, _outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client));
        let uri = Url::parse("untitled:completion-priority.tex").unwrap();
        backend.workspace.write().await.documents.insert(
            uri.clone(),
            workspace::Document::new(&uri, r"\inp".into(), Some(1)),
        );
        let mut finishes = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..3 {
            let queries = backend.queries.clone();
            let (started, start) = oneshot::channel();
            let (finish, finished) = oneshot::channel();
            workers.push(tokio::spawn(async move {
                queries
                    .run_background(move |_| {
                        let _ = started.send(());
                        let _ = finished.blocking_recv();
                        Ok(())
                    })
                    .await
            }));
            start.await.unwrap();
            finishes.push(finish);
        }
        let entered = Arc::new(AtomicBool::new(false));
        let called = entered.clone();
        let old = backend.clone();
        let mut queue = dispatch::RequestQueue::default();
        let background = tokio::spawn(queue.ordered(async move {
            old.read_background_query(move |_| called.store(true, Ordering::SeqCst))
                .await
        }));
        let writer = backend.clone();
        queue
            .ordered(async move {
                writer.workspace.write().await.client.snippets = true;
            })
            .await;
        let completion = tokio::time::timeout(
            Duration::from_secs(1),
            queue.ordered(async move {
                backend
                    .completion(
                        serde_json::from_value(serde_json::json!({
                            "textDocument":{"uri":uri}, "position":{"line":0,"character":4}
                        }))
                        .unwrap(),
                    )
                    .await
            }),
        )
        .await;
        let obsolete = tokio::time::timeout(Duration::from_secs(1), background).await;
        for finish in finishes {
            let _ = finish.send(());
        }
        for worker in workers {
            worker.await.unwrap().unwrap();
        }
        let response = completion
            .expect("completion waited for obsolete background work")
            .unwrap()
            .unwrap();
        let items = match response {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };
        let item = items.iter().find(|item| item.label == "input").unwrap();
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(
            i64::from(
                obsolete
                    .expect("obsolete queued view waited for a worker")
                    .unwrap()
                    .unwrap_err()
                    .code
            ),
            -32801
        );
        assert!(
            !entered.load(Ordering::SeqCst),
            "obsolete queued analysis executed"
        );
    }

    #[tokio::test]
    async fn immutable_snapshot_boundaries_share_indexes_without_retaining_them_across_edits() {
        fn query(workspace: &Workspace) -> bool {
            let scope = workspace.documents.keys().cloned().collect();
            let first = crate::symbols::query_cache::project_signatures(workspace, &scope);
            let second = crate::symbols::query_cache::project_signatures(workspace, &scope);
            assert!(
                Arc::ptr_eq(&first, &second),
                "snapshot did not share its immutable analysis"
            );
            first.command_unchanged("input")
        }
        let (client, _outgoing) = Client::new();
        let backend = Backend::new(client);
        let uri = Url::parse("untitled:snapshot-cache.tex").unwrap();
        backend.workspace.write().await.documents.insert(
            uri.clone(),
            workspace::Document::new(&uri, r"\input{child}".into(), Some(1)),
        );
        assert!(backend.read_query(query).await.unwrap());
        assert!(backend.read_background_query(query).await.unwrap());
        assert!(backend.read_edit_query(|ws| Ok(query(ws))).await.unwrap());
        backend.workspace.write().await.documents.insert(
            uri.clone(),
            workspace::Document::new(&uri, r"\def\input#1{}".into(), Some(2)),
        );
        assert!(!backend.read_query(query).await.unwrap());
        assert!(!backend.read_background_query(query).await.unwrap());
        assert!(!backend.read_edit_query(|ws| Ok(query(ws))).await.unwrap());
    }

    #[tokio::test]
    async fn running_background_views_receive_revision_cancellation_and_never_return_stale_results()
    {
        let (client, _outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client));
        let (started, start) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let (worker_finished, worker_done) = oneshot::channel();
        let saw_cancellation = Arc::new(AtomicBool::new(false));
        let observed = saw_cancellation.clone();
        let reader = backend.clone();
        let background = tokio::spawn(async move {
            reader
                .read_background_query(move |_| {
                    crate::analysis::AnalysisContext::new().run(|context| {
                        let _ = started.send(());
                        let _ = finished.blocking_recv();
                        observed.store(context.cancelled(), Ordering::SeqCst);
                        let _ = worker_finished.send(());
                        42
                    })
                })
                .await
        });
        start.await.unwrap();
        backend.workspace.write().await.client.snippets = true;
        let _ = finish.send(());
        assert_eq!(
            i64::from(background.await.unwrap().unwrap_err().code),
            -32801
        );
        worker_done.await.unwrap();
        assert!(saw_cancellation.load(Ordering::SeqCst));
        assert_eq!(backend.read_background_query(|_| 7).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn request_cancellation_reaches_nested_analysis_contexts() {
        let (client, _outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client));
        let (started, start) = oneshot::channel();
        let (finished, finish) = oneshot::channel();
        let query_backend = backend.clone();
        let query = tokio::spawn(async move {
            query_backend
                .read_query(move |_| {
                    crate::analysis::AnalysisContext::new().run(|context| {
                        let _ = started.send(());
                        while !context.cancelled() {
                            std::thread::yield_now();
                        }
                        let _ = finished.send(());
                    });
                })
                .await
        });
        start.await.unwrap();
        query.abort();
        assert!(query.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), finish)
            .await
            .expect("The worker did not observe request cancellation")
            .unwrap();
        backend.cleanup().await;
    }

    #[tokio::test]
    async fn edit_queries_retry_metadata_updates_but_reject_changed_source_positions() {
        for source_changed in [false, true] {
            let (client, _outgoing) = Client::new();
            let backend = Arc::new(Backend::new(client));
            let uri = Url::parse("untitled:edit-query.tex").unwrap();
            backend
                .workspace
                .write()
                .await
                .open(uri.clone(), "old".into(), 1);
            let mut queue = dispatch::RequestQueue::default();
            let (started, start) = oneshot::channel();
            let (finish, finished) = oneshot::channel();
            let signals = Arc::new(std::sync::Mutex::new(Some((started, finished))));
            let first_backend = backend.clone();
            let first = tokio::spawn(queue.ordered(async move {
                first_backend
                    .read_edit_query(move |workspace| {
                        let signals = signals.lock().unwrap().take();
                        if let Some((started, finished)) = signals {
                            let _ = started.send(());
                            let _ = finished.blocking_recv();
                        }
                        Ok(workspace.client.snippets)
                    })
                    .await
            }));
            start.await.unwrap();
            let writer = queue.ordered(async move {
                let mut workspace = backend.workspace.write().await;
                workspace.client.snippets = true;
                if source_changed {
                    workspace.open(uri, "new".into(), 2);
                }
            });
            let updated = tokio::time::timeout(Duration::from_secs(1), writer).await;
            let _ = finish.send(());
            updated.expect("An edit query blocked newer document inputs");
            let result = first.await.unwrap();
            if source_changed {
                assert_eq!(i64::from(result.unwrap_err().code), -32801);
            } else {
                assert!(result.unwrap(), "The metadata update was not incorporated");
            }
        }
    }

    #[tokio::test]
    async fn slow_analysis_keeps_its_snapshot_without_blocking_writes_or_later_reads() {
        let (client, _outgoing) = Client::new();
        let service = Arc::new(Backend::new(client));
        let mut queue = dispatch::RequestQueue::default();
        let (started, start) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let first_service = service.clone();
        let first = tokio::spawn(queue.ordered(async move {
            first_service
                .read_query(move |workspace| {
                    let _ = started.send(());
                    let _ = finished.blocking_recv();
                    workspace.client.snippets
                })
                .await
                .unwrap()
        }));
        start.await.unwrap();

        let read_service = service.clone();
        let read = queue.ordered(async move {
            read_service
                .read_query(|workspace| workspace.client.snippets)
                .await
                .unwrap()
        });
        let result = tokio::time::timeout(Duration::from_secs(1), read).await;
        // Unblock the worker even if the assertion fails.
        if result.is_err() {
            let _ = finish.send(());
            panic!("An unrelated read waited for the slow analysis");
        }
        assert!(!result.unwrap());

        let (writing, write_started) = oneshot::channel();
        let writer_service = service.clone();
        let writer = tokio::spawn(queue.ordered(async move {
            let _ = writing.send(());
            writer_service.workspace.write().await.client.snippets = true;
        }));
        write_started.await.unwrap();
        let later = tokio::spawn(queue.ordered(async move {
            service
                .read_query(|workspace| workspace.client.snippets)
                .await
                .unwrap()
        }));
        let result = tokio::time::timeout(Duration::from_secs(1), later).await;
        let _ = finish.send(());
        assert!(result
            .expect("Latest read waited for obsolete analysis")
            .unwrap());
        writer.await.unwrap();
        assert!(!first.await.unwrap(), "Old analysis observed a later write");
    }

    #[tokio::test]
    async fn cancelling_analysis_does_not_leave_a_worker_holding_the_workspace_lock() {
        let (client, _outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client));
        let mut queue = dispatch::RequestQueue::default();
        let (started, start) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let query_backend = backend.clone();
        let query = tokio::spawn(queue.ordered(async move {
            query_backend
                .read_query(move |_| {
                    let _ = started.send(());
                    let _ = finished.blocking_recv();
                })
                .await
        }));
        start.await.unwrap();
        query.abort();
        assert!(query.await.unwrap_err().is_cancelled());
        let (writing, write_started) = oneshot::channel();
        let writer = tokio::spawn(queue.ordered(async move {
            let _ = writing.send(());
            backend.workspace.write().await.client.snippets = true;
        }));
        write_started.await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), writer).await;
        let _ = finish.send(());
        result
            .expect("Cancelled analysis blocked a document update")
            .unwrap();
    }

    #[tokio::test]
    async fn queued_diagnostics_do_not_block_edits_or_cache_old_text_as_the_new_revision() {
        let (client, _outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client));
        let uri = Url::parse("untitled:diagnostic-snapshot.tex").unwrap();
        backend
            .workspace
            .write()
            .await
            .open(uri.clone(), "{}".into(), 1);
        let cache = backend.diagnostic_cache.clone().lock_owned().await;
        let mut queue = dispatch::RequestQueue::default();
        let request_backend = backend.clone();
        let request_uri = uri.clone();
        let first = tokio::spawn(queue.ordered(async move {
            request_backend
                .diagnostic(
                    serde_json::from_value(serde_json::json!({
                        "textDocument":{"uri":request_uri}
                    }))
                    .unwrap(),
                )
                .await
                .unwrap()
        }));
        let writer_backend = backend.clone();
        let writer_uri = uri.clone();
        let writer = queue.ordered(async move {
            let mut workspace = writer_backend.workspace.write().await;
            workspace.open(writer_uri, "{".into(), 2);
        });
        let result = tokio::time::timeout(Duration::from_secs(1), writer).await;
        drop(cache);
        result.expect("Waiting diagnostics retained the workspace read lock");
        let old = serde_json::to_value(first.await.unwrap()).unwrap();
        assert_eq!(old["items"], serde_json::json!([]));
        let latest = backend
            .diagnostic(
                serde_json::from_value(serde_json::json!({
                    "textDocument":{"uri":uri}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(latest).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn obsolete_pull_reports_are_replaced_by_one_refresh_and_a_current_report() {
        let (client, mut outgoing) = Client::new();
        let backend = Arc::new(Backend::new(client.clone()));
        let uri = Url::parse("untitled:coalesced-diagnostics.tex").unwrap();
        {
            let mut workspace = backend.workspace.write().await;
            workspace.diagnostic_delivery = diagnostic::Delivery::new(true, true);
            workspace.open(uri.clone(), "{}".into(), 1);
        }
        let cache = backend.diagnostic_cache.clone().lock_owned().await;
        let mut queue = dispatch::RequestQueue::default();
        let first_backend = backend.clone();
        let first_uri = uri.clone();
        let first = tokio::spawn(queue.ordered(async move {
            first_backend
                .diagnostic(
                    serde_json::from_value(serde_json::json!({
                        "textDocument":{"uri":first_uri}
                    }))
                    .unwrap(),
                )
                .await
        }));
        let update_backend = backend.clone();
        let update_uri = uri.clone();
        tokio::time::timeout(
            Duration::from_secs(1),
            queue.ordered(async move {
                let mut workspace = update_backend.workspace.write().await;
                workspace.open(update_uri, "{".into(), 2);
            }),
        )
        .await
        .unwrap();
        let obsolete = tokio::time::timeout(Duration::from_secs(1), first).await;
        drop(cache);
        let error = obsolete
            .expect("obsolete report waited for the diagnostic cache")
            .unwrap()
            .unwrap_err();
        assert_eq!(i64::from(error.code), -32802);
        assert_eq!(error.data.unwrap()["retriggerRequest"], false);
        let refresh = tokio::time::timeout(Duration::from_secs(1), outgoing.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refresh["method"], "workspace/diagnostic/refresh");
        client.respond(
            &serde_json::from_value(refresh["id"].clone()).unwrap(),
            Ok(serde_json::Value::Null),
        );
        let latest = backend
            .diagnostic(
                serde_json::from_value(serde_json::json!({
                    "textDocument":{"uri":uri}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(latest).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
