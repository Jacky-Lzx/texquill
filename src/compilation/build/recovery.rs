//! A persistent interruption marker, locked for the entire build workflow.
use crate::{compilation::build_clean, config::BuildConfig, project::workspace::Workspace};
use lsp_types::Url;
use std::{
    cell::Cell,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) struct Recovery {
    file: File,
    path: PathBuf,
    target: PathBuf,
    owner: Vec<u8>,
    started: Cell<bool>,
}

impl Recovery {
    pub(super) fn begin(
        root: &Path,
        config: &BuildConfig,
        protected: &[PathBuf],
    ) -> Result<Self, String> {
        let path = config
            .artifact_path(root, "texquill-state")
            .ok_or("Invalid recovery marker path")?;
        let owner = serde_json::to_vec(&root.canonicalize().unwrap_or_else(|_| root.to_owned()))
            .map_err(|error| error.to_string())?;
        if protected
            .iter()
            .chain(std::iter::once(&root.to_owned()))
            .any(|source| {
                source
                    .canonicalize()
                    .ok()
                    .is_some_and(|source| Some(source) == path.canonicalize().ok())
            })
        {
            return Err("Build recovery marker overlaps a source file".into());
        }
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|error| error.to_string())?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| format!("Cannot open build recovery marker: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err("Build recovery marker must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err("Build recovery marker must not be hard linked".into());
            }
        }
        file.try_lock().map_err(|_| {
            "Another compiler still owns this build's auxiliary files; wait for it to finish"
                .to_string()
        })?;
        let mut previous = Vec::new();
        (&mut file)
            .take(16385)
            .read_to_end(&mut previous)
            .map_err(|error| error.to_string())?;
        let target = path.canonicalize().map_err(|error| error.to_string())?;
        let recovery = Self {
            file,
            path,
            target,
            owner,
            started: Cell::new(false),
        };
        recovery.validate_binding()?;
        if !previous.is_empty() {
            if previous != recovery.owner {
                return Err("Interrupted build marker belongs to another source or is damaged; inspect its auxiliary directory before removing the .texquill-state file".into());
            }
            let uri = Url::from_file_path(root).map_err(|_| "Recovery requires a local file")?;
            let mut workspace = Workspace::default();
            workspace.refresh_project(&uri);
            let mut protected = protected.to_vec();
            // A supplied bibliography may be the only available copy. Without
            // a declared readable database, recovery cannot regenerate it.
            if config.bibliography_tool.is_none()
                && !workspace
                    .bibliography_files(&uri)
                    .iter()
                    .any(|uri| uri.to_file_path().is_ok_and(|path| path.is_file()))
            {
                protected.extend(config.artifact_path(root, "bbl"));
            }
            let plan =
                build_clean::Plan::prepare_protected(&workspace, &uri, config, false, &protected)?;
            recovery.validate_binding()?;
            let result = plan.execute(false)?;
            if result["errors"]
                .as_array()
                .is_none_or(|errors| !errors.is_empty())
            {
                return Err(format!(
                    "Cannot recover interrupted build: {}",
                    result["errors"]
                ));
            }
        }
        Ok(recovery)
    }

    fn validate_binding(&self) -> Result<(), String> {
        let named = std::fs::symlink_metadata(&self.path).map_err(|error| error.to_string())?;
        if !named.is_file() || self.path.canonicalize().ok().as_ref() != Some(&self.target) {
            return Err("Build recovery marker binding changed; build again".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let locked = self.file.metadata().map_err(|error| error.to_string())?;
            if named.dev() != locked.dev() || named.ino() != locked.ino() || named.nlink() != 1 {
                return Err("Build recovery marker was replaced; build again".into());
            }
        }
        Ok(())
    }

    pub(super) fn mark_running(&mut self) -> Result<(), String> {
        self.validate_binding()?;
        self.file.set_len(0).map_err(|error| error.to_string())?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        self.file
            .write_all(&self.owner)
            .map_err(|error| error.to_string())?;
        self.file.sync_all().map_err(|error| error.to_string())
    }

    pub(crate) fn inherit(&self, command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd, unix::process::CommandExt};
            let fd = self.file.as_raw_fd();
            // Only this child inherits the lock; unrelated concurrent spawns
            // keep CLOEXEC. Descendants retain ownership if the LSP crashes.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        let _ = command;
    }

    pub(super) fn spawned(&self) {
        self.started.set(true);
    }

    pub(super) fn finish(&self, completed: bool) -> Result<(), String> {
        if completed || !self.started.get() {
            self.validate_binding()?;
            self.file.set_len(0).map_err(|error| error.to_string())?;
            self.file.sync_all().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn compiler_retains_the_job_lock_after_its_server_handle_is_dropped() {
        use std::os::unix::process::CommandExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let main = root.join("main.tex");
        std::fs::write(&main, "\\documentclass{article}").unwrap();
        let config = BuildConfig::default();
        let mut recovery = Recovery::begin(&main, &config, &[]).unwrap();
        recovery.mark_running().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]).process_group(0);
        recovery.inherit(&mut command);
        let mut child = command.spawn().unwrap();
        drop(recovery);
        let blocked = Recovery::begin(&main, &config, &[]);
        crate::process::kill_group(&mut child);
        child.wait().unwrap();
        assert!(blocked.is_err(), "a live compiler lost its inherited lock");
        assert!(Recovery::begin(&main, &config, &[]).is_ok());
    }

    #[test]
    fn recovery_does_not_follow_a_marker_from_another_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let first = root.join("first.tex");
        let second = root.join("second.tex");
        for path in [&first, &second] {
            std::fs::write(path, "source").unwrap();
        }
        let config = BuildConfig {
            job_name: Some("shared".into()),
            ..Default::default()
        };
        let mut recovery = Recovery::begin(&first, &config, &[]).unwrap();
        recovery.mark_running().unwrap();
        drop(recovery);
        let aux = config.artifact_path(&first, "aux").unwrap();
        std::fs::write(&aux, "preserve").unwrap();
        assert!(Recovery::begin(&second, &config, &[]).is_err());
        assert_eq!(std::fs::read_to_string(aux).unwrap(), "preserve");
    }

    #[test]
    #[cfg(unix)]
    fn retargeting_the_auxiliary_directory_does_not_rebind_the_locked_marker() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let main = root.join("main.tex");
        std::fs::write(&main, "source").unwrap();
        for name in ["before", "after"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let alias = root.join("out");
        symlink(root.join("before"), &alias).unwrap();
        let config = BuildConfig {
            output_directory: Some("out".into()),
            ..Default::default()
        };
        let mut recovery = Recovery::begin(&main, &config, &[]).unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(root.join("after"), &alias).unwrap();
        assert!(recovery.mark_running().is_err());
        assert!(std::fs::read_dir(root.join("after"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    #[cfg(unix)]
    fn linked_markers_cannot_modify_source_files() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let main = root.join("main.tex");
        std::fs::write(&main, "source").unwrap();
        let config = BuildConfig::default();
        let marker = config.artifact_path(&main, "texquill-state").unwrap();
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        for hard in [false, true] {
            if hard {
                std::fs::hard_link(&main, &marker).unwrap();
            } else {
                symlink(&main, &marker).unwrap();
            }
            assert!(Recovery::begin(&main, &config, &[]).is_err());
            assert_eq!(std::fs::read_to_string(&main).unwrap(), "source");
            std::fs::remove_file(&marker).unwrap();
        }
    }
}
