//! Execute TeX tools and collect their observed outputs.
use super::{recovery::Recovery, BuildInputs};
use crate::{
    compilation::{
        build_log,
        file_checks::{fingerprint, ArtifactFingerprint, Snapshot},
    },
    config::BuildConfig,
    process::kill_group,
    project::workspace::Workspace,
};
use lsp_types::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

pub struct Outcome {
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub message: String,
    pub pdf: Option<PathBuf>,
    pub pages: Option<u32>,
    pub log: Option<PathBuf>,
    pub diagnostics: BTreeMap<Url, Vec<Diagnostic>>,
    pub diagnostics_truncated: bool,
    /// Summary available only from this run's observed final TeX log.
    pub analysis: Option<Value>,
    /// Sources whose file-entry events were present in the final TeX log.
    pub diagnostic_sources: BTreeSet<Url>,
    pub output: String,
    pub inputs: Option<BTreeSet<Url>>,
    /// Recorder-confirmed image/data types, before resolving symlink names.
    pub binary_inputs: BTreeSet<Url>,
    pub bindings: BTreeMap<PathBuf, PathBuf>,
    pub unresolved_bindings: BTreeSet<PathBuf>,
    pub bibliography_inputs: Option<BTreeSet<Url>>,
    pub bibliography_databases: BTreeSet<Url>,
    pub bibliography_bindings: BTreeMap<PathBuf, PathBuf>,
    pub bibliography_unresolved_bindings: BTreeSet<PathBuf>,
    pub pdf_updated: bool,
    pub(crate) generated_bibliography: Vec<crate::compilation::build_tools::Watched>,
}

impl Outcome {
    pub(crate) fn error(message: String) -> Self {
        Self {
            status: "error",
            exit_code: None,
            message,
            pdf: None,
            pages: None,
            log: None,
            diagnostics: BTreeMap::new(),
            diagnostics_truncated: false,
            analysis: None,
            diagnostic_sources: BTreeSet::new(),
            output: String::new(),
            inputs: None,
            binary_inputs: BTreeSet::new(),
            bindings: BTreeMap::new(),
            unresolved_bindings: BTreeSet::new(),
            bibliography_inputs: None,
            bibliography_databases: BTreeSet::new(),
            bibliography_bindings: BTreeMap::new(),
            bibliography_unresolved_bindings: BTreeSet::new(),
            generated_bibliography: vec![],
            pdf_updated: false,
        }
    }
}

pub(crate) fn capture(reader: impl Read) -> Vec<u8> {
    crate::process::output::capture(reader, MAX_OUTPUT)
}

pub fn run(root: &Path, config: &BuildConfig, cancel: &AtomicBool) -> Outcome {
    run_with_mode(
        root,
        config,
        cancel,
        false,
        None,
        BuildInputs {
            paths: vec![root.to_path_buf()],
            protected: vec![],
        },
    )
}

pub fn rebuild(root: &Path, config: &BuildConfig, cancel: &AtomicBool) -> Outcome {
    run_with_mode(
        root,
        config,
        cancel,
        true,
        None,
        BuildInputs {
            paths: vec![root.to_path_buf()],
            protected: vec![],
        },
    )
}

pub(crate) fn run_with_mode(
    root: &Path,
    config: &BuildConfig,
    cancel: &AtomicBool,
    rebuild: bool,
    previous_pdf: Option<&Snapshot>,
    inputs: BuildInputs,
) -> Outcome {
    // A task can be cancelled while waiting for client progress registration,
    // before its worker starts. Preserve existing artifacts in that case.
    if cancel.load(Ordering::Relaxed) {
        return Outcome {
            status: "cancelled",
            ..Outcome::error("Compilation cancelled".into())
        };
    }
    // On first opening a project there is no content history for its existing
    // PDF. latexmk may skip a source saved with identical contents even though
    // its timestamp is newer. Force one pass to establish a verified PDF and
    // recorder instead of rejecting that legitimate incremental build.
    let config = config.for_build_mode(root, rebuild);
    let config = match config.resolve_for_build(root, cancel) {
        Ok(config) => config,
        Err(message) => return Outcome::error(message),
    };
    let config = &config;
    // Reject invalid arguments and explicit workflows before recovery can
    // touch any artifacts, just as an ordinary build does.
    if let Err(message) = if rebuild {
        config.expanded_rebuild_arguments(root).map(|_| ())
    } else {
        config
            .expanded_arguments(root)
            .ok_or_else(|| "Invalid root filename".to_string())
            .map(|_| ())
    } {
        return Outcome::error(message);
    }
    if !config.index_tools.is_empty() || config.bibliography_tool.is_some() {
        if let Err(message) = config.rebuild_arguments() {
            return Outcome::error(message);
        }
        if let Err(failure) = crate::compilation::index_processing::Plan::prepare(root, config) {
            return Outcome::error(failure.message);
        }
        if let Err(failure) =
            crate::compilation::bibliography_processing::Plan::prepare(root, config)
        {
            return Outcome::error(failure.message);
        }
    }
    let protected: Vec<_> = inputs
        .protected
        .into_iter()
        .chain(inputs.paths.iter().cloned())
        .collect();
    let mut recovery = match Recovery::begin(root, config, &protected) {
        Ok(recovery) => recovery,
        Err(message) => return Outcome::error(message),
    };
    if let Err(message) = recovery.mark_running() {
        return Outcome::error(message);
    }
    let default_latexmk = config.arguments.is_none()
        && config.rebuild_arguments.is_none()
        && Path::new(&config.executable)
            .file_stem()
            .is_some_and(|name| name == "latexmk");
    let refresh = !rebuild
        && default_latexmk
        && config.artifact_path(root, "pdf").is_some_and(|pdf| {
            previous_pdf.is_some_and(|previous| {
                previous.matches_pdf(&pdf) && previous.changed_path().is_some()
            }) || previous_pdf.is_none_or(|previous| !previous.matches_pdf(&pdf))
                && outdated_pdf_input(&pdf, inputs.paths.into_iter(), None).is_some()
        });
    let mut outcome = run_inner(root, config, cancel, rebuild || refresh, &recovery);
    // Another compiler invocation can update latexmk's cache and PDF without
    // updating this server's input history. If latexmk then skips TeX, establish
    // a fresh history rather than repeatedly rejecting its cached result.
    // Only retry a no-op: ordinary source changes already rerun TeX themselves.
    if default_latexmk
        && !rebuild
        && !refresh
        && outcome.status == "success"
        && !outcome.pdf_updated
        && outcome.pdf.as_ref().is_some_and(|pdf| {
            previous_pdf.is_some_and(|previous| {
                previous.matches_pdf(pdf)
                    && (previous.pdf_changed()
                        || previous.changed_input().is_some()
                        || previous.changed_path().is_some())
            })
        })
    {
        outcome = run_inner(root, config, cancel, true, &recovery);
    }
    if let Err(message) = recovery
        .finish(outcome.exit_code.is_some() && !matches!(outcome.status, "cancelled" | "timeout"))
    {
        outcome = Outcome::error(format!("Cannot finish build recovery state: {message}"));
    }
    reject_outdated_pdf(
        root,
        &mut outcome,
        std::iter::once(root.to_path_buf()),
        previous_pdf,
    );
    if outcome.status == "error"
        && !outcome
            .diagnostics
            .values()
            .flatten()
            .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
    {
        if let Ok(uri) = Url::from_file_path(root) {
            outcome
                .diagnostics
                .entry(uri)
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
    outcome
}

pub(crate) fn reject_outdated_pdf(
    root: &Path,
    outcome: &mut Outcome,
    sources: impl Iterator<Item = PathBuf>,
    previous_pdf: Option<&Snapshot>,
) {
    if outcome.status != "success" || outcome.pdf_updated {
        return;
    }
    let Some(pdf_path) = outcome.pdf.as_ref() else {
        return;
    };
    if let Some(source) = outdated_pdf_input(pdf_path, sources, previous_pdf) {
        outcome.status = "error";
        outcome.pdf = None;
        outcome.pages = None;
        outcome.message = format!(
            "Compiler left an outdated PDF unchanged; rebuild after changes to {}",
            source.display()
        );
        if let Ok(uri) = Url::from_file_path(root) {
            outcome
                .diagnostics
                .entry(uri)
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
}

fn outdated_pdf_input(
    pdf_path: &Path,
    mut sources: impl Iterator<Item = PathBuf>,
    previous_pdf: Option<&Snapshot>,
) -> Option<PathBuf> {
    let pdf = fingerprint(pdf_path).and_then(|(_, modified)| modified)?;
    // Editors can rewrite identical inputs after the PDF was produced, while
    // latexmk correctly skips TeX. Only override the timestamp check using the
    // input history of this exact, unmodified PDF; unknown inputs stay unsafe.
    let previous_pdf =
        previous_pdf.filter(|previous| previous.matches_pdf(pdf_path) && !previous.pdf_changed());
    sources.find(|source| {
        std::fs::metadata(source)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .is_none_or(|modified| {
                modified > pdf
                    && previous_pdf.is_none_or(|previous| {
                        Url::from_file_path(source).map_or(true, |uri| previous.input_changed(&uri))
                    })
            })
    })
}

fn run_inner(
    root: &Path,
    config: &BuildConfig,
    cancel: &AtomicBool,
    rebuild: bool,
    recovery: &Recovery,
) -> Outcome {
    if config.index_tools.is_empty() && config.bibliography_tool.is_none() {
        return run_single(root, config, cancel, rebuild, None, None, recovery);
    }
    // Validate the entire explicit workflow before the first compiler touches
    // logs, including custom forced-pass arguments and style dependencies.
    if let Err(message) = config.rebuild_arguments() {
        return Outcome::error(message);
    }
    let plan = match crate::compilation::index_processing::Plan::prepare(root, config) {
        Ok(plan) => plan,
        Err(failure) => return Outcome::error(failure.message),
    };
    let bibliography =
        match crate::compilation::bibliography_processing::Plan::prepare(root, config) {
            Ok(plan) => plan,
            Err(failure) => return Outcome::error(failure.message),
        };
    let bibliography_before = config
        .auxiliary_path(root)
        .map(|output| bibliography_logs(&output))
        .unwrap_or_default();
    let mut first = run_single(root, config, cancel, rebuild, None, None, recovery);
    // An index-only document can legitimately have no pages until its first
    // .ind has been generated. A successful TeX process can still feed tools.
    if !matches!(first.status, "success" | "error")
        || first.exit_code != Some(0)
        || first
            .diagnostics
            .values()
            .flatten()
            .any(|item| item.severity == Some(DiagnosticSeverity::ERROR))
    {
        return first;
    }
    for round in 1..=4 {
        let bibliography_processed = match bibliography
            .as_ref()
            .map(|plan| plan.run(cancel, &first.generated_bibliography, recovery))
            .transpose()
        {
            Ok(processed) => processed,
            Err(failed) => {
                if let Some(report) = failed.report {
                    first.diagnostics_truncated |= report.truncated;
                    for (uri, diagnostics) in report.diagnostics {
                        first
                            .diagnostics
                            .entry(uri)
                            .or_default()
                            .extend(diagnostics);
                    }
                    first.bibliography_inputs = Some(report.inputs.all);
                    first.bibliography_databases = report.inputs.bibliography_databases;
                    first.bibliography_bindings = report.inputs.bindings;
                    first.bibliography_unresolved_bindings = report.inputs.unresolved_bindings;
                }
                return stage_failure(first, failed.failure);
            }
        };
        let processed = match plan.run(config, cancel, recovery) {
            Ok(processed) => processed,
            Err(failed) => {
                first.diagnostics_truncated |= failed.report.truncated;
                for (uri, diagnostics) in failed.report.diagnostics {
                    first
                        .diagnostics
                        .entry(uri)
                        .or_default()
                        .extend(diagnostics);
                }
                return stage_failure(first, failed.failure);
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return Outcome {
                status: "cancelled",
                ..Outcome::error("Compilation cancelled".into())
            };
        }
        first.output = output_tail(&format!(
            "{}\n{}\n{}",
            first.output,
            bibliography_processed
                .as_ref()
                .map_or("", |processed| processed.output.as_str()),
            processed.output
        ));
        for _ in 0..if bibliography.is_some() { 2 } else { 1 } {
            let mut final_pass = run_single(
                root,
                config,
                cancel,
                true,
                Some(&bibliography_before),
                bibliography_processed
                    .as_ref()
                    .map(|processed| processed.search_output.as_str()),
                recovery,
            );
            final_pass.output = output_tail(&format!("{}\n{}", first.output, final_pass.output));
            final_pass.pdf_updated |= first.pdf_updated;
            if let Some(inputs) = first.inputs.take() {
                final_pass
                    .inputs
                    .get_or_insert_with(BTreeSet::new)
                    .extend(inputs);
                final_pass.binary_inputs.extend(first.binary_inputs);
                for (path, target) in first.bindings {
                    final_pass.bindings.entry(path).or_insert(target);
                }
                final_pass
                    .unresolved_bindings
                    .extend(first.unresolved_bindings);
            }
            first = final_pass;
            if first.status != "success" {
                return first;
            }
        }
        if processed.inputs_unchanged()
            && bibliography_processed
                .as_ref()
                .is_none_or(|processed| processed.inputs_unchanged())
        {
            first.generated_bibliography = bibliography_processed
                .as_ref()
                .map_or_else(Vec::new, |processed| processed.generated.clone());
            first.diagnostics_truncated |= processed.report.truncated;
            for (uri, diagnostics) in processed.report.diagnostics {
                first
                    .diagnostics
                    .entry(uri)
                    .or_default()
                    .extend(diagnostics);
            }
            if let Some(analysis) = first.analysis.as_mut().and_then(Value::as_object_mut) {
                if !config.index_tools.is_empty() {
                    analysis.insert("processedIndexTools".into(), Value::Array(processed.tools));
                    analysis.insert("indexProcessingRounds".into(), Value::from(round));
                }
                if let Some(processed) = bibliography_processed {
                    analysis.insert("processedBibliographyTool".into(), processed.tool);
                    analysis.insert("bibliographyProcessingRounds".into(), Value::from(round));
                }
            }
            return first;
        }
    }
    first.status = "error";
    first.pdf = None;
    first.pages = None;
    first.analysis = None;
    first.message = "Build tool inputs did not stabilize after four processing rounds; check the document and build again".into();
    first
}

fn stage_failure(
    mut outcome: Outcome,
    failure: crate::compilation::build_tools::Failure,
) -> Outcome {
    outcome.status = failure.status;
    outcome.exit_code = failure.exit_code;
    outcome.message = failure.message;
    outcome.pdf = None;
    outcome.pages = None;
    outcome.analysis = None;
    outcome.output = output_tail(&format!("{}\n{}", outcome.output, failure.output));
    outcome
}

fn output_tail(output: &str) -> String {
    output
        .chars()
        .rev()
        .take(8192)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn run_single(
    root: &Path,
    config: &BuildConfig,
    cancel: &AtomicBool,
    rebuild: bool,
    bibliography_baseline: Option<&BTreeMap<PathBuf, ArtifactFingerprint>>,
    bibliography_search: Option<&str>,
    recovery: &Recovery,
) -> Outcome {
    let Some(parent) = root.parent() else {
        return Outcome::error("Invalid root path".into());
    };
    // Validate custom rebuild arguments before touching logs or artifacts.
    let arguments = if rebuild {
        match config.expanded_rebuild_arguments(root) {
            Ok(arguments) => arguments,
            Err(message) => return Outcome::error(message),
        }
    } else {
        let Some(arguments) = config.expanded_arguments(root) else {
            return Outcome::error("Invalid root filename".into());
        };
        arguments
    };
    let output_dir = config.auxiliary_path(root).unwrap();
    for directory in [config.output_path(root), config.final_output_path(root)]
        .into_iter()
        .flatten()
    {
        if let Err(error) = std::fs::create_dir_all(directory) {
            return Outcome::error(format!("Cannot create build directory: {error}"));
        }
    }
    if let Err(error) = std::fs::create_dir_all(&output_dir) {
        return Outcome::error(format!("Cannot create build directory: {error}"));
    }
    if matches!(
        Path::new(&config.executable)
            .file_stem()
            .and_then(|name| name.to_str()),
        Some("latexmk" | "pdflatex" | "xelatex" | "lualatex")
    ) {
        if let Err(error) = prepare_include_directories(root, &output_dir) {
            return Outcome::error(format!(
                "Cannot prepare chapter auxiliary directories: {error}"
            ));
        }
    }
    let Some(log_path) = config.artifact_path(root, "log") else {
        return Outcome::error("Invalid root filename".into());
    };
    let pdf_path = config.artifact_path(root, "pdf").unwrap();
    let pdf_before = ArtifactFingerprint::capture(&pdf_path);
    let recorder_path = config.artifact_path(root, "fls").unwrap();
    let recorder_before = ArtifactFingerprint::capture(&recorder_path);
    let bibliography_at_start = bibliography_logs(&output_dir);
    let bibliography_before = bibliography_baseline.unwrap_or(&bibliography_at_start);
    // latexmk can reuse a failed invocation without rerunning TeX. Keep its
    // transcript intact; decide after the process exits whether it is current.
    let log_before = ArtifactFingerprint::capture(&log_path);
    let diagnostic_project = build_log::ProjectSources::capture(root);
    let mut command = Command::new(&config.executable);
    command
        .args(arguments)
        .current_dir(parent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    recovery.inherit(&mut command);
    let mut retries = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            // A newly installed/replaced executable can briefly be ETXTBSY.
            // Retry this transient OS error only, with a bounded backoff.
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && retries < 3 => {
                std::thread::sleep(Duration::from_millis(10 << retries));
                retries += 1;
            }
            Err(error) => {
                return Outcome::error(format!("Cannot start {}: {error}", config.executable))
            }
        }
    };
    recovery.spawned();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || capture(stdout));
    let stderr = std::thread::spawn(move || capture(stderr));
    let started = Instant::now();
    let mut outcome = Outcome::error("Compilation failed".into());
    loop {
        if cancel.load(Ordering::Relaxed)
            || started.elapsed() > Duration::from_secs(config.timeout_seconds)
        {
            outcome.status = if cancel.load(Ordering::Relaxed) {
                "cancelled"
            } else {
                "timeout"
            };
            outcome.message = if outcome.status == "cancelled" {
                "Compilation cancelled"
            } else {
                "Compilation timed out"
            }
            .into();
            kill_group(&mut child);
            let _ = child.wait();
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                outcome.exit_code = status.code();
                if status.success() {
                    if std::fs::metadata(&pdf_path).is_ok_and(|metadata| metadata.len() > 0) {
                        outcome.status = "success";
                        outcome.message = "Compilation succeeded".into();
                        outcome.pdf = Some(pdf_path);
                    } else {
                        outcome.message = "Compiler exited successfully but produced no PDF".into();
                    }
                } else {
                    outcome.message = format!("{} exited with {}", config.executable, status);
                }
                kill_group(&mut child);
                break;
            }
            Err(error) => {
                outcome.message = format!("Cannot wait for compiler: {error}");
                kill_group(&mut child);
                let _ = child.wait();
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let output = format!(
        "{}\n{}",
        build_log::decode_output(&stdout),
        build_log::decode_output(&stderr)
    );
    let output = if let Some(search) = bibliography_search {
        format!("{search}\n{output}")
    } else {
        output
    };
    // An unchanged transcript is authoritative only when latexmk explicitly
    // reuses its previous invocation. Spawn failures return above; timeouts,
    // cancellation and unrelated tool failures must not revive an old log.
    let reused_log = outcome.exit_code.is_some()
        && matches!(outcome.status, "success" | "error")
        && Path::new(&config.executable)
            .file_stem()
            .and_then(|name| name.to_str())
            == Some("latexmk")
        && output
            .lines()
            .any(|line| line.starts_with("Latexmk: Nothing to do for '"));
    let current_log = reused_log || ArtifactFingerprint::capture(&log_path) != log_before;
    let log = if current_log {
        read_log(&log_path).unwrap_or_default()
    } else {
        String::new()
    };
    outcome.pdf_updated = outcome
        .pdf
        .as_ref()
        .is_some_and(|pdf| ArtifactFingerprint::capture(pdf) != pdf_before);
    let recorder_after = ArtifactFingerprint::capture(&recorder_path);
    if recorder_after.is_some() && recorder_after != recorder_before {
        // Partial dependency lists must not replace the last complete recorder.
        if let Some(recorder) =
            read_log(&recorder_path).filter(|log| !crate::process::output::truncated(log))
        {
            if config.bibliography_tool.is_some() {
                outcome.generated_bibliography =
                    crate::compilation::recorder::bibliography_outputs(
                        root,
                        &output_dir,
                        &recorder,
                    )
                    .iter()
                    .filter_map(|path| crate::compilation::build_tools::Watched::capture(path).ok())
                    .collect();
            }
            let inputs = crate::compilation::recorder::classified_build_inputs(
                root,
                &recorder,
                config.artifact_path(root, "run.xml").as_deref(),
            );
            outcome.inputs = Some(inputs.all);
            outcome.binary_inputs = inputs.binary;
            outcome.bindings = inputs.bindings;
            outcome.unresolved_bindings = inputs.unresolved_bindings;
        }
    }
    // latexmk can run TeX several times. The final .log is authoritative;
    // earlier stdout passes may contain references that are already resolved.
    let diagnostic_log = if log.is_empty() { &output } else { &log };
    outcome.diagnostics_truncated = crate::process::output::truncated(diagnostic_log);
    (outcome.diagnostics, outcome.diagnostic_sources) =
        build_log::parse_with_project_sources(root, diagnostic_log, &diagnostic_project);
    // A failed PDF driver may report its cause only on stderr. Retain the final
    // TeX log's authority: fill an unexplained failure, without reviving an old
    // backend error after a successful pass or a different final TeX error.
    if !log.is_empty()
        && outcome.status == "error"
        && !outcome
            .diagnostics
            .values()
            .flatten()
            .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
    {
        let driver_diagnostics = build_log::parse_driver_output(root, &output);
        if !driver_diagnostics.is_empty() {
            outcome.diagnostics_truncated |= crate::process::output::truncated(&output);
        }
        for (uri, diagnostics) in driver_diagnostics {
            let entries = outcome.diagnostics.entry(uri).or_default();
            for diagnostic in diagnostics {
                if !entries.contains(&diagnostic) {
                    entries.push(diagnostic);
                }
            }
        }
    }
    let mut bibliography_complete = true;
    for (path, fingerprint) in bibliography_logs(&output_dir) {
        // An old .blg left by a previous failed build is not this build's result.
        if bibliography_before.get(&path) == Some(&fingerprint) {
            continue;
        }
        if let Some(log) = read_log(&path) {
            let truncated = crate::process::output::truncated(&log);
            bibliography_complete &= !truncated;
            outcome.diagnostics_truncated |= truncated;
            let paths = crate::compilation::recorder::BibliographyPaths::new(root, &path, &log)
                .with_search_output(&output);
            if !truncated {
                let inputs =
                    crate::compilation::recorder::classified_bibliography_inputs(&paths, &log);
                outcome
                    .bibliography_inputs
                    .get_or_insert_with(BTreeSet::new)
                    .extend(inputs.all);
                outcome
                    .bibliography_databases
                    .extend(inputs.bibliography_databases);
                outcome.bibliography_bindings.extend(inputs.bindings);
                outcome
                    .bibliography_unresolved_bindings
                    .extend(inputs.unresolved_bindings);
            }
            let mut diagnostics = build_log::parse_bibliography_in(root, &paths, &log);
            if !outcome.diagnostics_truncated {
                build_log::locate_missing_bibliography_styles(
                    &outcome.diagnostic_sources,
                    &mut diagnostics,
                );
            }
            for (uri, diagnostics) in diagnostics {
                outcome
                    .diagnostics
                    .entry(uri)
                    .or_default()
                    .extend(diagnostics);
            }
        }
    }
    if !bibliography_complete {
        outcome.bibliography_inputs = None;
        outcome.bibliography_databases.clear();
        outcome.bibliography_bindings.clear();
        outcome.bibliography_unresolved_bindings.clear();
    }
    if Path::new(&config.executable)
        .file_stem()
        .and_then(|name| name.to_str())
        == Some("latexmk")
    {
        for (uri, diagnostics) in
            build_log::parse_latexmk_missing_databases(root, &log_path, &output)
        {
            for diagnostic in diagnostics {
                // An actual BibTeX error is more useful than the driver's
                // warning about the same missing database.
                let duplicate = outcome.diagnostics.values().flatten().any(|item| {
                    item.code == diagnostic.code
                        && item.data.as_ref().and_then(|data| data.get("file"))
                            == diagnostic.data.as_ref().and_then(|data| data.get("file"))
                });
                if !duplicate {
                    outcome
                        .diagnostics
                        .entry(uri.clone())
                        .or_default()
                        .push(diagnostic);
                }
            }
        }
    }
    if outcome.status == "success"
        && outcome
            .diagnostics
            .values()
            .flatten()
            .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
    {
        outcome.status = "error";
        outcome.message = "Compiler reported errors; generated PDF may be incomplete".into();
        outcome.pdf = None;
    }
    if current_log && log_path.is_file() {
        outcome.log = Some(log_path);
    }
    if outcome.status == "error" {
        if let Some(error) = outcome
            .diagnostics
            .values()
            .flatten()
            .find(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        {
            outcome.message = error.message.clone();
        }
    }
    if outcome.pdf.is_some() {
        match build_log::output_summary(if log.is_empty() { &output } else { &log }) {
            Some(build_log::OutputSummary::Pages(pages)) => outcome.pages = Some(pages),
            Some(build_log::OutputSummary::NoPages) => {
                outcome.status = "error";
                outcome.pdf = None;
                outcome.message =
                    "Compiler reported no pages of output; no PDF was produced".into();
            }
            None => {}
        }
    }
    // Notifications carry a bounded tail; pipe readers above still drain all output.
    outcome.output = output
        .chars()
        .rev()
        .take(8192)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if !log.is_empty() && matches!(outcome.status, "success" | "error") {
        outcome.analysis = Some(build_log::compilation_analysis(
            &outcome.diagnostics,
            outcome.diagnostics_truncated,
        ));
    }
    outcome
}

/// TeX writes `chapters/name.aux` below the output directory, but does not
/// create `chapters/`. latexmk's recovery is not reliable with file-line errors.
/// Prepare only static paths contained in that output directory.
fn prepare_include_directories(root: &Path, output: &Path) -> std::io::Result<()> {
    let Ok(uri) = Url::from_file_path(root) else {
        return Ok(());
    };
    let mut workspace = Workspace::default();
    workspace.refresh_project(&uri);
    let inputs = crate::project::source_inputs::Index::new(&workspace, &uri);
    let mut directories = BTreeSet::new();
    for (uri, doc) in &workspace.documents {
        let indirect = inputs
            .calls
            .get(uri)
            .into_iter()
            .flatten()
            .filter(|call| call.certain)
            .map(|call| &call.command);
        for command in doc
            .syntax
            .commands
            .iter()
            .chain(indirect)
            .filter(|command| command.name == "include")
            .take(4096)
        {
            let Some(argument) = crate::parser::syntax::file_argument(&doc.text.source, command)
            else {
                continue;
            };
            let name = argument.value.trim();
            if name.is_empty() || name.contains(['\\', '%', '#', '$', '{', '}', '\n', '\r', '\0']) {
                continue;
            }
            let path = Path::new(name);
            if path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            }) {
                continue;
            }
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                directories.insert(output.join(parent));
                if directories.len() >= 1024 {
                    break;
                }
            }
        }
        if directories.len() >= 1024 {
            break;
        }
    }
    for directory in directories {
        std::fs::create_dir_all(directory)?;
    }
    Ok(())
}

fn read_log(path: &Path) -> Option<String> {
    let text = crate::process::output::read(path, MAX_OUTPUT * 2).ok()?;
    Some(build_log::decode_output(&text).into_owned())
}

fn bibliography_logs(directory: &Path) -> BTreeMap<PathBuf, ArtifactFingerprint> {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? != "blg" {
                return None;
            }
            if !entry.metadata().ok()?.is_file() {
                return None;
            }
            let snapshot = ArtifactFingerprint::capture(&path)?;
            Some((path, snapshot))
        })
        .take(64)
        .collect()
}
