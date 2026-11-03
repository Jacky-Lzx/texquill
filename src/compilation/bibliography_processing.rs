//! Explicit bibliography backends use private outputs and the build lifecycle.
use crate::{
    compilation::{
        build_tools::{self, Failure, Watched},
        recorder::{BibliographyPaths, InputFiles},
    },
    config::{BibliographyBackend, BibliographyToolConfig, BuildConfig},
};
use lsp_types::{Diagnostic, DiagnosticSeverity, Url};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

const STAGED_JOB: &str = "texquill-bibliography";

pub(crate) struct Report {
    pub diagnostics: BTreeMap<Url, Vec<Diagnostic>>,
    pub inputs: InputFiles,
    pub truncated: bool,
}
pub(crate) struct Failed {
    pub failure: Failure,
    pub report: Option<Box<Report>>,
}
impl From<Failure> for Failed {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            report: None,
        }
    }
}
pub(crate) struct Plan {
    root: PathBuf,
    output: PathBuf,
    job: String,
    config: BibliographyToolConfig,
    dependencies: Vec<Watched>,
}
pub(crate) struct Processed {
    pub output: String,
    pub search_output: String,
    pub tool: serde_json::Value,
    pub generated: Vec<Watched>,
    watched: Vec<Watched>,
    auxiliaries: Vec<Auxiliary>,
}
impl Processed {
    pub(crate) fn inputs_unchanged(&self) -> bool {
        self.watched.iter().all(Watched::unchanged)
            && self.auxiliaries.iter().all(Auxiliary::unchanged)
    }
}
pub(crate) fn dependencies(root: &Path, build: &BuildConfig) -> Vec<PathBuf> {
    root.parent()
        .into_iter()
        .flat_map(|parent| {
            build
                .bibliography_tool
                .iter()
                .flat_map(move |tool| tool.dependencies.iter().map(move |name| parent.join(name)))
        })
        .collect()
}

impl Plan {
    pub(crate) fn prepare(root: &Path, build: &BuildConfig) -> Result<Option<Self>, Failure> {
        let Some(config) = &build.bibliography_tool else {
            return Ok(None);
        };
        config.validate().map_err(Failure::error)?;
        let output = build
            .auxiliary_path(root)
            .ok_or_else(|| Failure::error("Invalid bibliography output directory".into()))?;
        let job = build
            .job_name(root)
            .and_then(|name| name.to_str())
            .ok_or_else(|| Failure::error("Invalid bibliography job name".into()))?
            .to_owned();
        let dependencies = dependencies(root, build)
            .iter()
            .map(|path| Watched::capture(path))
            .collect::<Result<_, _>>()?;
        Ok(Some(Self {
            root: root.to_owned(),
            output,
            job,
            config: config.clone(),
            dependencies,
        }))
    }
    pub(crate) fn run(
        &self,
        cancel: &AtomicBool,
        generated: &[Watched],
        recovery: &crate::compilation::build::recovery::Recovery,
    ) -> Result<Processed, Failed> {
        let executable = self
            .config
            .executable
            .as_deref()
            .unwrap_or(self.config.backend.executable());
        if cancel.load(Ordering::Relaxed) {
            return Err(build_tools::cancelled(executable).into());
        }
        if self
            .dependencies
            .iter()
            .any(|dependency| !dependency.unchanged())
        {
            return Err(Failure::error(
                "Bibliography dependencies changed during compilation; build again".into(),
            )
            .into());
        }
        let final_bbl = self.output.join(format!("{}.bbl", self.job));
        if std::fs::symlink_metadata(&final_bbl).is_ok_and(|meta| !meta.file_type().is_file()) {
            return Err(Failure::error("Bibliography output must be a regular file".into()).into());
        }
        let stage = tempfile::Builder::new()
            .prefix("texquill-bibliography-")
            .tempdir_in(&self.output)
            .map_err(|error| Failure::error(format!("Cannot stage bibliography: {error}")))?;
        let extension = match self.config.backend {
            BibliographyBackend::Bibtex => "aux",
            BibliographyBackend::Biber => "bcf",
        };
        let input = self.output.join(format!("{}.{extension}", self.job));
        let mut watched = self.dependencies.clone();
        let mut auxiliaries = vec![];
        if self.config.backend == BibliographyBackend::Bibtex {
            AuxCopies {
                output: &self.output,
                stage: stage.path(),
                names: BTreeMap::new(),
                watched: &mut watched,
                auxiliaries: &mut auxiliaries,
                bytes: 0,
            }
            .copy(&input, &format!("{STAGED_JOB}.aux"), 0)?;
        } else {
            watched.push(Watched::capture(&input)?);
            let bytes = read(&input, 16 * 1024 * 1024)?;
            std::fs::write(stage.path().join(format!("{STAGED_JOB}.bcf")), bytes).map_err(
                |error| Failure::error(format!("Cannot stage bibliography input: {error}")),
            )?;
        }
        let parent = self.root.parent().unwrap();
        let mut command = Command::new(executable);
        recovery.inherit(&mut command);
        command.args(&self.config.arguments);
        let mut search_output = String::new();
        for name in ["BIBINPUTS", "BSTINPUTS"] {
            let value = search_path(parent, &self.output, name);
            command.env(name, &value);
            search_output.push_str(&format!("{name}='{}'.\n", value.to_string_lossy()));
        }
        match self.config.backend {
            BibliographyBackend::Bibtex => {
                command
                    .current_dir(stage.path())
                    .arg(format!("./{STAGED_JOB}.aux"));
            }
            BibliographyBackend::Biber => {
                command
                    .current_dir(parent)
                    .arg("--input-directory")
                    .arg(stage.path())
                    .arg("--output-directory")
                    .arg(stage.path())
                    .arg(STAGED_JOB);
            }
        }
        let result = build_tools::execute(command, executable, self.config.timeout_seconds, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err(build_tools::cancelled(executable).into());
        }
        if watched.iter().any(|file| !file.unchanged()) {
            return Err(Failure::error(
                "Bibliography inputs or dependencies changed while processing; build again".into(),
            )
            .into());
        }
        let staged_log = stage.path().join(format!("{STAGED_JOB}.blg"));
        let final_log = self.output.join(format!("{}.blg", self.job));
        let log = read_generated(&staged_log, 8 * 1024 * 1024);
        let (report, log) = match log {
            Ok(bytes) => {
                let log = crate::compilation::build_log::decode_output(&bytes).into_owned();
                let paths = BibliographyPaths::new(&self.root, &final_log, &log)
                    .with_search_output(&search_output);
                let report = Report {
                    diagnostics: crate::compilation::build_log::parse_bibliography_in(
                        &self.root, &paths, &log,
                    ),
                    inputs: crate::compilation::recorder::classified_bibliography_inputs(
                        &paths, &log,
                    ),
                    truncated: crate::process::output::truncated(&log),
                };
                install(&staged_log, &final_log)?;
                (Some(Box::new(report)), log)
            }
            Err(error) if result.is_ok() => return Err(error.into()),
            Err(_) => (None, String::new()),
        };
        let output = match result {
            Ok(output) => output,
            Err(mut failure) => {
                failure.output = format!("{}\n{}\n{}", search_output, failure.output, log);
                return Err(Failed { failure, report });
            }
        };
        if let Some(error) = report
            .as_ref()
            .into_iter()
            .flat_map(|report| report.diagnostics.values().flatten())
            .find(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        {
            let mut failure = Failure::error(error.message.clone());
            failure.output = format!("{search_output}\n{output}\n{log}");
            return Err(Failed { failure, report });
        }
        let generated: Vec<_> = generated
            .iter()
            .filter(|input| {
                input.snapshot().is_some_and(|(uri, _)| {
                    report
                        .as_ref()
                        .is_some_and(|report| report.inputs.bibliography_databases.contains(&uri))
                })
            })
            .cloned()
            .collect();
        if generated.iter().any(|input| !input.unchanged()) {
            return Err(Failure::error(
                "Generated bibliography input changed while the backend was running; build again"
                    .into(),
            )
            .into());
        }
        let mut convergence_watched = if self.config.backend == BibliographyBackend::Bibtex {
            self.dependencies.clone()
        } else {
            watched
        };
        if let Some(report) = &report {
            // A subsequent TeX pass can rewrite a generated database or style
            // while leaving every citation/BCF directive unchanged.
            let sources: std::collections::BTreeSet<_> = report
                .inputs
                .bindings
                .keys()
                .cloned()
                .chain(
                    report
                        .inputs
                        .all
                        .iter()
                        .filter_map(|uri| uri.to_file_path().ok()),
                )
                .collect();
            for source in sources {
                convergence_watched.push(Watched::capture(&source)?);
            }
        }
        let staged_bbl = stage.path().join(format!("{STAGED_JOB}.bbl"));
        if let Err(mut failure) = install(&staged_bbl, &final_bbl) {
            failure.output = format!("{search_output}\n{output}\n{log}");
            return Err(Failed { failure, report });
        }
        convergence_watched.extend(generated.iter().cloned());
        Ok(Processed {
            output: format!("{search_output}\n{output}"),
            search_output,
            tool: serde_json::json!({"backend":self.config.backend,"executable":executable,"inputUri":Url::from_file_path(&input).ok(),"outputUri":Url::from_file_path(&final_bbl).ok(),"transcriptUri":Url::from_file_path(&final_log).ok()}),
            // BibTeX ignores labels, bibcite records and other auxiliary data.
            // Preserve full snapshots while the tool runs, but use only its
            // actual directives to decide whether another backend pass is needed.
            watched: convergence_watched,
            auxiliaries,
            generated,
        })
    }
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>, Failure> {
    if !std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_file() && meta.len() <= limit)
    {
        return Err(Failure::error(format!("Bibliography input or output is missing, not a regular file, or exceeds the size limit: {}", path.display())));
    }
    let mut bytes = vec![];
    std::fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|error| Failure::error(format!("Cannot read {}: {error}", path.display())))?;
    if bytes.len() as u64 > limit {
        return Err(Failure::error(
            "Bibliography file exceeded its size limit".into(),
        ));
    }
    Ok(bytes)
}
fn read_generated(path: &Path, limit: u64) -> Result<Vec<u8>, Failure> {
    if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Err(Failure::error(format!(
            "Bibliography tool did not produce a new regular file: {}",
            path.display()
        )));
    }
    read(path, limit)
}

fn install(source: &Path, target: &Path) -> Result<(), Failure> {
    let bytes = read_generated(source, 16 * 1024 * 1024)?;
    let mut temporary = tempfile::NamedTempFile::new_in(target.parent().unwrap())
        .map_err(|error| Failure::error(format!("Cannot stage bibliography output: {error}")))?;
    use std::io::Write;
    temporary
        .write_all(&bytes)
        .map_err(|error| Failure::error(format!("Cannot copy bibliography output: {error}")))?;
    temporary
        .persist(target)
        .map_err(|error| Failure::error(format!("Cannot install bibliography output: {error}")))?;
    Ok(())
}
fn search_path(parent: &Path, output: &Path, name: &str) -> std::ffi::OsString {
    let inherited = std::env::var_os(name).unwrap_or_default();
    let inherited = inherited.to_string_lossy();
    let separator = if cfg!(windows) { ';' } else { ':' };
    let mut paths = vec![parent.to_string_lossy().into_owned()];
    // TeX can generate bibliography databases or styles in its output folder
    // (for example filecontents and biblatex's BibTeX control database).
    // The private backend cwd must not hide those actual build inputs.
    if parent != output {
        paths.push(output.to_string_lossy().into_owned());
    }
    for entry in inherited.split(separator) {
        let (marker, entry) = entry
            .strip_prefix("!!")
            .map_or(("", entry), |tail| ("!!", tail));
        let path = Path::new(entry);
        if entry.is_empty() || path.is_absolute() || entry.starts_with(['$', '~']) {
            paths.push(format!("{marker}{entry}"));
        } else {
            paths.push(format!("{marker}{}", parent.join(path).display()));
        }
    }
    paths.join(&separator.to_string()).into()
}
struct AuxCopies<'a> {
    output: &'a Path,
    stage: &'a Path,
    names: BTreeMap<PathBuf, String>,
    watched: &'a mut Vec<Watched>,
    auxiliaries: &'a mut Vec<Auxiliary>,
    bytes: usize,
}
impl AuxCopies<'_> {
    fn copy(&mut self, source: &Path, name: &str, depth: usize) -> Result<(), Failure> {
        let key = crate::project::workspace::normalize_path(source);
        if self.names.contains_key(&key) {
            return Ok(());
        }
        if depth >= 64 || self.names.len() >= 1024 {
            return Err(Failure::error(
                "Bibliography auxiliary input limit exceeded".into(),
            ));
        }
        self.names.insert(key, name.into());
        self.watched.push(Watched::capture(source)?);
        let bytes = read(source, 8 * 1024 * 1024)?;
        self.bytes += bytes.len();
        if self.bytes > 16 * 1024 * 1024 {
            return Err(Failure::error(
                "Bibliography auxiliary inputs exceeded 16 MiB".into(),
            ));
        }
        self.auxiliaries.push(Auxiliary {
            path: source.to_owned(),
            target: source
                .canonicalize()
                .map_err(|error| Failure::error(format!("Cannot bind auxiliary input: {error}")))?,
            signature: auxiliary_signature(&bytes),
        });
        let mut rewritten = Vec::new();
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if let Some(tail) = line.strip_prefix(b"\\@input{") {
                let end = tail.iter().position(|byte| *byte == b'}').ok_or_else(|| {
                    Failure::error("Incomplete bibliography auxiliary input".into())
                })?;
                let input = std::str::from_utf8(&tail[..end])
                    .map_err(|_| {
                        Failure::error("Bibliography auxiliary filename is not UTF-8".into())
                    })?
                    .trim_matches('"');
                let input = self.output.join(input);
                let key = crate::project::workspace::normalize_path(&input);
                let child = self
                    .names
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| format!("texquill-aux/{}.aux", self.names.len()));
                self.copy(&input, &child, depth + 1)?;
                rewritten.extend_from_slice(format!("\\@input{{{child}}}").as_bytes());
                rewritten.extend_from_slice(&tail[end + 1..]);
            } else {
                rewritten.extend_from_slice(line);
            }
        }
        let destination = self.stage.join(name);
        std::fs::create_dir_all(destination.parent().unwrap())
            .and_then(|()| std::fs::write(&destination, rewritten))
            .map_err(|error| Failure::error(format!("Cannot stage auxiliary input: {error}")))?;
        Ok(())
    }
}

struct Auxiliary {
    path: PathBuf,
    target: PathBuf,
    signature: Vec<u8>,
}
impl Auxiliary {
    fn unchanged(&self) -> bool {
        self.path.canonicalize().ok().as_ref() == Some(&self.target)
            && read(&self.path, 8 * 1024 * 1024)
                .is_ok_and(|bytes| auxiliary_signature(&bytes) == self.signature)
    }
}
fn auxiliary_signature(bytes: &[u8]) -> Vec<u8> {
    let mut signature = vec![];
    for line in bytes.split(|byte| *byte == b'\n') {
        // BibTeX's aux parser is line based and case sensitive. Whitespace
        // before a command makes that line ordinary ignored auxiliary data.
        let command = line
            .iter()
            .position(|byte| *byte == b'{')
            .map(|end| &line[..end]);
        if matches!(
            command,
            Some(b"\\citation" | b"\\bibdata" | b"\\bibstyle" | b"\\@input")
        ) {
            signature.extend_from_slice(line);
            signature.push(b'\n');
        }
    }
    signature
}
