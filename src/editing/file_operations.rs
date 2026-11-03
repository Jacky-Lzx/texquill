use crate::rpc::{Error, Result};
use crate::{
    language::{features, navigation},
    parser::syntax,
    project::{
        graphics,
        workspace::{
            normalize_path, resolve_file_path, resolve_file_path_with, resolve_path, Workspace,
        },
    },
};
use lsp_types::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub struct Renames(Vec<(PathBuf, PathBuf)>);

/// Filesystem notifications can name a directory without listing its children.
pub struct FileChanges(Vec<PathBuf>);

impl FileChanges {
    pub fn new<'a>(uris: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        let mut paths = BTreeSet::new();
        for uri in uris {
            let path = Url::parse(uri)
                .ok()
                .and_then(|uri| uri.to_file_path().ok())
                .ok_or_else(|| Error::invalid_params("File events require local file URIs"))?;
            paths.insert(normalize_path(&path));
            // Preserve the logical path for retarget/deletion checks, and
            // match physical inputs when editors report a directory alias.
            if let Some(physical) = path.ancestors().find_map(|parent| {
                parent
                    .canonicalize()
                    .ok()
                    .map(|physical| physical.join(path.strip_prefix(parent).unwrap()))
            }) {
                paths.insert(normalize_path(&physical));
            }
        }
        Ok(Self(paths.into_iter().collect()))
    }

    /// Only passive notifications for already known regular source files can
    /// be redundant. Directories, aliases, conditions and config need discovery.
    pub(crate) fn unchanged_sources(&self, workspace: &Workspace) -> Option<BTreeSet<Url>> {
        self.0
            .iter()
            .map(|path| {
                if !crate::project::workspace::is_source(path)
                    || !std::fs::symlink_metadata(path).ok()?.file_type().is_file()
                {
                    return None;
                }
                let uri = Url::from_file_path(path).ok()?;
                let doc = workspace.documents.get(&uri)?;
                let source = std::fs::read_to_string(path).ok()?;
                (source == doc.text.source).then_some(uri)
            })
            .collect()
    }

    pub(crate) fn changes_configuration(&self) -> bool {
        self.0.iter().any(|path| {
            path.file_name()
                .is_some_and(|name| name == ".texquill.json")
        })
    }

    pub fn affects(&self, uri: &Url) -> bool {
        uri.to_file_path()
            .is_ok_and(|path| self.0.iter().any(|changed| path.starts_with(changed)))
    }

    pub fn refresh(&self, workspace: &mut Workspace) -> BTreeSet<Url> {
        self.refresh_inner(workspace, false)
    }

    pub(crate) fn refresh_watched(&self, workspace: &mut Workspace) -> BTreeSet<Url> {
        self.refresh_inner(workspace, true)
    }

    fn refresh_inner(&self, workspace: &mut Workspace, verify_inputs: bool) -> BTreeSet<Url> {
        let mut affected = if verify_inputs {
            self.unchanged_sources(workspace)
        } else {
            None
        }
        .unwrap_or_else(|| workspace.refresh_paths(&self.0));
        // A new file may satisfy an earlier missing input. A deleted file may
        // only remain in the previous compiler's source/recorder information.
        let invalidated: Vec<_> = workspace
            .project
            .build_diagnostics
            .iter()
            .filter(|(root, diagnostics)| {
                let mut checked = BTreeMap::new();
                let mut changed = |uri: &Url| {
                    self.affects(uri)
                        && (!verify_inputs
                            || *checked.entry(uri.clone()).or_insert_with(|| {
                                workspace
                                    .project
                                    .build_checks
                                    .get(*root)
                                    .is_none_or(|checks| checks.input_changed(uri))
                            }))
                };
                changed(root)
                    || workspace
                        .project
                        .build_checks
                        .get(*root)
                        .is_some_and(|checks| {
                            checks.watched().iter().any(|uri| self.affects(uri))
                                && checks.changed_path().is_some()
                        })
                    || diagnostics.keys().any(&mut changed)
                    || workspace.project_files(root).iter().any(&mut changed)
                    || workspace
                        .project_bindings(root)
                        .filter_map(|(_, target)| Url::from_file_path(target).ok())
                        .any(|uri| changed(&uri))
                    || [
                        &workspace.project.recorded_inputs,
                        &workspace.project.bibliography_inputs,
                        &workspace.project.build_diagnostic_sources,
                    ]
                    .iter()
                    .any(|inputs| {
                        inputs
                            .get(*root)
                            .is_some_and(|uris| uris.iter().any(&mut changed))
                    })
            })
            .map(|(root, _)| root.clone())
            .collect();
        for root in invalidated {
            if let Some(diagnostics) = workspace.project.build_diagnostics.remove(&root) {
                affected.extend(diagnostics.into_keys());
            }
            workspace.project.build_diagnostic_sources.remove(&root);
        }
        // Keep recorder inputs until the next build: they let artifact checks
        // continue to reject PDFs whose inputs have disappeared.
        affected
    }
}

impl Renames {
    pub fn new(files: &[FileRename]) -> Result<Self> {
        let mut paths = BTreeMap::new();
        let mut destinations = BTreeSet::new();
        for file in files {
            let local = |uri: &str| {
                Url::parse(uri)
                    .ok()
                    .and_then(|uri| uri.to_file_path().ok())
                    .ok_or_else(|| Error::invalid_params("File renames require local file URIs"))
            };
            let old = local(&file.old_uri)?;
            let old = old.canonicalize().unwrap_or_else(|_| normalize_path(&old));
            let new = normalize_path(&local(&file.new_uri)?);
            if paths.insert(old, new.clone()).is_some() || !destinations.insert(new) {
                return Err(Error::invalid_params(
                    "Ambiguous file rename: duplicate source or destination",
                ));
            }
        }
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort_by_key(|(old, _)| std::cmp::Reverse(old.components().count()));
        Ok(Self(paths))
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        for (old, new) in &self.0 {
            if let Ok(tail) = path.strip_prefix(old) {
                return if tail.as_os_str().is_empty() {
                    new.clone()
                } else {
                    new.join(tail)
                };
            }
        }
        path.into()
    }

    pub fn uri(&self, uri: &Url) -> Url {
        uri.to_file_path()
            .ok()
            .and_then(|path| Url::from_file_path(self.path(&path)).ok())
            .unwrap_or_else(|| uri.clone())
    }

    /// Resolve a file at its proposed location, including directory moves and
    /// nested overrides. Paths moved away no longer exist in this view.
    fn file(&self, path: &Path) -> Option<PathBuf> {
        let path = normalize_path(path);
        for (old, new) in &self.0 {
            let Ok(tail) = path.strip_prefix(new) else {
                continue;
            };
            let original = if tail.as_os_str().is_empty() {
                old.clone()
            } else {
                old.join(tail)
            };
            if self.path(&original) == path && original.is_file() {
                return Some(path);
            }
        }
        if self.path(&path) != path {
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        (canonical.is_file() && self.path(&canonical) == canonical).then_some(canonical)
    }

    fn image(
        &self,
        root: &Url,
        name: &str,
        extensions: &[&str],
        directories: &[PathBuf],
    ) -> Option<Url> {
        let directories: Vec<_> = directories.iter().map(|path| self.path(path)).collect();
        graphics::resolve_with(&self.uri(root), name, extensions, &directories, |path| {
            self.file(path)
        })
    }

    fn source(&self, uri: &Url, root: &Url, command: &str, name: &str) -> Option<Url> {
        let extensions = syntax::inferred_file_extensions(command)?;
        [root, uri]
            .into_iter()
            .filter(|origin| !syntax::code_input_command(command) || *origin == root)
            .find_map(|origin| {
                resolve_file_path_with(&self.uri(origin), command, name, extensions, |path| {
                    self.file(path)
                })
            })
    }

    pub fn affects(&self, uri: &Url) -> bool {
        self.uri(uri) != *uri
    }
}

fn relative_path(parent: &Path, target: &Path) -> PathBuf {
    let from: Vec<_> = parent.components().collect();
    let to: Vec<_> = target.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return target.into();
    }
    let mut result = PathBuf::new();
    for _ in common..from.len() {
        result.push("..");
    }
    for component in &to[common..] {
        result.push(component.as_os_str());
    }
    result
}

fn replacement(
    renames: &Renames,
    uri: &Url,
    target: &Url,
    original: &str,
    graphics: &[PathBuf],
    root: &Url,
    command: Option<(&str, &[&str])>,
) -> Result<String> {
    let (command, inferred) = command.map_or((None, &["tex"][..]), |(name, extensions)| {
        (Some(name), extensions)
    });
    let quoted_image = command == Some("includegraphics") && original.contains('"');
    let decoded = if command == Some("includegraphics") {
        Some(
            graphics::unquote(original)
                .ok_or_else(|| Error::invalid_params("Unbalanced image filename quotes"))?,
        )
    } else {
        None
    };
    let original = decoded.as_deref().unwrap_or(original);
    let target_path = target
        .to_file_path()
        .map_err(|_| Error::invalid_params("Invalid linked file URI"))?;
    let new_target = renames.path(&target_path);
    if original.starts_with("file:") {
        // Use the source suffix: four-argument hyperref links add a destination
        // to the navigation target that is not part of this editable argument.
        // Retain TeX escapes and empty query/fragment delimiters verbatim too.
        let suffix = original.find(['?', '#']).map_or("", |mut start| {
            if start > 0 && original.as_bytes()[start - 1] == b'\\' {
                start -= 1;
            }
            &original[start..]
        });
        return Url::from_file_path(new_target)
            .map(|uri| format!("{uri}{suffix}"))
            .map_err(|_| Error::invalid_params("Invalid renamed file URI"));
    }
    let extension = target_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    let resolve = |origin| match command {
        Some(command) => resolve_file_path(origin, command, original, inferred),
        None => resolve_path(origin, original, extension),
    };
    let origin = if command == Some("includegraphics") || resolve(root).as_ref() == Some(target) {
        root.clone()
    } else if resolve(uri).as_ref() == Some(target) {
        uri.clone()
    } else {
        root.clone()
    };
    let graphic_origin = if resolve(&origin).as_ref() != Some(target) {
        graphics.iter().find(|directory| {
            graphics::resolve(directory, original, extension).as_ref() == Some(target)
        })
    } else {
        None
    };
    let origin = renames
        .uri(&origin)
        .to_file_path()
        .map_err(|_| Error::invalid_params("Invalid document URI"))?;
    let path = if Path::new(original).is_absolute() {
        new_target.clone()
    } else if let Some(directory) = graphic_origin
        .map(|directory| renames.path(directory))
        .filter(|directory| new_target.starts_with(directory))
    {
        relative_path(&directory, &new_target)
    } else {
        relative_path(
            origin
                .parent()
                .ok_or_else(|| Error::invalid_params("Invalid document path"))?,
            &new_target,
        )
    };
    let mut value = reference_path(&path, command)?;
    if (original.starts_with("./") && !value.starts_with('.')) || value.starts_with(' ') {
        // A leading filename space would be discarded by LaTeX. Keep it
        // inside the path, where one literal space retains its identity.
        value.insert_str(0, "./");
    }
    if command.and_then(syntax::package_class_extension).is_some()
        || !original.ends_with(&format!(".{extension}"))
    {
        if let Some(extension) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .filter(|extension| inferred.contains(extension))
        {
            let end = value.len() - extension.len() - 1;
            // A trailing stem space can be read by TeX before its inferred
            // suffix, but our static filename spans trim it. Keep the explicit
            // suffix so navigation and subsequent renames retain the target.
            if command.and_then(syntax::package_class_extension).is_some()
                || syntax::is_literal_file_path(&value[..end])
            {
                value.truncate(end);
            }
        }
    }
    if let Some(command) = command {
        let target_after = Url::from_file_path(&new_target)
            .map_err(|_| Error::invalid_params("Invalid renamed file URI"))?;
        let resolves_to_target = |value: &str| {
            if command != "includegraphics"
                && !crate::bibliography::bib_resources::references_file(command)
                && !syntax::is_literal_file_path(value)
            {
                return false;
            }
            let selected = if command == "includegraphics" {
                renames.image(root, value, inferred, graphics)
            } else {
                renames.source(uri, root, command, value)
            };
            selected.as_ref() == Some(&target_after)
        };
        if !resolves_to_target(&value) {
            let explicit = if Path::new(original).is_absolute() {
                new_target
            } else {
                let root_path = renames
                    .uri(root)
                    .to_file_path()
                    .map_err(|_| Error::invalid_params("Invalid compilation root URI"))?;
                relative_path(
                    root_path
                        .parent()
                        .ok_or_else(|| Error::invalid_params("Invalid compilation root path"))?,
                    &new_target,
                )
            };
            value = reference_path(&explicit, Some(command))?;
            if let Some(extension) = syntax::package_class_extension(command) {
                if let Some(base) = value.strip_suffix(&format!(".{extension}")) {
                    value = base.to_owned();
                }
            }
            if (original.starts_with("./") && !value.starts_with('.')) || value.starts_with(' ') {
                value.insert_str(0, "./");
            }
            if !resolves_to_target(&value) {
                return Err(Error::invalid_params(
                    "Renamed file reference would select a different file",
                ));
            }
        }
    }
    // A comma in a list argument changes one filename into multiple items.
    // Resolving the raw spelling as one filesystem path would miss that
    // change in TeX semantics. Single-file commands keep literal commas.
    if command.is_some_and(syntax::file_list) && value.contains(',') {
        return Err(Error::invalid_params(
            "Renamed path contains a comma that would split the file list",
        ));
    }
    if quoted_image {
        value = format!("\"{value}\"");
    }
    Ok(value)
}

fn reference_path(path: &Path, command: Option<&str>) -> Result<String> {
    if !command.is_some_and(crate::bibliography::bib_resources::references_file) {
        return safe_path(path);
    }
    let value = path
        .to_str()
        .ok_or_else(|| Error::invalid_params("File name is not valid UTF-8"))?;
    #[cfg(windows)]
    let normalized = value.replace('\\', "/");
    #[cfg(windows)]
    let value = normalized.as_str();
    if !crate::bibliography::bib_resources::literal_path(value) {
        return Err(Error::invalid_params(
            "Renamed biblatex resource is not a literal filename",
        ));
    }
    Ok(value.to_string())
}

fn safe_path(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::invalid_params("File name is not valid UTF-8"))?
        .to_owned();
    #[cfg(windows)]
    let value = value.replace('\\', "/");
    if value.chars().any(char::is_control)
        || value.contains([
            '\\', '%', '#', '&', '$', '{', '}', '^', '~', '"', '\r', '\n',
        ])
    {
        return Err(Error::invalid_params("Renamed path contains characters that cannot be represented safely in a LaTeX file reference"));
    }
    Ok(value)
}

fn directory_replacement(
    renames: &Renames,
    directory: &graphics::Directory,
    original: &str,
) -> Result<String> {
    if renames.path(&directory.path) == directory.path && !renames.affects(&directory.origin) {
        return Ok(original.into());
    }
    let decoded = graphics::unquote(original)
        .ok_or_else(|| Error::invalid_params("Unbalanced graphics directory quotes"))?;
    let origin = renames
        .uri(&directory.origin)
        .to_file_path()
        .map_err(|_| Error::invalid_params("Invalid graphics search directory origin"))?;
    let target = renames.path(&directory.path);
    let path = if Path::new(decoded.as_ref()).is_absolute() {
        target
    } else {
        relative_path(
            origin
                .parent()
                .ok_or_else(|| Error::invalid_params("Invalid root path"))?,
            &target,
        )
    };
    let mut new_text = safe_path(&path)?;
    if new_text.is_empty() {
        new_text.push('.');
    } else if decoded.starts_with("./") && !new_text.starts_with('.') {
        new_text.insert_str(0, "./");
    }
    if !new_text.ends_with('/') {
        new_text.push('/');
    }
    if graphics::unquote(&new_text).as_deref() != Some(new_text.as_str()) {
        return Err(Error::invalid_params(
            "Renamed graphics directory would select a different path after TeX whitespace normalization",
        ));
    }
    Ok(graphics::quoted_replacement(new_text, original, false))
}

/// Edits target the old URIs: the client applies them before moving the files.
pub fn will_rename(workspace: &Workspace, files: &[FileRename]) -> Result<Option<WorkspaceEdit>> {
    if files.is_empty() {
        return Ok(None);
    }
    let renames = Renames::new(files)?;
    let mut changes = BTreeMap::new();
    let regions = workspace.saved_projects();
    for (uri, doc) in &workspace.documents {
        let roots = workspace.source_roots_in(uri, &regions);
        let mut directories = BTreeMap::new();
        for root in &roots {
            for directory in graphics::declarations_in(workspace, uri, root) {
                let original = &doc.text.source[directory.span.clone()];
                let new_text = directory_replacement(&renames, &directory, original)?;
                let span = (directory.span.start, directory.span.end);
                if directories
                    .get(&span)
                    .is_some_and(|value| *value != new_text)
                {
                    return Err(Error::invalid_params("Shared graphics search path would require different replacements in different projects"));
                }
                directories.insert(span, new_text);
            }
        }
        for ((start, end), new_text) in directories {
            if new_text != doc.text.source[start..end] {
                changes
                    .entry(uri.clone())
                    .or_insert_with(Vec::new)
                    .push(TextEdit {
                        range: doc.text.range(start, end),
                        new_text,
                    });
            }
        }
        let mut links = BTreeMap::<(usize, usize), (String, usize)>::new();
        let mut indirect_inputs = Vec::new();
        for root in &roots {
            let mut inputs = crate::project::source_inputs::Index::new(workspace, root);
            let calls = inputs.calls.remove(uri).unwrap_or_default();
            let commands: Vec<_> = calls
                .into_iter()
                .chain(inputs.loaders.remove(uri).unwrap_or_default())
                .chain(inputs.forwarders.remove(uri).unwrap_or_default())
                .chain(inputs.resource_references.remove(uri).unwrap_or_default())
                .chain(inputs.selectors.remove(uri).unwrap_or_default())
                .chain(inputs.checks.remove(uri).unwrap_or_default())
                .chain(
                    graphics::calls(workspace, root)
                        .remove(uri)
                        .unwrap_or_default(),
                )
                .filter(|call| call.certain)
                .map(|call| call.command)
                .collect();
            let image_directories =
                crate::project::graphics_settings::Index::directories(workspace, root);
            let image_extensions = crate::project::graphics_settings::Index::new(workspace, root);
            for link in navigation::document_links_in(workspace, uri, root) {
                let Some(target) = link.target.filter(|target| target.scheme() == "file") else {
                    continue;
                };
                let (Some(start), Some(end)) = (
                    doc.text.offset(link.range.start),
                    doc.text.offset(link.range.end),
                ) else {
                    continue;
                };
                let original = &doc.text.source[start..end];
                let target_changed = target
                    .to_file_path()
                    .ok()
                    .is_some_and(|path| renames.path(&path) != path);
                let document_changed = renames.affects(uri);
                let root_changed = renames.affects(root);
                if crate::project::imports::is_link(workspace, uri, root, link.range) {
                    if target_changed || document_changed || root_changed {
                        return Err(Error::invalid_params("Moving import-relative sources requires updating their import directory explicitly"));
                    }
                    continue;
                }
                let command = doc.syntax.commands.iter().chain(&commands).find(|command| {
                    syntax::file_extensions(&command.name).is_some()
                        && syntax::file_argument(&doc.text.source, command).is_some_and(
                            |argument| argument.span.start <= start && argument.span.end >= end,
                        )
                });
                // Distribution loaders keep their spelling when the project
                // moves: kpsewhich's search path is independent of its cwd.
                // Revalidate if the move introduces a local shadow instead.
                if !target_changed
                    && command.is_some_and(|command| {
                        workspace
                            .distribution
                            .resolve(&command.name, original)
                            .as_ref()
                            == Some(&target)
                            && renames.source(uri, root, &command.name, original).is_none()
                    })
                {
                    continue;
                }
                let graphics = command
                    .filter(|command| command.name == "includegraphics")
                    .and_then(|command| {
                        image_directories.paths_at(root, uri, command.span.start, true)
                    })
                    .unwrap_or_default();
                let inferred = if let Some(command) = command {
                    if command.name == "includegraphics" {
                        image_extensions
                            .at(uri, command.span.start)
                            .unwrap_or_default()
                    } else {
                        syntax::inferred_file_extensions(&command.name)
                            .unwrap_or_default()
                            .to_vec()
                    }
                } else {
                    vec!["tex"]
                };
                // An unrelated file can arrive ahead of this target in the
                // search order even when neither source nor target moves.
                let reference_redirected = command.is_some_and(|command| {
                    let selected = match command.name.as_str() {
                        "includegraphics" => renames.image(root, original, &inferred, &graphics),
                        name => renames.source(uri, root, name, original),
                    };
                    selected != Some(renames.uri(&target))
                });
                // Retain spelling in contexts that do not require a change too:
                // every consumer of a shared parameter must agree on its edit.
                let needs_update =
                    target_changed || document_changed || root_changed || reference_redirected;
                let new_text = if needs_update {
                    let required_extension = command.and_then(|command| {
                        let extension = match command.name.as_str() {
                            "include" | "includeonly" => "tex",
                            "bibliographystyle" => "bst",
                            "bibliography" | "nobibliography" => "bib",
                            name => syntax::package_class_extension(name)?,
                        };
                        Some((command, extension))
                    });
                    if let Some((command, extension)) =
                        required_extension.filter(|_| target_changed)
                    {
                        let renamed = renames
                            .uri(&target)
                            .to_file_path()
                            .map_err(|_| Error::invalid_params("Invalid renamed file URI"))?;
                        if target
                            .to_file_path()
                            .ok()
                            .as_ref()
                            .and_then(|path| path.extension())
                            != renamed.extension()
                            && renamed.extension().and_then(|extension| extension.to_str())
                                != Some(extension)
                        {
                            return Err(Error::invalid_params(format!(
                                "File loaded by \\{} must retain the .{extension} extension",
                                command.name
                            )));
                        }
                    }
                    replacement(
                        &renames,
                        uri,
                        &target,
                        original,
                        if command.is_some_and(|command| command.name == "includegraphics") {
                            &graphics
                        } else {
                            &[]
                        },
                        if link.tooltip.as_deref() == Some("TeX root document") {
                            uri
                        } else {
                            root
                        },
                        command.map(|command| (command.name.as_str(), inferred.as_slice())),
                    )?
                } else {
                    original.into()
                };
                if new_text != original
                    && command.is_some_and(|command| {
                        crate::bibliography::bib_resources::references_file(&command.name)
                            && !crate::bibliography::bib_resources::insertion_preserves_filename(
                                workspace,
                                uri,
                                std::slice::from_ref(root),
                                Some(command.span.start),
                                &new_text,
                            )
                    })
                {
                    return Err(Error::invalid_params("Renamed resource filename would change meaning in a precollected macro argument"));
                }
                if new_text != original
                    && command.is_some_and(|command| {
                        crate::bibliography::bib_section_files::file_command(&command.name)
                    })
                    && (!crate::bibliography::bib_section_files::editable_item(&new_text)
                        || inputs.section_labels.contains(&new_text))
                {
                    return Err(Error::invalid_params("Renamed section resource would change list syntax or select a resource label instead of its file"));
                }
                if let Some((previous, count)) = links.get_mut(&(start, end)) {
                    if *previous != new_text {
                        return Err(Error::invalid_params("Shared file reference would require different replacements in different projects"));
                    }
                    *count += 1;
                } else {
                    links.insert((start, end), (new_text, 1));
                }
            }
            indirect_inputs.extend(commands);
        }
        for ((start, end), (new_text, count)) in links {
            if new_text != doc.text.source[start..end] {
                if count != roots.len() {
                    return Err(Error::invalid_params(
                        "Shared file reference cannot be updated consistently across build roots",
                    ));
                }
                let input = doc
                    .syntax
                    .commands
                    .iter()
                    .chain(&indirect_inputs)
                    .filter(|command| command.name == "input")
                    .filter_map(|command| command.argument.as_ref())
                    .find(|argument| {
                        argument.span.start <= start
                            && end <= argument.span.end
                            && syntax::input_filename_needs_braces(
                                &doc.text.source,
                                argument,
                                &new_text,
                            )
                    });
                let fancy = doc.syntax.commands.iter().find(|command| {
                    crate::project::code_inputs::wrapped_filename(&command.name)
                        && syntax::file_argument(&doc.text.source, command).is_some_and(
                            |argument| argument.span.start == start && argument.span.end == end,
                        )
                });
                let edit = if let Some(command) = fancy {
                    if command.name == "inputminted"
                        && new_text.chars().any(char::is_whitespace)
                        && roots.iter().any(|root| {
                            !crate::project::code_inputs::allows_spaces(workspace, uri, root)
                        })
                    {
                        return Err(Error::invalid_params(
                            "Minted verbatim/draft mode requires a filename without spaces",
                        ));
                    }
                    crate::project::code_inputs::filename_edit(&doc.text, command, new_text)
                        .ok_or_else(|| {
                            Error::invalid_params("Cannot safely update code filename")
                        })?
                } else if let Some(argument) = input {
                    syntax::input_filename_edit(&doc.text, argument, new_text)
                } else {
                    TextEdit {
                        range: doc.text.range(start, end),
                        new_text,
                    }
                };
                changes
                    .entry(uri.clone())
                    .or_insert_with(Vec::new)
                    .push(edit);
            }
        }
    }
    Ok((!changes.is_empty()).then(|| features::workspace_edit(workspace, changes)))
}

pub fn did_rename(workspace: &mut Workspace, files: &[FileRename]) -> Result<BTreeSet<Url>> {
    if files.is_empty() {
        return Ok(BTreeSet::new());
    }
    let renames = Renames::new(files)?;
    let mut affected = BTreeSet::new();
    for project in workspace.project.build_diagnostics.values() {
        affected.extend(project.keys().cloned());
    }
    workspace.project.build_diagnostics.clear();
    workspace.project.build_diagnostic_sources.clear();
    workspace.project.build_checks.clear();
    // Preserve historical condition paths so moving a checked file invalidates
    // its old PDF even after the source reference is updated.
    workspace.project.pdf_checks = std::mem::take(&mut workspace.project.pdf_checks)
        .into_iter()
        .map(|(root, checks)| (renames.uri(&root), checks))
        .collect();
    workspace.project.bibliography_checks =
        std::mem::take(&mut workspace.project.bibliography_checks)
            .into_iter()
            .map(|(root, checks)| {
                (
                    renames.uri(&root),
                    std::sync::Arc::new(checks.renamed_inputs(|path| renames.path(path))),
                )
            })
            .collect();
    for inputs_by_root in [
        &mut workspace.project.recorded_inputs,
        &mut workspace.project.recorded_binary_inputs,
        &mut workspace.project.bibliography_inputs,
        &mut workspace.project.bibliography_databases,
    ] {
        *inputs_by_root = std::mem::take(inputs_by_root)
            .into_iter()
            .map(|(root, inputs)| {
                (
                    renames.uri(&root),
                    inputs.iter().map(|uri| renames.uri(uri)).collect(),
                )
            })
            .collect();
    }
    for bindings_by_root in [
        &mut workspace.project.recorded_bindings,
        &mut workspace.project.bibliography_bindings,
    ] {
        *bindings_by_root = std::mem::take(bindings_by_root)
            .into_iter()
            .map(|(root, bindings)| {
                (
                    renames.uri(&root),
                    bindings
                        .into_iter()
                        .map(|(path, target)| (renames.path(&path), renames.path(&target)))
                        .collect(),
                )
            })
            .collect();
    }
    workspace.roots = workspace
        .roots
        .iter()
        .map(|root| renames.path(root))
        .collect();
    let mut moved = Vec::new();
    for (uri, doc) in std::mem::take(&mut workspace.documents) {
        let new_uri = renames.uri(&uri);
        if new_uri != uri {
            affected.insert(uri);
            affected.insert(new_uri.clone());
            moved.push((new_uri.clone(), doc.relocated(&new_uri)));
        } else {
            workspace.documents.insert(uri, doc);
        }
    }
    workspace.documents.extend(moved);
    // Closed documents may also contain edits returned by willRenameFiles.
    let documents: Vec<_> = workspace.documents.keys().cloned().collect();
    for uri in documents {
        workspace.reload(&uri);
    }
    Ok(affected)
}
