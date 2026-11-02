//! Expensive build observations are prepared off-lock and conditionally merged.
use super::*;
use crate::project::{
    documents::{Documents, Updates},
    state::ProjectState,
    workspace::Workspace,
};

pub(super) struct Input {
    pub root_uri: Url,
    pub root_path: PathBuf,
    pub snapshots: BTreeMap<Url, (Option<i32>, String)>,
    pub file_snapshots: BTreeMap<Url, InputSnapshot>,
    pub open_aliases: BTreeMap<Url, Url>,
    pub project: BTreeSet<Url>,
    pub static_code_inputs: BTreeSet<Url>,
    pub checks: Arc<crate::compilation::file_checks::Snapshot>,
    pub outcome: Outcome,
    pub wall_started: std::time::SystemTime,
    pub started: Instant,
    pub metadata: Value,
}

pub(super) fn prepare(workspace: Workspace, input: Input) -> Publication {
    let Input {
        root_uri,
        root_path,
        snapshots,
        file_snapshots,
        open_aliases,
        mut project,
        static_code_inputs,
        checks,
        mut outcome,
        wall_started,
        started,
        metadata,
    } = input;
    let mut current = workspace;
    let before = current.documents.clone();
    let pending = current.pending_dependencies.clone();
    let settings = serde_json::to_value(&current.config).unwrap();
    let roots = current.roots.clone();
    current.refresh_pending_dependencies();
    let mut checks = checks;
    if let Some(inputs) = &outcome.inputs {
        let recorded =
            current.project_inputs_with_binary(&root_uri, inputs, &outcome.binary_inputs);
        project.extend(recorded.iter().cloned());
        if matches!(outcome.status, "success" | "error") {
            current
                .project
                .recorded_inputs
                .insert(root_uri.clone(), recorded.clone());
            current.project.recorded_binary_inputs.insert(
                root_uri.clone(),
                outcome
                    .binary_inputs
                    .intersection(&recorded)
                    .cloned()
                    .collect(),
            );
            // Keep observations of known opened paths even if a new
            // external target is outside the source indexing scope.
            // Dropping that path would erase an in-flight retarget.
            let bindings: BTreeMap<_, _> = outcome
                .bindings
                .iter()
                .filter(|(path, target)| {
                    Url::from_file_path(target).is_ok_and(|uri| recorded.contains(&uri))
                        || current
                            .project
                            .recorded_bindings
                            .get(&root_uri)
                            .is_some_and(|previous| previous.contains_key(*path))
                })
                .map(|(path, target)| (path.clone(), target.clone()))
                .chain(outcome.unresolved_bindings.iter().filter_map(|path| {
                    current
                        .project
                        .recorded_bindings
                        .get(&root_uri)?
                        .get(path)
                        .map(|target| (path.clone(), target.clone()))
                }))
                .collect();
            current
                .project
                .recorded_bindings
                .insert(root_uri.clone(), bindings);
            for uri in &recorded {
                current.reload_recorded(uri, outcome.binary_inputs.contains(uri));
            }
        }
    }
    if let Some(inputs) = &outcome.bibliography_inputs {
        // Database/style roles come from the bibliography tool, not a
        // canonical suffix which could look like an image or dataset.
        let recorded = current.project_inputs_with_binary(&root_uri, inputs, &BTreeSet::new());
        project.extend(recorded.iter().cloned());
        if matches!(outcome.status, "success" | "error") {
            current
                .project
                .bibliography_inputs
                .insert(root_uri.clone(), recorded.clone());
            let bindings = outcome
                .bibliography_bindings
                .iter()
                .filter(|(path, target)| {
                    Url::from_file_path(target).is_ok_and(|uri| recorded.contains(&uri))
                        || current
                            .project
                            .bibliography_bindings
                            .get(&root_uri)
                            .is_some_and(|previous| previous.contains_key(*path))
                })
                .map(|(path, target)| (path.clone(), target.clone()))
                .chain(
                    outcome
                        .bibliography_unresolved_bindings
                        .iter()
                        .filter_map(|path| {
                            current
                                .project
                                .bibliography_bindings
                                .get(&root_uri)?
                                .get(path)
                                .map(|target| (path.clone(), target.clone()))
                        }),
                )
                .collect();
            current
                .project
                .bibliography_bindings
                .insert(root_uri.clone(), bindings);
            current.project.bibliography_databases.insert(
                root_uri.clone(),
                outcome
                    .bibliography_databases
                    .intersection(&recorded)
                    .cloned()
                    .collect(),
            );
            for uri in &recorded {
                current.reload_bibliography(uri);
            }
        }
    }
    if matches!(outcome.status, "success" | "error") {
        let empty = BTreeMap::new();
        checks = Arc::new(
            checks.with_bindings([
                (
                    current
                        .project
                        .recorded_bindings
                        .get(&root_uri)
                        .unwrap_or(&empty),
                    if outcome.inputs.is_some() {
                        crate::compilation::file_checks::BindingStatus::Updated
                    } else if outcome.pdf_updated {
                        crate::compilation::file_checks::BindingStatus::Reused
                    } else {
                        crate::compilation::file_checks::BindingStatus::Unchanged
                    },
                ),
                (
                    current
                        .project
                        .bibliography_bindings
                        .get(&root_uri)
                        .unwrap_or(&empty),
                    if outcome.bibliography_inputs.is_some() {
                        crate::compilation::file_checks::BindingStatus::Updated
                    } else {
                        crate::compilation::file_checks::BindingStatus::Reused
                    },
                ),
            ]),
        );
    }
    if outcome.bibliography_inputs.is_none() {
        if let Some(previous) =
            current
                .project
                .bibliography_checks
                .get(&root_uri)
                .filter(|previous| {
                    outcome
                        .pdf
                        .as_ref()
                        .is_some_and(|pdf| previous.matches_pdf(pdf))
                })
        {
            checks = Arc::new(checks.with_reused_inputs(previous));
        }
    }
    // A no-op driver may deliberately ignore an \includeonly-excluded
    // file. Prefer recorded inputs when available, retaining BibTeX
    // sources which TeX's own recorder does not normally list.
    let mut artifact_inputs = current
        .project
        .recorded_inputs
        .get(&root_uri)
        .cloned()
        .unwrap_or_else(|| project.clone());
    artifact_inputs.insert(root_uri.clone());
    if let Some(inputs) = current.project.bibliography_inputs.get(&root_uri) {
        artifact_inputs.extend(inputs.iter().cloned());
    }
    artifact_inputs.extend(current.bibliography_files(&root_uri));
    // Recorder classification cannot enumerate arbitrary code suffixes.
    // Retain static data inputs in the PDF's content history too.
    // An in-flight source edit must not erase the original data scope.
    artifact_inputs.extend(static_code_inputs);
    artifact_inputs.extend(crate::project::code_inputs::inputs(&current, &root_uri));
    artifact_inputs.extend(
        current
            .project_bindings(&root_uri)
            .filter_map(|(_, target)| Url::from_file_path(target).ok()),
    );
    reject_outdated_pdf(
        &root_path,
        &mut outcome,
        artifact_inputs
            .iter()
            .filter_map(|uri| uri.to_file_path().ok()),
        current.project.pdf_checks.get(&root_uri).map(Arc::as_ref),
    );
    if outcome.status == "success" && !outcome.pdf_updated {
        if let Some(reason) = outcome.pdf.as_ref().and_then(|pdf| {
            current
                .project
                .pdf_checks
                .get(&root_uri)
                .filter(|previous| previous.matches_pdf(pdf))
                .and_then(|previous| {
                    previous
                        .changed_path()
                        .map(|path| format!("file condition changed ({})", path.display()))
                        .or_else(|| {
                            previous
                                .changed_input()
                                .map(|uri| format!("input content changed ({uri})"))
                        })
                        .or_else(|| {
                            previous
                                .pdf_changed()
                                .then(|| format!("build artifacts changed ({})", pdf.display()))
                        })
                })
        }) {
            outcome.status = "error";
            outcome.message = format!("Compiler returned an outdated PDF: {reason}");
            outcome.pdf = None;
            outcome.pages = None;
            outcome
                .diagnostics
                .entry(root_uri.clone())
                .or_default()
                .push(Diagnostic {
                    range: Range::default(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("latex-build".into()),
                    message: outcome.message.clone(),
                    ..Default::default()
                });
        }
    }
    // Accept only writes confirmed by a fresh recorder and consumed by
    // a successful explicit backend. Open buffers retain their original
    // version/content checks, even when TeX overwrites their disk file.
    let generated_changed = outcome
        .generated_bibliography
        .iter()
        .any(|input| !input.unchanged());
    let generated: Vec<_> = outcome
        .generated_bibliography
        .iter()
        .filter(|input| {
            outcome.status == "success"
                && outcome.pdf_updated
                && input.unchanged()
                && input.snapshot().is_some_and(|(uri, _)| {
                    snapshots
                        .get(&uri)
                        .is_none_or(|(version, _)| version.is_none())
                        && current
                            .documents
                            .get(&uri)
                            .is_none_or(|doc| doc.version.is_none())
                })
        })
        .cloned()
        .collect();
    let generated_inputs: BTreeMap<_, _> = generated
        .iter()
        .filter_map(|input| input.snapshot())
        .collect();
    checks = Arc::new(checks.with_generated_bibliography(&generated));
    // Include editor aliases of inputs first discovered by the recorder.
    project.extend(current.open_file_aliases(&project).into_keys());
    let stale_reason = generated_changed
        .then(|| "Generated bibliography inputs changed".to_string())
        .or_else(|| {
            open_aliases
                .iter()
                .find(|(alias, target)| {
                    alias
                        .to_file_path()
                        .ok()
                        .and_then(|path| path.canonicalize().ok())
                        != target.to_file_path().ok()
                })
                .map(|(alias, _)| format!("Input binding changed: {alias}"))
        })
        .or_else(|| {
            checks
                .changed_path()
                .map(|path| format!("File condition changed: {}", path.display()))
        })
        .or_else(|| {
            project
                .iter()
                .find(|uri| {
                    if let Some(before) = generated_inputs.get(uri) {
                        return before.changed(uri);
                    }
                    if let Some((version, text)) = snapshots.get(uri) {
                        current.documents.get(uri).is_none_or(|doc| {
                            doc.version != *version
                                || doc.text.source != *text
                                || uri
                                    .to_file_path()
                                    .ok()
                                    .and_then(|path| std::fs::read_to_string(path).ok())
                                    .is_none_or(|saved| !doc.text.matches_saved(&saved))
                        }) || file_snapshots
                            .get(uri)
                            .is_none_or(|before| before.changed(uri))
                    } else if let Some(before) = file_snapshots.get(uri) {
                        before.changed(uri)
                    } else {
                        let Ok(path) = uri.to_file_path() else {
                            return true;
                        };
                        current.documents.get(uri).is_some_and(|doc| {
                            doc.version.is_some()
                                && std::fs::read_to_string(&path)
                                    .ok()
                                    .is_none_or(|saved| !doc.text.matches_saved(&saved))
                        }) || std::fs::metadata(path)
                            .ok()
                            .and_then(|metadata| metadata.modified().ok())
                            .is_none_or(|modified| modified > wall_started)
                    }
                })
                .map(|uri| format!("Input changed: {uri}"))
        });
    let stale = stale_reason.is_some();
    let diagnostic_sources = artifact_inputs
        .intersection(&outcome.diagnostic_sources)
        .cloned()
        .collect();
    if !stale {
        // System package implementations are not user declarations.
        // Use the confirmed project inputs, including local .sty/.cls
        // and indexed external sources, for database source lookup.
        if !outcome.diagnostics_truncated {
            build_log::locate_missing_bibliography_resources(
                &diagnostic_sources,
                &mut outcome.diagnostics,
            );
        }
        crate::symbols::anchors::resolve_diagnostics(
            &current,
            &diagnostic_sources,
            &mut outcome.diagnostics,
        );
        crate::diagnostics::label_diagnostics::resolve_diagnostics(
            &current,
            &diagnostic_sources,
            &mut outcome.diagnostics,
        );
    }
    let count = outcome.diagnostics.values().map(Vec::len).sum::<usize>();
    let errors = outcome
        .diagnostics
        .values()
        .flatten()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .count();
    let warnings = outcome
        .diagnostics
        .values()
        .flatten()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::WARNING))
        .count();
    let pdf_size = outcome
        .pdf
        .as_ref()
        .and_then(|pdf| std::fs::metadata(pdf).ok())
        .map(|metadata| metadata.len());
    let result = json!({
        "buildId":metadata["buildId"], "rootUri":root_uri, "status":outcome.status,
        "exitCode":outcome.exit_code, "message":if stale { format!("{} (sources changed; diagnostics discarded)",outcome.message) } else { outcome.message },
        "pdfUri":outcome.pdf.as_ref().and_then(|path| Url::from_file_path(path).ok()),
        "logUri":outcome.log.as_ref().and_then(|path| Url::from_file_path(path).ok()),
        "diagnosticsCount":count, "stale":stale, "staleReason":stale_reason, "durationMs":started.elapsed().as_millis() as u64,
        "errorsCount":errors, "warningsCount":warnings,
        "diagnosticsTruncated":outcome.diagnostics_truncated,
        "analysis":if stale { None } else { outcome.analysis.as_ref() },
        "pages":outcome.pages, "pdfSize":pdf_size,
        "dependenciesCount":project.len(),
        "executable":metadata["executable"], "engine":metadata["engine"],
        "jobName":metadata["jobName"],
        "configuration":metadata["configuration"],
        "rebuild":metadata["rebuild"],
        "output":outcome.output,
    });
    // Preserve the inputs which produced the PDF even when this run
    // became stale; a later no-op must not adopt changed source contents.
    let inputs: BTreeMap<Url, InputSnapshot> = if matches!(outcome.status, "success" | "error") {
        project
            .union(&artifact_inputs)
            .map(|uri| {
                let snapshot = if let Some(generated) = generated_inputs.get(uri) {
                    generated.clone()
                } else if let Some(before) = file_snapshots.get(uri) {
                    before.clone()
                } else if let Some((_, text)) = snapshots.get(uri) {
                    InputSnapshot::text(text)
                } else if let Some(doc) =
                    current.documents.get(uri).filter(|_| project.contains(uri))
                {
                    InputSnapshot::text(&doc.text.source)
                } else {
                    uri.to_file_path()
                        .map_or(InputSnapshot::File(None), |path| InputSnapshot::file(&path))
                };
                (uri.clone(), snapshot)
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    if outcome.bibliography_inputs.is_some() && matches!(outcome.status, "success" | "error") {
        let mut bibliography = current
            .project
            .bibliography_inputs
            .get(&root_uri)
            .cloned()
            .unwrap_or_default();
        bibliography.extend(
            current
                .project
                .bibliography_bindings
                .get(&root_uri)
                .into_iter()
                .flat_map(|bindings| bindings.values())
                .filter_map(|path| Url::from_file_path(path).ok()),
        );
        let contents = bibliography
            .into_iter()
            .map(|uri| {
                let before = inputs.get(&uri).cloned().unwrap_or_else(|| {
                    uri.to_file_path()
                        .map_or(InputSnapshot::File(None), |path| InputSnapshot::file(&path))
                });
                (uri, before)
            })
            .collect();
        crate::compilation::file_checks::Snapshot::remember(
            &mut current.project.bibliography_checks,
            root_uri.clone(),
            Arc::new(checks.with_inputs(contents)),
        );
    }
    let mut affected = BTreeSet::new();
    // Even discarded diagnostics can accompany a newly written PDF.
    // Retain the inputs and contents of the PDF actually produced.
    // A successful no-op must not adopt a different recorder scope.
    if outcome.status == "success"
        && (!stale || outcome.pdf_updated)
        && outcome.pdf.as_ref().is_some_and(|pdf| {
            outcome.pdf_updated
                || current
                    .project
                    .pdf_checks
                    .get(&root_uri)
                    .is_none_or(|previous| !previous.matches_pdf(pdf))
        })
    {
        crate::compilation::file_checks::Snapshot::remember(
            &mut current.project.pdf_checks,
            root_uri.clone(),
            Arc::new(
                checks
                    .with_inputs(
                        inputs
                            .iter()
                            .filter(|(uri, _)| artifact_inputs.contains(*uri))
                            .map(|(uri, snapshot)| (uri.clone(), snapshot.clone()))
                            .collect(),
                    )
                    .with_artifact_contents(),
            ),
        );
    }
    if stale {
        if let Some(old) = current.project.build_diagnostics.remove(&root_uri) {
            affected.extend(old.into_keys());
        }
        current.project.build_diagnostic_sources.remove(&root_uri);
        current.project.build_checks.remove(&root_uri);
        affected.insert(root_uri.clone());
    } else if matches!(outcome.status, "success" | "error") {
        crate::compilation::file_checks::Snapshot::remember(
            &mut current.project.build_checks,
            root_uri.clone(),
            Arc::new(checks.with_inputs(inputs.clone())),
        );
        current
            .project
            .build_diagnostic_sources
            .insert(root_uri.clone(), diagnostic_sources);
        if let Some(old) = current
            .project
            .build_diagnostics
            .insert(root_uri.clone(), outcome.diagnostics)
        {
            affected.extend(old.into_keys());
        }
        if let Some(new) = current.project.build_diagnostics.get(&root_uri) {
            affected.extend(new.keys().cloned());
        }
        affected.insert(root_uri.clone());
    }
    affected.extend(
        current
            .documents
            .iter()
            .filter_map(|(uri, doc)| doc.version.is_some().then_some(uri.clone())),
    );
    // Publish compiler diagnostics on an open editor alias, retaining
    // its version instead of also sending an unversioned physical URI.
    let aliases = current.open_file_aliases(&affected);
    let physical: BTreeSet<_> = aliases.into_values().collect();
    affected.retain(|uri| {
        !physical.contains(uri)
            || current
                .documents
                .get(uri)
                .is_some_and(|doc| doc.version.is_some())
    });
    let delivery = current.diagnostic_delivery.clone();
    let notifications = if delivery.is_pull() {
        vec![]
    } else {
        current.diagnostic_snapshots(affected.clone())
    };
    let scope = project.union(&artifact_inputs).cloned().collect();
    publication::Publication {
        documents: current.documents.updates_since(&before),
        before,
        pending,
        configuration: settings,
        roots,
        scope,
        project: current.project,
        result,
        checks,
        inputs,
        completed: matches!(outcome.status, "success" | "error"),
        affected,
        notifications,
    }
}

pub(super) struct Publication {
    pub before: Documents,
    pub pending: BTreeSet<Url>,
    pub configuration: Value,
    pub roots: BTreeSet<PathBuf>,
    pub scope: BTreeSet<Url>,
    pub documents: Updates,
    pub project: ProjectState,
    pub result: Value,
    pub checks: Arc<crate::compilation::file_checks::Snapshot>,
    pub inputs: BTreeMap<Url, InputSnapshot>,
    pub completed: bool,
    pub affected: BTreeSet<Url>,
    pub notifications: Vec<crate::diagnostics::diagnostic::Snapshot>,
}

impl Publication {
    pub fn invalidates_completion(&self, current: &Workspace, root: &Url) -> bool {
        !self.documents.is_empty() || !self.project.same_completion_inputs(&current.project, root)
    }

    /// No filesystem reads or parsing here. A late edit invalidates diagnostics,
    /// while PDF/input history still records the artifact actually produced.
    pub fn merge_into(&mut self, current: &mut Workspace, root: &Url) -> bool {
        let configuration_changed = self.configuration
            != serde_json::to_value(&current.config).unwrap()
            || self.roots != current.roots;
        let late_edit = configuration_changed
            || !self
                .before
                .same_sources_for(&current.documents, &self.scope);
        let reschedule_push = late_edit || !self.before.same_revisions(&current.documents);
        if late_edit && self.completed {
            if self.result["stale"] != true {
                self.result["message"] = json!(format!(
                    "{} (sources changed; diagnostics discarded)",
                    self.result["message"].as_str().unwrap_or_default()
                ));
            }
            self.result["stale"] = json!(true);
            self.result["staleReason"] = json!("Inputs changed during build result preparation");
            self.result["analysis"] = Value::Null;
            self.project.build_diagnostics.remove(root);
            self.project.build_diagnostic_sources.remove(root);
            self.project.build_checks.remove(root);
        }
        if !configuration_changed {
            if self.before.same_revisions(&current.documents) {
                for uri in &self.pending {
                    current.pending_dependencies.remove(uri);
                }
            }
            current
                .documents
                .apply_updates(std::mem::take(&mut self.documents));
        }
        current.project.merge_build(root, &mut self.project);
        reschedule_push
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::workspace::Document;

    fn captured(before: &Workspace, prepared: Workspace, root: &Url) -> Publication {
        Publication {
            before: before.documents.clone(),
            pending: before.pending_dependencies.clone(),
            configuration: serde_json::to_value(&before.config).unwrap(),
            roots: before.roots.clone(),
            scope: [root.clone()].into(),
            documents: prepared.documents.updates_since(&before.documents),
            project: prepared.project,
            result: json!({"status":"success", "stale":false, "message":"Compilation succeeded", "analysis":{"pages":1}}),
            checks: Arc::default(),
            inputs: BTreeMap::new(),
            completed: true,
            affected: [root.clone()].into(),
            notifications: vec![],
        }
    }

    fn workspace() -> (Workspace, Url) {
        let root = Url::parse("file:///publication/main.tex").unwrap();
        let mut workspace = Workspace::default();
        workspace.documents.insert(
            root.clone(),
            Document::new(&root, "Original.\n".into(), Some(1)),
        );
        workspace.project.build_diagnostics.insert(
            root.clone(),
            [(root.clone(), vec![Diagnostic::default()])].into(),
        );
        (workspace, root)
    }

    #[test]
    fn diagnostics_and_pdf_history_do_not_invalidate_completion_inputs() {
        let (before, root) = workspace();
        let mut prepared = before.clone();
        prepared.project.build_diagnostics.remove(&root);
        prepared
            .project
            .pdf_checks
            .insert(root.clone(), Arc::default());
        let publication = captured(&before, prepared, &root);
        assert!(!publication.invalidates_completion(&before, &root));
    }

    #[test]
    fn edits_after_result_preparation_preserve_buffers_and_artifact_history() {
        let (before, root) = workspace();
        let mut prepared = before.clone();
        prepared
            .project
            .pdf_checks
            .insert(root.clone(), Arc::default());
        let mut result = captured(&before, prepared, &root);
        let mut current = before.clone();
        current.documents.insert(
            root.clone(),
            Document::new(&root, "New unsaved text.\n".into(), Some(2)),
        );
        assert!(result.merge_into(&mut current, &root));
        assert_eq!(current.documents[&root].text.source, "New unsaved text.\n");
        assert_eq!(result.result["stale"], true);
        assert!(!current.project.build_diagnostics.contains_key(&root));
        assert!(current.project.pdf_checks.contains_key(&root));
    }

    #[test]
    fn publication_does_not_replace_other_roots_or_reject_unrelated_edits() {
        let (before, root) = workspace();
        let other = root.join("other.tex").unwrap();
        let mut result = captured(&before, before.clone(), &root);
        let mut current = before.clone();
        current.documents.insert(
            other.clone(),
            Document::new(&other, "Unrelated.\n".into(), Some(1)),
        );
        current
            .project
            .recorded_inputs
            .insert(other.clone(), [other.clone()].into());
        current
            .project
            .build_diagnostics
            .insert(other.clone(), BTreeMap::new());
        assert!(result.merge_into(&mut current, &root));
        assert_eq!(result.result["stale"], false);
        assert!(current.project.build_diagnostics.contains_key(&root));
        assert!(current.project.build_diagnostics.contains_key(&other));
        assert_eq!(current.project.recorded_inputs[&other], [other].into());
    }

    #[test]
    fn changed_configuration_prevents_old_source_reload_and_diagnostic_publication() {
        let (before, root) = workspace();
        let mut prepared = before.clone();
        let child = root.join("child.tex").unwrap();
        prepared.documents.insert(
            child.clone(),
            Document::new(&child, "Loaded from old configuration.".into(), None),
        );
        let mut result = captured(&before, prepared, &root);
        let mut current = before.clone();
        current.config.build.output_directory = Some("new-build".into());
        assert!(result.merge_into(&mut current, &root));
        assert_eq!(result.result["stale"], true);
        assert!(!current.documents.contains_key(&child));
        assert!(!current.project.build_diagnostics.contains_key(&root));
    }
}
