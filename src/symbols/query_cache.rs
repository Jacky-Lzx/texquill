//! Query-local symbol indexes and revision-checked shared command grammars.
use super::{label_declarations, named_references};
use crate::{
    analysis::AnalysisContext,
    project::workspace::{DocumentKind, Workspace},
    symbols::commands::CommandSignatures,
};
use lsp_types::Url;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const MAX_ENTRIES: usize = 4;
const MAX_BYTES: usize = 8 * 1024 * 1024;
type Named = BTreeMap<(usize, Url), Arc<named_references::Index>>;
type Labels = BTreeMap<(usize, BTreeSet<Url>), Arc<label_declarations::Index>>;
type Signatures = BTreeMap<(usize, BTreeSet<Url>), Arc<CommandSignatures<'static>>>;

#[derive(Default)]
pub(crate) struct Cache {
    named: Named,
    labels: Labels,
    signatures: Signatures,
    source_signatures: Signatures,
    bytes: usize,
}

#[cfg(test)]
fn with_cache<T>(work: impl FnOnce() -> T) -> T {
    AnalysisContext::new().run(|_| work())
}

fn shared<K: Ord, V>(
    context: &AnalysisContext,
    key: K,
    select: impl Fn(&mut Cache) -> &mut BTreeMap<K, Arc<V>>,
    build: impl FnOnce() -> Arc<V>,
    bytes: impl FnOnce(&V) -> usize,
) -> Arc<V> {
    if let Some(index) = select(&mut context.symbols.borrow_mut()).get(&key).cloned() {
        return index;
    }
    // Recursive analyses must not retain a cache borrow while computing.
    let index = build();
    let mut cache = context.symbols.borrow_mut();
    let bytes = bytes(&index);
    if select(&mut cache).len() < MAX_ENTRIES && bytes <= MAX_BYTES - cache.bytes {
        select(&mut cache).insert(key, index.clone());
        cache.bytes += bytes;
    }
    index
}

pub(crate) fn named(
    workspace: &Workspace,
    origin: &Url,
    build: impl FnOnce() -> named_references::Index,
) -> Arc<named_references::Index> {
    let Some(context) = AnalysisContext::current_query() else {
        return Arc::new(build());
    };
    shared(
        &context,
        (workspace as *const Workspace as usize, origin.clone()),
        |cache| &mut cache.named,
        || Arc::new(build()),
        |index| {
            index
                .cache_bytes()
                .saturating_add(origin.as_str().len() + 64)
        },
    )
}

pub(crate) fn labels(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
    build: impl FnOnce() -> label_declarations::Index,
) -> Arc<label_declarations::Index> {
    let Some(context) = AnalysisContext::current_query() else {
        return Arc::new(build());
    };
    let key_bytes = scope
        .iter()
        .map(|uri| uri.as_str().len() + 64)
        .sum::<usize>();
    shared(
        &context,
        (workspace as *const Workspace as usize, scope.clone()),
        |cache| &mut cache.labels,
        || Arc::new(build()),
        |index| index.cache_bytes().saturating_add(key_bytes),
    )
}

pub(crate) fn signatures(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
    build: impl FnOnce() -> CommandSignatures<'static>,
) -> Arc<CommandSignatures<'static>> {
    let Some(context) = AnalysisContext::current_query() else {
        return Arc::new(build());
    };
    let key_bytes = scope
        .iter()
        .map(|uri| uri.as_str().len() + 64)
        .sum::<usize>();
    shared(
        &context,
        (workspace as *const Workspace as usize, scope.clone()),
        |cache| &mut cache.signatures,
        || persistent_signatures(&context, workspace, scope, false, build),
        |signatures| signatures.cache_bytes().saturating_add(key_bytes),
    )
}

fn persistent_signatures(
    context: &AnalysisContext,
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
    source: bool,
    build: impl FnOnce() -> CommandSignatures<'static>,
) -> Arc<CommandSignatures<'static>> {
    if let Some(value) = context
        .binding
        .as_ref()
        .and_then(|binding| binding.cache.grammar(workspace, scope, source))
    {
        return value;
    }
    let value = build();
    if let Some(binding) = context.binding.as_ref().filter(|_| !context.cancelled()) {
        binding
            .cache
            .intern(workspace, scope, source, value, binding.revision)
    } else {
        Arc::new(value)
    }
}

/// Use the same ordered LaTeX sources for every analysis of a project scope.
/// Unbound callers keep borrowed keys; retained analyses own theirs. Pure
/// grammars can also be reused between mutations after checking every source.
pub(crate) fn project_signatures<'a>(
    workspace: &'a Workspace,
    scope: &BTreeSet<Url>,
) -> Arc<CommandSignatures<'a>> {
    let build = || {
        CommandSignatures::new(
            scope
                .iter()
                .filter_map(|uri| workspace.documents.get(uri))
                .filter(|doc| doc.kind == DocumentKind::Latex)
                .map(|doc| (doc.text.source.as_str(), &doc.syntax)),
        )
    };
    if AnalysisContext::current_query().is_none() {
        if let Some(context) =
            AnalysisContext::current().filter(|context| context.binding.is_some())
        {
            persistent_signatures(&context, workspace, scope, false, || build().into_owned())
        } else {
            Arc::new(build())
        }
    } else {
        signatures(workspace, scope, || build().into_owned())
    }
}

/// Source execution uses a filtered grammar, distinct from editing signatures.
/// Retained grammars certify each source revision; mutable updates never reuse
/// a workspace index just because one of these pure grammars still agrees.
pub(crate) fn source_signatures(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
) -> Arc<CommandSignatures<'static>> {
    let build = || {
        CommandSignatures::source_context(
            scope
                .iter()
                .filter_map(|uri| workspace.documents.get(uri))
                .filter(|doc| doc.kind == DocumentKind::Latex)
                .map(|doc| (doc.text.source.as_str(), &doc.syntax)),
        )
    };
    let Some(context) = AnalysisContext::current_query()
        .or_else(|| AnalysisContext::current().filter(|context| context.binding.is_some()))
    else {
        return Arc::new(build().into_owned());
    };
    let key_bytes = scope
        .iter()
        .map(|uri| uri.as_str().len() + 64)
        .sum::<usize>();
    let build = || {
        persistent_signatures(&context, workspace, scope, true, || {
            let preliminary = project_signatures(workspace, scope);
            CommandSignatures::source_context_with(
                scope
                    .iter()
                    .filter_map(|uri| workspace.documents.get(uri))
                    .filter(|doc| doc.kind == DocumentKind::Latex)
                    .map(|doc| (doc.text.source.as_str(), &doc.syntax)),
                &preliminary,
            )
            .into_owned()
        })
    };
    if AnalysisContext::current_query().is_none() {
        return build();
    }
    shared(
        &context,
        (workspace as *const Workspace as usize, scope.clone()),
        |cache| &mut cache.source_signatures,
        build,
        |signatures| signatures.cache_bytes().saturating_add(key_bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::workspace::Document;

    fn document(workspace: &mut Workspace, uri: &Url, source: &str) {
        workspace
            .documents
            .insert(uri.clone(), Document::new(uri, source.into(), Some(1)));
    }

    #[test]
    fn named_queries_share_work_but_refresh_meanings_and_restore_nested_scopes() {
        let uri = Url::parse("untitled:query.tex").unwrap();
        let source = r"\label{key}\ref{key}";
        let mut workspace = Workspace::default();
        document(&mut workspace, &uri, source);
        let at = source.rfind("key").unwrap();
        let retained = with_cache(|| {
            let index = named_references::Index::new(&workspace, &uri);
            assert!(index.target(&uri, at).is_some());
            assert!(Arc::ptr_eq(
                &index,
                &named_references::Index::new(&workspace, &uri)
            ));
            let nested = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_cache(|| {
                    assert!(!Arc::ptr_eq(
                        &index,
                        &named_references::Index::new(&workspace, &uri)
                    ));
                    panic!("abandon nested analysis");
                })
            }));
            assert!(nested.is_err());
            assert!(Arc::ptr_eq(
                &index,
                &named_references::Index::new(&workspace, &uri)
            ));
            index
        });
        // The same URI/address now has a custom ref meaning. A later query
        // must not retain the previously confirmed builtin reference.
        let changed = format!(r"\renewcommand\ref[1]{{#1}}{source}");
        document(&mut workspace, &uri, &changed);
        with_cache(|| {
            let index = named_references::Index::new(&workspace, &uri);
            assert!(!Arc::ptr_eq(&retained, &index));
            assert!(index.target(&uri, changed.rfind("key").unwrap()).is_none());
        });
    }

    #[test]
    fn label_proofs_distinguish_exact_project_scopes_and_workspaces() {
        let consumer = Url::parse("untitled:consumer.tex").unwrap();
        let provider = Url::parse("untitled:provider.tex").unwrap();
        let mut workspace = Workspace::default();
        document(&mut workspace, &consumer, r"\typeout{\label{ghost}}");
        document(&mut workspace, &provider, r"\renewcommand\typeout[1]{#1}");
        let mut other = workspace.clone();
        document(&mut other, &provider, "No redefinition");
        let single = BTreeSet::from([consumer.clone()]);
        let both = BTreeSet::from([consumer.clone(), provider]);
        let entry = &workspace.documents[&consumer].syntax.labels[0];
        with_cache(|| {
            let local = label_declarations::Index::for_scope(&workspace, &single);
            assert!(!local.contains(&consumer, entry));
            assert!(Arc::ptr_eq(
                &local,
                &label_declarations::Index::for_scope(&workspace, &single)
            ));
            assert!(
                label_declarations::Index::for_scope(&workspace, &both).contains(&consumer, entry)
            );
            assert!(!label_declarations::Index::for_scope(&other, &both).contains(&consumer, entry));
            assert!(!label_declarations::Index::for_scope(&workspace, &single)
                .contains(&consumer, entry));
        });
    }

    fn project_signatures(
        workspace: &Workspace,
        scope: &BTreeSet<Url>,
    ) -> Arc<CommandSignatures<'static>> {
        signatures(workspace, scope, || {
            CommandSignatures::new(scope.iter().map(|uri| {
                let doc = &workspace.documents[uri];
                (doc.text.source.as_str(), &doc.syntax)
            }))
            .into_owned()
        })
    }

    #[test]
    fn source_filtering_reuses_editing_grammar_without_inheriting_deferred_meanings() {
        let uri = Url::parse("untitled:filtered-grammar.tex").unwrap();
        let source = r"\def\install{\def\reader#1{#1}}\def\forward{\input}\input{live}";
        let mut workspace = Workspace::default();
        document(&mut workspace, &uri, source);
        let scope = BTreeSet::from([uri.clone()]);
        with_cache(|| {
            let raw = super::project_signatures(&workspace, &scope);
            assert!(!raw.command_unchanged("reader"));
            let filtered = source_signatures(&workspace, &scope);
            assert!(filtered.command_unchanged("reader"));
            assert!(Arc::ptr_eq(
                &raw,
                &super::project_signatures(&workspace, &scope)
            ));
            let doc = &workspace.documents[&uri];
            let independent = CommandSignatures::source_context([(source, &doc.syntax)]);
            assert_eq!(
                filtered.source_command_positions(source, &doc.syntax),
                independent.source_command_positions(source, &doc.syntax)
            );
        });
    }

    #[test]
    fn project_signatures_preserve_reader_proofs_and_refresh_all_query_consumers() {
        let uri = Url::parse("untitled:shared-analysis.tex").unwrap();
        let source = r"\def\forward{\UseHook}\typeout{\label{ghost}}\label{live}\ref{live}";
        let mut workspace = Workspace::default();
        document(&mut workspace, &uri, source);
        let scope = workspace.symbol_scope(&uri);
        let first = super::project_signatures(&workspace, &scope);
        assert!(!Arc::ptr_eq(
            &first,
            &super::project_signatures(&workspace, &scope)
        ));
        assert_eq!(first.forwarded_builtin("forward"), Some("UseHook"));
        drop(first);
        with_cache(|| {
            let signatures = super::project_signatures(&workspace, &scope);
            let labels = label_declarations::Index::for_scope(&workspace, &scope);
            let entries = &workspace.documents[&uri].syntax.labels;
            assert!(!labels.contains(&uri, &entries[0]));
            assert!(labels.contains(&uri, &entries[1]));
            let named = named_references::Index::new(&workspace, &uri);
            assert!(named.target(&uri, source.rfind("live").unwrap()).is_some());
            assert!(Arc::ptr_eq(
                &signatures,
                &super::project_signatures(&workspace, &scope)
            ));
            assert_eq!(
                AnalysisContext::current_query()
                    .unwrap()
                    .symbols
                    .borrow()
                    .signatures
                    .len(),
                1
            );
        });
        let changed = format!(r"\renewcommand\typeout[1]{{#1}}\renewcommand\ref[1]{{#1}}{source}");
        document(&mut workspace, &uri, &changed);
        with_cache(|| {
            let labels = label_declarations::Index::for_scope(&workspace, &scope);
            assert!(labels.contains(&uri, &workspace.documents[&uri].syntax.labels[0]));
            assert!(named_references::Index::new(&workspace, &uri)
                .target(&uri, changed.rfind("live").unwrap())
                .is_none());
        });
    }

    #[test]
    fn signature_queries_own_their_bindings_and_distinguish_scopes_workspaces_and_lifetimes() {
        let consumer = Url::parse("untitled:consumer.tex").unwrap();
        let provider = Url::parse("untitled:provider.tex").unwrap();
        let mut workspace = Workspace::default();
        document(&mut workspace, &consumer, r"\def\forward{\UseHook}");
        document(&mut workspace, &provider, r"\renewcommand\UseHook[1]{Text}");
        let single = BTreeSet::from([consumer.clone()]);
        let both = BTreeSet::from([consumer.clone(), provider.clone()]);
        let mut other = workspace.clone();
        document(&mut other, &provider, "Unchanged builtin");
        let retained = with_cache(|| {
            let local = project_signatures(&workspace, &single);
            assert_eq!(local.forwarded_builtin("forward"), Some("UseHook"));
            assert!(Arc::ptr_eq(
                &local,
                &project_signatures(&workspace, &single)
            ));
            assert!(project_signatures(&workspace, &both)
                .forwarded_builtin("forward")
                .is_none());
            assert_eq!(
                project_signatures(&other, &both).forwarded_builtin("forward"),
                Some("UseHook")
            );
            let nested = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_cache(|| {
                    assert!(!Arc::ptr_eq(
                        &local,
                        &project_signatures(&workspace, &single)
                    ));
                    panic!("abandon nested signature query");
                });
            }));
            assert!(nested.is_err());
            assert!(Arc::ptr_eq(
                &local,
                &project_signatures(&workspace, &single)
            ));
            local
        });
        document(&mut workspace, &consumer, r"\def\forward{\RemoveFromHook}");
        with_cache(|| {
            let current = project_signatures(&workspace, &single);
            assert!(!Arc::ptr_eq(&retained, &current));
            assert_eq!(current.forwarded_builtin("forward"), Some("RemoveFromHook"));
        });
        drop(workspace);
        assert_eq!(retained.forwarded_builtin("forward"), Some("UseHook"));
    }

    #[test]
    fn signature_query_budget_falls_back_to_fresh_analyses() {
        let mut workspace = Workspace::default();
        let uri = Url::parse("untitled:signatures.tex").unwrap();
        document(&mut workspace, &uri, r"\def\forward{\UseHook}");
        let scope = BTreeSet::from([uri]);
        assert!(!Arc::ptr_eq(
            &project_signatures(&workspace, &scope),
            &project_signatures(&workspace, &scope)
        ));
        with_cache(|| {
            shared(
                &AnalysisContext::current_query().unwrap(),
                (0, Url::parse("untitled:full").unwrap()),
                |cache| &mut cache.named,
                || Arc::new(named_references::Index::default()),
                |_| MAX_BYTES,
            );
            let first = project_signatures(&workspace, &scope);
            let second = project_signatures(&workspace, &scope);
            assert!(!Arc::ptr_eq(&first, &second));
            assert_eq!(second.forwarded_builtin("forward"), Some("UseHook"));
        });
        with_cache(|| {
            for index in 0..MAX_ENTRIES {
                let uri = Url::parse(&format!("untitled:scope{index}.tex")).unwrap();
                document(&mut workspace, &uri, "No definitions");
                project_signatures(&workspace, &BTreeSet::from([uri]));
            }
            let first = project_signatures(&workspace, &scope);
            assert!(!Arc::ptr_eq(
                &first,
                &project_signatures(&workspace, &scope)
            ));
        });
    }

    #[test]
    fn source_signature_queries_keep_filtered_meanings_separate_and_refresh_inputs() {
        let uri = Url::parse("untitled:source-signatures.tex").unwrap();
        let provider = Url::parse("untitled:provider.tex").unwrap();
        let mut workspace = Workspace::default();
        document(
            &mut workspace,
            &uri,
            r"\def\install{\def\input#1{}}\input{child}",
        );
        document(&mut workspace, &provider, r"\def\input#1{}");
        let scope = BTreeSet::from([uri.clone()]);
        let both = BTreeSet::from([uri.clone(), provider.clone()]);
        let retained = with_cache(|| {
            let source = source_signatures(&workspace, &scope);
            assert!(source.command_unchanged("input"));
            assert!(Arc::ptr_eq(&source, &source_signatures(&workspace, &scope)));
            assert!(!project_signatures(&workspace, &scope).command_unchanged("input"));
            assert!(!source_signatures(&workspace, &both).command_unchanged("input"));
            let mut other = workspace.clone();
            document(&mut other, &provider, "No override");
            assert!(source_signatures(&other, &both).command_unchanged("input"));
            source
        });
        document(&mut workspace, &uri, r"\def\input#1{}\input{child}");
        with_cache(|| {
            let current = source_signatures(&workspace, &scope);
            assert!(!Arc::ptr_eq(&current, &retained));
            assert!(!current.command_unchanged("input"));
        });
        assert!(retained.command_unchanged("input"));
        assert!(!Arc::ptr_eq(
            &source_signatures(&workspace, &scope),
            &source_signatures(&workspace, &scope)
        ));
        with_cache(|| {
            AnalysisContext::current_query()
                .unwrap()
                .symbols
                .borrow_mut()
                .bytes = MAX_BYTES;
            assert!(!Arc::ptr_eq(
                &source_signatures(&workspace, &scope),
                &source_signatures(&workspace, &scope)
            ));
        });
    }

    #[test]
    fn full_caches_compute_results_without_retaining_more_analyses() {
        with_cache(|| {
            let get = |key, bytes| {
                shared(
                    &AnalysisContext::current_query().unwrap(),
                    (0, Url::parse(&format!("untitled:{key}")).unwrap()),
                    |cache| &mut cache.named,
                    || Arc::new(named_references::Index::default()),
                    |_| bytes,
                )
            };
            let oversized = get("oversized", MAX_BYTES + 1);
            assert!(!Arc::ptr_eq(&oversized, &get("oversized", MAX_BYTES + 1)));
            let retained = get("retained", MAX_BYTES);
            assert!(Arc::ptr_eq(&retained, &get("retained", MAX_BYTES)));
            let uncached = get("no-space", 1);
            assert!(!Arc::ptr_eq(&uncached, &get("no-space", 1)));
        });
        with_cache(|| {
            let workspace = Workspace::default();
            for index in 0..MAX_ENTRIES {
                named_references::Index::new(
                    &workspace,
                    &Url::parse(&format!("untitled:{index}")).unwrap(),
                );
            }
            let overflow = Url::parse("untitled:overflow").unwrap();
            let first = named_references::Index::new(&workspace, &overflow);
            assert!(!Arc::ptr_eq(
                &first,
                &named_references::Index::new(&workspace, &overflow)
            ));
        });
    }
}
