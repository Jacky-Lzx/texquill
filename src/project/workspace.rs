use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use lsp_types::Url;

use crate::{parser::syntax::Syntax, text::Text};

pub const SOURCE_EXTENSIONS: &[&str] = &[
    "tex", "ltx", "bib", "bst", "sty", "cls", "def", "clo", "fd", "ldf", "bbx", "cbx", "lbx", "dbx",
];

pub fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|extension| SOURCE_EXTENSIONS.contains(&extension))
}

/// Editors and filesystem URLs can spell the same percent-encoded path
/// differently. Keep one document identity without resolving symlinks or
/// changing the case of filesystem names.
pub(crate) fn document_uri(uri: Url) -> Url {
    let Ok(path) = uri.to_file_path() else {
        return uri;
    };
    let Ok(mut normalized) = Url::from_file_path(path) else {
        return uri;
    };
    normalized.set_query(uri.query());
    normalized.set_fragment(uri.fragment());
    normalized
}

#[cfg(test)]
mod uri_tests {
    use super::*;

    #[test]
    fn file_uri_identity_preserves_path_case_queries_fragments_and_non_file_schemes() {
        let uri =
            Url::parse("file:///tmp/%e7%ab%a0%e8%8a%82%20Draft.tex?mode=%2f#part%2f").unwrap();
        let normalized = document_uri(uri);
        assert_eq!(
            normalized.as_str(),
            "file:///tmp/%E7%AB%A0%E8%8A%82%20Draft.tex?mode=%2f#part%2f"
        );
        assert_eq!(document_uri(normalized.clone()), normalized);
        for value in [
            "untitled:Draft.tex",
            "https://example.test/%e7%ab%a0.tex",
            "file://remote-host/Chapter.tex",
        ] {
            let uri = Url::parse(value).unwrap();
            if uri.to_file_path().is_err() {
                assert_eq!(document_uri(uri.clone()), uri);
            }
        }
        assert_ne!(
            document_uri(Url::parse("file:///tmp/Draft.tex").unwrap()),
            document_uri(Url::parse("file:///tmp/draft.tex").unwrap())
        );
    }
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

#[derive(Clone, Debug)]
pub struct Document {
    pub text: Text,
    pub syntax: Syntax,
    pub version: Option<i32>,
    pub kind: DocumentKind,
    /// Project input role, which takes precedence over editor language and URI.
    pub kind_hint: Option<DocumentKind>,
    /// Editor language selection, valid only while this document is open.
    client_kind: Option<DocumentKind>,
    pub(crate) extra_symbols: std::sync::OnceLock<Vec<crate::symbols::extra_symbols::Occurrence>>,
    pub(crate) semantic_full:
        std::sync::OnceLock<std::sync::Arc<crate::language::semantic::Snapshot>>,
    pub(crate) outline:
        std::sync::OnceLock<std::sync::Arc<crate::language::symbol_cache::Snapshot>>,
    pub(crate) string_context: crate::symbols::string_test_context::Cache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    Latex,
    Bibtex,
    BibtexStyle,
}

impl DocumentKind {
    fn from_language_id(language_id: &str) -> Option<Self> {
        match language_id {
            "latex" | "tex" | "plaintex" => Some(Self::Latex),
            "bibtex" | "bib" => Some(Self::Bibtex),
            "bst" | "bibtex-style" => Some(Self::BibtexStyle),
            _ => None,
        }
    }
}

impl Document {
    pub fn new(uri: &Url, source: String, version: Option<i32>) -> Self {
        Self::new_with_kind(uri, source, version, None)
    }

    pub(crate) fn new_with_kind(
        uri: &Url,
        source: String,
        version: Option<i32>,
        kind_hint: Option<DocumentKind>,
    ) -> Self {
        Self::new_with_language(uri, source, version, kind_hint, None)
    }

    fn new_with_language(
        uri: &Url,
        source: String,
        version: Option<i32>,
        kind_hint: Option<DocumentKind>,
        client_kind: Option<DocumentKind>,
    ) -> Self {
        let text = Text::new(source);
        let kind = kind_hint.or(client_kind).unwrap_or_else(|| {
            if uri.path().ends_with(".bib") {
                DocumentKind::Bibtex
            } else if uri.path().ends_with(".bst") {
                DocumentKind::BibtexStyle
            } else {
                DocumentKind::Latex
            }
        });
        let syntax = Self::parse(&text, kind);
        Self {
            text,
            syntax,
            version,
            kind,
            kind_hint,
            client_kind,
            semantic_full: Default::default(),
            outline: Default::default(),
            extra_symbols: Default::default(),
            string_context: Default::default(),
        }
    }

    pub(crate) fn with_text(&self, uri: &Url, source: String, version: Option<i32>) -> Self {
        Self::new_with_language(uri, source, version, self.kind_hint, self.client_kind)
    }

    pub(crate) fn relocated(self, uri: &Url) -> Self {
        Self::new_with_language(
            uri,
            self.text.source,
            self.version,
            self.kind_hint,
            self.client_kind,
        )
    }

    fn parse(text: &Text, kind: DocumentKind) -> Syntax {
        match kind {
            DocumentKind::Bibtex => Syntax::bibtex(text),
            DocumentKind::Latex => Syntax::latex(text),
            // BibTeX styles use a stack language, not LaTeX commands/braces.
            DocumentKind::BibtexStyle => Syntax::default(),
        }
    }

    fn set_kind_hint(&mut self, kind: DocumentKind) {
        if self.kind != kind {
            self.kind = kind;
            self.syntax = Self::parse(&self.text, kind);
            self.semantic_full = Default::default();
            self.outline = Default::default();
            self.extra_symbols = Default::default();
            self.string_context = Default::default();
        }
        self.kind_hint = Some(kind);
    }
}

/// The first real magic comment in the first 20 lines, excluding raw content.
pub fn root_directive(doc: &Document) -> Option<crate::parser::syntax::Argument> {
    magic_directive(doc, &["root"])
}

fn root_marker(doc: &Document, command: &crate::parser::syntax::Command) -> bool {
    if doc.syntax.control_operands.contains(&command.span.start)
        && !doc.syntax.inspection_operand(&doc.text.source, command)
    {
        return false;
    }
    command.name == "documentclass"
        || command.name == "begin"
            && command.argument.as_ref().map_or_else(
                || {
                    crate::parser::syntax::environment_name_argument(
                        &doc.text.source,
                        command.span.end,
                    )
                    .is_some_and(|argument| argument.value == "document")
                },
                |argument| argument.value == "document",
            )
}

fn magic_directive(doc: &Document, fields: &[&str]) -> Option<crate::parser::syntax::Argument> {
    let mut offset = 0;
    for line in doc.text.source.split_inclusive('\n').take(20) {
        let trimmed = line.trim_start();
        let comment_start = offset + line.len() - trimmed.len();
        if (doc.kind != DocumentKind::Latex
            || doc
                .syntax
                .ignored
                .iter()
                .any(|span| span.start == comment_start))
            && trimmed.starts_with('%')
        {
            if let Some((name, value)) = trimmed[1..].trim().split_once('=') {
                if fields
                    .iter()
                    .any(|field| name.trim().eq_ignore_ascii_case(&format!("!tex {field}")))
                {
                    let value = value.trim().trim_matches('"');
                    let start = offset + line.find('=').unwrap() + 1;
                    let start = start + line[start - offset..].find(value).unwrap();
                    return Some(crate::parser::syntax::Argument {
                        value: value.into(),
                        span: start..start + value.len(),
                        outer_span: start..start + value.len(),
                    });
                }
            }
        }
        offset += line.len();
    }
    None
}

#[derive(Clone, Default)]
pub struct Workspace {
    pub client: super::state::ClientCapabilities,
    pub project: super::state::ProjectState,
    pub distribution: crate::project::distribution::Distribution,
    pub index_truncated: BTreeSet<String>,
    pub documents: super::documents::Documents,
    pub roots: BTreeSet<PathBuf>,
    pub diagnostic_delivery: crate::diagnostics::diagnostic::Delivery,
    pub config: crate::config::Config,
}

pub(crate) struct SymbolProjects {
    pub scope: BTreeSet<Url>,
    pub regions: BTreeMap<Url, BTreeSet<Url>>,
}

impl Workspace {
    /// Keep one extra candidate for the protocol's incomplete-list response.
    pub(crate) fn completion_budget(&self) -> usize {
        self.config.indexing.completion_limit.saturating_add(1)
    }

    fn refresh_context(&mut self) {
        for _ in 0..8 {
            crate::symbols::string_test_context::refresh(self);
            let changed = crate::project::resource_context::refresh(self);
            if changed.is_empty() {
                break;
            }
            let roots: BTreeSet<_> = changed
                .iter()
                .flat_map(|uri| self.source_roots(uri))
                .collect();
            for root in roots {
                self.load_dependencies_inner(
                    &root,
                    &root,
                    0,
                    &mut BTreeSet::new(),
                    false,
                    &mut None,
                );
            }
        }
    }
    /// A quoted document marker does not make an example a compilation root.
    /// Resolve primitive meanings in this candidate's source context, without
    /// invoking root selection while discovering that context.
    pub(crate) fn is_root(&self, uri: &Url) -> bool {
        let Some(doc) = self.documents.get(uri) else {
            return false;
        };
        let Some(marker) = doc
            .syntax
            .commands
            .iter()
            .find(|command| root_marker(doc, command))
        else {
            return false;
        };
        let preceding_quote = doc
            .syntax
            .commands
            .iter()
            .take_while(|command| command.span.start < marker.span.start)
            .any(|command| {
                matches!(
                    command.name.as_str(),
                    "detokenize" | "string" | "meaning" | "csuse" | "endcsname"
                )
            });
        // Preserve ordinary declarations before any apparent quote without
        // constructing another project index for common editor requests.
        if !preceding_quote {
            return true;
        }
        let index = crate::project::source_inputs::Index::new(self, uri);
        let quoted = index.quoted_file_positions(self, uri);
        doc.syntax
            .commands
            .iter()
            .any(|command| root_marker(doc, command) && !quoted.contains(&command.span.start))
    }

    pub(crate) fn is_saved_root(&self, uri: &Url) -> bool {
        self.is_root(uri)
            && uri.to_file_path().is_ok_and(|path| {
                matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("tex" | "ltx")
                ) && file_uri(&path).is_ok()
            })
    }

    /// Editor defaults, ancestor project files, then the selected root's override.
    pub fn settings(&self, uri: &Url) -> crate::config::Config {
        let root = self.root_document(uri).ok();
        crate::config::project_config::load(self, uri, root.as_ref()).0
    }

    pub fn build_config(&self, root: &Url) -> crate::config::BuildConfig {
        let mut config = crate::config::project_config::load(self, root, Some(root))
            .0
            .build;
        let latexmk = Path::new(&config.executable)
            .file_stem()
            .and_then(|name| name.to_str())
            == Some("latexmk");
        if latexmk && config.engine.is_none() && config.arguments.is_none() {
            config.engine = self
                .documents
                .get(root)
                .and_then(|doc| magic_directive(doc, &["program", "ts-program"]))
                .and_then(|argument| crate::config::Engine::from_program(&argument.value));
        }
        root.to_file_path()
            .map(|path| config.resolved(&path))
            .unwrap_or(config)
    }

    pub fn diagnostics(&self, uri: &Url) -> Vec<lsp_types::Diagnostic> {
        self.diagnostic_snapshots([uri.clone()])
            .pop()
            .map(|(_, diagnostics, _)| diagnostics)
            .unwrap_or_default()
    }

    pub fn diagnostic_snapshots(
        &self,
        uris: impl IntoIterator<Item = Url>,
    ) -> Vec<(Url, Vec<lsp_types::Diagnostic>, Option<i32>)> {
        self.diagnostic_snapshots_cancelled(uris, || false)
    }

    pub(crate) fn diagnostic_snapshots_cancelled(
        &self,
        uris: impl IntoIterator<Item = Url>,
        cancelled: impl Fn() -> bool,
    ) -> Vec<(Url, Vec<lsp_types::Diagnostic>, Option<i32>)> {
        crate::analysis::AnalysisContext::new()
            .run(|_| self.diagnostic_snapshots_in_query(uris, cancelled))
    }

    fn diagnostic_snapshots_in_query(
        &self,
        uris: impl IntoIterator<Item = Url>,
        cancelled: impl Fn() -> bool,
    ) -> Vec<(Url, Vec<lsp_types::Diagnostic>, Option<i32>)> {
        if cancelled() {
            return vec![];
        }
        let uris: BTreeSet<_> = uris.into_iter().collect();
        let mut duplicates = crate::diagnostics::project_diagnostics::duplicates(self, &uris);
        let environments = crate::diagnostics::environment_diagnostics::Index::new(self, &uris);
        let mut math = crate::diagnostics::math_diagnostics::Index::new(self, &uris);
        uris.into_iter()
            .take_while(|_| !cancelled())
            .map(|uri| {
                let version = self.documents.get(&uri).and_then(|doc| doc.version);
                let mut diagnostics = self.local_and_build_diagnostics(&uri);
                diagnostics.extend(crate::diagnostics::reference_diagnostics::diagnostics(
                    self, &uri,
                ));
                diagnostics.extend(crate::bibliography::bib_field_diagnostics::diagnostics(
                    self, &uri,
                ));
                diagnostics.extend(math.take(&uri));
                diagnostics.extend(crate::symbols::extra_symbols::diagnostics(self, &uri));
                let root = self.root_document(&uri).ok();
                for message in crate::config::project_config::load(self, &uri, root.as_ref()).1 {
                    diagnostics.push(lsp_types::Diagnostic {
                        range: Default::default(),
                        severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                        source: Some("texquill".into()),
                        code: Some(lsp_types::NumberOrString::String(
                            "project-configuration".into(),
                        )),
                        message,
                        ..Default::default()
                    });
                }
                if let Some(doc) = self.documents.get(&uri) {
                    environments.replace_diagnostics(&uri, doc, &mut diagnostics);
                }
                diagnostics.extend(duplicates.remove(&uri).unwrap_or_default());
                diagnostics
                    .sort_by_key(|diagnostic| (diagnostic.range.start, diagnostic.message.clone()));
                (uri, diagnostics, version)
            })
            .collect()
    }

    fn local_and_build_diagnostics(&self, uri: &Url) -> Vec<lsp_types::Diagnostic> {
        let labels = crate::symbols::label_declarations::Index::new(self, uri);
        let mut diagnostics = self
            .documents
            .get(uri)
            .map(|doc| crate::language::features::diagnostics_at(doc, uri, &labels))
            .unwrap_or_default();
        diagnostics.extend(
            self.project
                .lint_diagnostics
                .get(uri)
                .into_iter()
                .flatten()
                .cloned(),
        );
        let physical = uri
            .to_file_path()
            .ok()
            .and_then(|path| crate::diagnostics::diagnostic_inputs::canonicalize(&path).ok())
            .and_then(|path| Url::from_file_path(path).ok());
        for project in self.project.build_diagnostics.values() {
            if let Some(build) = project
                .get(uri)
                .or_else(|| physical.as_ref().and_then(|uri| project.get(uri)))
            {
                diagnostics.extend(build.iter().cloned());
            }
        }
        diagnostics.sort_by_key(|diagnostic| (diagnostic.range.start, diagnostic.message.clone()));
        diagnostics.dedup_by(|a, b| {
            a.range == b.range
                && a.message == b.message
                && a.source == b.source
                && a.code == b.code
                && a.data == b.data
        });
        diagnostics
    }

    pub fn clear_build_diagnostics(&mut self, uri: &Url) {
        self.project.lint_diagnostics.remove(uri);
        let physical = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .and_then(|path| Url::from_file_path(path).ok());
        for project in self.project.build_diagnostics.values_mut() {
            project.remove(uri);
            if let Some(physical) = &physical {
                project.remove(physical);
            }
        }
    }

    /// Keep editor identities while associating open buffers with build inputs.
    /// Never canonicalize the document map: aliases may be retargeted later.
    pub(crate) fn open_file_aliases(&self, inputs: &BTreeSet<Url>) -> BTreeMap<Url, Url> {
        let physical: BTreeSet<_> = inputs
            .iter()
            .filter_map(|uri| uri.to_file_path().ok()?.canonicalize().ok())
            .collect();
        self.documents
            .iter()
            .filter_map(|(uri, doc)| {
                doc.version?;
                let path = uri.to_file_path().ok()?.canonicalize().ok()?;
                let target = Url::from_file_path(&path).ok()?;
                (physical.contains(&path) && *uri != target).then(|| (uri.clone(), target))
            })
            .collect()
    }

    pub fn dependencies(&self, uri: &Url) -> Vec<Url> {
        self.dependencies_from(uri, uri)
    }

    fn dependencies_from(&self, uri: &Url, root: &Url) -> Vec<Url> {
        if !self.documents.contains_key(uri) {
            return vec![];
        }
        let inputs = crate::project::source_inputs::Index::new(self, root);
        self.dependencies_with_inputs(uri, root, &inputs)
    }

    fn dependencies_with_inputs(
        &self,
        uri: &Url,
        root: &Url,
        inputs: &crate::project::source_inputs::Index,
    ) -> Vec<Url> {
        if !self.documents.contains_key(uri) {
            return vec![];
        }
        let mut dependencies = self
            .source_dependencies_with_inputs(uri, root, inputs)
            .into_iter()
            .map(|(uri, _)| uri)
            .collect::<Vec<_>>();
        if uri == root {
            if let Some(recorded) = self.project.recorded_inputs.get(root) {
                // Recorder suffixes cannot distinguish displayed TeX code
                // from executed source. Explicit includes above retain their
                // source role independently, including in another project.
                let displayed = crate::project::code_inputs::displayed(self, root);
                dependencies.extend(recorded.difference(&displayed).cloned());
            }
            if let Some(recorded) = self.project.bibliography_inputs.get(root) {
                dependencies.extend(recorded.iter().cloned());
            }
        }
        dependencies
    }

    fn source_dependencies_with_inputs(
        &self,
        uri: &Url,
        root: &Url,
        inputs: &crate::project::source_inputs::Index,
    ) -> Vec<(Url, DocumentKind)> {
        let mut dependencies = inputs.literal_dependencies(self, uri, root);
        dependencies.extend(
            inputs
                .dependencies(self, uri, root)
                .into_iter()
                .map(|uri| (uri, DocumentKind::Latex)),
        );
        dependencies.extend(inputs.resource_dependencies(self, uri, root));
        dependencies
    }

    pub(crate) fn literal_source_dependencies_with_kinds(
        &self,
        uri: &Url,
        root: &Url,
    ) -> Vec<(Url, DocumentKind)> {
        self.literal_source_dependencies_filtered(uri, root, |_| true)
    }

    pub(crate) fn literal_source_dependencies_filtered(
        &self,
        uri: &Url,
        root: &Url,
        include: impl Fn(&crate::parser::syntax::Command) -> bool,
    ) -> Vec<(Url, DocumentKind)> {
        let Some(doc) = self.documents.get(uri) else {
            return vec![];
        };
        doc.syntax
            .commands
            .iter()
            .filter(|command| include(command))
            .flat_map(|command| self.literal_command_dependencies(uri, root, command))
            .collect()
    }

    pub(crate) fn literal_command_dependencies(
        &self,
        uri: &Url,
        root: &Url,
        command: &crate::parser::syntax::Command,
    ) -> Vec<(Url, DocumentKind)> {
        let Some(doc) = self.documents.get(uri) else {
            return vec![];
        };
        if let Some(targets) = crate::project::imports::dependencies(self, uri, root, command) {
            return targets
                .into_iter()
                .map(|uri| (uri, DocumentKind::Latex))
                .collect();
        }
        let extension = match command.name.as_str() {
            "input" | "InputIfFileExists" | "include" | "subfile" | "loadglsentries" => "tex",
            "bibliography" | "nobibliography" => "bib",
            name if crate::bibliography::bib_resources::loads(name) => "bib",
            "bibliographystyle" => "bst",
            name => match crate::parser::syntax::loader_extension(name) {
                Some(extension) => extension,
                None => return vec![],
            },
        };
        let command = doc.syntax.potential_file_command(&doc.text.source, command);
        crate::parser::syntax::file_argument(&doc.text.source, &command)
            .map(|arg| {
                let multiple = crate::parser::syntax::file_list(&command.name);
                let values = if let Some(spans) =
                    crate::parser::syntax::file_list_spans(&doc.text.source, &command)
                {
                    spans
                        .into_iter()
                        .map(|span| &doc.text.source[span])
                        .collect::<Vec<_>>()
                } else if multiple {
                    arg.value.split(',').collect::<Vec<_>>()
                } else {
                    vec![arg.value.as_str()]
                };
                values
                    .into_iter()
                    .filter_map(|value| {
                        let value = if command.name == "bibliographystyle" {
                            value
                        } else {
                            value.trim()
                        };
                        let kind = match extension {
                            "bib" => DocumentKind::Bibtex,
                            "bst" => DocumentKind::BibtexStyle,
                            _ => DocumentKind::Latex,
                        };
                        resolve_file_path(root, &command.name, value, &[extension])
                            .or_else(|| resolve_file_path(uri, &command.name, value, &[extension]))
                            .map(|uri| (uri, kind))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Do not index TeX's distribution packages outside the project. Explicitly
    /// opened or already indexed external documents remain part of the project.
    /// Recorded images and data can be shared outside the roots; their presence
    /// alone does not load their contents into the source document index.
    pub fn project_inputs(&self, root: &Url, inputs: &BTreeSet<Url>) -> BTreeSet<Url> {
        let binary = inputs
            .iter()
            .filter(|uri| {
                uri.to_file_path().ok().is_some_and(|path| {
                    crate::project::graphics::is_image(&path)
                        || crate::compilation::recorder::is_data(&path)
                })
            })
            .cloned()
            .collect();
        self.project_inputs_with_binary(root, inputs, &binary)
    }

    pub(crate) fn project_inputs_with_binary(
        &self,
        root: &Url,
        inputs: &BTreeSet<Url>,
        binary: &BTreeSet<Url>,
    ) -> BTreeSet<Url> {
        let parent = root
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        inputs
            .iter()
            .filter(|uri| {
                self.documents.contains_key(uri)
                    || binary.contains(*uri)
                    || uri.to_file_path().ok().is_some_and(|path| {
                        parent
                            .as_ref()
                            .is_some_and(|parent| path.starts_with(parent))
                            || self.roots.iter().any(|root| path.starts_with(root))
                    })
            })
            .take(self.config.indexing.max_files)
            .cloned()
            .collect()
    }

    pub fn project_documents(&self, root: &Url) -> BTreeSet<Url> {
        self.project_documents_from(root, root)
    }

    /// Explicit source traversal, independent of recorder and data inputs.
    pub(crate) fn source_documents_from(&self, source: &Url, root: &Url) -> BTreeSet<Url> {
        let inputs = crate::project::source_inputs::Index::new(self, root);
        let mut visited = BTreeSet::new();
        let mut pending = vec![source.clone()];
        while let Some(uri) = pending.pop() {
            if visited.insert(uri.clone()) {
                pending.extend(
                    self.source_dependencies_with_inputs(&uri, root, &inputs)
                        .into_iter()
                        .map(|(uri, _)| uri),
                );
            }
        }
        visited
    }

    pub(crate) fn project_documents_from(&self, source: &Url, root: &Url) -> BTreeSet<Url> {
        crate::project::source_inputs::project_documents(self, source, root, || {
            let inputs = crate::project::source_inputs::Index::new(self, root);
            self.project_documents_with_inputs(source, root, &inputs)
        })
    }

    pub(crate) fn project_documents_with_inputs(
        &self,
        source: &Url,
        root: &Url,
        inputs: &crate::project::source_inputs::Index,
    ) -> BTreeSet<Url> {
        let mut visited = BTreeSet::new();
        let mut pending = vec![source.clone()];
        while let Some(uri) = pending.pop() {
            if visited.insert(uri.clone()) {
                pending.extend(self.dependencies_with_inputs(&uri, root, inputs));
            }
        }
        visited
    }

    pub(crate) fn saved_projects(&self) -> BTreeMap<Url, BTreeSet<Url>> {
        self.documents
            .iter()
            .filter(|(uri, _)| self.is_saved_root(uri))
            .map(|(root, _)| (root.clone(), self.project_documents(root)))
            .collect()
    }

    /// Every source root supplies symbol meanings, including new buffers
    /// whose filesystem path has not been created yet.
    pub(crate) fn symbol_regions(&self) -> BTreeMap<Url, BTreeSet<Url>> {
        self.documents
            .keys()
            .filter(|uri| self.is_root(uri))
            .map(|root| (root.clone(), self.project_documents(root)))
            .collect()
    }

    pub(crate) fn symbol_projects(&self, uri: &Url) -> SymbolProjects {
        let regions = self.symbol_regions();
        let selected = self
            .is_root(uri)
            .then(|| uri.clone())
            .or_else(|| self.root_document(uri).ok())
            .filter(|root| {
                self.is_root(uri)
                    || regions.contains_key(root)
                    || *root != *uri
                    || crate::config::project_config::load(self, uri, None)
                        .0
                        .build
                        .root_file
                        .is_some()
            });
        let mut scope: BTreeSet<_> = if let Some(root) = selected {
            regions
                .get(&root)
                .cloned()
                .unwrap_or_else(|| self.project_documents(&root))
        } else {
            regions
                .values()
                .filter(|project| project.contains(uri))
                .flat_map(|project| project.iter().cloned())
                .collect()
        };
        if scope.is_empty() {
            // Unassociated buffers retain workspace-wide editing before saving.
            let folder = uri.to_file_path().ok().and_then(|path| {
                self.roots
                    .iter()
                    .filter(|root| path.starts_with(root))
                    .max_by_key(|root| root.components().count())
            });
            scope.extend(
                self.documents
                    .keys()
                    .filter(|other| {
                        folder.is_none_or(|folder| {
                            other
                                .to_file_path()
                                .is_ok_and(|path| path.starts_with(folder))
                        })
                    })
                    .cloned(),
            );
            // Explicit inputs may live above/outside the workspace folder.
            // Folder isolation must not discard this buffer's source graph.
            scope.extend(self.project_documents(uri));
        }
        scope.insert(uri.clone());
        SymbolProjects { scope, regions }
    }

    pub(crate) fn symbol_scope(&self, uri: &Url) -> BTreeSet<Url> {
        if self.is_root(uri) {
            let mut scope = self.project_documents(uri);
            scope.insert(uri.clone());
            return scope;
        }
        if let Ok(root) = self.root_document(uri) {
            if root != *uri
                || crate::config::project_config::load(self, uri, None)
                    .0
                    .build
                    .root_file
                    .is_some()
                || self.is_root(&root)
            {
                let mut scope = self.project_documents(&root);
                scope.insert(uri.clone());
                return scope;
            }
        }
        self.symbol_projects(uri).scope
    }

    pub(crate) fn source_roots(&self, uri: &Url) -> Vec<Url> {
        self.source_roots_in(uri, &self.saved_projects())
    }

    pub(crate) fn source_roots_in(
        &self,
        uri: &Url,
        regions: &BTreeMap<Url, BTreeSet<Url>>,
    ) -> Vec<Url> {
        let roots: Vec<_> = regions
            .iter()
            .filter(|(_, project)| project.contains(uri))
            .map(|(root, _)| root.clone())
            .collect();
        if roots.is_empty() {
            vec![self.root_document(uri).unwrap_or_else(|_| uri.clone())]
        } else {
            roots
        }
    }

    /// Opened path names and selected physical targets, independently of the
    /// source index. A known external alias can select an unindexed target.
    pub(crate) fn project_bindings<'a>(
        &'a self,
        root: &'a Url,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a PathBuf)> {
        [
            &self.project.recorded_bindings,
            &self.project.bibliography_bindings,
        ]
        .into_iter()
        .filter_map(move |by_root| by_root.get(root))
        .flat_map(|bindings| bindings.iter())
    }

    /// Static bibliography references retain their role even when their
    /// physical targets have unrelated suffixes or cannot be parsed as UTF-8.
    pub(crate) fn bibliography_files(&self, root: &Url) -> BTreeSet<Url> {
        let inputs = crate::project::source_inputs::Index::new(self, root);
        self.project_documents_with_inputs(root, root, &inputs)
            .into_iter()
            .flat_map(|uri| self.source_dependencies_with_inputs(&uri, root, &inputs))
            .filter_map(|(uri, kind)| (kind != DocumentKind::Latex).then_some(uri))
            .collect()
    }

    /// Saved sources and image dependencies used by builds and artifact checks.
    pub fn project_files(&self, root: &Url) -> BTreeSet<Url> {
        let inputs = crate::project::source_inputs::Index::new(self, root);
        let mut files = self.project_documents_with_inputs(root, root, &inputs);
        let image_calls = if inputs.incomplete {
            BTreeMap::new()
        } else {
            crate::project::graphics::calls_in(self, &files)
        };
        files.extend(crate::project::code_inputs::inputs(self, root));
        let images: BTreeSet<_> = files
            .iter()
            .filter(|uri| {
                image_calls
                    .get(*uri)
                    .is_some_and(|calls| calls.iter().any(|call| call.certain))
            })
            .flat_map(|uri| crate::project::graphics::images_in(self, uri, root))
            .collect();
        files.extend(images);
        files
    }

    /// Explicit root, then TeX magic comments, then the unique containing document.
    pub fn root_document(&self, uri: &Url) -> Result<Url, String> {
        let path = uri
            .to_file_path()
            .map_err(|_| "Compilation requires a local file URI")?;
        let folder_config = crate::config::project_config::load(self, uri, None).0;
        if let Some(root_file) = &folder_config.build.root_file {
            // Build notifications expose resolved roots. Recognize those URIs
            // before applying a relative setting again in a different folder.
            let requested = crate::diagnostics::diagnostic_inputs::canonicalize(&path)
                .unwrap_or_else(|_| normalize_path(&path));
            for folder in &self.roots {
                let configured = folder.join(root_file);
                let identity = crate::diagnostics::diagnostic_inputs::canonicalize(&configured)
                    .unwrap_or_else(|_| normalize_path(&configured));
                if identity == requested {
                    return file_uri(&configured);
                }
            }
            let folder = self
                .roots
                .iter()
                .filter(|root| path.starts_with(root))
                .max_by_key(|root| root.components().count())
                .map(PathBuf::as_path)
                .or_else(|| path.parent())
                .ok_or("Cannot determine workspace directory")?;
            return file_uri(&folder.join(root_file));
        }
        let mut root = uri.clone();
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(root.clone()) {
                return Err("Cyclic TeX root directives".into());
            }
            let disk = if self.documents.contains_key(&root) {
                None
            } else {
                root.to_file_path()
                    .ok()
                    .and_then(|path| {
                        crate::diagnostics::diagnostic_inputs::observe(&path);
                        std::fs::read_to_string(path).ok()
                    })
                    .map(|text| Document::new(&root, text, None))
            };
            let doc = self
                .documents
                .get(&root)
                .or(disk.as_ref())
                .ok_or("Save the document before compiling")?;
            let magic = root_directive(doc);
            if let Some(magic) = magic {
                let parent = root
                    .to_file_path()
                    .map_err(|_| "Invalid root URI")?
                    .parent()
                    .ok_or("Invalid root path")?
                    .to_path_buf();
                root = file_uri(&parent.join(magic.value))?;
            } else {
                if root != *uri {
                    return Ok(root);
                }
                break;
            }
        }
        if self.is_root(uri) {
            let selected = file_uri(&path);
            if selected.is_ok()
                || matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("tex" | "ltx")
                )
            {
                return selected;
            }
            // Package/support files may define a command containing
            // begin{document}; continue looking for their containing project.
        }
        let candidates = self
            .documents
            .iter()
            .filter(|(candidate, _)| {
                *candidate != uri
                    && self.is_saved_root(candidate)
                    && (self.project_documents(candidate).contains(uri)
                        || self.project_files(candidate).contains(uri))
            })
            .map(|(uri, _)| uri.clone())
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [root] => Ok(root.clone()),
            [] => file_uri(&path),
            _ => Err("Multiple root documents include this file; set build.rootFile or a % !TeX root directive".into()),
        }
    }

    pub fn add_root(&mut self, root: PathBuf) {
        if self.roots.insert(root.clone()) {
            self.scan(&root, 0);
            self.refresh_context();
        }
    }

    pub(crate) fn add_root_in_background(&mut self, root: PathBuf, cancelled: &impl Fn() -> bool) {
        if self.roots.insert(root.clone()) {
            self.scan_with(&root, 0, cancelled);
        }
        if !cancelled() {
            self.refresh_context();
        }
    }

    fn scan(&mut self, directory: &Path, depth: usize) {
        self.scan_with(directory, depth, &|| false);
    }
    fn scan_with(&mut self, directory: &Path, depth: usize, cancelled: &impl Fn() -> bool) {
        if cancelled() {
            self.index_truncated.insert("cancelled".into());
            return;
        }
        if depth > self.config.indexing.max_depth {
            self.index_truncated.insert("maxDepth".into());
            return;
        }
        if self.documents.len() >= self.config.indexing.max_files {
            self.index_truncated.insert("maxFiles".into());
            return;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if cancelled() {
                self.index_truncated.insert("cancelled".into());
                break;
            }
            if self.documents.len() >= self.config.indexing.max_files {
                self.index_truncated.insert("maxFiles".into());
                break;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.')
                    && !self
                        .config
                        .indexing
                        .exclude
                        .iter()
                        .any(|excluded| excluded == name.as_ref())
                {
                    self.scan_with(&path, depth + 1, cancelled);
                }
            } else if kind.is_file() && is_source(&path) {
                if let Ok(uri) = Url::from_file_path(&path) {
                    self.reload_document(&uri, self.documents.contains_key(&uri));
                }
            }
        }
    }

    pub fn reload(&mut self, uri: &Url) {
        self.reload_document(uri, self.documents.contains_key(uri));
        self.refresh_context();
    }

    fn recorded_kind(&self, uri: &Url) -> Option<DocumentKind> {
        if self
            .project
            .bibliography_databases
            .values()
            .any(|databases| databases.contains(uri))
        {
            return Some(DocumentKind::Bibtex);
        }
        let path = uri.to_file_path().ok()?;
        let mut style = false;
        for (name, target) in self
            .project
            .bibliography_bindings
            .values()
            .flat_map(|bindings| bindings.iter())
        {
            if *target == path {
                match name.extension().and_then(|ext| ext.to_str()) {
                    Some("bib") => return Some(DocumentKind::Bibtex),
                    Some("bst") => style = true,
                    _ => {}
                }
            }
        }
        if style {
            return Some(DocumentKind::BibtexStyle);
        }
        self.project
            .recorded_bindings
            .values()
            .flat_map(|bindings| bindings.iter())
            .any(|(name, target)| {
                *target == path && is_source(name) && !crate::compilation::recorder::is_data(name)
            })
            .then_some(DocumentKind::Latex)
    }

    pub(crate) fn reload_recorded(&mut self, uri: &Url, binary: bool) {
        if binary && !self.documents.contains_key(uri) {
            return;
        }
        // A source alias may resolve to a data-looking suffix. Its recorder
        // role is stronger evidence than the canonical name's extension.
        if binary {
            self.reload_document(uri, true);
        } else {
            self.reload_document_as(uri, true, Some(DocumentKind::Latex));
        }
    }

    pub(crate) fn reload_bibliography(&mut self, uri: &Url) {
        // A fresh tool log is a new observation, unlike a passive disk reload.
        let kind_hint = self.recorded_kind(uri);
        self.reload_document_as(uri, true, kind_hint);
    }

    fn reload_document(&mut self, uri: &Url, parse_source: bool) {
        let kind_hint = self
            .documents
            .get(uri)
            .and_then(|doc| doc.kind_hint)
            .or_else(|| self.recorded_kind(uri));
        self.reload_document_as(uri, parse_source || kind_hint.is_some(), kind_hint);
    }

    pub(crate) fn reload_document_as(
        &mut self,
        uri: &Url,
        parse_source: bool,
        kind_hint: Option<DocumentKind>,
    ) {
        // Updating the parser role must preserve unsaved editor contents.
        if let Some(doc) = self
            .documents
            .get_mut(uri)
            .filter(|doc| doc.version.is_some())
        {
            if let Some(kind) = kind_hint {
                doc.set_kind_hint(kind);
            }
            return;
        }
        if self.documents.len() >= self.config.indexing.max_files
            && !self.documents.contains_key(uri)
        {
            self.index_truncated.insert("maxFiles".into());
            return;
        }
        let source = uri.to_file_path().ok().and_then(|path| {
            if crate::project::graphics::is_image(&path) && kind_hint.is_none()
                || !parse_source
                    && (crate::compilation::recorder::is_data(&path)
                        || self
                            .project
                            .recorded_binary_inputs
                            .values()
                            .any(|inputs| inputs.contains(uri)))
            {
                return None;
            }
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            if metadata.len() > self.config.indexing.max_file_size {
                self.index_truncated.insert("maxFileSize".into());
                return None;
            }
            let total: u64 = self
                .documents
                .iter()
                .filter(|(other, _)| *other != uri)
                .map(|(_, doc)| doc.text.source.len() as u64)
                .sum();
            if total.saturating_add(metadata.len()) > self.config.indexing.max_total_bytes {
                self.index_truncated.insert("maxTotalBytes".into());
                return None;
            }
            std::fs::read_to_string(path).ok()
        });
        if let Some(source) = source {
            self.documents.insert(
                uri.clone(),
                Document::new_with_kind(uri, source, None, kind_hint),
            );
        } else {
            self.documents.remove(uri);
        }
    }

    pub(crate) fn refresh_paths(&mut self, paths: &[PathBuf]) -> BTreeSet<Url> {
        if paths.iter().any(|path| {
            path.file_name()
                .is_some_and(|name| name == ".texquill.json")
        }) {
            crate::config::project_config::invalidate();
            self.project.lint_diagnostics.clear();
            for uri in self.documents.keys().cloned().collect::<Vec<_>>() {
                self.load_dependencies(&uri, 0);
            }
        }
        let matches = |uri: &Url| {
            uri.to_file_path()
                .is_ok_and(|path| paths.iter().any(|changed| path.starts_with(changed)))
        };
        let mut affected: BTreeSet<_> = self
            .documents
            .keys()
            .filter(|uri| matches(uri))
            .cloned()
            .collect();
        for uri in &affected {
            self.reload(uri);
        }
        for path in paths {
            if path.is_dir() {
                self.scan(path, 0);
            } else if is_source(path) {
                if let Ok(uri) = Url::from_file_path(path) {
                    self.reload(&uri);
                    affected.insert(uri);
                }
            }
        }
        affected.extend(self.documents.keys().filter(|uri| matches(uri)).cloned());
        let mut refresh = affected.clone();
        refresh.extend(
            self.saved_projects()
                .into_iter()
                .filter(|(_, project)| project.iter().any(&matches))
                .map(|(root, _)| root),
        );
        for uri in &refresh {
            self.load_dependencies(uri, 0);
        }
        affected.extend(self.documents.keys().filter(|uri| matches(uri)).cloned());
        // A non-source file can select a different conditional-load hook.
        self.refresh_context();
        affected
    }

    pub(crate) fn close(&mut self, uri: &Url) {
        let previous_hint = self.documents.remove(uri).and_then(|doc| doc.kind_hint);
        let kind_hint = previous_hint.or_else(|| self.recorded_kind(uri));
        // Discard unsaved text while retaining the independently known role.
        self.reload_document_as(uri, kind_hint.is_some(), kind_hint);
        self.refresh_context();
    }

    pub fn open(&mut self, uri: Url, source: String, version: i32) {
        self.open_with_language(uri, source, version, "");
    }

    pub fn open_with_language(
        &mut self,
        uri: Url,
        source: String,
        version: i32,
        language_id: &str,
    ) {
        let uri = document_uri(uri);
        if self.roots.is_empty() {
            if let Ok(path) = uri.to_file_path() {
                if let Some(parent) = path.parent() {
                    // The editor text below may differ from the saved root.
                    // Scan once, then resolve contexts after installing that
                    // text and loading its dependencies, rather than first
                    // analyzing a disk-only project which is immediately stale.
                    self.roots.insert(parent.to_path_buf());
                    self.scan(parent, 0);
                }
            }
        }
        let kind_hint = self
            .documents
            .get(&uri)
            .and_then(|doc| doc.kind_hint)
            .or_else(|| self.recorded_kind(&uri));
        self.documents.insert(
            uri.clone(),
            Document::new_with_language(
                &uri,
                source,
                Some(version),
                kind_hint,
                DocumentKind::from_language_id(language_id),
            ),
        );
        self.load_dependencies(&uri, 0);
    }

    pub fn load_dependencies(&mut self, uri: &Url, depth: usize) {
        crate::analysis::AnalysisContext::lexical().run(|_| {
            self.load_dependencies_inner(uri, uri, depth, &mut BTreeSet::new(), false, &mut None);
            self.refresh_context();
        });
    }

    /// Reload each saved dependency before walking its includes: closed child
    /// files may have gained new dependencies since the previous build.
    pub fn refresh_project(&mut self, root: &Url) {
        self.load_dependencies_inner(root, root, 0, &mut BTreeSet::new(), true, &mut None);
        self.refresh_context();
        // Displayed .tex files may already have been scanned as documents.
        // Refresh closed text without following its apparent LaTeX commands.
        for uri in crate::project::code_inputs::inputs(self, root) {
            if self.documents.contains_key(&uri) {
                self.reload(&uri);
            }
        }
    }

    fn load_dependencies_inner(
        &mut self,
        uri: &Url,
        root: &Url,
        depth: usize,
        visited: &mut BTreeSet<Url>,
        refresh: bool,
        inputs: &mut Option<crate::project::source_inputs::Index>,
    ) {
        if depth >= self.config.indexing.max_depth {
            *inputs = None;
            self.index_truncated.insert("maxDepth".into());
            return;
        }
        if !visited.insert(uri.clone()) {
            return;
        }
        if refresh {
            self.reload_document(uri, self.documents.contains_key(uri));
            *inputs = None;
        }
        let mut followed = BTreeSet::new();
        loop {
            // Share a root interpretation until loading or reparsing changes
            // its inputs. Parent loops then rebuild to discover new aliases.
            let index =
                inputs.get_or_insert_with(|| crate::project::source_inputs::Index::new(self, root));
            let dependencies: Vec<_> = self
                .dependencies_with_inputs(uri, root, index)
                .into_iter()
                .filter(|dependency| !followed.contains(dependency))
                .collect();
            if dependencies.is_empty() || followed.len() >= self.config.indexing.max_files {
                break;
            }
            let sources: BTreeMap<_, _> = self
                .source_dependencies_with_inputs(uri, root, index)
                .into_iter()
                .collect();
            for dependency in dependencies {
                if !followed.insert(dependency.clone()) {
                    continue;
                }
                if let Some(kind) = sources.get(&dependency) {
                    if let Some(doc) = self.documents.get(&dependency) {
                        if doc.kind != *kind {
                            *inputs = None;
                        }
                        if doc.kind != *kind || doc.kind_hint != Some(*kind) {
                            self.documents
                                .get_mut(&dependency)
                                .unwrap()
                                .set_kind_hint(*kind);
                        }
                    } else {
                        self.reload_document_as(&dependency, true, Some(*kind));
                        *inputs = None;
                    }
                } else if !self.documents.contains_key(&dependency) {
                    // Recorder-only data stays unparsed; explicit includes
                    // retain their source role even for unusual extensions.
                    self.reload_document(&dependency, false);
                    *inputs = None;
                }
                self.load_dependencies_inner(
                    &dependency,
                    root,
                    depth + 1,
                    visited,
                    refresh,
                    inputs,
                );
            }
            // Newly loaded local declarations can expose another source alias
            // in this parent, even when the parent itself did not change.
        }
    }
}

fn file_uri(path: &Path) -> Result<Url, String> {
    let path = crate::diagnostics::diagnostic_inputs::canonicalize(path)
        .map_err(|error| format!("Cannot read root document {}: {error}", path.display()))?;
    if !path.is_file()
        || !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("tex" | "ltx")
        )
    {
        return Err("Root document must be a saved .tex or .ltx file".into());
    }
    Url::from_file_path(path).map_err(|_| "Cannot convert root document path to URI".into())
}

pub(crate) fn resolve_file_path(
    uri: &Url,
    command: &str,
    value: &str,
    extensions: &[&str],
) -> Option<Url> {
    resolve_file_path_with(uri, command, value, extensions, existing_file)
}

/// Share TeX's filename lookup with the proposed filesystem used for renames.
pub(crate) fn resolve_file_path_with(
    uri: &Url,
    command: &str,
    value: &str,
    extensions: &[&str],
    file: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<Url> {
    let extensions = if crate::bibliography::bib_resources::exact_filename(command) {
        &[""][..]
    } else {
        extensions
    };
    crate::parser::syntax::file_lookup_names(command, value).find_map(|name| {
        extensions
            .iter()
            .find_map(|extension| resolve_path_with(uri, &name, extension, &file))
    })
}

fn existing_file(path: &Path) -> Option<PathBuf> {
    let canonical = crate::diagnostics::diagnostic_inputs::canonicalize(path).ok()?;
    canonical.is_file().then_some(canonical)
}

pub fn resolve_path(uri: &Url, value: &str, extension: &str) -> Option<Url> {
    resolve_path_with(uri, value, extension, &existing_file)
}

fn resolve_path_with(
    uri: &Url,
    value: &str,
    extension: &str,
    file: &impl Fn(&Path) -> Option<PathBuf>,
) -> Option<Url> {
    let mut value = value.to_owned();
    for character in ['_', '%', '#', '&', '$', ' '] {
        value = value.replace(&format!("\\{character}"), &character.to_string());
    }
    if value.is_empty() || value.contains(['\\', '{', '}']) {
        return None;
    }
    let parent = uri.to_file_path().ok()?.parent()?.to_path_buf();
    let path = parent.join(&value);
    let selected = file(&path).or_else(|| {
        if extension.is_empty()
            || path.extension().and_then(|value| value.to_str()) == Some(extension)
        {
            return None;
        }
        let mut name = path.as_os_str().to_os_string();
        name.push(".");
        name.push(extension);
        file(&PathBuf::from(name))
    })?;
    Url::from_file_path(selected).ok()
}
