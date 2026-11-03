//! Commit completed builds and deliver connection notifications.
use super::*;

impl BuildManager {
    pub(super) fn launch(&self, input: BuildInput, build_id: u64, cancel: Arc<AtomicBool>) {
        let BuildInput {
            root: root_uri,
            root_path,
            prepared_documents: _,
            snapshots,
            file_snapshots,
            open_aliases,
            config,
            progress_supported,
            project,
            static_code_inputs,
            checks,
            rebuild,
        } = input;
        let manager = self.clone();
        let guard = TaskGuard {
            tasks: self.tasks.clone(),
            idle: self.idle.clone(),
        };
        let client = self.client.clone();
        let workspace = self.workspace.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let executable = config.executable.clone();
            let configuration = config.resolution.clone();
            let engine = if rebuild && config.rebuild_arguments.is_some() {
                None
            } else {
                config.effective_engine()
            };
            let job_name = config
                .job_name(&root_path)
                .map(|name| name.to_string_lossy().into_owned());
            let _ = manager
                .while_open(client.send_notification::<BuildStarted>(json!({
                    "buildId": build_id,
                    "rootUri": root_uri,
                    "executable": executable,
                    "engine": engine,
                    "jobName": job_name,
                    "configuration": configuration,
                    "rebuild": rebuild,
                })))
                .await;
            let started = Instant::now();
            let token = NumberOrString::String(format!("latex-build-{build_id}"));
            // Some clients advertise progress but never answer create requests.
            // A bounded wait keeps compilation and shutdown responsive.
            let progress = progress_supported
                && matches!(
                    manager
                        .while_open(tokio::time::timeout(
                            Duration::from_secs(1),
                            client.send_request::<request::WorkDoneProgressCreate>(
                                WorkDoneProgressCreateParams {
                                    token: token.clone()
                                }
                            )
                        ))
                        .await,
                    Some(Ok(Ok(())))
                );
            if progress {
                let _ = manager
                    .while_open(client.send_notification::<notification::Progress>(
                        ProgressParams {
                            token: token.clone(),
                            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                                WorkDoneProgressBegin {
                                    title: format!(
                                        "LaTeX: {}",
                                        root_path.file_name().unwrap_or_default().to_string_lossy()
                                    ),
                                    cancellable: Some(true),
                                    message: Some(format!("Running {}", config.executable)),
                                    percentage: None,
                                },
                            )),
                        },
                    ))
                    .await;
            }
            let worker_root = root_path.clone();
            let previous_pdf = workspace
                .read()
                .await
                .project
                .pdf_checks
                .get(&root_uri)
                .cloned();
            let worker_inputs: Vec<_> = project
                .iter()
                .filter_map(|uri| uri.to_file_path().ok())
                .collect();
            let wall_started = std::time::SystemTime::now();
            let outcome = tokio::task::spawn_blocking(move || {
                run_with_mode(
                    &worker_root,
                    &config,
                    &cancel,
                    rebuild,
                    previous_pdf.as_deref(),
                    worker_inputs.into_iter(),
                )
            })
            .await;
            let outcome = outcome
                .unwrap_or_else(|error| Outcome::error(format!("Compiler task failed: {error}")));
            let snapshot = workspace.read_unindexed().await.clone();
            let worker_root = root_uri.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                publication::prepare(
                    snapshot,
                    publication::Input {
                        root_uri: worker_root,
                        root_path,
                        snapshots,
                        file_snapshots,
                        open_aliases,
                        project,
                        static_code_inputs,
                        checks,
                        outcome,
                        wall_started,
                        started,
                        metadata: json!({"buildId":build_id, "executable":executable,
                        "engine":engine, "jobName":job_name,
                        "configuration":configuration, "rebuild":rebuild}),
                    },
                )
            })
            .await;
            let mut prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    let result = json!({"buildId":build_id, "rootUri":root_uri,
                        "status":"error", "message":format!("Build result task failed: {error}")});
                    {
                        let mut state = state.lock().unwrap();
                        let record = state.entry(root_uri.clone()).or_default();
                        record.active = None;
                        record.result = Some(result.clone());
                    }
                    let _ = manager
                        .while_open(client.send_notification::<BuildFinished>(result))
                        .await;
                    return;
                }
            };
            let (inlay_refresh, delivery, reschedule_push) = {
                // All disk reads, reloads, diagnostics and artifact fingerprints
                // are complete. Merge only this root and unchanged documents.
                let mut current = workspace.write_prepared().await;
                let invalidate = prepared.invalidates_completion(&current, &root_uri);
                let reschedule = prepared.merge_into(&mut current, &root_uri);
                if invalidate {
                    workspace.invalidate_completion();
                }
                (
                    current.client.inlay_refresh,
                    current.diagnostic_delivery.clone(),
                    reschedule,
                )
            };
            let publication::Publication {
                result,
                checks,
                inputs,
                completed,
                affected,
                notifications,
                ..
            } = prepared;
            {
                let mut state = state.lock().unwrap();
                let record = state.entry(root_uri.clone()).or_default();
                if completed {
                    record.checks = Arc::new(checks.with_inputs(inputs));
                }
                record.active = None;
                record.result = Some(result.clone());
            }
            if reschedule_push && !delivery.is_pull() {
                delivery.schedule(&client, workspace.clone(), affected);
            } else {
                let _ = manager
                    .while_open(delivery.publish(&client, notifications))
                    .await;
            }
            if progress {
                let _ = manager
                    .while_open(client.send_notification::<notification::Progress>(
                        ProgressParams {
                            token,
                            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                                WorkDoneProgressEnd {
                                    message: result["message"].as_str().map(str::to_owned),
                                },
                            )),
                        },
                    ))
                    .await;
            }
            let _ = manager
                .while_open(client.send_notification::<BuildFinished>(result))
                .await;
            if inlay_refresh {
                // UI refresh must not hold the build queue or shutdown open.
                let client = client.clone();
                tokio::spawn(async move {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(5), client.inlay_hint_refresh())
                            .await;
                });
            }
            let queued = state.lock().unwrap().get_mut(&root_uri).and_then(|record| {
                // A new manual build may already own the record. Its saves
                // belong to that build and must remain in its queue.
                if record.active.is_some() {
                    return None;
                }
                record
                    .queued
                    .take()
                    .map(|queued| (queued, record.cancel_generation))
            });
            if let Some((queued, generation)) = queued {
                if !manager.closing.load(Ordering::SeqCst)
                    && (queued.rebuild
                        || workspace.read().await.settings(&queued.uri).build.on_save)
                {
                    if let Err(error) = manager
                        .start_saved_request(
                            queued.uri,
                            Some((root_uri, generation)),
                            queued.rebuild,
                            None,
                            queued.saved,
                        )
                        .await
                    {
                        let _ = manager
                            .while_open(client.log_message(
                                MessageType::WARNING,
                                format!("Queued compilation skipped: {}", error.message),
                            ))
                            .await;
                    }
                }
            }
        });
    }
}
