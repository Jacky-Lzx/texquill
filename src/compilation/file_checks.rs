//! Captured file conditions and content fingerprints for build validity.
use crate::{
    config::BuildConfig,
    parser::syntax,
    project::{
        graphics, graphics_settings,
        workspace::{normalize_path, resolve_file_path_with, Workspace},
    },
};
use lsp_types::Url;
use std::{
    cell::RefCell,
    collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_PATHS: usize = 4096;
const MAX_ROOTS: usize = 128;

/// Whether a tool supplied new path observations or reused an older artifact.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum BindingStatus {
    Updated,
    Reused,
    Unchanged,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    paths: BTreeMap<PathBuf, Option<PathBuf>>,
    configuration: BTreeMap<PathBuf, (Option<PathBuf>, InputSnapshot)>,
    recorded: BTreeMap<PathBuf, Option<PathBuf>>,
    ambiguous: BTreeSet<PathBuf>,
    watched: BTreeSet<Url>,
    images: BTreeMap<PathBuf, (Option<PathBuf>, InputSnapshot)>,
    artifact: Option<PathBuf>,
    inputs: BTreeMap<Url, InputSnapshot>,
    pdf_contents: Option<InputSnapshot>,
    synctex_contents: BTreeMap<PathBuf, InputSnapshot>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InputSnapshot {
    Text { length: usize, hash: u64 },
    File(Option<(u64, u64)>),
}

impl InputSnapshot {
    pub(crate) fn text(text: &str) -> Self {
        let mut hash = DefaultHasher::new();
        text.hash(&mut hash);
        Self::Text {
            length: text.len(),
            hash: hash.finish(),
        }
    }

    pub(crate) fn file(path: &Path) -> Self {
        let read = || {
            if !std::fs::metadata(path).ok()?.is_file() {
                return None;
            }
            let mut file = std::fs::File::open(path).ok()?;
            let mut hash = DefaultHasher::new();
            let mut length = 0;
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).ok()?;
                if count == 0 {
                    break;
                }
                length += count as u64;
                hash.write(&buffer[..count]);
            }
            Some((length, hash.finish()))
        };
        Self::File(read())
    }

    pub(crate) fn changed(&self, uri: &Url) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return true;
        };
        self.changed_file(&path)
    }

    fn changed_file(&self, path: &Path) -> bool {
        match self {
            Self::Text { length, .. } => {
                std::fs::metadata(path).map_or(true, |metadata| metadata.len() != *length as u64)
                    || std::fs::read_to_string(path)
                        .ok()
                        .is_none_or(|text| Self::text(&text) != *self)
            }
            Self::File(before) => {
                let length = std::fs::metadata(path)
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .map(|metadata| metadata.len());
                length != before.as_ref().map(|(length, _)| *length) || Self::file(path) != *self
            }
        }
    }
}

pub(crate) fn fingerprint(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata
        .is_file()
        .then_some((metadata.len(), metadata.modified().ok()))
}

/// Metadata detects rewrites of identical outputs; contents detect replacements
/// by tools which preserve the output's length and modification time.
#[derive(Debug, PartialEq)]
pub(crate) struct ArtifactFingerprint {
    metadata: (u64, Option<std::time::SystemTime>),
    contents: InputSnapshot,
}

impl ArtifactFingerprint {
    pub(crate) fn capture(path: &Path) -> Option<Self> {
        Some(Self {
            metadata: fingerprint(path)?,
            contents: InputSnapshot::file(path),
        })
    }
}

fn file(path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    path.is_file().then_some(path)
}

// A missing file cannot be canonicalized, but an existing parent may be a
// symlink. Editors can report its eventual creation through the physical path.
fn canonical_candidate(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|parent| {
        parent
            .canonicalize()
            .ok()
            .map(|canonical| canonical.join(path.strip_prefix(parent).unwrap()))
    })
}

impl Snapshot {
    pub(crate) fn capture(workspace: &Workspace, root: &Url, config: &BuildConfig) -> Self {
        let Ok(root_path) = root.to_file_path() else {
            return Self::default();
        };
        let config = config.resolved(&root_path);
        let sources = workspace.project_documents(root);
        let inputs = crate::project::source_inputs::Index::new(workspace, root);
        let active_sources =
            crate::project::source_inputs::active_direct_file_positions(workspace, &sources);
        let images = if inputs.incomplete {
            BTreeMap::new()
        } else {
            graphics::calls_in(workspace, &sources)
        };
        let redefined_file_commands: BTreeSet<_> = sources
            .iter()
            .filter_map(|uri| workspace.documents.get(uri))
            .flat_map(|doc| &doc.syntax.definitions)
            .filter(|entry| {
                syntax::loader_extension(&entry.name).is_some()
                    || crate::bibliography::bib_resources::loads(&entry.name)
                    || matches!(
                        entry.name.as_str(),
                        "bibliography"
                            | "nobibliography"
                            | "bibliographystyle"
                            | "includegraphics"
                            | "graphicspath"
                    )
            })
            .map(|entry| entry.name.as_str())
            .collect();
        let track_images = !redefined_file_commands.contains("includegraphics")
            && !redefined_file_commands.contains("graphicspath")
            && !sources
                .iter()
                .filter_map(|uri| workspace.documents.get(uri))
                .flat_map(|doc| &doc.syntax.commands)
                .any(|command| command.name == "DeclareGraphicsRule");
        let image_extensions = graphics_settings::Index::new(workspace, root);
        let image_directories =
            track_images.then(|| graphics_settings::Index::directories(workspace, root));
        // A missing chapter is meaningful only when it can be selected.
        // Unknown or conflicting filters must not guess that TeX reads it.
        let include_filters = crate::project::source_inputs::selection_filters(workspace, &sources);
        let mut outputs: BTreeSet<_> = [
            "pdf",
            "log",
            "fls",
            "aux",
            "bbl",
            "blg",
            "toc",
            "lof",
            "lot",
            "out",
            "nav",
            "snm",
            "vrb",
            "dvi",
            "xdv",
            "synctex",
            "synctex.gz",
            "fdb_latexmk",
            "run.xml",
            "bcf",
        ]
        .into_iter()
        .filter_map(|extension| config.artifact_path(&root_path, extension))
        .map(|path| normalize_path(&path))
        .collect();
        // \include creates auxiliary files under the configured output folder.
        if let Some(output) = config.auxiliary_path(&root_path) {
            for source in &sources {
                if let Some(doc) = workspace.documents.get(source) {
                    for command in doc
                        .syntax
                        .commands
                        .iter()
                        .chain(
                            inputs
                                .calls
                                .get(source)
                                .into_iter()
                                .flatten()
                                .filter(|call| call.certain)
                                .map(|call| &call.command),
                        )
                        .filter(|command| command.name == "include")
                    {
                        let potential =
                            doc.syntax.potential_file_command(&doc.text.source, command);
                        let command = potential.as_ref();
                        if let Some(argument) = syntax::file_argument(&doc.text.source, command) {
                            let name = argument.value.trim();
                            if syntax::is_literal_file_path(name) {
                                let base = name.strip_suffix(".tex").unwrap_or(name);
                                outputs.insert(normalize_path(&output.join(format!("{base}.aux"))));
                            }
                        }
                    }
                }
            }
        }
        let paths = RefCell::new(BTreeMap::new());
        let image_snapshots = RefCell::new(BTreeMap::new());
        let observe_path = |path: &Path| {
            let before = file(path);
            let mut paths = paths.borrow_mut();
            if paths.len() < MAX_PATHS && !outputs.contains(&normalize_path(path)) {
                paths.insert(path.to_path_buf(), before.clone());
            }
            before
        };
        let observe = |origin: &Url, command: &str, name: &str, extension: &str| {
            resolve_file_path_with(origin, command, name, &[extension], observe_path)
        };
        'sources: for source in sources {
            let Some(doc) = workspace.documents.get(&source) else {
                continue;
            };
            let quoted = inputs.quoted_file_positions(workspace, &source);
            for command in doc
                .syntax
                .commands
                .iter()
                .filter(|command| command.name != "includegraphics")
                .filter(|command| {
                    !(syntax::loader_extension(&command.name).is_some()
                        || crate::bibliography::bib_resources::loads(&command.name)
                        || matches!(
                            command.name.as_str(),
                            "bibliography" | "nobibliography" | "bibliographystyle"
                        ))
                        || !quoted.contains(&command.span.start)
                })
                .filter(|command| {
                    !(matches!(
                        command.name.as_str(),
                        "input"
                            | "include"
                            | "subfile"
                            | "InputIfFileExists"
                            | "IfFileExists"
                            | "includegraphics"
                    ) || syntax::code_input_command(&command.name)
                        || crate::bibliography::bib_resources::loads(&command.name))
                        || active_sources
                            .get(&source)
                            .is_some_and(|positions| positions.contains(&command.span.start))
                })
                .chain(
                    inputs
                        .calls
                        .get(&source)
                        .into_iter()
                        .flatten()
                        .chain(inputs.loaders.get(&source).into_iter().flatten())
                        .chain(inputs.resources.get(&source).into_iter().flatten())
                        .chain(inputs.checks.get(&source).into_iter().flatten())
                        .chain(images.get(&source).into_iter().flatten())
                        .filter(|call| call.certain)
                        .map(|call| &call.command),
                )
            {
                let potential = doc.syntax.potential_file_command(&doc.text.source, command);
                let command = potential.as_ref();
                if paths.borrow().len() >= MAX_PATHS {
                    break 'sources;
                }
                if syntax::code_input_command(&command.name) {
                    if crate::project::code_inputs::supported(workspace, &source, root, command) {
                        if let Some(argument) = syntax::file_argument(&doc.text.source, command) {
                            observe(root, &command.name, &argument.value, "");
                        }
                    }
                    continue;
                }
                if crate::bibliography::bib_resources::loads(&command.name) {
                    if !redefined_file_commands.contains(command.name.as_str()) {
                        if let Some(argument) = syntax::file_argument(&doc.text.source, command) {
                            for origin in [root, &source] {
                                if observe(origin, &command.name, &argument.value, "").is_some()
                                    || source == *root
                                {
                                    break;
                                }
                            }
                        }
                    }
                    continue;
                }
                if command.name == "includegraphics" {
                    if let (Some(directories), Some(argument), Some(extensions)) = (
                        image_directories.as_ref().and_then(|index| {
                            index.paths_at(root, &source, command.span.start, false)
                        }),
                        &command.argument,
                        image_extensions.at(&source, command.span.start),
                    ) {
                        let name = argument.value.trim();
                        if syntax::is_literal_file_path(name) {
                            graphics::resolve_with(root, name, &extensions, &directories, |path| {
                                let before = observe_path(path);
                                if paths.borrow().contains_key(path) {
                                    image_snapshots.borrow_mut().insert(
                                        path.to_path_buf(),
                                        (before.clone(), InputSnapshot::file(path)),
                                    );
                                }
                                before
                            });
                        }
                    }
                    continue;
                }
                let loader_extension = syntax::loader_extension(&command.name);
                if loader_extension.is_some()
                    || matches!(
                        command.name.as_str(),
                        "bibliography" | "nobibliography" | "bibliographystyle"
                    )
                {
                    if let Some(extension) = loader_extension {
                        // The public loaders forward to others of the same
                        // kind. A project override can change their lookup.
                        if redefined_file_commands
                            .iter()
                            .any(|name| syntax::loader_extension(name) == Some(extension))
                        {
                            continue;
                        }
                    } else if command.name == "bibliographystyle" {
                        if redefined_file_commands.contains("bibliographystyle") {
                            continue;
                        }
                    } else if redefined_file_commands.contains("bibliography")
                        || redefined_file_commands.contains("nobibliography")
                    {
                        continue;
                    }
                    let (names, extension) =
                        if loader_extension == Some("cls") || command.name == "bibliographystyle" {
                            let Some(argument) = syntax::file_argument(&doc.text.source, command)
                            else {
                                continue;
                            };
                            (
                                vec![argument.value.clone()],
                                loader_extension.unwrap_or("bst"),
                            )
                        } else {
                            let Some(spans) =
                                syntax::complete_file_list_spans(&doc.text.source, command)
                            else {
                                continue;
                            };
                            (
                                spans
                                    .into_iter()
                                    .map(|span| doc.text.source[span].to_owned())
                                    .collect(),
                                loader_extension.unwrap_or("bib"),
                            )
                        };
                    for name in names {
                        if paths.borrow().len() >= MAX_PATHS {
                            break 'sources;
                        }
                        // Shared lookup keeps real suffix rules: package and
                        // class loaders always append theirs; bibliography
                        // commands accept an explicit single .bib or .bst.
                        for origin in [root, &source] {
                            if observe(origin, &command.name, &name, extension).is_some()
                                || source == *root
                            {
                                break;
                            }
                        }
                    }
                    continue;
                }
                if !matches!(
                    command.name.as_str(),
                    "IfFileExists" | "InputIfFileExists" | "include" | "input" | "subfile"
                ) {
                    continue;
                }
                let Some(argument) = syntax::file_argument(&doc.text.source, command) else {
                    continue;
                };
                let name = argument.value.trim();
                if !syntax::is_literal_file_path(name) {
                    continue;
                }
                if command.name == "include" {
                    let base = name.strip_suffix(".tex").unwrap_or(name);
                    let selected = include_filters
                        .as_ref()
                        .is_some_and(|filters| filters.iter().all(|names| names.contains(base)));
                    if !selected
                        || [root, &source].into_iter().any(|origin| {
                            resolve_file_path_with(origin, "include", name, &["tex"], file)
                                .is_some()
                        })
                    {
                        continue;
                    }
                }
                for origin in [root, &source] {
                    let selected = observe(origin, &command.name, name, "tex");
                    if selected.is_some() || source == *root {
                        break;
                    }
                }
            }
        }
        let paths = paths.into_inner();
        let mut configuration = BTreeMap::new();
        for path in config.latexmkrc_paths(&root_path) {
            let binding = file(&path);
            let contents = InputSnapshot::file(&path);
            configuration.insert(path, (binding, contents));
            // Candidates already select the first existing file in each layer;
            // native resolution adds included configuration dependencies.
        }
        for path in crate::compilation::index_processing::dependencies(&root_path, &config)
            .into_iter()
            .chain(crate::compilation::bibliography_processing::dependencies(
                &root_path, &config,
            ))
        {
            let binding = file(&path);
            let contents = InputSnapshot::file(&path);
            configuration.insert(path, (binding, contents));
        }
        let recorded = [
            &workspace.project.recorded_bindings,
            &workspace.project.bibliography_bindings,
        ]
        .into_iter()
        .filter_map(|by_root| by_root.get(root))
        .flat_map(|bindings| bindings.keys())
        .map(|path| (path.clone(), file(path)))
        .collect();
        let mut snapshot = Self {
            paths,
            images: image_snapshots.into_inner(),
            configuration,
            recorded,
            artifact: config
                .artifact_path(&root_path, "pdf")
                .map(|path| normalize_path(&path)),
            ..Self::default()
        };
        snapshot.refresh_watched();
        snapshot
    }

    fn refresh_watched(&mut self) {
        self.watched = self
            .paths
            .iter()
            .chain(&self.recorded)
            .chain(
                self.configuration
                    .iter()
                    .map(|(path, (before, _))| (path, before)),
            )
            .flat_map(|(path, before)| {
                let mut watched = vec![path.clone()];
                watched.extend(before.clone().or_else(|| canonical_candidate(path)));
                // File URIs collapse `..`, while the kernel traverses it after
                // following symlinks. Retain intermediate paths for events on
                // a directory alias which disappears from the normalized URI.
                if path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
                {
                    watched.extend(path.ancestors().skip(1).map(Path::to_path_buf));
                }
                watched
            })
            .filter_map(|path| Url::from_file_path(normalize_path(&path)).ok())
            .collect();
    }

    /// Merge independent tool scopes. Reused tool outputs retain identities
    /// from their last run. Without a new TeX log or PDF, retain the filesystem
    /// conditions captured before this build; the old PDF has its own history.
    pub(crate) fn with_bindings<'a>(
        &self,
        scopes: impl IntoIterator<Item = (&'a BTreeMap<PathBuf, PathBuf>, BindingStatus)>,
    ) -> Self {
        let mut snapshot = self.clone();
        snapshot.recorded.clear();
        snapshot.ambiguous.clear();
        for (bindings, status) in scopes {
            for (path, target) in bindings {
                let before = self
                    .recorded
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| Some(target.clone()));
                // A retarget during compilation makes the PDF's actual input
                // uncertain, even if the alias is restored before completion.
                if status != BindingStatus::Unchanged && before.as_ref() != Some(target) {
                    snapshot.ambiguous.insert(path.clone());
                }
                // An unchanged tool log describes the last run of that tool,
                // not the current filesystem. A new TeX PDF can still reuse an
                // old .bbl; it must not adopt a retargeted bibliography alias.
                snapshot.recorded.insert(
                    path.clone(),
                    if status == BindingStatus::Reused {
                        Some(target.clone())
                    } else {
                        before
                    },
                );
            }
        }
        snapshot.refresh_watched();
        snapshot
    }

    /// A new TeX PDF can reuse a .bbl generated from older bibliography data.
    /// Once those inputs differ, keep that PDF uncertain even after restoration.
    pub(crate) fn with_reused_inputs(&self, previous: &Self) -> Self {
        let mut snapshot = self.clone();
        snapshot.ambiguous.extend(
            previous
                .inputs
                .iter()
                .filter(|(uri, before)| before.changed(uri))
                .filter_map(|(uri, _)| uri.to_file_path().ok()),
        );
        snapshot
    }

    /// Bibliography tool history follows moved inputs and output directories;
    /// historical PDF checks deliberately retain their original paths.
    pub(crate) fn renamed_inputs(&self, rename: impl Fn(&Path) -> PathBuf) -> Self {
        Self {
            artifact: self.artifact.as_deref().map(&rename),
            inputs: self
                .inputs
                .iter()
                .filter_map(|(uri, before)| {
                    Some((
                        Url::from_file_path(rename(&uri.to_file_path().ok()?)).ok()?,
                        before.clone(),
                    ))
                })
                .collect(),
            ..Self::default()
        }
    }

    pub(crate) fn with_inputs(&self, inputs: BTreeMap<Url, InputSnapshot>) -> Self {
        Self {
            inputs,
            ..self.clone()
        }
    }

    /// A verified pipeline may create a previously missing database itself.
    /// Do not relax existing identities or any other conditional candidate.
    pub(crate) fn with_generated_bibliography(
        &self,
        generated: &[crate::compilation::build_tools::Watched],
    ) -> Self {
        let mut updated = self.clone();
        for (path, before) in &mut updated.paths {
            if before.is_none()
                && generated
                    .iter()
                    .any(|input| input.matches_path(path) && input.unchanged())
            {
                *before = file(path);
            }
        }
        updated
    }

    /// Compare the inputs of this published build, independently of which
    /// revision a different root most recently loaded into the workspace.
    pub(crate) fn changed_matching_input(&self, affects: impl Fn(&Url) -> bool) -> bool {
        self.inputs
            .iter()
            .any(|(uri, before)| affects(uri) && before.changed(uri))
    }

    pub(crate) fn input_changed(&self, uri: &Url) -> bool {
        // Unknown inputs invalidate conservatively, including newly resolved
        // dependencies and histories evicted by the root limit.
        self.inputs
            .get(uri)
            .is_none_or(|before| before.changed(uri))
    }

    pub(crate) fn changed_input(&self) -> Option<&Url> {
        self.inputs
            .iter()
            .find_map(|(uri, before)| before.changed(uri).then_some(uri))
    }

    pub(crate) fn with_artifact_contents(mut self) -> Self {
        self.pdf_contents = self.artifact.as_deref().map(InputSnapshot::file);
        if let Some(pdf) = &self.artifact {
            // Record both names, including missing alternatives: introducing a
            // second format can change which mapping the external tool reads.
            self.synctex_contents = ["synctex.gz", "synctex"]
                .into_iter()
                .map(|extension| {
                    let path = pdf.with_extension(extension);
                    let contents = InputSnapshot::file(&path);
                    (path, contents)
                })
                .collect();
        }
        self
    }

    pub(crate) fn pdf_changed(&self) -> bool {
        self.artifact
            .as_deref()
            .zip(self.pdf_contents.as_ref())
            .is_some_and(|(path, before)| before.changed_file(path))
    }

    pub(crate) fn synctex_changed(&self) -> bool {
        self.synctex_contents
            .iter()
            .any(|(path, before)| before.changed_file(path))
    }

    pub(crate) fn watched(&self) -> &BTreeSet<Url> {
        &self.watched
    }

    /// Check only image dependencies, including unselected lookup candidates
    /// and the opened names of recorder paths whose targets have other suffixes.
    pub(crate) fn image_changed(&self, affects: impl Fn(&Url) -> bool) -> bool {
        let affected = |path: &Path, before: &Option<PathBuf>| {
            [
                Some(path.to_path_buf()),
                before.clone(),
                canonical_candidate(path),
            ]
            .into_iter()
            .flatten()
            .filter_map(|path| Url::from_file_path(normalize_path(&path)).ok())
            .any(|uri| affects(&uri))
        };
        self.images.iter().any(|(path, (binding, contents))| {
            affected(path, binding) && (file(path) != *binding || contents.changed_file(path))
        }) || self.recorded.iter().any(|(path, binding)| {
            graphics::is_image(path)
                && affected(path, binding)
                && (file(path) != *binding
                    || binding.as_ref().is_some_and(|target| {
                        Url::from_file_path(target).is_ok_and(|uri| self.input_changed(&uri))
                    }))
        }) || self.inputs.iter().any(|(uri, contents)| {
            affects(uri)
                && uri
                    .to_file_path()
                    .is_ok_and(|path| graphics::is_image(&path))
                && contents.changed(uri)
        })
    }

    pub(crate) fn changed_path(&self) -> Option<&Path> {
        self.paths
            .iter()
            .chain(&self.recorded)
            .find_map(|(path, before)| (file(path) != *before).then_some(path.as_path()))
            .or_else(|| {
                self.configuration
                    .iter()
                    .find_map(|(path, (binding, before))| {
                        (file(path) != *binding || before.changed_file(path))
                            .then_some(path.as_path())
                    })
            })
            .or_else(|| self.ambiguous.first().map(PathBuf::as_path))
    }

    pub(crate) fn matches_pdf(&self, pdf: &Path) -> bool {
        self.artifact.as_ref() == Some(&normalize_path(pdf))
    }

    pub(crate) fn remember(map: &mut BTreeMap<Url, Arc<Self>>, root: Url, snapshot: Arc<Self>) {
        if snapshot.paths.is_empty()
            && snapshot.configuration.is_empty()
            && snapshot.recorded.is_empty()
            && snapshot.inputs.is_empty()
        {
            map.remove(&root);
            return;
        }
        map.insert(root.clone(), snapshot);
        while map.len() > MAX_ROOTS {
            let victim = map.keys().find(|key| **key != root).unwrap().clone();
            map.remove(&victim);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn latexmk_configuration_contents_and_candidate_creation_invalidate_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let main = root.join("main.tex");
        let uri = Url::from_file_path(&main).unwrap();
        std::fs::write(&main, "Source").unwrap();
        let mut workspace = Workspace::default();
        workspace.open(uri.clone(), "Source".into(), 1);
        let capture = || Snapshot::capture(&workspace, &uri, &workspace.build_config(&uri));
        let before = capture();
        assert!(before.changed_path().is_none());
        let rc = root.join("latexmkrc");
        std::fs::write(&rc, "$out_dir = 'build';").unwrap();
        assert_eq!(before.changed_path(), Some(rc.as_path()));
        let before = capture();
        assert!(before.changed_path().is_none());
        let modified = std::fs::metadata(&rc).unwrap().modified().unwrap();
        std::fs::write(&rc, "$out_dir = 'other';").unwrap();
        std::fs::File::open(&rc)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(before.changed_path(), Some(rc.as_path()));
        let before = capture();
        let hidden = root.join(".latexmkrc");
        std::fs::write(&hidden, "$out_dir = 'build';").unwrap();
        assert_eq!(before.changed_path(), Some(hidden.as_path()));
        let before = capture();
        assert!(before
            .watched()
            .contains(&Url::from_file_path(&hidden).unwrap()));
        // The visible rc is no longer read while .latexmkrc exists.
        std::fs::write(&rc, "$out_dir = 'unused';").unwrap();
        assert!(before.changed_path().is_none());
        std::fs::remove_file(&hidden).unwrap();
        assert_eq!(before.changed_path(), Some(hidden.as_path()));
    }

    #[test]
    fn custom_latexmk_arguments_watch_only_effective_rcs_and_literal_includes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let main = root.join("main.tex");
        let uri = Url::from_file_path(&main).unwrap();
        std::fs::write(&main, "Source").unwrap();
        let rc = root.join(".latexmkrc");
        let selected = root.join("selected rc");
        let included = root.join("included.rc");
        std::fs::write(&rc, "$pdf_mode = 5;").unwrap();
        std::fs::write(&selected, "do './included.rc';").unwrap();
        std::fs::write(&included, "$pdf_mode = 5;").unwrap();
        let mut workspace = Workspace::default();
        workspace.open(uri.clone(), "Source".into(), 1);
        workspace.config.build.arguments = Some(vec![
            "--norc".into(),
            "--r".into(),
            selected.to_string_lossy().into_owned(),
            "%f".into(),
        ]);
        let config = workspace.build_config(&uri);
        let before = Snapshot::capture(&workspace, &uri, &config);
        std::fs::write(&rc, "$pdf_mode = 4;").unwrap();
        assert!(before.changed_path().is_none());
        let modified = std::fs::metadata(&included).unwrap().modified().unwrap();
        std::fs::write(&included, "$pdf_mode = 4;").unwrap();
        std::fs::File::open(&included)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(before.changed_path(), Some(included.as_path()));
        let before = Snapshot::capture(&workspace, &uri, &config);
        std::fs::write(&selected, "$pdf_mode = 1;").unwrap();
        assert_eq!(before.changed_path(), Some(selected.as_path()));
        workspace.config.build.arguments = Some(vec!["-pdf".into(), "%f".into()]);
        let before = Snapshot::capture(&workspace, &uri, &workspace.build_config(&uri));
        std::fs::write(&rc, "$pdf_mode = 1;").unwrap();
        assert_eq!(before.changed_path(), Some(rc.as_path()));
    }

    #[test]
    fn forced_custom_arguments_watch_their_own_rc_and_expand_path_templates_once() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let main = root.join("main.tex");
        let uri = Url::from_file_path(&main).unwrap();
        std::fs::write(&main, "Source").unwrap();
        std::fs::create_dir(root.join("out%j")).unwrap();
        let normal = root.join("normal.rc");
        let forced = root.join("out%j/forced.rc");
        std::fs::write(&normal, "$pdf_mode = 1;").unwrap();
        std::fs::write(&forced, "$pdf_mode = 5;").unwrap();
        let mut workspace = Workspace::default();
        workspace.open(uri.clone(), "Source".into(), 1);
        workspace.config.build.output_directory = Some("out%j".into());
        workspace.config.build.arguments = Some(vec![
            "-norc".into(),
            "-r".into(),
            "normal.rc".into(),
            "%f".into(),
        ]);
        workspace.config.build.rebuild_arguments = Some(vec![
            "-norc".into(),
            "-r".into(),
            "%o/forced.rc".into(),
            "%f".into(),
        ]);
        let config = workspace.build_config(&uri).for_build_mode(&main, true);
        let before = Snapshot::capture(&workspace, &uri, &config);
        std::fs::write(&normal, "$pdf_mode = 4;").unwrap();
        assert!(before.changed_path().is_none());
        std::fs::write(&forced, "$pdf_mode = 4;").unwrap();
        assert_eq!(before.changed_path(), Some(forced.as_path()));
    }

    #[test]
    fn moved_bibliography_history_keeps_content_fingerprints_and_tracks_the_new_output_path() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old");
        let new = directory.path().join("new");
        std::fs::create_dir(&old).unwrap();
        let bibliography = old.join("refs.bib");
        std::fs::write(&bibliography, "OLD\n").unwrap();
        let history = Snapshot {
            artifact: Some(old.join("main.pdf")),
            inputs: [(
                Url::from_file_path(&bibliography).unwrap(),
                InputSnapshot::file(&bibliography),
            )]
            .into(),
            ..Default::default()
        };
        std::fs::rename(&old, &new).unwrap();
        let moved = history.renamed_inputs(|path| new.join(path.strip_prefix(&old).unwrap()));
        assert!(moved.matches_pdf(&new.join("main.pdf")));
        assert!(moved.changed_input().is_none());
        std::fs::write(new.join("refs.bib"), "NEW\n").unwrap();
        assert_eq!(
            moved.changed_input(),
            Some(&Url::from_file_path(new.join("refs.bib")).unwrap())
        );
        let uncertain = Snapshot::default().with_reused_inputs(&moved);
        std::fs::write(new.join("refs.bib"), "OLD\n").unwrap();
        assert!(moved.changed_input().is_none());
        assert_eq!(
            uncertain.changed_path(),
            Some(new.join("refs.bib").as_path())
        );
    }

    #[test]
    fn tex_and_bibliography_scopes_refresh_independently_and_keep_unread_bibliography_history() {
        let directory = tempfile::tempdir().unwrap();
        let root = Url::from_file_path(directory.path().join("main.tex")).unwrap();
        let chapter = directory.path().join("chapter.tex");
        let first = directory.path().join("first.bib");
        let second = directory.path().join("second.bib");
        let alias = directory.path().join("refs.bib");
        for path in [&chapter, &first, &second] {
            std::fs::write(path, "contents\n").unwrap();
        }
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let mut workspace = Workspace::default();
        let tex: BTreeMap<_, _> = [(chapter.clone(), chapter.clone())].into();
        let bib: BTreeMap<_, _> = [(alias.clone(), first.clone())].into();
        workspace
            .project
            .recorded_bindings
            .insert(root.clone(), tex);
        workspace
            .project
            .bibliography_bindings
            .insert(root.clone(), bib.clone());
        let before = Snapshot::capture(&workspace, &root, &BuildConfig::default());
        let completed = before.with_bindings([
            (&BTreeMap::new(), BindingStatus::Updated),
            (&bib, BindingStatus::Reused),
        ]);
        std::fs::remove_file(chapter).unwrap();
        assert!(completed.changed_path().is_none());
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        assert_eq!(completed.changed_path(), Some(alias.as_path()));
        // A new TeX run which did not rerun BibTeX still used the old .bbl.
        let next = Snapshot::capture(&workspace, &root, &BuildConfig::default());
        let unread = next.with_bindings([
            (&BTreeMap::new(), BindingStatus::Updated),
            (&bib, BindingStatus::Reused),
        ]);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        assert_eq!(unread.changed_path(), Some(alias.as_path()));
        let rebuilt =
            Snapshot::capture(&workspace, &root, &BuildConfig::default()).with_bindings([
                (&BTreeMap::new(), BindingStatus::Updated),
                (&bib, BindingStatus::Updated),
            ]);
        assert!(rebuilt.changed_path().is_none());
    }

    #[test]
    fn recorder_parent_traversal_watches_the_directory_alias_before_dotdot() {
        let directory = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::fs::create_dir(shared.path().join("child")).unwrap();
        let data = shared.path().join("table.csv");
        std::fs::write(&data, "data\n").unwrap();
        let alias = directory.path().join("link");
        std::os::unix::fs::symlink(shared.path().join("child"), &alias).unwrap();
        let opened = alias.join("../table.csv");
        let root = Url::from_file_path(directory.path().join("main.tex")).unwrap();
        let mut workspace = Workspace::default();
        workspace
            .project
            .recorded_bindings
            .insert(root.clone(), [(opened.clone(), data.clone())].into());
        let before = Snapshot::capture(&workspace, &root, &BuildConfig::default());
        assert!(before
            .watched()
            .contains(&Url::from_file_path(&alias).unwrap()));
        assert!(before
            .watched()
            .contains(&Url::from_file_path(&data).unwrap()));
        let lexical = directory.path().join("table.csv");
        assert!(before
            .watched()
            .contains(&Url::from_file_path(lexical).unwrap()));
        std::fs::remove_file(alias).unwrap();
        assert_eq!(before.changed_path(), Some(opened.as_path()));
    }

    #[test]
    fn recorded_bindings_capture_current_identity_and_keep_ambiguous_pdfs_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.csv");
        let second = directory.path().join("second.csv");
        let alias = directory.path().join("table.csv");
        for path in [&first, &second] {
            std::fs::write(path, "same bytes\n").unwrap();
        }
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let root = Url::from_file_path(directory.path().join("main.tex")).unwrap();
        let mut workspace = Workspace::default();
        workspace
            .project
            .recorded_bindings
            .insert(root.clone(), [(alias.clone(), first.clone())].into());
        let before = Snapshot::capture(&workspace, &root, &BuildConfig::default());
        assert!(before.changed_path().is_none());
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        assert_eq!(before.changed_path(), Some(alias.as_path()));
        let complete = before.with_bindings([(
            &[(alias.clone(), second.clone())].into(),
            BindingStatus::Updated,
        )]);
        assert_eq!(complete.changed_path(), Some(alias.as_path()));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        assert!(before.changed_path().is_none());
        // The newly written PDF has uncertain provenance; restoring the old
        // alias must not retroactively validate it.
        assert_eq!(complete.changed_path(), Some(alias.as_path()));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        let next = Snapshot::capture(&workspace, &root, &BuildConfig::default());
        assert!(next
            .watched()
            .contains(&Url::from_file_path(&second).unwrap()));
        let rebuilt = next.with_bindings([(&[(alias, second)].into(), BindingStatus::Updated)]);
        assert!(rebuilt.changed_path().is_none());
    }

    #[test]
    fn new_recorder_scope_drops_unused_bindings_but_keeps_static_conditions() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("table.csv");
        let condition = directory.path().join("optional.tex");
        std::fs::write(&data, "OLD\n").unwrap();
        let before = Snapshot {
            paths: [(condition.clone(), None)].into(),
            recorded: [(data.clone(), Some(data.clone()))].into(),
            ..Default::default()
        };
        let complete = before.with_bindings([(&BTreeMap::new(), BindingStatus::Updated)]);
        assert!(!complete
            .watched()
            .contains(&Url::from_file_path(&data).unwrap()));
        assert!(complete
            .watched()
            .contains(&Url::from_file_path(&condition).unwrap()));
        std::fs::remove_file(&data).unwrap();
        assert!(complete.changed_path().is_none());
        std::fs::write(&condition, "new\n").unwrap();
        assert_eq!(complete.changed_path(), Some(condition.as_path()));
    }
}

#[cfg(test)]
mod bibliography_conditions {
    use super::*;

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn biblatex_resource_conditions_track_exact_files_and_skip_unexecuted_or_nonlocal_calls() {
        for name in ["addbibresource", "addglobalbib"] {
            for call in [
                format!("\\{name}"),
                format!("\\csuse{{{name}}}"),
                "\\load".into(),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let source = format!("\\def\\load{{\\{name}}}{call}[label=refs]{{missing.bib}}");
                let before = capture(root, &source);
                assert!(
                    before.paths.contains_key(&root.join("missing.bib")),
                    "{source}: {:?}",
                    before.paths
                );
                std::fs::write(root.join("missing.bib.bib"), "unrelated").unwrap();
                assert!(before.changed_path().is_none());
                std::fs::write(root.join("missing.bib"), "restored").unwrap();
                assert!(before.changed_path().is_some());
            }
            for context in [
                format!("\\newcommand\\later{{\\{name}{{missing.bib}}}}"),
                format!("\\newcommand\\hold[1]{{}}\\hold\\{name}{{missing.bib}}"),
                format!("\\renewcommand\\{name}[1]{{}}\\{name}{{missing.bib}}"),
                format!("\\{name}[location=remote]{{missing.bib}}"),
                format!("\\{name}[glob]{{missing.bib}}"),
                format!("\\{name}*[label=x]{{missing.bib}}"),
                format!("\\detokenize{{\\{name}{{missing.bib}}}}"),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let before = capture(directory.path(), &context);
                assert!(
                    !before
                        .paths
                        .contains_key(&directory.path().join("missing.bib")),
                    "{context}"
                );
            }
        }
    }

    #[test]
    fn static_bibliography_conditions_watch_real_suffixes_comments_lists_and_compacted_names() {
        for (command, extension, stem) in [
            ("bibliography", "bib", "mi ssing"),
            ("nobibliography", "bib", "mi ssing"),
            ("bibliographystyle", "bst", "missing,style"),
        ] {
            for explicit in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                std::fs::create_dir(root.join("db")).unwrap();
                let suffix = if explicit {
                    format!(".{extension}")
                } else {
                    String::new()
                };
                let name = format!("db/{stem}{suffix}");
                let item = if extension == "bib" {
                    format!("first,% 😀 before\r\n {name}% after,\r\n,second")
                } else {
                    format!("% 😀 before\r\n {name}% after\r\n")
                };
                let source = format!("\\{command}{{{item}}}\r\n");
                let snapshot = capture(root, &source);
                assert!(snapshot.changed_path().is_none(), "{source}");
                let actual = root.join(format!("db/{}.{extension}", stem.replace(' ', "")));
                assert!(
                    snapshot
                        .watched()
                        .contains(&Url::from_file_path(&actual).unwrap()),
                    "{snapshot:?}"
                );
                let doubled = actual.with_file_name(format!(
                    "{}.{extension}",
                    actual.file_name().unwrap().to_str().unwrap()
                ));
                std::fs::write(doubled, "Decoy.").unwrap();
                assert!(snapshot.changed_path().is_none(), "{source}");
                std::fs::write(&actual, "New input.\n").unwrap();
                assert!(snapshot.changed_path().is_some(), "{source}");
            }
        }
    }

    #[test]
    fn dynamic_incomplete_ignored_and_redefined_declarations_do_not_invent_conditions() {
        for source in [
            r"\bibliography{missing,\chosen}",
            r"\nobibliography{missing,\chosen}",
            r"\bibliography*{missing}",
            r"\nobibliography*{missing}",
            r"\bibliography[note]{missing}",
            "\\bibliography{mi% split\nssing}",
            r"\bibliographystyle{\chosen}",
            r"\bibliographystyle*{missing}",
            r"\bibliographystyle{ missing}",
            r"\newcommand{\bibliography}[1]{}\bibliography{missing}",
            r"\newcommand{\nobibliography}[1]{}\bibliography{missing}",
            r"\newcommand{\bibliographystyle}[1]{}\bibliographystyle{missing}",
            r"\verb|\bibliography{missing}|",
            r"% \bibliographystyle{missing}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let snapshot = capture(root, source);
            assert!(snapshot.paths.is_empty(), "{source}\n{snapshot:?}");
            for extension in ["bib", "bst"] {
                std::fs::write(root.join(format!("missing.{extension}")), "New input.").unwrap();
            }
            assert!(snapshot.changed_path().is_none(), "{source}");
        }
    }

    #[test]
    fn database_and_style_redefinitions_only_disable_their_own_conditions() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for (source, extension) in [
            (
                r"\newcommand{\bibliographystyle}[1]{}\bibliography{missing}",
                "bib",
            ),
            (
                r"\newcommand{\bibliography}[1]{}\bibliographystyle{missing}",
                "bst",
            ),
        ] {
            let snapshot = capture(root, source);
            let actual = root.join(format!("missing.{extension}"));
            std::fs::write(&actual, "New input.").unwrap();
            assert!(snapshot.changed_path().is_some(), "{source}");
            std::fs::remove_file(actual).unwrap();
        }
    }

    #[test]
    fn bibliography_lookups_watch_root_candidates_before_source_relative_fallbacks() {
        for (command, extension) in [("bibliography", "bib"), ("bibliographystyle", "bst")] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir(root.join("sub")).unwrap();
            std::fs::write(
                root.join("sub/settings.tex"),
                format!("\\{command}{{missing}}\n"),
            )
            .unwrap();
            std::fs::write(
                root.join(format!("sub/missing.{extension}")),
                "Existing input.\n",
            )
            .unwrap();
            let snapshot = capture(root, r"\input{sub/settings}");
            assert!(snapshot.changed_path().is_none());
            std::fs::write(
                root.join(format!("missing.{extension}")),
                "Higher priority input.\n",
            )
            .unwrap();
            assert!(snapshot.changed_path().is_some());
        }
    }

    #[test]
    #[cfg(unix)]
    fn first_static_bibliography_aliases_detect_retargets_without_tool_history() {
        use std::os::unix::fs::symlink;
        for (command, extension) in [("bibliography", "bib"), ("bibliographystyle", "bst")] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let old = root.join(format!("old.{extension}"));
            let new = root.join(format!("new.{extension}"));
            std::fs::write(&old, "Old input.\n").unwrap();
            std::fs::write(&new, "New input.\n").unwrap();
            let alias = root.join(format!("alias.{extension}"));
            symlink(&old, &alias).unwrap();
            let snapshot = capture(root, &format!("\\{command}{{alias}}"));
            assert!(snapshot.changed_path().is_none());
            std::fs::remove_file(&alias).unwrap();
            symlink(&new, &alias).unwrap();
            assert!(snapshot.changed_path().is_some());
        }
    }

    #[test]
    #[cfg(unix)]
    fn missing_bibliography_files_under_directory_aliases_watch_physical_paths() {
        use std::os::unix::fs::symlink;
        for (command, extension) in [("bibliography", "bib"), ("bibliographystyle", "bst")] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let project = root.join("project");
            let shared = root.join("shared");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(&shared).unwrap();
            symlink(&shared, project.join("alias")).unwrap();
            let snapshot = capture(&project, &format!("\\{command}{{alias/missing}}"));
            let physical = shared.join(format!("missing.{extension}"));
            assert!(
                snapshot
                    .watched()
                    .contains(&Url::from_file_path(&physical).unwrap()),
                "{snapshot:?}"
            );
            std::fs::write(physical, "New input.\n").unwrap();
            assert!(snapshot.changed_path().is_some());
        }
    }

    #[test]
    fn bibliography_conditions_share_the_existing_candidate_budget() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let names = (0..MAX_PATHS + 10)
            .map(|n| format!("missing{n}"))
            .collect::<Vec<_>>()
            .join(",");
        let snapshot = capture(root, &format!("\\bibliography{{{names}}}"));
        assert_eq!(snapshot.paths.len(), MAX_PATHS);
        assert!(snapshot.changed_path().is_none());
    }
}

#[cfg(test)]
mod loader_conditions {
    use super::*;

    const LOADERS: [(&str, &str); 6] = [
        ("usepackage", "sty"),
        ("RequirePackage", "sty"),
        ("RequirePackageWithOptions", "sty"),
        ("documentclass", "cls"),
        ("LoadClass", "cls"),
        ("LoadClassWithOptions", "cls"),
    ];

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn loader_conditions_share_name_parsing_and_always_append_the_loading_suffix() {
        for (command, extension) in LOADERS {
            for explicit in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                std::fs::create_dir(root.join("local")).unwrap();
                let stem = if extension == "sty" {
                    "mi ssing类"
                } else {
                    "mi ssing,类"
                };
                let argument = format!(
                    "local/{stem}{}",
                    if explicit {
                        format!(".{extension}")
                    } else {
                        String::new()
                    }
                );
                let item = if extension == "sty" {
                    format!("first,% 😀 before\r\n {argument}% after,\r\n,second")
                } else {
                    format!("% 😀 before\r\n {argument}% after\r\n")
                };
                let options = if syntax::loader_forwards_options(command) {
                    ""
                } else {
                    "[dummy [literal]"
                };
                let source = format!("\\{command}{options}{{{item}}}[2020/01/01]\r\n");
                let snapshot = capture(root, &source);
                assert!(snapshot.changed_path().is_none(), "{source}");
                let literal = if extension == "sty" {
                    syntax::loader_name(&argument)
                } else {
                    argument.clone()
                };
                let actual = root.join(format!("{literal}.{extension}"));
                assert!(
                    snapshot
                        .watched()
                        .contains(&Url::from_file_path(&actual).unwrap()),
                    "{source}\n{snapshot:?}"
                );
                // Even an explicitly suffixed loader argument is a base name.
                // A file at the unappended name must not satisfy this lookup.
                std::fs::write(root.join(&literal), "Decoy.\n").unwrap();
                assert!(snapshot.changed_path().is_none(), "{source}");
                std::fs::write(&actual, "\\endinput\n").unwrap();
                assert!(snapshot.changed_path().is_some(), "{source}");
            }
        }
    }

    #[test]
    fn dynamic_incomplete_or_ignored_loader_arguments_do_not_invent_conditions() {
        for (command, extension) in LOADERS {
            let mut sources = vec![
                format!("\\{command}{{\\chosen}}"),
                format!("\\{command}*{{missing}}"),
                format!("\\{command}{{mi% split\nssing}}"),
                format!("\\{command}{{missing"),
                format!("\\{command}[first][second]{{missing}}"),
                format!("\\verb|\\{command}{{missing}}|"),
                format!("% \\{command}{{missing}}"),
                format!("\\newcommand{{\\{command}}}[1]{{}}\\{command}{{missing}}"),
            ];
            if extension == "sty" {
                sources.push(format!("\\{command}{{missing,\\chosen}}"));
            }
            if syntax::loader_forwards_options(command) {
                sources.push(format!("\\{command}[bad]{{missing}}"));
            }
            for source in sources {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let snapshot = capture(root, &source);
                assert!(snapshot.paths.is_empty(), "{source}\n{snapshot:?}");
                std::fs::write(root.join(format!("missing.{extension}")), "\\endinput\n").unwrap();
                assert!(snapshot.changed_path().is_none(), "{source}");
            }
        }
        for source in [
            r"\PassOptionsToPackage{option}{missing}",
            r"\PassOptionsToClass{option}{missing}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            assert!(capture(directory.path(), source).paths.is_empty());
        }
    }

    #[test]
    fn project_loader_redefinitions_disable_only_their_own_loading_family() {
        for (redefined, extension) in LOADERS {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::write(
                root.join("settings.tex"),
                format!("\\renewcommand{{\\{redefined}}}[1]{{}}\n"),
            )
            .unwrap();
            let source = r"\input{settings}\usepackage{missing}\RequirePackage{missing}\RequirePackageWithOptions{missing}\documentclass{missing}\LoadClass{missing}\LoadClassWithOptions{missing}\bibliography{missing}\bibliographystyle{missing}";
            let snapshot = capture(root, source);
            assert!(
                !snapshot
                    .paths
                    .contains_key(&root.join(format!("missing.{extension}"))),
                "{redefined}\n{snapshot:?}"
            );
            for other in ["sty", "cls", "bib", "bst"]
                .into_iter()
                .filter(|other| *other != extension)
            {
                let actual = root.join(format!("missing.{other}"));
                assert!(
                    snapshot.paths.contains_key(&actual),
                    "{redefined}\n{snapshot:?}"
                );
            }
        }
    }

    #[test]
    fn loader_conditions_watch_root_before_source_relative_fallbacks() {
        for (command, extension) in LOADERS {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir(root.join("sub")).unwrap();
            std::fs::write(
                root.join("sub/settings.tex"),
                format!("\\{command}{{missing}}\n"),
            )
            .unwrap();
            std::fs::write(
                root.join(format!("sub/missing.{extension}")),
                "\\endinput\n",
            )
            .unwrap();
            let snapshot = capture(root, r"\input{sub/settings}");
            assert!(snapshot.changed_path().is_none());
            std::fs::write(
                root.join(format!("missing.{extension}")),
                "Higher priority.\n",
            )
            .unwrap();
            assert!(snapshot.changed_path().is_some(), "{command}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn first_loader_aliases_detect_retargets_without_recorder_history() {
        use std::os::unix::fs::symlink;
        for (command, extension) in LOADERS {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            for name in ["old", "new"] {
                std::fs::write(root.join(format!("{name}.{extension}")), "\\endinput\n").unwrap();
            }
            let alias = root.join(format!("alias.{extension}"));
            symlink(root.join(format!("old.{extension}")), &alias).unwrap();
            let snapshot = capture(root, &format!("\\{command}{{alias}}"));
            assert!(snapshot.changed_path().is_none());
            std::fs::remove_file(&alias).unwrap();
            symlink(root.join(format!("new.{extension}")), &alias).unwrap();
            assert!(snapshot.changed_path().is_some(), "{command}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn missing_loaders_under_directory_aliases_watch_physical_paths() {
        use std::os::unix::fs::symlink;
        for (command, extension) in LOADERS {
            let project = tempfile::tempdir().unwrap();
            let shared = tempfile::tempdir().unwrap();
            symlink(shared.path(), project.path().join("alias")).unwrap();
            let snapshot = capture(project.path(), &format!("\\{command}{{alias/missing}}"));
            let physical = shared.path().join(format!("missing.{extension}"));
            assert!(
                snapshot
                    .watched()
                    .contains(&Url::from_file_path(&physical).unwrap()),
                "{command}\n{snapshot:?}"
            );
            std::fs::write(physical, "\\endinput\n").unwrap();
            assert!(snapshot.changed_path().is_some(), "{command}");
        }
    }

    #[test]
    fn loaders_and_bibliography_share_the_existing_candidate_budget() {
        for (command, extension) in LOADERS {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let names = (0..MAX_PATHS - 1)
                .map(|n| format!("db{n}"))
                .collect::<Vec<_>>()
                .join(",");
            let snapshot = capture(
                root,
                &format!("\\bibliography{{{names}}}\\{command}{{last}}\\{command}{{overflow}}"),
            );
            assert_eq!(snapshot.paths.len(), MAX_PATHS, "{command}");
            assert!(snapshot
                .paths
                .contains_key(&root.join(format!("last.{extension}"))));
            assert!(!snapshot
                .paths
                .contains_key(&root.join(format!("overflow.{extension}"))));
            assert!(snapshot.changed_path().is_none());
        }
    }
}

#[cfg(test)]
mod graphic_conditions {
    use super::*;

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn missing_graphics_watch_root_and_declared_directories_even_before_they_exist() {
        for (declaration, actual) in [
            ("", "missing.png"),
            (r"\graphicspath{{assets/}}", "assets/missing.png"),
            (
                r"\graphicspath{% before\n{assets/}% after\n}",
                "assets/missing.png",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let source = format!(
                "{}\\includegraphics*[width=1cm]{{ missing.png }}",
                declaration.replace("\\n", "\n")
            );
            let snapshot = capture(root, &source);
            let actual = root.join(actual);
            assert!(
                snapshot
                    .watched()
                    .contains(&Url::from_file_path(&actual).unwrap()),
                "{source}\n{snapshot:?}"
            );
            assert!(snapshot.changed_path().is_none());
            std::fs::create_dir_all(actual.parent().unwrap()).unwrap();
            std::fs::write(actual, b"PNG existence fixture").unwrap();
            assert!(snapshot.changed_path().is_some(), "{source}");
        }
    }

    #[test]
    fn graphic_conditions_follow_extension_precedence_and_stop_after_the_selected_input() {
        for (argument, changed, expected) in [
            ("plot", "plot.pdf", true),
            ("plot", "assets/plot.pdf", true),
            ("plot", "plot.png", true),
            ("plot", "assets/plot.jpg", false),
            ("plot.png", "plot.pdf", false),
            ("plot.png", "plot.png", true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir(root.join("assets")).unwrap();
            std::fs::write(root.join("assets/plot.png"), "Selected input.").unwrap();
            let snapshot = capture(
                root,
                &format!("\\graphicspath{{{{assets/}}}}\\includegraphics{{{argument}}}"),
            );
            std::fs::write(root.join(changed), "New candidate.").unwrap();
            assert_eq!(
                snapshot.changed_path().is_some(),
                expected,
                "{argument}: {changed}\n{snapshot:?}"
            );
        }
    }

    #[test]
    fn quoted_graphics_watch_normalized_names_and_respect_candidate_precedence() {
        for (argument, changed, expected) in [
            (r#""plot image""#, "plot image.pdf", true),
            (r#""plot image""#, "plot image.jpg", false),
            (r#""plot image".png"#, "plot image.pdf", false),
            (r#""plot image".png"#, "assets/plot image.png", true),
            ("\" plot\n image.png \"", "plot image.png", true),
            (r#""plot  image.png""#, "assets/plot image.png", true),
            (r#""plot  image.png""#, "assets/plot  image.png", false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir(root.join("assets")).unwrap();
            std::fs::write(root.join("assets/plot image.png"), "selected").unwrap();
            let snapshot = capture(
                root,
                &format!("\\graphicspath{{{{assets/}}}}\\includegraphics{{{argument}}}"),
            );
            assert!(snapshot.changed_path().is_none());
            if changed == "assets/plot image.png" {
                std::fs::remove_file(root.join(changed)).unwrap();
            } else {
                std::fs::write(root.join(changed), "new candidate").unwrap();
            }
            assert_eq!(
                snapshot.changed_path().is_some(),
                expected,
                "{argument}: {changed}\n{snapshot:?}"
            );
        }
    }

    #[test]
    fn quoted_graphics_directories_watch_normalized_candidates_before_the_selected_image() {
        for (entry, magic, supported) in [
            (r#""figure files/""#, "", true),
            (r#""figure files"/"#, "", true),
            (r#""figure  files/""#, "", true),
            (r#""figure files/""#, "% !TeX program = xelatex\n", false),
            (r#""figure files"/"#, "% !TeX program = lualatex\n", false),
        ] {
            for (changed, expected) in [
                ("plot.png", true),
                ("figure files/plot.png", true),
                ("plot.pdf", false),
                ("figure  files/plot.png", false),
                ("\"figure files\"/plot.png", false),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                for folder in ["figure files", "figure  files", "\"figure files\""] {
                    std::fs::create_dir(root.join(folder)).unwrap();
                }
                std::fs::write(root.join("figure files/plot.png"), "selected").unwrap();
                let source = format!("{magic}\\def\\Paths{{\\graphicspath}}\\Paths{{{{{entry}}}}}\\DeclareGraphicsExtensions{{.png,.pdf}}\\includegraphics{{plot}}");
                let snapshot = capture(root, &source);
                assert!(snapshot.changed_path().is_none());
                assert_eq!(
                    snapshot.watched().contains(
                        &Url::from_file_path(root.join("figure files/plot.png")).unwrap()
                    ),
                    supported,
                    "{source}"
                );
                if changed == "figure files/plot.png" {
                    std::fs::remove_file(root.join(changed)).unwrap();
                } else {
                    std::fs::write(root.join(changed), "candidate").unwrap();
                }
                assert_eq!(
                    snapshot.changed_path().is_some(),
                    supported && expected,
                    "{source}: {changed}\n{snapshot:?}"
                );
            }
        }
    }

    #[test]
    fn declared_graphics_extensions_track_only_earlier_and_selected_candidates() {
        for (argument, changed, expected) in [
            ("plot", "plot.png", true),
            ("plot", "assets/plot.png", true),
            ("plot", "plot.pdf", false),
            ("plot", "assets/plot.jpg", false),
            ("plot.pdf", "plot.png", false),
            ("plot.pdf", "plot.pdf", true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir(root.join("assets")).unwrap();
            for extension in ["png", "pdf"] {
                std::fs::write(root.join(format!("assets/plot.{extension}")), "image").unwrap();
            }
            let snapshot = capture(root, &format!(
                "\\graphicspath{{{{assets/}}}}\\DeclareGraphicsExtensions{{.png,.pdf}}\\includegraphics{{{argument}}}"
            ));
            if changed == "assets/plot.png" {
                std::fs::remove_file(root.join(changed)).unwrap();
            } else {
                std::fs::write(root.join(changed), "new candidate").unwrap();
            }
            assert_eq!(
                snapshot.changed_path().is_some(),
                expected,
                "{argument}: {changed}\n{snapshot:?}"
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let snapshot = capture(
            root,
            r"\DeclareGraphicsExtensions{.png}\includegraphics{missing}",
        );
        std::fs::write(root.join("missing.pdf"), "excluded input").unwrap();
        assert!(snapshot.changed_path().is_none());
        std::fs::write(root.join("missing.png"), "included input").unwrap();
        assert!(snapshot.changed_path().is_some());
    }

    #[test]
    fn global_graphics_extensions_control_build_candidate_precedence() {
        for (setting, selected) in [
            (r"{\global\DeclareGraphicsExtensions{.png,.pdf}}", "png"),
            (
                r"{\globaldefs=1 \DeclareGraphicsExtensions{.png,.pdf}\globaldefs=0 }",
                "png",
            ),
            (
                r"{\globaldefs=-1 \global\DeclareGraphicsExtensions{.png,.pdf}}",
                "pdf",
            ),
            (
                r"\def\Ext{\DeclareGraphicsExtensions}{\global\Ext{.png,.pdf}}",
                "png",
            ),
            (
                r"{\global\csuse{DeclareGraphicsExtensions}{.png,.pdf}}",
                "png",
            ),
            (
                r"{\globaldefs=-1 \global\csname DeclareGraphicsExtensions\endcsname{.png,.pdf}}",
                "pdf",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            for extension in ["png", "pdf"] {
                std::fs::write(root.join(format!("plot.{extension}")), "image").unwrap();
            }
            let snapshot = capture(
                root,
                &format!("\\DeclareGraphicsExtensions{{.pdf}}{setting}\\includegraphics{{plot}}"),
            );
            let other = if selected == "png" { "pdf" } else { "png" };
            std::fs::write(
                root.join(format!("plot.{other}")),
                "unselected image changed",
            )
            .unwrap();
            assert!(snapshot.changed_path().is_none(), "{setting}: {snapshot:?}");
            std::fs::remove_file(root.join(format!("plot.{selected}"))).unwrap();
            assert!(snapshot.changed_path().is_some(), "{setting}: {snapshot:?}");
        }
    }

    #[test]
    fn dotted_image_names_retain_graphicx_extension_fallbacks() {
        for (argument, actual) in [
            ("plot.v1", "assets/plot.v1.png"),
            ("plot.png", "assets/plot.png.pdf"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir(root.join("assets")).unwrap();
            let snapshot = capture(
                root,
                &format!("\\graphicspath{{{{assets/}}}}\\includegraphics{{{argument}}}"),
            );
            std::fs::write(root.join(actual), "New input.").unwrap();
            assert!(snapshot.changed_path().is_some(), "{argument}: {actual}");
        }
    }

    #[test]
    fn graphics_use_the_build_root_and_preserve_static_source_directory_declarations() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(
            root.join("sub/child.tex"),
            r"\graphicspath{{assets/}}\includegraphics{missing.png}",
        )
        .unwrap();
        let snapshot = capture(root, r"\input{sub/child}");
        assert!(snapshot
            .paths
            .contains_key(&root.join("assets/missing.png")));
        assert!(!snapshot.paths.contains_key(&root.join("sub/missing.png")));
        std::fs::write(root.join("sub/missing.png"), "Inaccessible input.").unwrap();
        assert!(snapshot.changed_path().is_none());
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/missing.png"), "New input.").unwrap();
        assert!(snapshot.changed_path().is_some());
    }

    #[test]
    fn dynamic_redefined_and_incomplete_graphic_searches_do_not_invent_conditions() {
        for source in [
            r"\includegraphics{\chosen}",
            "\\includegraphics{mi% split\nssing.png}",
            r"\includegraphics{missing.png",
            r"\verb|\includegraphics{missing.png}|",
            r"% \includegraphics{missing.png}",
            r"\newcommand{\includegraphics}[1]{}\includegraphics{missing.png}",
            r"\newcommand{\graphicspath}[1]{}\includegraphics{missing.png}",
            r"\DeclareGraphicsExtensions{\chosen}\includegraphics{missing.png}",
            r"\DeclareGraphicsRule{.png}{png}{.png}{}\includegraphics{missing.png}",
            r"\graphicspath{{assets/}{\unknown/}}\includegraphics{missing.png}",
            r"\graphicspath{{assets/}garbage}\includegraphics{missing.png}",
            r"\graphicspath{{assets/}{missing}}\includegraphics{missing.png}",
            r"\graphicspath*{{assets/}}\includegraphics{missing.png}",
            r"\graphicspath[option]{{assets/}}\includegraphics{missing.png}",
            r"\graphicspath\includegraphics{missing.png}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let snapshot = capture(root, source);
            assert!(snapshot.paths.is_empty(), "{source}\n{snapshot:?}");
            std::fs::write(root.join("missing.png"), "New input.").unwrap();
            assert!(snapshot.changed_path().is_none(), "{source}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn first_image_and_search_directory_aliases_detect_retargets_without_recorder_history() {
        use std::os::unix::fs::symlink;
        for directory_alias in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            for name in ["old", "new"] {
                std::fs::create_dir(root.join(name)).unwrap();
                std::fs::write(root.join(name).join("plot.png"), "Image fixture.").unwrap();
            }
            let (alias, old, new, source) = if directory_alias {
                (
                    root.join("alias"),
                    root.join("old"),
                    root.join("new"),
                    r"\graphicspath{{alias/}}\includegraphics{plot.png}",
                )
            } else {
                (
                    root.join("alias.png"),
                    root.join("old/plot.png"),
                    root.join("new/plot.png"),
                    r"\includegraphics{alias.png}",
                )
            };
            symlink(&old, &alias).unwrap();
            let snapshot = capture(root, source);
            assert!(snapshot.changed_path().is_none());
            std::fs::remove_file(&alias).unwrap();
            symlink(&new, &alias).unwrap();
            assert!(snapshot.changed_path().is_some(), "{source}\n{snapshot:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn missing_graphics_under_directory_aliases_watch_external_physical_creation() {
        use std::os::unix::fs::symlink;
        for path_argument in [false, true] {
            let project = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            symlink(external.path(), project.path().join("alias")).unwrap();
            let source = if path_argument {
                r"\includegraphics{alias/nested/图 片.png}"
            } else {
                r"\graphicspath{{alias/nested/}}\includegraphics{图 片.png}"
            };
            let snapshot = capture(project.path(), source);
            let actual = external.path().join("nested/图 片.png");
            assert!(
                snapshot
                    .watched()
                    .contains(&Url::from_file_path(&actual).unwrap()),
                "{source}\n{snapshot:?}"
            );
            std::fs::create_dir(actual.parent().unwrap()).unwrap();
            std::fs::write(actual, "New image.").unwrap();
            assert!(snapshot.changed_path().is_some());
        }
    }

    #[test]
    fn graphic_candidates_share_the_path_budget_and_exclude_known_build_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let names = (0..MAX_PATHS - 1)
            .map(|n| format!("db{n}"))
            .collect::<Vec<_>>()
            .join(",");
        let snapshot = capture(
            root,
            &format!("\\bibliography{{{names}}}\\includegraphics{{last}}"),
        );
        assert_eq!(snapshot.paths.len(), MAX_PATHS);
        assert!(snapshot.paths.contains_key(&root.join("last.pdf")));
        assert!(!snapshot.paths.contains_key(&root.join("last.png")));
        let snapshot = capture(root, r"\includegraphics{build/texquill/main.pdf}");
        let output = root.join("build/texquill/main.pdf");
        assert!(!snapshot.paths.contains_key(&output));
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(output, b"%PDF-1.4\n").unwrap();
        assert!(snapshot.changed_path().is_none());
    }

    #[test]
    fn incomplete_project_graphic_searches_and_excessive_directory_lists_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("settings.tex"),
            r"\graphicspath{{assets/}{\unknown/}}",
        )
        .unwrap();
        let snapshot = capture(root, r"\input{settings}\includegraphics{missing.png}");
        assert!(!snapshot.paths.contains_key(&root.join("missing.png")));
        let names = (0..257)
            .map(|n| format!("{{assets{n}/}}"))
            .collect::<String>();
        let snapshot = capture(
            root,
            &format!("\\graphicspath{{{names}}}\\includegraphics{{missing.png}}"),
        );
        assert!(snapshot.paths.is_empty());
        // A declaration in a separate project does not affect this build.
        let snapshot = capture(root, r"\includegraphics{missing.png}");
        assert!(snapshot.paths.contains_key(&root.join("missing.png")));
    }

    #[test]
    fn repeated_graphic_paths_track_only_the_active_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for (setting, selected) in [
            (
                r"\graphicspath{{first/}}\graphicspath{{second/}}",
                Some("second"),
            ),
            (
                r"\graphicspath{{first/}}\graphicspath{{first/}}",
                Some("first"),
            ),
            (r"\graphicspath{{first/}}\graphicspath{}", None),
            (
                r"\graphicspath{{first/}}{\graphicspath{{second/}}}",
                Some("first"),
            ),
            (
                r"\graphicspath{{first/}}{\global\graphicspath{{second/}}}",
                Some("second"),
            ),
            (
                r"\graphicspath{{first/}}{\globaldefs=-1 \global\graphicspath{{second/}}}",
                Some("first"),
            ),
        ] {
            let snapshot = capture(root, &format!("{setting}\\includegraphics{{missing.png}}"));
            assert!(
                snapshot.paths.contains_key(&root.join("missing.png")),
                "{setting}"
            );
            for name in ["first", "second"] {
                assert_eq!(
                    snapshot
                        .paths
                        .contains_key(&root.join(format!("{name}/missing.png"))),
                    selected == Some(name),
                    "{setting}: {snapshot:?}"
                );
            }
        }
        std::fs::write(root.join("settings.tex"), r"\graphicspath{{first/}}").unwrap();
        let snapshot = capture(
            root,
            r"\input{settings}\graphicspath{{second/}}\includegraphics{missing.png}",
        );
        assert!(!snapshot.paths.contains_key(&root.join("first/missing.png")));
        assert!(snapshot
            .paths
            .contains_key(&root.join("second/missing.png")));
    }
}

#[cfg(test)]
mod listing_conditions {
    use super::*;

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn missing_and_shadowing_listing_candidates_are_watched_before_the_first_build() {
        for name in ["README", "sample.rs", "sample.unknown", "sample.tex"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let snapshot = capture(root, &format!("\\lstinputlisting[title={{A,B}}]{{{name}}}"));
            assert!(
                snapshot.paths.contains_key(&root.join(name)),
                "{name}: {snapshot:?}"
            );
            std::fs::write(root.join(name), "Code.").unwrap();
            assert!(snapshot.changed_path().is_some(), "{name}");
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("sample.rs"), "Code.").unwrap();
        let snapshot = capture(root, r"\lstinputlisting{sample.rs}");
        assert!(snapshot.paths.contains_key(&root.join("sample.rs.tex")));
        std::fs::write(root.join("sample.rs.tex"), "Shadowing code.").unwrap();
        assert!(snapshot.changed_path().is_some());
    }

    #[test]
    fn verbatim_candidates_follow_starred_default_search_and_alias_changes() {
        for name in [
            "README",
            "sample.rs",
            "sample.unknown",
            "sample.tex",
            "样例, one.rs",
        ] {
            for star in ["", "*", " * % keep\n"] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let snapshot = capture(root, &format!("\\verbatiminput{star}{{{name}}}"));
                assert!(
                    snapshot.paths.contains_key(&root.join(name)),
                    "{name}: {snapshot:?}"
                );
                std::fs::write(root.join(name), "Code.").unwrap();
                assert!(snapshot.changed_path().is_some());
                if !name.ends_with(".tex") {
                    let snapshot = capture(root, &format!("\\verbatiminput{star}{{{name}}}"));
                    std::fs::write(root.join(format!("{name}.tex")), "Shadowing code.").unwrap();
                    assert!(snapshot.changed_path().is_some());
                }
            }
        }
        for source in [
            r"\verbatiminput[x]{missing.rs}",
            r"\verbatiminput**{missing.rs}",
            r"\renewcommand{\verbatiminput}[1]{}\verbatiminput{missing.rs}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            assert!(
                capture(directory.path(), source).paths.is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn fancyverb_candidates_watch_literal_quoted_and_detokenized_names() {
        for (command, value) in ["VerbatimInput", "LVerbatimInput", "BVerbatimInput"]
            .into_iter()
            .flat_map(|command| {
                [
                    "sample.unknown",
                    "\"sample one.rs\"",
                    "\\detokenize{\"样例 one.rs\"}",
                ]
                .map(|value| (command, value))
            })
        {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let name = value
                .strip_prefix("\\detokenize{")
                .and_then(|v| v.strip_suffix('}'))
                .unwrap_or(value)
                .trim_matches('"');
            let source = format!("\\{command} *[frame=single]{{{value}}}");
            let snapshot = capture(root, &source);
            assert!(
                snapshot.paths.contains_key(&root.join(name)),
                "{value}: {snapshot:?}"
            );
            std::fs::write(root.join(name), "Code.").unwrap();
            assert!(snapshot.changed_path().is_some());
            let snapshot = capture(root, &source);
            std::fs::write(root.join(format!("{name}.tex")), "Shadow.").unwrap();
            assert!(snapshot.changed_path().is_some());
        }
        for source in [
            r"\VerbatimInput[commandchars=abc]{missing.rs}",
            r"\fvset{commandchars=abc}\VerbatimInput{missing.rs}",
            r"\VerbatimInput{missing file.rs}",
            r"\def\detokenize#1{}\VerbatimInput{missing.rs}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            assert!(
                capture(directory.path(), source).paths.is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn unsupported_listing_searches_do_not_invent_conditions() {
        for source in [
            r"\lstinputlisting*{missing.rs}",
            r"\lstinputlisting[a][b]{missing.rs}",
            r"\lstinputlisting[inputpath=other]{missing.rs}",
            r"\lstset{style=custom}\lstinputlisting{missing.rs}",
            r"\renewcommand{\lstinputlisting}[1]{}\lstinputlisting{missing.rs}",
            r"\lstinputlisting{\macro.rs}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let snapshot = capture(directory.path(), source);
            assert!(snapshot.paths.is_empty(), "{source}: {snapshot:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn code_aliases_watch_both_opened_paths_and_their_physical_targets() {
        for command in [
            "lstinputlisting",
            "verbatiminput *",
            "VerbatimInput *",
            "LVerbatimInput *",
            "BVerbatimInput *",
        ] {
            check_code_alias(command);
        }
    }

    #[cfg(unix)]
    fn check_code_alias(command: &str) {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = directory.path();
        let first = external.path().join("first.unknown");
        let second = external.path().join("second.tex");
        std::fs::write(&first, "Code.").unwrap();
        std::fs::write(&second, "Code.").unwrap();
        let alias = root.join("sample.rs");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let snapshot = capture(root, &format!("\\{command}{{sample.rs}}"));
        assert!(snapshot
            .watched
            .contains(&Url::from_file_path(&first).unwrap()));
        assert!(snapshot
            .watched
            .contains(&Url::from_file_path(&alias).unwrap()));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        assert!(snapshot.changed_path().is_some());
    }
}

#[cfg(test)]
mod indirect_source_checks_tests {
    use super::*;

    #[test]
    fn missing_indirect_input_files_are_observed_without_tracking_include_outputs() {
        for call in [
            r"\csuse{input}{chapte}",
            r"\csname input\endcsname{chapte}",
            r"\readpart{chapte}",
            r"\csuse{readpart}{chapte}",
            r"\csname readpart\endcsname{chapte}",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("main.tex");
            let uri = Url::from_file_path(&root).unwrap();
            std::fs::write(
                &root,
                format!(r"\def\readpart{{\input}}{call}\csuse{{include}}{{sub/chapter}}\IfFileExists{{build/texquill/sub/chapter.aux}}{{}}{{}}"),
            )
            .unwrap();
            let mut workspace = Workspace::default();
            workspace.refresh_project(&uri);
            let snapshot = Snapshot::capture(&workspace, &uri, &BuildConfig::default());
            assert!(
                snapshot
                    .paths
                    .contains_key(&directory.path().join("chapte.tex")),
                "{call}: {snapshot:?}"
            );
            assert!(!snapshot
                .paths
                .contains_key(&directory.path().join("build/texquill/sub/chapter.aux")));
            assert!(snapshot.changed_path().is_none());
            std::fs::write(directory.path().join("chapte.tex"), "Chapter").unwrap();
            assert!(snapshot.changed_path().is_some(), "{call}");
        }
    }
}
#[cfg(test)]
mod indirect_loader_conditions {
    use super::*;

    const LOADERS: [(&str, &str); 6] = [
        ("usepackage", "sty"),
        ("RequirePackage", "sty"),
        ("RequirePackageWithOptions", "sty"),
        ("documentclass", "cls"),
        ("LoadClass", "cls"),
        ("LoadClassWithOptions", "cls"),
    ];

    fn call(form: &str, loader: &str) -> String {
        match form {
            "named" => format!("\\csuse{{{loader}}}"),
            "literal" => format!("\\csname {loader}\\endcsname"),
            "alias" => r"\readpart".into(),
            "named-alias" => r"\csuse{readpart}".into(),
            "literal-alias" => r"\csname readpart\endcsname".into(),
            _ => panic!(),
        }
    }

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn static_indirect_loaders_watch_missing_candidates_and_appended_extensions() {
        for (loader, extension) in LOADERS {
            for form in ["named", "literal", "alias", "named-alias", "literal-alias"] {
                for explicit in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path();
                    std::fs::create_dir(root.join("sub")).unwrap();
                    let name = if extension == "sty" {
                        "sub/fixt ure"
                    } else {
                        "sub/fixt ure,类"
                    };
                    let argument = format!(
                        "{name}{}",
                        if explicit {
                            format!(".{extension}")
                        } else {
                            String::new()
                        }
                    );
                    let options = if syntax::loader_forwards_options(loader) {
                        ""
                    } else {
                        "[draft]"
                    };
                    let source = format!("\\def\\readpart{{\\{loader}}}\n{}{options}{{% head 😀\r\n{argument}% tail\r\n}}[2020/01/01]\n", call(form, loader));
                    let snapshot = capture(root, &source);
                    let normalized = if extension == "sty" {
                        syntax::loader_name(&argument)
                    } else {
                        argument
                    };
                    let actual = root.join(format!("{normalized}.{extension}"));
                    assert!(
                        snapshot
                            .watched()
                            .contains(&Url::from_file_path(&actual).unwrap()),
                        "{loader}/{form}/{explicit}: {snapshot:?}"
                    );
                    assert!(snapshot.changed_path().is_none());
                    std::fs::write(root.join(normalized), "Decoy").unwrap();
                    assert!(
                        snapshot.changed_path().is_none(),
                        "{loader}/{form}/{explicit}"
                    );
                    std::fs::write(actual, "Restored input").unwrap();
                    assert!(
                        snapshot.changed_path().is_some(),
                        "{loader}/{form}/{explicit}"
                    );
                }
            }
        }
    }

    #[test]
    fn uncertain_indirect_loaders_and_nonloading_forwarders_do_not_watch_guessed_targets() {
        for (loader, extension) in LOADERS {
            for form in ["named", "literal", "alias", "named-alias", "literal-alias"] {
                let ctrl = call(form, loader);
                for body in [
                    format!("\\newcommand{{\\hold}}[1]{{}}\\hold{{{ctrl}{{fixture}}}}"),
                    format!("\\newcommand{{\\later}}{{{ctrl}{{fixture}}}}"),
                    format!("{ctrl}{{\\filename}}"),
                    format!("{ctrl}{{fixt% split\nure}}"),
                    format!("\\renewcommand{{\\{loader}}}[1]{{}}{ctrl}{{fixture}}"),
                    format!("\\endinput{ctrl}{{fixture}}"),
                    format!("{ctrl}{{fixture,\\filename}}"),
                    format!("{ctrl}{{fixture"),
                ] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path();
                    let source = format!("\\def\\readpart{{\\{loader}}}\n{body}");
                    let snapshot = capture(root, &source);
                    let target = root.join(format!("fixture.{extension}"));
                    assert!(
                        !snapshot
                            .watched()
                            .contains(&Url::from_file_path(&target).unwrap()),
                        "{loader}/{form}/{body}: {snapshot:?}"
                    );
                    std::fs::write(target, "Unrelated target").unwrap();
                    assert!(snapshot.changed_path().is_none(), "{loader}/{form}/{body}");
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let snapshot = capture(
            root,
            r"\def\readpart{\PassOptionsToPackage}\readpart{draft}{fixture}",
        );
        std::fs::write(root.join("fixture.sty"), "Options do not load").unwrap();
        assert!(snapshot.changed_path().is_none());
    }
}
#[cfg(test)]
mod indirect_bibliography_conditions {
    use super::*;

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn indirect_bibliography_calls_watch_missing_candidates_with_real_suffix_rules() {
        for (command, extension) in [
            ("bibliography", "bib"),
            ("nobibliography", "bib"),
            ("bibliographystyle", "bst"),
        ] {
            for call in [
                format!("\\csuse{{{command}}}"),
                format!("\\csname {command}\\endcsname"),
                r"\loadbib".into(),
                r"\csuse{loadbib}".into(),
                r"\csname loadbib\endcsname".into(),
            ] {
                for explicit in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path();
                    let stem = if extension == "bib" {
                        "fixt ure"
                    } else {
                        "fixture,part"
                    };
                    let name = format!(
                        "{stem}{}",
                        if explicit {
                            format!(".{extension}")
                        } else {
                            String::new()
                        }
                    );
                    let source = format!(
                        "\\def\\loadbib{{\\{command}}}\n{call}{{% head 😀\r\n{name}% tail\r\n}}"
                    );
                    let snapshot = capture(root, &source);
                    let normalized = if extension == "bib" {
                        syntax::loader_name(stem)
                    } else {
                        stem.into()
                    };
                    let actual = root.join(format!("{normalized}.{extension}"));
                    assert!(
                        snapshot
                            .watched()
                            .contains(&Url::from_file_path(&actual).unwrap()),
                        "{command}/{call}/{explicit}: {snapshot:?}"
                    );
                    std::fs::write(
                        root.join(format!("{normalized}.{extension}.{extension}")),
                        "Wrong double suffix",
                    )
                    .unwrap();
                    assert!(snapshot.changed_path().is_none());
                    std::fs::write(actual, "New resource").unwrap();
                    assert!(snapshot.changed_path().is_some());
                }
            }
        }
    }

    #[test]
    fn uncertain_bibliography_calls_do_not_invent_missing_resource_conditions() {
        for (command, extension) in [
            ("bibliography", "bib"),
            ("nobibliography", "bib"),
            ("bibliographystyle", "bst"),
        ] {
            for body in [
                r"\newcommand{\hold}[1]{}\hold{\loadbib{fixture}}".into(),
                r"\newcommand{\later}{\loadbib{fixture}}".into(),
                r"\loadbib{\filename}".into(),
                format!("\\renewcommand{{\\{command}}}[1]{{}}\\loadbib{{fixture}}"),
                "\\loadbib{fixt% split\nure}".into(),
                r"\endinput\loadbib{fixture}".into(),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let source = format!("\\def\\loadbib{{\\{command}}}\n{body}");
                let snapshot = capture(root, &source);
                let target = root.join(format!("fixture.{extension}"));
                assert!(
                    !snapshot
                        .watched()
                        .contains(&Url::from_file_path(&target).unwrap()),
                    "{command}/{body}: {snapshot:?}"
                );
                std::fs::write(target, "Unrelated").unwrap();
                assert!(snapshot.changed_path().is_none());
            }
        }
    }
}

#[cfg(test)]
mod indirect_selection_conditions {
    use super::*;

    #[test]
    fn only_selected_missing_chapters_change_conditions_with_static_indirect_filters() {
        for control in [
            r"\includeonly",
            r"\csuse{includeonly}",
            r"\csname includeonly\endcsname",
            r"\choose",
            r"\csuse{choose}",
            r"\csname choose\endcsname",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let main = root.join("main.tex");
            let source=format!("\\documentclass{{article}}\\def\\choose{{\\includeonly}}{control}{{selected.tex}}\\begin{{document}}\\include{{selected}}\\include{{excluded}}\\end{{document}}");
            std::fs::write(&main, source).unwrap();
            let main_uri = Url::from_file_path(&main).unwrap();
            let mut workspace = Workspace::default();
            workspace.add_root(root.into());
            let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
            assert!(snapshot.changed_path().is_none());
            std::fs::write(root.join("excluded.tex"), "Excluded").unwrap();
            assert!(
                snapshot.changed_path().is_none(),
                "{control}: excluded chapter changed conditions"
            );
            std::fs::write(root.join("selected.tex"), "Selected").unwrap();
            assert!(
                snapshot.changed_path().is_some(),
                "{control}: restored selected chapter was missed"
            );
        }
    }
}

#[cfg(test)]
mod indirect_conditional_file_conditions {
    use super::*;

    #[test]
    fn static_indirect_conditional_files_track_creation_without_importing_check_only_sources() {
        for loader in ["InputIfFileExists", "IfFileExists"] {
            for control in [
                format!("\\{loader}"),
                format!("\\csuse{{{loader}}}"),
                format!("\\csname {loader}\\endcsname"),
                r"\readpart".into(),
                r"\csuse{readpart}".into(),
                r"\csname readpart\endcsname".into(),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let main = root.join("main.tex");
                let source = format!("\\documentclass{{article}}\\def\\readpart{{\\{loader}}}\\begin{{document}}{control}{{% file 😀\r\nchapter% tail\r\n}}{{Before}}{{Missing}}\\end{{document}}");
                std::fs::write(&main, source).unwrap();
                let main_uri = Url::from_file_path(&main).unwrap();
                let mut workspace = Workspace::default();
                workspace.add_root(root.into());
                let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
                assert!(snapshot.changed_path().is_none());
                std::fs::write(root.join("unrelated.tex"), "Unrelated").unwrap();
                assert!(snapshot.changed_path().is_none());
                std::fs::write(root.join("chapter.tex"), "Chapter").unwrap();
                assert!(
                    snapshot.changed_path().is_some(),
                    "{loader}/{control}: file restoration missed"
                );
                workspace.refresh_project(&main_uri);
                let chapter = Url::from_file_path(root.join("chapter.tex")).unwrap();
                assert_eq!(
                    workspace.project_documents(&main_uri).contains(&chapter),
                    loader == "InputIfFileExists",
                    "{loader}/{control}"
                );
            }
        }
    }
}

#[cfg(test)]
mod active_direct_source_conditions {
    use super::*;
    use crate::project::workspace::Document;

    fn call(loader: &str) -> String {
        format!(
            "\\{loader}{{chapter}}{}",
            if matches!(loader, "InputIfFileExists" | "IfFileExists") {
                "{Before}{Missing}"
            } else {
                ""
            }
        )
    }

    #[test]
    fn direct_source_conditions_follow_active_calls_without_executing_macro_data() {
        for loader in [
            "input",
            "include",
            "subfile",
            "InputIfFileExists",
            "IfFileExists",
        ] {
            let command = call(loader);
            for (body, active) in [
                (command.clone(), true),
                (format!("\\textbf{{{command}}}"), true),
                (format!("\\hold{{ignored}}{command}"), true),
                (format!("\\take ignoredSTOP{command}"), true),
                (format!("\\newcommand{{\\unused}}{{{command}}}"), false),
                (format!("\\hold{{{command}}}"), false),
                (format!("\\hold{command}"), false),
                (format!("\\take {command}STOP"), false),
                (format!("\\string{command}"), false),
                (format!("\\detokenize{{{command}}}"), false),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let main = root.join("main.tex");
                let source = format!("\\documentclass{{article}}\\newcommand{{\\hold}}[1]{{}}\\def\\take#1STOP{{}}\\begin{{document}}{body}\\end{{document}}");
                std::fs::write(&main, source).unwrap();
                let main_uri = Url::from_file_path(&main).unwrap();
                let mut workspace = Workspace::default();
                workspace.add_root(root.into());
                let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
                assert!(snapshot.changed_path().is_none());
                std::fs::write(root.join("chapter.tex"), "Chapter").unwrap();
                assert_eq!(snapshot.changed_path().is_some(), active, "{loader}/{body}");
            }
        }
    }

    #[test]
    fn redefined_direct_source_commands_do_not_certify_file_lookup_conditions() {
        for loader in [
            "input",
            "include",
            "subfile",
            "InputIfFileExists",
            "IfFileExists",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let main = root.join("main.tex");
            let source = format!(
                "\\documentclass{{article}}\\def\\{loader}#1{{}}{}",
                call(loader)
            );
            std::fs::write(&main, source).unwrap();
            let main_uri = Url::from_file_path(&main).unwrap();
            let mut workspace = Workspace::default();
            workspace.add_root(root.into());
            let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
            std::fs::write(root.join("chapter.tex"), "Chapter").unwrap();
            assert!(snapshot.changed_path().is_none(), "{loader}");
        }
    }

    #[test]
    fn direct_conditions_keep_recorder_only_sources_and_use_their_macro_providers() {
        for provider in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let main = root.join("main.tex");
            let recorded = root.join("recorded.tex");
            let source = if provider {
                r"\documentclass{article}\hold{\IfFileExists{chapter}{Before}{Missing}}"
            } else {
                r"\documentclass{article}Main"
            };
            let recorded_source = if provider {
                r"\newcommand{\hold}[1]{}"
            } else {
                r"\IfFileExists{chapter}{Before}{Missing}"
            };
            std::fs::write(&main, source).unwrap();
            let main_uri = Url::from_file_path(&main).unwrap();
            let mut workspace = Workspace::default();
            workspace.add_root(root.into());
            // Add after the static scan: only the previous recorder connects it.
            std::fs::write(&recorded, recorded_source).unwrap();
            let recorded_uri = Url::from_file_path(&recorded).unwrap();
            workspace.documents.insert(
                recorded_uri.clone(),
                Document::new(&recorded_uri, recorded_source.into(), None),
            );
            workspace
                .project
                .recorded_inputs
                .insert(main_uri.clone(), BTreeSet::from([recorded_uri.clone()]));
            assert!(workspace
                .project_documents(&main_uri)
                .contains(&recorded_uri));
            let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
            std::fs::write(root.join("chapter.tex"), "Chapter").unwrap();
            assert_eq!(
                snapshot.changed_path().is_some(),
                !provider,
                "provider={provider}"
            );
        }
    }
}

#[cfg(test)]
mod direct_include_argument_conditions {
    use super::*;

    #[test]
    fn include_body_groups_after_unsupported_modifiers_do_not_create_file_conditions() {
        for modifier in ["", "*", "[note]", "*[note]", "[first][second]"] {
            for argument in [
                "{chapter}",
                "% before 😀\r\n{% file\r\nchapter.tex% tail\r\n}",
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let main = root.join("main.tex");
                let source = format!("\\documentclass{{article}}\\begin{{document}}Main\\include{modifier}{argument}\\end{{document}}");
                std::fs::write(&main, source).unwrap();
                let main_uri = Url::from_file_path(&main).unwrap();
                let mut workspace = Workspace::default();
                workspace.add_root(root.into());
                let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
                assert!(snapshot.changed_path().is_none());
                std::fs::write(root.join("chapter.tex"), "Chapter").unwrap();
                assert_eq!(
                    snapshot.changed_path().is_some(),
                    modifier.is_empty(),
                    "{modifier}/{argument}"
                );
            }
        }
    }
}

#[cfg(test)]
mod active_direct_graphic_conditions {
    use super::*;
    use crate::project::workspace::Document;

    #[test]
    fn graphic_conditions_skip_declarations_and_consumed_tokens_but_keep_executed_calls() {
        for control in [r"\includegraphics", r"\includegraphics*"] {
            let command = format!("{control}[width=1cm]{{picture}}");
            for (body, active) in [
                (command.clone(), true),
                (format!("\\textbf{{{command}}}"), true),
                (format!("\\hold{{ignored}}{command}"), true),
                (format!("\\take ignoredSTOP{command}"), true),
                (format!("\\newcommand{{\\unused}}{{{command}}}"), false),
                (format!("\\hold{{{command}}}"), false),
                (format!("\\hold{command}"), false),
                (format!("\\take {command}STOP"), false),
                (format!("\\string{command}"), false),
                (format!("\\detokenize{{{command}}}"), false),
                (format!("\\endinput\n{command}"), false),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let main = root.join("main.tex");
                std::fs::write(
                    &main,
                    format!("\\newcommand{{\\hold}}[1]{{}}\\def\\take#1STOP{{}}{body}"),
                )
                .unwrap();
                let uri = Url::from_file_path(&main).unwrap();
                let mut workspace = Workspace::default();
                workspace.add_root(root.into());
                let snapshot = Snapshot::capture(&workspace, &uri, &BuildConfig::default());
                assert!(snapshot.changed_path().is_none());
                std::fs::write(root.join("picture.png"), b"image").unwrap();
                assert_eq!(snapshot.changed_path().is_some(), active, "{body}");
            }
        }
    }

    #[test]
    fn aliases_track_selected_and_earlier_candidates_but_skip_later_images() {
        for call in [r"\Image", r"\csuse{Image}", r"\csname Image\endcsname"] {
            for (changed, expected) in [
                ("plot.pdf", false),
                ("plot.png", true),
                ("assets/plot.png", true),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                std::fs::create_dir(root.join("assets")).unwrap();
                std::fs::write(root.join("assets/plot.png"), "selected").unwrap();
                let source = format!("\\def\\Image{{\\includegraphics}}\\DeclareGraphicsExtensions{{.png,.pdf}}\\graphicspath{{{{assets/}}}}{call}{{plot}}");
                let main = root.join("main.tex");
                std::fs::write(&main, source).unwrap();
                let uri = Url::from_file_path(main).unwrap();
                let mut workspace = Workspace::default();
                workspace.add_root(root.into());
                let snapshot = Snapshot::capture(&workspace, &uri, &BuildConfig::default());
                assert!(snapshot.changed_path().is_none());
                if changed == "assets/plot.png" {
                    std::fs::remove_file(root.join(changed)).unwrap();
                } else {
                    std::fs::write(root.join(changed), "new image").unwrap();
                }
                assert_eq!(
                    snapshot.changed_path().is_some(),
                    expected,
                    "{call}/{changed}"
                );
            }
        }
    }

    #[test]
    fn incomplete_source_discovery_does_not_certify_image_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("part.tex"), r"\def\input#1{}\end{x}").unwrap();
        let main = root.join("main.tex");
        std::fs::write(&main, r"\documentclass{article}\def\readpart{\input}\begin{document}\begin{x}\readpart{part}\includegraphics{plot.png}\end{document}").unwrap();
        let uri = Url::from_file_path(main).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        let snapshot = Snapshot::capture(&workspace, &uri, &BuildConfig::default());
        std::fs::write(root.join("plot.png"), "new image").unwrap();
        assert!(snapshot.changed_path().is_none());
    }

    #[test]
    fn graphic_activity_uses_recorder_sources_and_their_macro_signatures() {
        for provider in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let main = root.join("main.tex");
            let recorded = root.join("recorded.tex");
            let source = if provider {
                r"\documentclass{article}\hold{\includegraphics{picture}}"
            } else {
                r"\documentclass{article}Main"
            };
            let recorded_source = if provider {
                r"\newcommand{\hold}[1]{}"
            } else {
                r"\includegraphics{picture}"
            };
            std::fs::write(&main, source).unwrap();
            let main_uri = Url::from_file_path(&main).unwrap();
            let mut workspace = Workspace::default();
            workspace.add_root(root.into());
            std::fs::write(&recorded, recorded_source).unwrap();
            let recorded_uri = Url::from_file_path(&recorded).unwrap();
            workspace.documents.insert(
                recorded_uri.clone(),
                Document::new(&recorded_uri, recorded_source.into(), None),
            );
            workspace
                .project
                .recorded_inputs
                .insert(main_uri.clone(), BTreeSet::from([recorded_uri]));
            let snapshot = Snapshot::capture(&workspace, &main_uri, &BuildConfig::default());
            std::fs::write(root.join("picture.png"), b"image").unwrap();
            assert_eq!(
                snapshot.changed_path().is_some(),
                !provider,
                "provider={provider}"
            );
        }
    }
}

#[cfg(test)]
mod active_direct_code_conditions {
    use super::*;

    #[test]
    fn code_conditions_require_active_calls_and_keep_completed_argument_boundaries() {
        for loader in [
            "verbatiminput",
            "lstinputlisting",
            "VerbatimInput",
            "LVerbatimInput",
            "BVerbatimInput",
        ] {
            for star in [false, true] {
                if star && loader == "lstinputlisting" {
                    continue;
                }
                let command = format!(
                    "\\{loader}{}{}{{sample.rs}}",
                    if star { "*" } else { "" },
                    if loader == "verbatiminput" {
                        ""
                    } else {
                        "[firstline=1]"
                    }
                );
                for (body, active) in [
                    (command.clone(), true),
                    (format!("\\hold{{ignored}}{command}"), true),
                    (format!("\\take ignoredSTOP{command}"), true),
                    (format!("\\newcommand{{\\unused}}{{{command}}}"), false),
                    (format!("\\hold{{{command}}}"), false),
                    (format!("\\hold{command}"), false),
                    (format!("\\take {command}STOP"), false),
                    (format!("\\string{command}"), false),
                    (format!("\\detokenize{{{command}}}"), false),
                    (format!("\\endinput\n{command}"), false),
                ] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path();
                    let main = root.join("main.tex");
                    std::fs::write(&main, format!("\\documentclass{{article}}\\newcommand{{\\hold}}[1]{{}}\\def\\take#1STOP{{}}{}{body}", if matches!(loader, "VerbatimInput" | "LVerbatimInput" | "BVerbatimInput") { r"\fvset{numbers=none}" } else { r"\lstset{columns=fullflexible}" })).unwrap();
                    let uri = Url::from_file_path(&main).unwrap();
                    let mut workspace = Workspace::default();
                    workspace.add_root(root.into());
                    let snapshot = Snapshot::capture(&workspace, &uri, &BuildConfig::default());
                    assert!(snapshot.changed_path().is_none());
                    std::fs::write(root.join("sample.rs"), "CodeUnique\n").unwrap();
                    assert_eq!(snapshot.changed_path().is_some(), active, "{body}");
                }
            }
        }
    }
}

#[cfg(test)]
mod quoted_file_conditions {
    use super::*;

    const COMMANDS: [(&str, &str); 9] = [
        ("usepackage", "sty"),
        ("RequirePackage", "sty"),
        ("RequirePackageWithOptions", "sty"),
        ("documentclass", "cls"),
        ("LoadClass", "cls"),
        ("LoadClassWithOptions", "cls"),
        ("bibliography", "bib"),
        ("nobibliography", "bib"),
        ("bibliographystyle", "bst"),
    ];

    fn capture(root: &Path, source: &str) -> Snapshot {
        std::fs::write(root.join("main.tex"), source).unwrap();
        let mut workspace = Workspace::default();
        workspace.add_root(root.into());
        Snapshot::capture(
            &workspace,
            &Url::from_file_path(root.join("main.tex")).unwrap(),
            &BuildConfig::default(),
        )
    }

    #[test]
    fn quoted_bibliography_and_loader_tokens_do_not_watch_unread_candidates() {
        for (name, extension) in COMMANDS {
            for argument in [
                "{refs}",
                "% before 😀\r\n{refs}% after\r\n",
                "{refs% filename 😀\r\n}",
                "{% before 😀\r\nrefs% filename 😀\r\n}",
            ] {
                let command = format!("\\{name}{argument}");
                for body in [
                    format!("\\meaning{command}"),
                    format!("\\string{command}"),
                    format!("\\detokenize{{{command}}}"),
                    format!("\\csuse{{meaning}}{command}"),
                    format!("\\csname meaning\\endcsname{command}"),
                    format!("\\newcommand{{\\future}}{{\\meaning{command}}}\\future"),
                ] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path();
                    let snapshot = capture(root, &body);
                    let actual = root.join(format!("refs.{extension}"));
                    assert!(snapshot.changed_path().is_none(), "{body}: {snapshot:?}");
                    assert!(
                        !snapshot
                            .watched()
                            .contains(&Url::from_file_path(&actual).unwrap()),
                        "{body}: {snapshot:?}"
                    );
                    std::fs::write(actual, "Unread input.\n").unwrap();
                    assert!(snapshot.changed_path().is_none(), "{body}: {snapshot:?}");
                }
            }
        }
    }

    #[test]
    fn real_direct_deferred_forwarded_and_conservatively_unknown_calls_keep_conditions() {
        for (name, extension) in COMMANDS {
            let command = format!("\\{name}{{refs}}");
            for body in [
                command.clone(),
                format!("\\newcommand{{\\future}}{{{command}}}\\future"),
                format!("\\newcommand{{\\execute}}[1]{{#1}}\\execute{{{command}}}"),
                format!("\\unknown{{{command}}}"),
                format!("\\newcommand{{\\future}}{{{command}}}"),
                format!("\\meaning{command}{command}"),
                format!("\\string{command}{command}"),
                format!("\\detokenize{{{command}}}{command}"),
                format!("\\meaning{{}}{command}"),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let snapshot = capture(root, &body);
                let actual = root.join(format!("refs.{extension}"));
                assert!(snapshot.changed_path().is_none(), "{body}");
                assert!(
                    snapshot
                        .watched()
                        .contains(&Url::from_file_path(&actual).unwrap()),
                    "{body}: {snapshot:?}"
                );
                std::fs::write(actual, "New input.\n").unwrap();
                assert!(snapshot.changed_path().is_some(), "{body}: {snapshot:?}");
            }
        }
    }

    #[test]
    fn uncertain_primitive_meanings_and_dynamic_sources_preserve_potential_conditions() {
        for (name, extension) in COMMANDS {
            let command = format!("\\meaning\\{name}{{refs}}");
            for body in [
                format!("\\def\\meaning#1{{#1}}{command}"),
                format!("\\newcommand{{\\install}}{{\\def\\meaning##1{{##1}}}}\\install{command}"),
                format!("\\newcommand{{\\execute}}[1]{{#1}}\\let\\meaning\\execute{command}"),
                format!("\\def\\picked{{provider}}\\input{{\\picked}}{command}"),
                format!("\\makeatletter\\@input{{provider.tex}}\\makeatother{command}"),
                format!("\\csname @input\\endcsname{{provider.tex}}{command}"),
                format!("\\def\\readername{{@input}}\\csname\\readername\\endcsname{{provider.tex}}{command}"),
                format!("\\newcommand{{\\execute}}[1]{{#1}}\\execute{{\\input{{provider.tex}}}}{command}"),
                format!("\\newcommand{{\\future}}{{\\input{{provider.tex}}}}\\future{command}"),
                format!("\\scantokens{{\\input{{provider.tex}}}}{command}"),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                let snapshot = capture(root, &body);
                std::fs::write(root.join(format!("refs.{extension}")), "Potential input.\n")
                    .unwrap();
                assert!(snapshot.changed_path().is_some(), "{body}: {snapshot:?}");
            }
        }
    }

    #[test]
    fn cross_file_primitive_rebindings_keep_the_lookup_in_its_project_context() {
        for (name, extension) in COMMANDS {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::write(root.join("settings.tex"), r"\def\meaning#1{#1}").unwrap();
            let source = format!("\\input{{settings}}\\meaning\\{name}{{refs}}");
            let snapshot = capture(root, &source);
            std::fs::write(root.join(format!("refs.{extension}")), "Potential input.\n").unwrap();
            assert!(snapshot.changed_path().is_some(), "{source}: {snapshot:?}");
        }
    }
}

#[cfg(test)]
mod generated_bibliography_tests {
    use super::*;

    #[test]
    fn verified_database_creation_preserves_other_file_conditions_and_changed_contents() {
        let directory = tempfile::tempdir().unwrap();
        let generated = directory.path().join("generated.bib");
        let unrelated = directory.path().join("other.tex");
        let before = Snapshot {
            paths: [(generated.clone(), None), (unrelated.clone(), None)].into(),
            ..Default::default()
        };
        std::fs::write(&generated, "database").unwrap();
        let watched = crate::compilation::build_tools::Watched::capture(&generated)
            .unwrap_or_else(|_| panic!("capture database"));
        let updated = before.with_generated_bibliography(std::slice::from_ref(&watched));
        assert!(updated.changed_path().is_none());
        std::fs::write(&unrelated, "new source").unwrap();
        assert_eq!(updated.changed_path(), Some(unrelated.as_path()));
        std::fs::remove_file(&unrelated).unwrap();
        std::fs::write(&generated, "modified").unwrap();
        assert_eq!(
            before
                .with_generated_bibliography(&[watched])
                .changed_path(),
            Some(generated.as_path())
        );
    }

    #[test]
    #[cfg(unix)]
    fn generated_database_evidence_does_not_rebind_existing_source_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("old.bib");
        let generated = directory.path().join("generated.bib");
        let alias = directory.path().join("alias.bib");
        std::fs::write(&source, "old").unwrap();
        std::fs::write(&generated, "new").unwrap();
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        let before = Snapshot {
            paths: [(alias.clone(), Some(source))].into(),
            ..Default::default()
        };
        let watched = crate::compilation::build_tools::Watched::capture(&generated)
            .unwrap_or_else(|_| panic!("capture database"));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&generated, &alias).unwrap();
        assert_eq!(
            before
                .with_generated_bibliography(&[watched])
                .changed_path(),
            Some(alias.as_path())
        );
    }
}
