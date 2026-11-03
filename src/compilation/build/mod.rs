//! Compiler execution API; service coordination lives in server::builds.
mod execution;
pub(crate) mod recovery;
pub(crate) struct BuildInputs {
    pub paths: Vec<std::path::PathBuf>,
    pub protected: Vec<std::path::PathBuf>,
}
pub(crate) use execution::{capture, reject_outdated_pdf, run_with_mode};
pub use execution::{rebuild, run, Outcome};

// Preserve existing public callers while internal users use the owning subsystem.
pub use crate::server::builds::{BuildFinished, BuildManager, BuildStarted};
