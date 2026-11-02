//! Negotiated client features and per-project build observations.
use lsp_types::Url;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Clone, Default)]
pub struct ClientCapabilities {
    pub inlay_refresh: bool,
    pub snippets: bool,
    pub completion_resolve_documentation: bool,
    pub watch_files: bool,
    pub hierarchical_symbols: bool,
    pub versioned_edits: bool,
    pub folding_capabilities: lsp_types::FoldingRangeClientCapabilities,
    pub build_progress: bool,
    pub code_action_literals: bool,
    pub code_action_resolve_edits: bool,
    pub apply_edits: bool,
}

#[derive(Clone, Default)]
pub struct ProjectState {
    pub lint_diagnostics: BTreeMap<Url, Vec<lsp_types::Diagnostic>>,
    pub build_diagnostics: BTreeMap<Url, BTreeMap<Url, Vec<lsp_types::Diagnostic>>>,
    /// Sources confirmed read for each root's latest published build diagnostics.
    pub build_diagnostic_sources: BTreeMap<Url, BTreeSet<Url>>,
    pub recorded_inputs: BTreeMap<Url, BTreeSet<Url>>,
    /// Image/data roles retained independently of canonical filename suffixes.
    pub recorded_binary_inputs: BTreeMap<Url, BTreeSet<Url>>,
    /// Opened recorder paths and the physical files selected by the last build.
    pub recorded_bindings: BTreeMap<Url, BTreeMap<PathBuf, PathBuf>>,
    pub bibliography_inputs: BTreeMap<Url, BTreeSet<Url>>,
    /// Database parser roles confirmed by complete bibliography tool logs.
    pub bibliography_databases: BTreeMap<Url, BTreeSet<Url>>,
    /// Paths selected by the last complete BibTeX/Biber logs, before symlinks.
    pub bibliography_bindings: BTreeMap<Url, BTreeMap<PathBuf, PathBuf>>,
    /// Input contents from each root's last complete bibliography tool logs.
    pub bibliography_checks:
        BTreeMap<Url, std::sync::Arc<crate::compilation::file_checks::Snapshot>>,
    /// File conditions and input snapshots for the latest published diagnostics.
    pub build_checks: BTreeMap<Url, std::sync::Arc<crate::compilation::file_checks::Snapshot>>,
    pub pdf_checks: BTreeMap<Url, std::sync::Arc<crate::compilation::file_checks::Snapshot>>,
}

impl ProjectState {
    /// Only this root's build observations belong to a completed build. Other
    /// roots and independent lint reports may have changed during analysis.
    pub(crate) fn merge_build(&mut self, root: &Url, update: &mut Self) {
        macro_rules! merge {
            ($($field:ident),* $(,)?) => { $(
                match update.$field.remove(root) {
                    Some(value) => { self.$field.insert(root.clone(), value); }
                    None => { self.$field.remove(root); }
                }
            )* };
        }
        merge!(
            build_diagnostics,
            build_diagnostic_sources,
            recorded_inputs,
            recorded_binary_inputs,
            recorded_bindings,
            bibliography_inputs,
            bibliography_databases,
            bibliography_bindings,
            bibliography_checks,
            build_checks,
            pdf_checks
        );
    }
}
