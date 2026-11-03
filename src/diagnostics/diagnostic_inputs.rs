//! Observe filesystem inputs actually consulted by one diagnostic calculation.
use std::{
    collections::BTreeMap,
    fs::Metadata,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

const MAX_FILES: usize = 4096;
const MAX_BYTES: usize = 512 * 1024;

#[derive(Clone, PartialEq)]
struct Stamp {
    modified: Option<SystemTime>,
    length: u64,
    file: bool,
    target: Option<PathBuf>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Stamp {
    fn read(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            length: metadata.len(),
            file: metadata.is_file(),
            target: path.canonicalize().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        })
    }
}

#[derive(Default)]
pub(crate) struct Files {
    paths: BTreeMap<PathBuf, Option<Stamp>>,
    pub bytes: usize,
    overflow: bool,
}

impl Files {
    pub(crate) fn capture<T>(work: impl FnOnce() -> T) -> (T, Arc<Self>) {
        let context = crate::analysis::AnalysisContext::observing();
        let result = context.run(|_| work());
        (result, context.take_files())
    }

    /// Open editor text is authoritative. Saving it may replace the inode or
    /// timestamps, but changing existence, type or a symlink target still
    /// changes source selection and invalidates the certificate.
    pub(crate) fn current_for_editor(
        &self,
        workspace: &crate::project::workspace::Workspace,
    ) -> bool {
        !self.overflow
            && self.paths.iter().all(|(path, old)| {
                let open = lsp_types::Url::from_file_path(path)
                    .ok()
                    .and_then(|uri| workspace.documents.get(&uri))
                    .is_some_and(|doc| doc.version.is_some());
                let current = Stamp::read(path);
                if open {
                    match (old, &current) {
                        (Some(old), Some(current)) => {
                            old.file == current.file && old.target == current.target
                        }
                        (None, None) => true,
                        _ => false,
                    }
                } else {
                    current == *old
                }
            })
    }

    pub(crate) fn current(&self) -> bool {
        !self.overflow
            && self
                .paths
                .iter()
                .all(|(path, old)| Stamp::read(path) == *old)
    }
}

impl Files {
    /// Reusing an index also reuses the exact filesystem observations which
    /// certified it. Conflicting observations cannot certify one snapshot.
    pub(crate) fn extend(&mut self, other: &Self) {
        if self.overflow || other.overflow {
            self.overflow = true;
            return;
        }
        for (path, stamp) in &other.paths {
            if let Some(existing) = self.paths.get(path) {
                if existing != stamp {
                    self.overflow = true;
                    return;
                }
            } else {
                let bytes = std::mem::size_of::<Stamp>()
                    + path.as_os_str().len()
                    + stamp
                        .as_ref()
                        .and_then(|stamp| stamp.target.as_ref())
                        .map_or(0, |path| path.as_os_str().len());
                if self.paths.len() >= MAX_FILES || self.bytes + bytes > MAX_BYTES {
                    self.overflow = true;
                    return;
                }
                self.bytes += bytes;
                self.paths.insert(path.clone(), stamp.clone());
            }
        }
    }

    pub(crate) fn observe(&mut self, path: &Path) {
        if self.overflow || self.paths.contains_key(path) {
            return;
        }
        let stamp = Stamp::read(path);
        let bytes = std::mem::size_of::<Stamp>()
            + path.as_os_str().len()
            + stamp
                .as_ref()
                .and_then(|stamp| stamp.target.as_ref())
                .map_or(0, |path| path.as_os_str().len());
        if self.paths.len() >= MAX_FILES || self.bytes + bytes > MAX_BYTES {
            self.overflow = true;
            return;
        }
        self.bytes += bytes;
        self.paths.insert(path.to_owned(), stamp);
    }
}

pub(crate) fn observe(path: &Path) {
    if let Some(context) = crate::analysis::AnalysisContext::current() {
        context.observe(path);
    }
}

pub(crate) fn metadata(path: &Path) -> std::io::Result<Metadata> {
    observe(path);
    std::fs::metadata(path)
}

pub(crate) fn is_file(path: &Path) -> bool {
    observe(path);
    path.is_file()
}

pub(crate) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    observe(path);
    path.canonicalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::workspace::{Document, Workspace};
    #[test]
    fn completion_observations_ignore_saved_open_text_but_not_disk_dependencies_or_missing_inputs()
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main.tex");
        let dependency = root.join("macros.tex");
        let missing = root.join("new.tex");
        std::fs::write(&main, "Saved").unwrap();
        std::fs::write(&dependency, "Old macro").unwrap();
        let uri = lsp_types::Url::from_file_path(&main).unwrap();
        let mut workspace = Workspace::default();
        workspace.documents.insert(
            uri.clone(),
            Document::new(&uri, "Unsaved editor".into(), Some(1)),
        );
        let (_, files) = Files::capture(|| {
            observe(&main);
            observe(&dependency);
            observe(&missing);
        });
        std::fs::write(
            root.join("replacement"),
            "Saved editor with a different length",
        )
        .unwrap();
        std::fs::rename(root.join("replacement"), &main).unwrap();
        assert!(!files.current());
        assert!(files.current_for_editor(&workspace));
        std::fs::write(&dependency, "New macro").unwrap();
        assert!(!files.current_for_editor(&workspace));
        let (_, files) = Files::capture(|| {
            observe(&main);
            observe(&dependency);
            observe(&missing);
        });
        std::fs::write(&missing, "New source").unwrap();
        assert!(!files.current_for_editor(&workspace));
    }
    #[cfg(unix)]
    #[test]
    fn saved_open_symlink_retargeting_still_invalidates_completion_observations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let alias = root.join("main.tex");
        for name in ["first.tex", "second.tex"] {
            std::fs::write(root.join(name), "Same text").unwrap();
        }
        std::os::unix::fs::symlink(root.join("first.tex"), &alias).unwrap();
        let uri = lsp_types::Url::from_file_path(&alias).unwrap();
        let mut workspace = Workspace::default();
        workspace.documents.insert(
            uri.clone(),
            Document::new(&uri, "Same text".into(), Some(1)),
        );
        let (_, files) = Files::capture(|| observe(&alias));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(root.join("second.tex"), &alias).unwrap();
        assert!(!files.current_for_editor(&workspace));
    }
}
