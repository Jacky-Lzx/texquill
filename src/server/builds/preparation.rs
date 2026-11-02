//! Prepare compiler inputs on an isolated workspace, without blocking editor locks.
use super::*;
use crate::project::workspace::Workspace;

pub(super) fn prepare(
    mut workspace: Workspace,
    original_documents: crate::project::documents::Documents,
    root: Url,
    rebuild: bool,
    cancel: &AtomicBool,
) -> Result<BuildInput> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::request_cancelled());
    }
    workspace.refresh_pending_dependencies();
    workspace.refresh_project(&root);
    let path = root
        .to_file_path()
        .map_err(|_| Error::invalid_params("Compilation requires a local file"))?;
    let mut config = workspace.build_config(&root).for_build_mode(&path, rebuild);
    if config.resolution.native || !config.resolution.issues.is_empty() {
        config = config
            .unresolved()
            .resolve_for_build(&path, cancel)
            .map_err(Error::invalid_params)?;
    }
    config
        .validate_resolution()
        .map_err(Error::invalid_params)?;
    let static_code_inputs = crate::project::code_inputs::inputs(&workspace, &root);
    let mut project = workspace.project_files(&root);
    // Known opened paths remain build dependencies even when their current
    // targets are outside the source index. Capture the actual target now,
    // rather than the one observed by the previous compiler invocation.
    project.extend(
        workspace
            .project_bindings(&root)
            .filter_map(|(path, _)| path.canonicalize().ok())
            .filter(|path| path.is_file())
            .filter_map(|path| Url::from_file_path(path).ok()),
    );
    if let Ok(path) = root.to_file_path() {
        project.extend(
            crate::compilation::index_processing::dependencies(&path, &config)
                .into_iter()
                .chain(crate::compilation::bibliography_processing::dependencies(
                    &path, &config,
                ))
                .flat_map(|path| {
                    let physical = path.canonicalize().ok();
                    [Some(path), physical].into_iter().flatten()
                })
                .filter_map(|path| Url::from_file_path(path).ok()),
        );
    }
    let open_aliases = workspace.open_file_aliases(&project);
    project.extend(open_aliases.keys().cloned());
    let mut snapshots = BTreeMap::new();
    let mut file_snapshots = BTreeMap::new();
    for uri in &project {
        if let Some(doc) = workspace.documents.get(uri) {
            let saved = uri
                .to_file_path()
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok());
            if !saved
                .as_deref()
                .is_some_and(|saved| doc.text.matches_saved(saved))
            {
                return Err(Error::invalid_params(format!(
                    "Save {} before compiling",
                    uri.path()
                )));
            }
            snapshots.insert(uri.clone(), (doc.version, doc.text.source.clone()));
            if let Ok(path) = uri.to_file_path() {
                // Retain the exact bytes TeX reads separately from the
                // editor's newline representation and document version.
                file_snapshots.insert(uri.clone(), InputSnapshot::file(&path));
            }
        } else if let Ok(path) = uri.to_file_path() {
            file_snapshots.insert(uri.clone(), InputSnapshot::file(&path));
        }
    }
    // Dynamic dependencies may only be known after TeX writes its recorder.
    // Capture other open buffers without requiring unrelated files be saved.
    for (uri, doc) in &workspace.documents {
        if doc.version.is_some() {
            snapshots
                .entry(uri.clone())
                .or_insert_with(|| (doc.version, doc.text.source.clone()));
            // A dynamic input may have unsaved editor text. The compiler
            // reads disk; retain both versions without requiring unrelated
            // open buffers to be saved.
            if !project.contains(uri) {
                if let Ok(path) = uri.to_file_path() {
                    file_snapshots.insert(uri.clone(), InputSnapshot::file(&path));
                }
            }
        }
    }
    if rebuild {
        config.rebuild_arguments().map_err(Error::invalid_params)?;
    }
    let checks = Arc::new(crate::compilation::file_checks::Snapshot::capture(
        &workspace, &root, &config,
    ));
    let progress_supported = workspace.client.build_progress;
    let root_path = root
        .to_file_path()
        .map_err(|_| Error::invalid_params("Compilation requires a local file"))?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::request_cancelled());
    }
    Ok(BuildInput {
        root,
        root_path,
        prepared_documents: workspace.documents.updates_since(&original_documents),
        snapshots,
        file_snapshots,
        open_aliases,
        config,
        progress_supported,
        project,
        static_code_inputs,
        checks,
        rebuild,
    })
}
