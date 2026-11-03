//! Root-specific discovery of static source loads and file references.
use crate::analysis::AnalysisContext;
use crate::{
    parser::syntax,
    project::workspace::{resolve_file_path, DocumentKind, Workspace},
    symbols::{
        commands::{CommandSignatures, SourceCall},
        query_cache::source_signatures,
    },
};
use lsp_types::Url;
use std::collections::{BTreeMap, BTreeSet};

const MAX_ROUNDS: usize = 32;
const MAX_SOURCES: usize = 1024;

#[derive(Default)]
pub(crate) struct Query {
    indexes: BTreeMap<(usize, Url), Index>,
    projects: BTreeMap<(usize, Url, Url), BTreeSet<Url>>,
}

#[cfg(test)]
fn with_query_cache<T>(work: impl FnOnce() -> T) -> T {
    AnalysisContext::new().run(|_| work())
}

/// Dependency traversal also resolves files and input roles. Reuse its result
/// only for this immutable query, never across an edit or filesystem event.
pub(crate) fn project_documents(
    workspace: &Workspace,
    source: &Url,
    root: &Url,
    compute: impl FnOnce() -> BTreeSet<Url>,
) -> BTreeSet<Url> {
    let key = (
        workspace as *const Workspace as usize,
        source.clone(),
        root.clone(),
    );
    let Some(context) = AnalysisContext::current_query() else {
        return compute();
    };
    if let Some(project) = context.sources.borrow().projects.get(&key).cloned() {
        return project;
    }
    let project = compute();
    let mut query = context.sources.borrow_mut();
    if query.projects.len() < 16 {
        query.projects.insert(key, project.clone());
    }
    project
}

#[derive(Clone, Default)]
pub(crate) struct Index {
    pub calls: BTreeMap<Url, Vec<SourceCall>>,
    pub loaders: BTreeMap<Url, Vec<SourceCall>>,
    pub forwarders: BTreeMap<Url, Vec<SourceCall>>,
    pub resources: BTreeMap<Url, Vec<SourceCall>>,
    pub resource_references: BTreeMap<Url, Vec<SourceCall>>,
    pub section_labels: BTreeSet<String>,
    pub selectors: BTreeMap<Url, Vec<SourceCall>>,
    pub checks: BTreeMap<Url, Vec<SourceCall>>,
    pub incomplete: bool,
    literal: Option<BTreeMap<Url, Vec<Url>>>,
    context: BTreeSet<Url>,
    signatures: Option<(
        super::documents::Revisions,
        std::sync::Arc<CommandSignatures<'static>>,
    )>,
}

impl Index {
    pub fn new(workspace: &Workspace, root: &Url) -> Self {
        let key = (workspace as *const Workspace as usize, root.clone());
        let Some(context) = AnalysisContext::current_query() else {
            return Self::build(workspace, root);
        };
        if let Some(index) = context.sources.borrow().indexes.get(&key).cloned() {
            return index;
        }
        let index = if let Some(binding) = context
            .binding
            .as_ref()
            .filter(|_| context.persistent_indexes())
        {
            if let Some(index) = binding.index(workspace, root) {
                index
            } else {
                let (index, files) = crate::diagnostics::diagnostic_inputs::Files::capture(|| {
                    AnalysisContext::new().run(|_| Self::build(workspace, root))
                });
                context.observe_files(&files);
                if !context.cancelled() {
                    binding.insert_index(workspace, root, &index, files);
                }
                index
            }
        } else {
            Self::build(workspace, root)
        };
        let mut query = context.sources.borrow_mut();
        if query.indexes.len() < 4 {
            query.indexes.insert(key, index.clone());
        }
        index
    }

    pub(crate) fn cache_bytes(&self) -> usize {
        let calls = [
            &self.calls,
            &self.loaders,
            &self.forwarders,
            &self.resources,
            &self.resource_references,
            &self.selectors,
            &self.checks,
        ];
        std::mem::size_of::<Self>()
            + calls
                .into_iter()
                .flat_map(|calls| calls.iter())
                .map(|(uri, calls)| {
                    uri.as_str().len()
                        + 256
                        + calls.capacity() * std::mem::size_of::<SourceCall>()
                        + calls
                            .iter()
                            .map(|call| {
                                call.command.name.capacity()
                                    + call
                                        .command
                                        .argument
                                        .as_ref()
                                        .map_or(0, |argument| argument.value.capacity() + 128)
                            })
                            .sum::<usize>()
                })
                .sum::<usize>()
            + self
                .context
                .iter()
                .map(|uri| uri.as_str().len() + 128)
                .sum::<usize>()
            + self
                .section_labels
                .iter()
                .map(|label| label.capacity() + 128)
                .sum::<usize>()
            + self
                .literal
                .iter()
                .flat_map(|map| map.iter())
                .map(|(uri, entries)| {
                    uri.as_str().len()
                        + 256
                        + entries
                            .iter()
                            .map(|uri| uri.as_str().len() + 128)
                            .sum::<usize>()
                })
                .sum::<usize>()
            + self
                .signatures
                .as_ref()
                .map_or(0, |(_, signatures)| signatures.cache_bytes())
    }

    fn build(workspace: &Workspace, root: &Url) -> Self {
        let mut index = Self::default();
        let context = AnalysisContext::current();
        let cancelled = || context.as_ref().is_some_and(|context| context.cancelled());
        if cancelled() {
            index.incomplete = true;
            return index;
        }
        let mut scope = index.scope(workspace, root);
        let mut seen = BTreeSet::new();
        for _ in 0..MAX_ROUNDS {
            if cancelled() {
                index.incomplete = true;
                break;
            }
            if index.incomplete || !seen.insert(scope.clone()) {
                break;
            }
            let signatures = source_signatures(workspace, &scope);
            let mut literal = BTreeMap::new();
            let mut loader_calls = BTreeMap::new();
            let mut forwarders = BTreeMap::new();
            let mut resources = BTreeMap::new();
            let mut resource_references = BTreeMap::new();
            let mut selectors = BTreeMap::new();
            let mut checks = BTreeMap::new();
            index.calls = scope
                .iter()
                .filter_map(|uri| {
                    let doc = workspace.documents.get(uri)?;
                    if doc.kind != DocumentKind::Latex {
                        return None;
                    }
                    let positions =
                        signatures.source_command_positions(&doc.text.source, &doc.syntax);
                    literal.insert(
                        uri.clone(),
                        workspace
                            .literal_source_dependencies_filtered(uri, root, |command| {
                                positions.contains(&command.span.start)
                            })
                            .into_iter()
                            .map(|(uri, _)| uri)
                            .collect(),
                    );
                    let loaders = signatures.indirect_loader_invocations(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    if !loaders.is_empty() {
                        loader_calls.insert(uri.clone(), loaders);
                    }
                    let options = signatures.indirect_option_invocations(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    if !options.is_empty() {
                        forwarders.insert(uri.clone(), options);
                    }
                    let conditional = signatures.indirect_conditional_checks(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    if !conditional.is_empty() {
                        checks.insert(uri.clone(), conditional);
                    }
                    let selection = signatures.indirect_selection_invocations(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    if !selection.is_empty() {
                        selectors.insert(uri.clone(), selection);
                    }
                    let bibliography = signatures.indirect_bibliography_invocations(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    if !bibliography.is_empty() {
                        resources.insert(uri.clone(), bibliography);
                    }
                    let references = signatures.indirect_bibliography_references(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    if !references.is_empty() {
                        resource_references.insert(uri.clone(), references);
                    }
                    let calls = signatures.indirect_source_invocations(
                        &doc.text.source,
                        &doc.syntax,
                        &positions,
                    );
                    (!calls.is_empty()).then_some((uri.clone(), calls))
                })
                .collect();
            match crate::bibliography::bib_section_files::calls(
                workspace,
                root,
                &scope,
                &signatures,
            ) {
                Some(selection) => {
                    index.section_labels = selection.labels;
                    for (uri, calls) in selection.calls {
                        resource_references
                            .entry(uri.clone())
                            .or_default()
                            .extend(calls.clone());
                        resources.entry(uri).or_default().extend(calls);
                    }
                }
                None => index.incomplete = true,
            }
            index.loaders = loader_calls;
            index.forwarders = forwarders;
            index.resources = resources;
            index.resource_references = resource_references;
            index.selectors = selectors;
            index.checks = checks;
            index.literal = Some(literal);
            let next = index.scope(workspace, root);
            if !index.incomplete && next == scope {
                // Loading dependencies mutates the workspace between reads, so
                // it cannot use a workspace query cache. Keep this pure source
                // grammar only while every document revision still agrees.
                if AnalysisContext::current_query().is_none()
                    && signatures.cache_bytes() <= 8 * 1024 * 1024
                {
                    index.signatures = Some((workspace.documents.revisions(), signatures));
                }
                index.context = scope;
                return index;
            }
            scope = next;
        }
        // Definitions discovered through a call may invalidate that very call.
        // Do not select an arbitrary interpretation of an oscillating graph.
        Self {
            calls: BTreeMap::new(),
            loaders: BTreeMap::new(),
            forwarders: BTreeMap::new(),
            resources: BTreeMap::new(),
            resource_references: BTreeMap::new(),
            section_labels: BTreeSet::new(),
            selectors: BTreeMap::new(),
            checks: BTreeMap::new(),
            incomplete: true,
            literal: None,
            context: BTreeSet::new(),
            signatures: None,
        }
    }

    fn source_signatures(
        &self,
        workspace: &Workspace,
        scope: &BTreeSet<Url>,
    ) -> std::sync::Arc<CommandSignatures<'static>> {
        if let Some((revisions, signatures)) = &self.signatures {
            if *scope == self.context && revisions.current(&workspace.documents) {
                return signatures.clone();
            }
        }
        source_signatures(workspace, scope)
    }

    /// Potential literal source paths retain unknown and deferred execution,
    /// while unchanged quoting primitives cannot import executable source.
    pub fn literal_dependencies(
        &self,
        workspace: &Workspace,
        uri: &Url,
        root: &Url,
    ) -> Vec<(Url, DocumentKind)> {
        let quoted = self.quoted_file_positions(workspace, uri);
        workspace.literal_source_dependencies_filtered(uri, root, |command| {
            !(editable_file_command(&command.name)
                || syntax::loader_extension(&command.name).is_some())
                || !quoted.contains(&command.span.start)
        })
    }

    /// Tokens proven to be printed by unchanged primitives. An empty set
    /// preserves potential file lookups when the execution context is uncertain.
    pub fn quoted_file_positions(&self, workspace: &Workspace, uri: &Url) -> BTreeSet<usize> {
        if self.incomplete
            || !workspace.documents.get(uri).is_some_and(|doc| {
                doc.syntax.commands.iter().any(|command| {
                    matches!(
                        command.name.as_str(),
                        "detokenize" | "string" | "meaning" | "csuse" | "endcsname"
                    )
                })
            })
        {
            return BTreeSet::new();
        }
        let mut scope = self.context.clone();
        scope.insert(uri.clone());
        let Some(documents) = bounded_latex_sources(workspace, &scope) else {
            return BTreeSet::new();
        };
        // A deferred rebinding or a dynamic source can change a primitive's
        // meaning before the apparent quote executes. Keep the established
        // potential dependencies whenever that binding cannot be confirmed.
        // Use the source graph's literal-name rules. Comments around a name
        // are harmless, while every package-list item must remain static.
        let dynamic = |source: &str, parsed: &syntax::Syntax, command: &syntax::Command| {
            let command = parsed.potential_file_command(source, command);
            if syntax::file_list(&command.name) {
                syntax::complete_file_list_spans(source, &command).is_none()
            } else {
                syntax::file_argument(source, &command)
                    .is_none_or(|argument| !syntax::is_literal_file_path(&argument.value))
            }
        };
        if documents.iter().any(|(_, doc)| {
            doc.syntax
                .definitions
                .iter()
                .any(|entry| matches!(entry.name.as_str(), "detokenize" | "string" | "meaning"))
                || doc
                    .syntax
                    .commands
                    .iter()
                    .enumerate()
                    .any(|(index, command)| {
                        // LaTeX's private input APIs are outside the static source
                        // graph and can install a different quoting primitive.
                        // Unresolved named calls can invoke those same APIs.
                        let invoked = match command.name.as_str() {
                            "csuse" => command
                                .argument
                                .as_ref()
                                .filter(|argument| syntax::is_static_key(&argument.value))
                                .map(|argument| argument.value.as_str()),
                            "csname" => syntax::literal_csname(
                                &doc.text.source,
                                &doc.syntax.commands,
                                index,
                            )
                            .map(|span| &doc.text.source[span]),
                            name => Some(name),
                        };
                        matches!(invoked, None | Some("@input" | "@input@" | "@@input"))
                            || matches!(
                                command.name.as_str(),
                                "let" | "futurelet" | "cslet" | "letcs" | "csletcs"
                            )
                            || (matches!(
                                command.name.as_str(),
                                "input" | "include" | "subfile" | "InputIfFileExists"
                            ) || syntax::loader_extension(&command.name).is_some())
                                && dynamic(&doc.text.source, &doc.syntax, command)
                    })
        }) || self
            .calls
            .iter()
            .chain(self.loaders.iter())
            .any(|(uri, calls)| {
                workspace.documents.get(uri).is_none_or(|doc| {
                    calls
                        .iter()
                        .any(|call| dynamic(&doc.text.source, &doc.syntax, &call.command))
                })
            })
        {
            return BTreeSet::new();
        }
        let Some(doc) = workspace.documents.get(uri) else {
            return BTreeSet::new();
        };
        let signatures = self.source_signatures(workspace, &scope);
        // Potential input in deferred code or a macro operand may execute a
        // provider which the active graph could not inspect. Retokenization
        // and displayed-code APIs can also execute otherwise opaque input.
        if documents.iter().any(|(_, source_doc)| {
            let potential = |command: &&syntax::Command| {
                matches!(
                    command.name.as_str(),
                    "input" | "include" | "subfile" | "InputIfFileExists" | "scantokens"
                ) || syntax::loader_extension(&command.name).is_some()
                    || syntax::code_input_command(&command.name)
            };
            if !source_doc
                .syntax
                .commands
                .iter()
                .any(|command| potential(&command))
            {
                return false;
            }
            let active =
                signatures.source_command_positions(&source_doc.text.source, &source_doc.syntax);
            let opaque =
                signatures.quoted_command_tokens(&source_doc.text.source, &source_doc.syntax);
            source_doc
                .syntax
                .commands
                .iter()
                .filter(potential)
                .any(|command| {
                    !opaque.contains(&command.span.start)
                        && (command.name == "scantokens"
                            || syntax::code_input_command(&command.name)
                            || !active.contains(&command.span.start))
                })
        }) {
            return BTreeSet::new();
        }
        signatures.quoted_command_tokens(&doc.text.source, &doc.syntax)
    }

    pub fn resource_dependencies(
        &self,
        workspace: &Workspace,
        uri: &Url,
        root: &Url,
    ) -> Vec<(Url, DocumentKind)> {
        let Some(doc) = workspace.documents.get(uri) else {
            return vec![];
        };
        self.resources
            .get(uri)
            .into_iter()
            .flatten()
            .flat_map(|call| resource_targets(&doc.text.source, uri, root, call))
            .collect()
    }

    fn scope(&mut self, workspace: &Workspace, root: &Url) -> BTreeSet<Url> {
        let mut result = BTreeSet::new();
        let mut pending = vec![root.clone()];
        while let Some(uri) = pending.pop() {
            if result.contains(&uri) {
                continue;
            }
            if result.len() >= MAX_SOURCES {
                self.incomplete = true;
                break;
            }
            result.insert(uri.clone());
            if let Some(literal) = &self.literal {
                pending.extend(literal.get(&uri).into_iter().flatten().cloned());
            } else {
                pending.extend(
                    workspace
                        .literal_source_dependencies_with_kinds(&uri, root)
                        .into_iter()
                        .map(|(uri, _)| uri),
                );
            }
            pending.extend(self.dependencies(workspace, &uri, root));
        }
        result
    }

    pub fn dependencies(&self, workspace: &Workspace, uri: &Url, root: &Url) -> Vec<Url> {
        self.calls
            .get(uri)
            .into_iter()
            .flatten()
            .filter_map(|call| target(uri, root, call))
            .chain(
                self.loaders
                    .get(uri)
                    .into_iter()
                    .flatten()
                    .flat_map(|call| {
                        workspace.documents.get(uri).map_or_else(Vec::new, |doc| {
                            loader_targets(&doc.text.source, uri, root, call)
                        })
                    }),
            )
            .collect()
    }
}

/// Bound project macro analysis before constructing its signature context.
pub(crate) fn bounded_latex_sources<'a>(
    workspace: &'a Workspace,
    scope: &BTreeSet<Url>,
) -> Option<Vec<(Url, &'a crate::project::workspace::Document)>> {
    let documents: Vec<_> = scope
        .iter()
        .filter_map(|uri| {
            let doc = workspace.documents.get(uri)?;
            (doc.kind == DocumentKind::Latex).then_some((uri.clone(), doc))
        })
        .collect();
    if documents.is_empty() || documents.len() > MAX_SOURCES {
        return None;
    }
    let mut bytes = 0usize;
    for (_, doc) in &documents {
        if doc.text.source.len() > 2 * 1024 * 1024 {
            return None;
        }
        bytes += doc.text.source.len();
        if bytes > 16 * 1024 * 1024 {
            return None;
        }
    }
    Some(documents)
}

/// Shared interpretation of chapter selection for build conditions and repairs.
/// Data resources cannot contribute executable selectors or macro declarations.
pub(crate) fn selection_filters(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
) -> Option<Vec<BTreeSet<String>>> {
    let documents = bounded_latex_sources(workspace, scope)?;
    let signatures = source_signatures(workspace, scope);
    let mut filters = Vec::new();
    for (_, doc) in documents {
        filters.extend(signatures.selection_filters(&doc.text.source, &doc.syntax)?);
    }
    Some(filters)
}

/// Direct file calls that can certify lookup conditions or a displayed-data role.
/// The caller includes recorder-discovered sources in this existing scope.
pub(crate) fn active_direct_file_positions(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
) -> BTreeMap<Url, BTreeSet<usize>> {
    direct_file_positions(workspace, scope, true)
}

/// Include active overrides so data consumers can reject uncertain execution.
pub(crate) fn possible_direct_file_positions(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
) -> BTreeMap<Url, BTreeSet<usize>> {
    direct_file_positions(workspace, scope, false)
}

fn direct_file_positions(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
    require_original_meaning: bool,
) -> BTreeMap<Url, BTreeSet<usize>> {
    let Some(documents) = bounded_latex_sources(workspace, scope) else {
        return BTreeMap::new();
    };
    let signatures = source_signatures(workspace, scope);
    documents
        .into_iter()
        .map(|(uri, doc)| {
            let positions = signatures.source_command_positions(&doc.text.source, &doc.syntax);
            let active = doc
                .syntax
                .commands
                .iter()
                .filter(|command| {
                    (matches!(
                        command.name.as_str(),
                        "input"
                            | "include"
                            | "subfile"
                            | "InputIfFileExists"
                            | "IfFileExists"
                            | "includegraphics"
                    ) || syntax::code_input_command(&command.name)
                        || crate::bibliography::bib_resources::references_file(&command.name))
                        && (!require_original_meaning
                            || signatures.command_unchanged(&command.name))
                        && positions.contains(&command.span.start)
                })
                .map(|command| command.span.start)
                .collect();
            (uri, active)
        })
        .collect()
}

/// Source paths which retain their built-in file-reference meaning.
pub(crate) fn source_file_command(name: &str) -> bool {
    matches!(
        name,
        "input"
            | "include"
            | "subfile"
            | "InputIfFileExists"
            | "IfFileExists"
            | "includeonly"
            | "loadglsentries"
    )
}

/// Source, bibliography and package/class paths share editing checks.
pub(crate) fn editable_file_command(name: &str) -> bool {
    source_file_command(name)
        || crate::bibliography::bib_resources::references_file(name)
        || syntax::package_class_extension(name).is_some()
        || matches!(
            name,
            "bibliography" | "nobibliography" | "bibliographystyle"
        )
}

/// Literal references remain editable inside deferred macro code, but not
/// when consumed as opaque arguments or when the file command is overridden.
pub(crate) fn editable_file_positions(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
) -> BTreeMap<Url, BTreeSet<usize>> {
    let Some(documents) = bounded_latex_sources(workspace, scope) else {
        return BTreeMap::new();
    };
    let signatures = source_signatures(workspace, scope);
    documents
        .into_iter()
        .map(|(uri, doc)| {
            let operands = signatures.argument_tokens(&doc.text.source, &doc.syntax);
            let positions = doc
                .syntax
                .commands
                .iter()
                .filter(|command| {
                    editable_file_command(&command.name)
                        && signatures.command_unchanged(&command.name)
                        && !operands.contains(&command.span.start)
                })
                .map(|command| command.span.start)
                .collect();
            (uri, positions)
        })
        .collect()
}

pub(crate) fn target(uri: &Url, root: &Url, call: &SourceCall) -> Option<Url> {
    let argument = call.command.argument.as_ref()?;
    if !call.certain || !syntax::is_literal_file_path(&argument.value) {
        return None;
    }
    resolve_file_path(root, &call.command.name, &argument.value, &["tex"])
        .or_else(|| resolve_file_path(uri, &call.command.name, &argument.value, &["tex"]))
}

/// Resolve complete literal package lists or a single literal class name.
fn loader_targets(source: &str, uri: &Url, root: &Url, call: &SourceCall) -> Vec<Url> {
    if !call.certain {
        return vec![];
    }
    let Some(extension) = syntax::loader_extension(&call.command.name) else {
        return vec![];
    };
    let names = if syntax::file_list(&call.command.name) {
        syntax::complete_file_list_spans(source, &call.command)
            .into_iter()
            .flatten()
            .map(|span| source[span].to_owned())
            .collect::<Vec<_>>()
    } else {
        syntax::file_argument(source, &call.command)
            .filter(|argument| syntax::is_literal_file_path(&argument.value))
            .map(|argument| vec![argument.value.to_string()])
            .unwrap_or_default()
    };
    names
        .into_iter()
        .filter_map(|name| {
            resolve_file_path(root, &call.command.name, &name, &[extension])
                .or_else(|| resolve_file_path(uri, &call.command.name, &name, &[extension]))
        })
        .collect()
}

/// Keep resource roles even when an alias uses an unconventional suffix.
fn resource_targets(
    source: &str,
    uri: &Url,
    root: &Url,
    call: &SourceCall,
) -> Vec<(Url, DocumentKind)> {
    if !call.certain {
        return vec![];
    }
    let (extension, kind) = match call.command.name.as_str() {
        "bibliography" | "nobibliography" => ("bib", DocumentKind::Bibtex),
        name if crate::bibliography::bib_resources::loads(name) => ("bib", DocumentKind::Bibtex),
        "bibliographystyle" => ("bst", DocumentKind::BibtexStyle),
        _ => return vec![],
    };
    let names = if syntax::file_list(&call.command.name) {
        syntax::file_list_spans(source, &call.command)
            .unwrap_or_default()
            .into_iter()
            .map(|span| source[span].to_owned())
            .collect::<Vec<_>>()
    } else {
        syntax::file_argument(source, &call.command)
            .map(|argument| vec![argument.value.clone()])
            .unwrap_or_default()
    };
    names
        .into_iter()
        .filter_map(|name| {
            resolve_file_path(root, &call.command.name, &name, &[extension])
                .or_else(|| resolve_file_path(uri, &call.command.name, &name, &[extension]))
                .map(|uri| (uri, kind))
        })
        .collect()
}

#[cfg(test)]
mod selection_filter_tests {
    use super::*;
    use crate::project::workspace::Document;

    #[test]
    fn traversal_signatures_reuse_only_current_revisions_and_the_exact_source_scope() {
        let uri = Url::parse("untitled:traversal.tex").unwrap();
        let mut workspace = Workspace::default();
        workspace.documents.insert(
            uri.clone(),
            Document::new(&uri, r"\def\forward{\input}".into(), Some(1)),
        );
        let index = Index::new(&workspace, &uri);
        let scope = BTreeSet::from([uri.clone()]);
        let first = index.source_signatures(&workspace, &scope);
        assert!(std::sync::Arc::ptr_eq(
            &first,
            &index.source_signatures(&workspace, &scope)
        ));
        assert_eq!(first.forwarded_builtin("forward"), Some("input"));
        assert!(index
            .source_signatures(&workspace, &BTreeSet::new())
            .command_unchanged("forward"));
        *workspace.documents.get_mut(&uri).unwrap() =
            Document::new(&uri, r"\def\forward{\include}".into(), Some(2));
        let changed = index.source_signatures(&workspace, &scope);
        assert!(!std::sync::Arc::ptr_eq(&first, &changed));
        assert_eq!(changed.forwarded_builtin("forward"), Some("include"));
        assert_eq!(first.forwarded_builtin("forward"), Some("input"));
    }

    #[test]
    fn query_graphs_do_not_survive_document_or_filesystem_changes() {
        let directory = tempfile::tempdir().unwrap();
        let main = Url::from_file_path(directory.path().join("main.tex")).unwrap();
        let child = Url::from_file_path(directory.path().join("child.tex")).unwrap();
        let mut workspace = Workspace::default();
        workspace.documents.insert(
            main.clone(),
            Document::new(&main, r"\input{child}".into(), Some(1)),
        );
        assert!(!with_query_cache(|| workspace.project_documents(&main)).contains(&child));
        std::fs::write(child.to_file_path().unwrap(), "Child").unwrap();
        assert!(with_query_cache(|| workspace.project_documents(&main)).contains(&child));
        workspace.documents.insert(
            main.clone(),
            Document::new(&main, "No input".into(), Some(2)),
        );
        assert!(!with_query_cache(|| workspace.project_documents(&main)).contains(&child));
    }

    #[test]
    fn query_projects_distinguish_subtrees_workspaces_and_nested_queries() {
        let directory = tempfile::tempdir().unwrap();
        let main = Url::from_file_path(directory.path().join("main.tex")).unwrap();
        let child = Url::from_file_path(directory.path().join("child.tex")).unwrap();
        let leaf = Url::from_file_path(directory.path().join("leaf.tex")).unwrap();
        let mut workspace = Workspace::default();
        for (uri, source) in [
            (&main, r"\input{child}"),
            (&child, r"\input{leaf}"),
            (&leaf, "Leaf"),
        ] {
            std::fs::write(uri.to_file_path().unwrap(), source).unwrap();
            workspace
                .documents
                .insert(uri.clone(), Document::new(uri, source.into(), Some(1)));
        }
        let mut other = workspace.clone();
        other.documents.insert(
            main.clone(),
            Document::new(&main, "No input".into(), Some(2)),
        );
        with_query_cache(|| {
            let project = workspace.project_documents(&main);
            assert_eq!(
                project,
                BTreeSet::from([main.clone(), child.clone(), leaf.clone()])
            );
            assert_eq!(
                workspace.project_documents_from(&child, &main),
                BTreeSet::from([child.clone(), leaf.clone()])
            );
            assert_eq!(
                other.project_documents(&main),
                BTreeSet::from([main.clone()])
            );
            with_query_cache(|| {
                assert_eq!(
                    other.project_documents(&main),
                    BTreeSet::from([main.clone()])
                )
            });
            assert_eq!(workspace.project_documents(&main), project);
        });
    }

    fn fixture(source: &str) -> (Workspace, BTreeSet<Url>) {
        let main = Url::parse("file:///tmp/texquill-selection-unit/main.tex").unwrap();
        let mut workspace = Workspace::default();
        workspace
            .documents
            .insert(main.clone(), Document::new(&main, source.into(), Some(7)));
        (workspace, BTreeSet::from([main]))
    }
    fn call(form: &str) -> &'static str {
        match form {
            "direct" => r"\includeonly",
            "named" => r"\csuse{includeonly}",
            "literal" => r"\csname includeonly\endcsname",
            "alias" => r"\choose",
            "named-alias" => r"\csuse{choose}",
            "literal-alias" => r"\csname choose\endcsname",
            _ => panic!(),
        }
    }

    #[test]
    fn selection_filters_use_known_aliases_across_sources_and_ignore_data_declarations() {
        for form in [
            "direct",
            "named",
            "literal",
            "alias",
            "named-alias",
            "literal-alias",
        ] {
            let source = format!("{}{{first,% keep 😀\r\nsecond.tex% tail\r\n}}", call(form));
            let (mut workspace, mut scope) = fixture(&source);
            let provider =
                Url::parse("file:///tmp/texquill-selection-unit/definitions.sty").unwrap();
            workspace.documents.insert(
                provider.clone(),
                Document::new(&provider, r"\def\choose{\includeonly}".into(), None),
            );
            scope.insert(provider);
            let data = Url::parse("file:///tmp/texquill-selection-unit/resource.tex").unwrap();
            let mut doc = Document::new(&data, r"\renewcommand{\includeonly}[1]{}".into(), None);
            doc.kind = DocumentKind::BibtexStyle;
            workspace.documents.insert(data.clone(), doc);
            scope.insert(data.clone());
            assert_eq!(
                selection_filters(&workspace, &scope),
                Some(vec![BTreeSet::from(["first".into(), "second".into()])]),
                "{form}"
            );
            workspace.documents.get_mut(&data).unwrap().kind = DocumentKind::Latex;
            assert!(selection_filters(&workspace, &scope).is_none());
        }
    }

    #[test]
    fn uncertain_or_unsupported_selectors_are_not_treated_as_an_absence_of_filters() {
        for body in [
            r"\includeonly{selected,\name}",
            r"\includeonly*{selected}",
            r"\includeonly[note]{selected}",
            r"\csuse{\name}{selected}",
            r"\csname \name\endcsname{selected}",
            r"\renewcommand{\includeonly}[1]{}\choose{selected}",
            r"\renewcommand{\csuse}[1]{}\includeonly{selected}",
            r"\def\choose{\includeonly}\choose{selected}",
            r"\edef\expanded{\includeonly}\expanded{selected}",
            r"\newcommand{\wrapped}{\includeonly{selected}}\wrapped",
            r"\newcommand{\hold}[1]{}\hold{\csuse{includeonly}{selected}}",
        ] {
            let source = format!("\\def\\choose{{\\includeonly}}\n{body}");
            let (workspace, scope) = fixture(&source);
            assert!(selection_filters(&workspace, &scope).is_none(), "{body}");
        }
    }

    #[test]
    fn selection_filters_preserve_empty_selection_and_multiple_static_constraints() {
        for (source, expected) in [
            (r"\def\choose{\includeonly}", vec![]),
            (r"\def\choose{\includeonly}\choose{}", vec![BTreeSet::new()]),
            (
                r"\def\choose{\includeonly}\includeonly{first,second}\choose{second.tex,third}",
                vec![
                    BTreeSet::from(["first".into(), "second".into()]),
                    BTreeSet::from(["second".into(), "third".into()]),
                ],
            ),
        ] {
            let (workspace, scope) = fixture(source);
            assert_eq!(
                selection_filters(&workspace, &scope),
                Some(expected),
                "{source}"
            );
        }
    }

    #[test]
    fn selection_filter_analysis_respects_source_count_and_text_budgets() {
        let (mut workspace, mut scope) = fixture(r"\includeonly{selected}");
        for index in 0..MAX_SOURCES {
            let uri = Url::parse(&format!(
                "file:///tmp/texquill-selection-unit/source-{index}.tex"
            ))
            .unwrap();
            workspace
                .documents
                .insert(uri.clone(), Document::new(&uri, String::new(), None));
            scope.insert(uri);
        }
        assert!(selection_filters(&workspace, &scope).is_none());
        let (workspace, scope) = fixture(&"%".repeat(2 * 1024 * 1024 + 1));
        assert!(selection_filters(&workspace, &scope).is_none());
        let (mut workspace, mut scope) = fixture(r"\includeonly{selected}");
        for index in 0..8 {
            let uri = Url::parse(&format!(
                "file:///tmp/texquill-selection-unit/big-{index}.tex"
            ))
            .unwrap();
            workspace.documents.insert(
                uri.clone(),
                Document::new(&uri, "%".repeat(2 * 1024 * 1024), None),
            );
            scope.insert(uri);
        }
        assert!(selection_filters(&workspace, &scope).is_none());
    }
}
