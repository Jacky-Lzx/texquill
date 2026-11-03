//! Explicit index/glossary stages owned by the normal build task.
use crate::{
    compilation::build_tools::{Failure, Watched},
    config::{BuildConfig, IndexToolConfig},
};
use lsp_types::Url;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

struct Stage {
    config: IndexToolConfig,
    input: PathBuf,
    output: PathBuf,
    dependencies: Vec<Watched>,
}
pub(crate) struct Plan {
    root: PathBuf,
    output: PathBuf,
    stages: Vec<Stage>,
}
pub(crate) struct Failed {
    pub failure: Failure,
    pub report: crate::diagnostics::index_diagnostics::Report,
}
pub(crate) struct Processed {
    pub output: String,
    pub tools: Vec<serde_json::Value>,
    pub report: crate::diagnostics::index_diagnostics::Report,
    inputs: Vec<Watched>,
}
impl Processed {
    pub(crate) fn inputs_unchanged(&self) -> bool {
        self.inputs.iter().all(Watched::unchanged)
    }
}

fn artifact(
    root: &Path,
    build: &BuildConfig,
    tool: &IndexToolConfig,
    extension: &str,
) -> Option<PathBuf> {
    let mut selected = build.clone();
    if tool.job_name.is_some() {
        selected.job_name.clone_from(&tool.job_name);
    }
    selected.artifact_path(root, extension)
}

/// Match the warning's actual input, including its processing kind. A basename
/// is relative to TeX's artifact directory; a qualified path is relative to
/// the compiler's root directory. Do not accept a matching basename elsewhere.
pub(crate) fn handles_warning(
    root: &Path,
    build: &BuildConfig,
    diagnostic: &lsp_types::Diagnostic,
) -> bool {
    let Some(request) = crate::compilation::build_log::required_index_input(diagnostic) else {
        return false;
    };
    let reported = Path::new(request.file);
    build.index_tools.iter().any(|tool| {
        if tool.validate().is_err() || request.glossary != (tool.input_extension != "idx") {
            return false;
        }
        let Some(expected) = artifact(root, build, tool, &tool.input_extension) else {
            return false;
        };
        if reported.is_relative()
            && reported
                .components()
                .filter(|part| *part != std::path::Component::CurDir)
                .count()
                == 1
        {
            return reported.file_name() == expected.file_name();
        }
        let Some(parent) = root.parent() else {
            return false;
        };
        crate::project::workspace::normalize_path(&parent.join(reported))
            == crate::project::workspace::normalize_path(&expected)
    })
}
fn transcript_path(output: &Path, tool: &IndexToolConfig) -> PathBuf {
    output.with_extension(match tool.input_extension.as_str() {
        "glo" => "glg",
        "acn" => "alg",
        "nlo" => "nlg",
        _ => "ilg",
    })
}
pub(crate) fn outputs(root: &Path, config: &BuildConfig) -> Vec<PathBuf> {
    config
        .index_tools
        .iter()
        .flat_map(|tool| {
            artifact(root, config, tool, &tool.output_extension)
                .into_iter()
                .flat_map(|path| {
                    let log = tool.is_makeindex().then(|| transcript_path(&path, tool));
                    [Some(path), log].into_iter().flatten()
                })
        })
        .collect()
}

pub(crate) fn dependencies(root: &Path, config: &BuildConfig) -> Vec<PathBuf> {
    let Some(parent) = root.parent() else {
        return vec![];
    };
    config
        .index_tools
        .iter()
        .flat_map(|tool| {
            tool.dependencies
                .iter()
                .map(|dependency| parent.join(dependency))
        })
        .collect()
}

impl Plan {
    pub(crate) fn prepare(root: &Path, build: &BuildConfig) -> Result<Self, Failure> {
        let output = build
            .auxiliary_path(root)
            .ok_or_else(|| Failure::error("Invalid build output directory".into()))?;
        let mut names = BTreeSet::new();
        let mut stages = vec![];
        for config in &build.index_tools {
            config.validate().map_err(Failure::error)?;
            let input = artifact(root, build, config, &config.input_extension)
                .ok_or_else(|| Failure::error("Invalid index input name".into()))?;
            let output = artifact(root, build, config, &config.output_extension)
                .ok_or_else(|| Failure::error("Invalid index output name".into()))?;
            if !names.insert(crate::project::workspace::normalize_path(&output)) {
                return Err(Failure::error(format!(
                    "Multiple index tools write {}",
                    output.display()
                )));
            }
            let dependencies = config
                .dependencies
                .iter()
                .map(|dependency| Watched::capture(&root.parent().unwrap().join(dependency)))
                .collect::<Result<_, _>>()?;
            stages.push(Stage {
                config: config.clone(),
                input,
                output,
                dependencies,
            });
        }
        Ok(Self {
            root: root.to_owned(),
            output,
            stages,
        })
    }
    pub(crate) fn run(
        &self,
        build: &BuildConfig,
        cancel: &AtomicBool,
        recovery: &crate::compilation::build::recovery::Recovery,
    ) -> Result<Processed, Failed> {
        let mut report = crate::diagnostics::index_diagnostics::Report::default();
        match self.run_inner(build, cancel, &mut report, recovery) {
            Ok(mut processed) => {
                processed.report = report;
                Ok(processed)
            }
            Err(failure) => Err(Failed { failure, report }),
        }
    }
    fn run_inner(
        &self,
        build: &BuildConfig,
        cancel: &AtomicBool,
        report: &mut crate::diagnostics::index_diagnostics::Report,
        recovery: &crate::compilation::build::recovery::Recovery,
    ) -> Result<Processed, Failure> {
        let mut output = String::new();
        let mut tools = vec![];
        let mut inputs = vec![];
        for stage in &self.stages {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            let input = Watched::capture(&stage.input)?;
            if stage
                .dependencies
                .iter()
                .any(|dependency| !dependency.unchanged())
            {
                return Err(Failure::error(
                    "Index tool dependencies changed during compilation; build again".into(),
                ));
            }
            if std::fs::symlink_metadata(&stage.output)
                .is_ok_and(|metadata| !metadata.file_type().is_file())
            {
                return Err(Failure::error(format!(
                    "Index output must be a regular file: {}",
                    stage.output.display()
                )));
            }
            let temporary = staged_path(&self.output, &stage.config.output_extension)?;
            let transcript = staged_path(&self.output, "ilg")?;
            let mut selected = build.clone();
            if stage.config.job_name.is_some() {
                selected.job_name.clone_from(&stage.config.job_name);
            }
            let job = selected.job_name(&self.root).unwrap().to_string_lossy();
            let root = self.root.to_string_lossy();
            let parent = self.root.parent().unwrap().to_string_lossy();
            let directory = self.output.to_string_lossy();
            let relative_input = Path::new(".").join(stage.input.file_name().unwrap());
            let input_name = relative_input.to_string_lossy();
            let temporary_name = temporary.file_name().unwrap().to_string_lossy();
            let transcript_name = transcript.file_name().unwrap().to_string_lossy();
            let substitutions = [
                ('l', transcript_name.as_ref()),
                ('f', root.as_ref()),
                ('r', parent.as_ref()),
                ('o', directory.as_ref()),
                ('j', job.as_ref()),
                ('i', input_name.as_ref()),
                ('t', temporary_name.as_ref()),
            ];
            let arguments = stage
                .config
                .arguments
                .iter()
                .map(|argument| expand(argument, &substitutions))
                .collect::<Vec<_>>();
            let mut command = Command::new(&stage.config.executable);
            recovery.inherit(&mut command);
            command.args(&arguments).current_dir(&self.output);
            let executed = crate::compilation::build_tools::execute(
                command,
                &stage.config.executable,
                stage.config.timeout_seconds,
                cancel,
            );
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            if !input.unchanged()
                || stage
                    .dependencies
                    .iter()
                    .any(|dependency| !dependency.unchanged())
            {
                return Err(Failure::error(format!(
                    "Index input or dependencies changed while processing {}; build again",
                    stage.input.display()
                )));
            }
            let transcript_output = transcript_path(&stage.output, &stage.config);
            let transcript_text = if stage.config.is_makeindex() {
                std::fs::symlink_metadata(&transcript)
                    .is_ok_and(|metadata| metadata.file_type().is_file())
                    .then(|| std::fs::File::open(&transcript).ok())
                    .flatten()
                    .map(crate::compilation::build::capture)
                    .map(|bytes| crate::compilation::build_log::decode_output(&bytes).into_owned())
            } else {
                None
            };
            if let Some(text) = &transcript_text {
                let parsed = crate::diagnostics::index_diagnostics::parse(
                    &self.root,
                    &stage.input,
                    &transcript_output,
                    text,
                );
                report.truncated |= parsed.truncated;
                for (uri, diagnostics) in parsed.diagnostics {
                    report
                        .diagnostics
                        .entry(uri)
                        .or_default()
                        .extend(diagnostics);
                }
                // Keep this invocation's controlled transcript even on errors.
                // Installing the log must not replace a prior successful index.
                transcript.persist(&transcript_output).map_err(|error| {
                    Failure::error(format!("Cannot install index transcript: {error}"))
                })?;
            }
            let stage_output = match executed {
                Ok(output) => output,
                Err(mut failure) => {
                    failure.output = format!(
                        "{}\n{}",
                        failure.output,
                        transcript_text.as_deref().unwrap_or_default()
                    );
                    return Err(failure);
                }
            };
            if stage.config.is_makeindex() {
                let rejected = transcript_text.as_deref().and_then(rejected_entries);
                let message = if rejected != Some(0) {
                    Some(rejected.map_or_else(
                        || "makeindex did not produce a complete processing transcript".into(),
                        |count| {
                            format!("makeindex rejected {count} index entries; see build output")
                        },
                    ))
                } else if report.truncated {
                    Some("makeindex processing transcript exceeded the diagnostic limit; cannot verify all entries or style errors".into())
                } else {
                    report
                        .first_error()
                        .map(|diagnostic| diagnostic.message.clone())
                };
                if let Some(message) = message {
                    let mut failure = Failure::error(message);
                    failure.output =
                        format!("{}\n{}", stage_output, transcript_text.unwrap_or_default());
                    return Err(failure);
                }
            }
            if !std::fs::symlink_metadata(&temporary)
                .is_ok_and(|metadata| metadata.file_type().is_file())
            {
                let mut failure = Failure::error(format!(
                    "{} exited successfully without producing the requested index output",
                    stage.config.executable
                ));
                failure.output = stage_output;
                return Err(failure);
            }
            temporary
                .persist(&stage.output)
                .map_err(|error| Failure::error(format!("Cannot install index output: {error}")))?;
            output.push_str(&format!(
                "\n{} {}\n{}",
                stage.config.executable,
                stage.input.display(),
                stage_output
            ));
            inputs.push(input);
            inputs.extend(stage.dependencies.iter().cloned());
            tools.push(serde_json::json!({"executable":stage.config.executable,"inputUri":Url::from_file_path(&stage.input).ok(),"outputUri":Url::from_file_path(&stage.output).ok(),"transcriptUri":stage.config.is_makeindex().then(|| Url::from_file_path(&transcript_output).ok()).flatten()}));
        }
        Ok(Processed {
            output,
            tools,
            report: Default::default(),
            inputs,
        })
    }
}
fn staged_path(directory: &Path, extension: &str) -> Result<tempfile::TempPath, Failure> {
    let path = tempfile::Builder::new()
        .prefix("texquill-index-")
        .suffix(&format!(".{extension}"))
        .tempfile_in(directory)
        .map_err(|error| Failure::error(format!("Cannot stage index output: {error}")))?
        .into_temp_path();
    std::fs::remove_file(&path)
        .map_err(|error| Failure::error(format!("Cannot prepare index output: {error}")))?;
    Ok(path)
}
fn rejected_entries(report: &str) -> Option<u64> {
    let counts: Option<Vec<u64>> = report
        .lines()
        .filter(|line| line.starts_with("Scanning input file ") || line.starts_with("done ("))
        .filter_map(|line| line.rsplit_once("done (").map(|(_, summary)| summary))
        // Style and output summaries can occur on separate lines after a
        // diagnostic. They are not input acceptance counts.
        .filter(|summary| {
            !summary.contains(" attributes redefined, ") && !summary.contains(" lines written")
        })
        .map(|summary| {
            summary
                .split_once(" entries accepted, ")?
                .1
                .strip_suffix(" rejected).")?
                .parse()
                .ok()
        })
        .collect();
    let counts = counts?;
    if counts.is_empty() {
        return None;
    }
    counts.into_iter().try_fold(0u64, u64::checked_add)
}

fn cancelled() -> Failure {
    Failure {
        status: "cancelled",
        exit_code: None,
        message: "Index processing cancelled".into(),
        output: String::new(),
    }
}
fn expand(template: &str, substitutions: &[(char, &str)]) -> String {
    let mut expanded = String::new();
    let mut tail = template;
    while let Some(at) = tail.find('%') {
        expanded.push_str(&tail[..at]);
        let marker = tail.as_bytes().get(at + 1).copied().map(char::from);
        if let Some((_, value)) = substitutions.iter().find(|(key, _)| Some(*key) == marker) {
            expanded.push_str(value);
            tail = &tail[at + 2..];
        } else {
            expanded.push('%');
            tail = &tail[at + 1..];
        }
    }
    expanded.push_str(tail);
    expanded
}
