//! Connection-owned analyses: exact input snapshots and interned pure grammars.
use crate::{
    diagnostics::diagnostic_inputs::Files,
    parser::syntax,
    project::{
        documents::Revisions,
        source_inputs,
        workspace::{Document, DocumentKind, Workspace},
    },
    symbols::commands::CommandSignatures,
};
use lsp_types::Url;
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};

const MAX_GRAMMARS: usize = 8;
const MAX_PROOFS: usize = 8;
const MAX_GRAMMAR_BYTES: usize = 32 * 1024 * 1024;
const MAX_INDEXES: usize = 4;
const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
const MAX_CODE_TABLES: usize = 64;
const MAX_CODE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DECLARATIONS: usize = 64;
const MAX_DECLARATION_BYTES: usize = 8 * 1024 * 1024;
const MAX_PREFIXES: usize = 16;
const MAX_PREFIX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) type PrefixInputs = Vec<(Weak<Document>, usize)>;

type Inputs = Vec<(Url, Option<Weak<Document>>)>;
struct Proof {
    inputs: Inputs,
    revision: u64,
}
struct Grammar {
    source: bool,
    proofs: Vec<Proof>,
    value: Arc<CommandSignatures<'static>>,
    bytes: usize,
}
struct SourceIndex {
    root: Url,
    epoch: u64,
    documents: Revisions,
    truncated: BTreeSet<String>,
    files: Arc<Files>,
    value: Arc<source_inputs::Index>,
    bytes: usize,
    revision: u64,
}
struct Code {
    document: Weak<Document>,
    data: bool,
    value: crate::symbols::expl3::Analysis,
    bytes: usize,
}
struct Declarations {
    document: Weak<Document>,
    value: Arc<crate::symbols::commands::Declarations>,
    bytes: usize,
}
struct Prefix {
    inputs: PrefixInputs,
    value: Arc<CommandSignatures<'static>>,
    bytes: usize,
}
#[derive(Default)]
struct State {
    grammars: VecDeque<Grammar>,
    indexes: VecDeque<SourceIndex>,
    code: VecDeque<Code>,
    declarations: VecDeque<Declarations>,
    prefixes: VecDeque<Prefix>,
}
#[derive(Default)]
pub(crate) struct Cache(Mutex<State>, crate::language::command_index::Cache);

pub(crate) struct Binding {
    pub cache: Arc<Cache>,
    owner: usize,
    epoch: u64,
    pub revision: u64,
    documents: HashMap<(usize, usize, usize), Weak<Document>>,
}
impl Binding {
    pub fn new(ws: &Workspace, epoch: u64, revision: u64, cache: Arc<Cache>) -> Self {
        Self {
            cache,
            owner: ws as *const Workspace as usize,
            epoch,
            revision,
            documents: ws
                .documents
                .weak_revisions()
                .into_iter()
                .filter_map(|(_, weak)| {
                    let doc = weak.upgrade()?;
                    Some((
                        (
                            doc.text.source.as_ptr() as usize,
                            doc.text.source.len(),
                            &doc.syntax as *const syntax::Syntax as usize,
                        ),
                        weak,
                    ))
                })
                .collect(),
        }
    }
    pub fn document(&self, source: &str, parsed: &syntax::Syntax) -> Option<Weak<Document>> {
        let weak = self.documents.get(&(
            source.as_ptr() as usize,
            source.len(),
            parsed as *const syntax::Syntax as usize,
        ))?;
        weak.upgrade()?;
        Some(weak.clone())
    }
    pub fn index(&self, ws: &Workspace, root: &Url) -> Option<source_inputs::Index> {
        if self.owner != ws as *const Workspace as usize || ws.has_pending_dependencies() {
            return None;
        }
        let found = self
            .cache
            .0
            .lock()
            .unwrap()
            .indexes
            .iter()
            .rev()
            .find(|entry| {
                entry.root == *root
                    && entry.epoch == self.epoch
                    && entry.truncated == ws.index_truncated
                    && entry.documents.current(&ws.documents)
            })
            .map(|entry| (entry.value.clone(), entry.files.clone()));
        let (value, files) = found?;
        if !files.current() {
            return None;
        }
        if let Some(context) = crate::analysis::AnalysisContext::current() {
            context.observe_files(&files);
        }
        let mut state = self.cache.0.lock().unwrap();
        if let Some(index) = state
            .indexes
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.value, &value))
        {
            let mut entry = state.indexes.remove(index).unwrap();
            entry.revision = entry.revision.max(self.revision);
            state.indexes.push_back(entry);
        }
        drop(state);
        Some(value.as_ref().clone())
    }
    pub fn insert_index(
        &self,
        ws: &Workspace,
        root: &Url,
        value: &source_inputs::Index,
        files: Arc<Files>,
    ) {
        if self.owner != ws as *const Workspace as usize
            || ws.has_pending_dependencies()
            || value.incomplete
            || !files.current()
        {
            return;
        }
        let bytes = value.cache_bytes()
            + files.bytes
            + ws.documents.len() * (std::mem::size_of::<Document>() + 128)
            + ws.documents
                .keys()
                .map(|uri| uri.as_str().len())
                .sum::<usize>();
        if bytes > MAX_INDEX_BYTES {
            return;
        }
        let mut state = self.cache.0.lock().unwrap();
        if state
            .indexes
            .iter()
            .any(|entry| entry.root == *root && entry.revision > self.revision)
        {
            return;
        }
        state.indexes.retain(|entry| entry.root != *root);
        state.indexes.push_back(SourceIndex {
            root: root.clone(),
            epoch: self.epoch,
            documents: ws.documents.revisions(),
            truncated: ws.index_truncated.clone(),
            files,
            value: Arc::new(value.clone()),
            bytes,
            revision: self.revision,
        });
        while state.indexes.len() > MAX_INDEXES
            || state.indexes.iter().map(|e| e.bytes).sum::<usize>() > MAX_INDEX_BYTES
        {
            state.indexes.pop_front();
        }
    }
}
impl Cache {
    pub fn command_candidates(&self) -> &crate::language::command_index::Cache {
        &self.1
    }
    pub fn prefix(&self, inputs: &PrefixInputs) -> Option<Arc<CommandSignatures<'static>>> {
        let mut state = self.0.lock().unwrap();
        let at = state
            .prefixes
            .iter()
            .position(|entry| same_prefixes(&entry.inputs, inputs))?;
        let entry = state.prefixes.remove(at).unwrap();
        let result = entry.value.clone();
        state.prefixes.push_back(entry);
        Some(result)
    }
    pub fn insert_prefix(
        &self,
        inputs: PrefixInputs,
        value: CommandSignatures<'static>,
    ) -> Arc<CommandSignatures<'static>> {
        let bytes = value.cache_bytes() + inputs.len() * (std::mem::size_of::<Document>() + 128);
        let value = Arc::new(value);
        if bytes > MAX_PREFIX_BYTES {
            return value;
        }
        let mut state = self.0.lock().unwrap();
        if let Some(entry) = state
            .prefixes
            .iter()
            .find(|entry| same_prefixes(&entry.inputs, &inputs))
        {
            return entry.value.clone();
        }
        state
            .prefixes
            .retain(|entry| entry.inputs.iter().all(|(doc, _)| doc.strong_count() > 0));
        state.prefixes.push_back(Prefix {
            inputs,
            value: value.clone(),
            bytes,
        });
        while state.prefixes.len() > MAX_PREFIXES
            || state
                .prefixes
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                > MAX_PREFIX_BYTES
        {
            state.prefixes.pop_front();
        }
        value
    }
    pub fn declarations(
        &self,
        doc: &Weak<Document>,
    ) -> Option<Arc<crate::symbols::commands::Declarations>> {
        let mut state = self.0.lock().unwrap();
        let at = state
            .declarations
            .iter()
            .position(|entry| entry.document.ptr_eq(doc))?;
        let entry = state.declarations.remove(at).unwrap();
        let result = entry.value.clone();
        state.declarations.push_back(entry);
        Some(result)
    }
    pub fn insert_declarations(
        &self,
        document: Weak<Document>,
        value: Arc<crate::symbols::commands::Declarations>,
    ) {
        let bytes = value.cache_bytes() + std::mem::size_of::<Document>() + 128;
        if bytes > MAX_DECLARATION_BYTES {
            return;
        }
        let mut state = self.0.lock().unwrap();
        state
            .declarations
            .retain(|entry| entry.document.strong_count() > 0 && !entry.document.ptr_eq(&document));
        state.declarations.push_back(Declarations {
            document,
            value,
            bytes,
        });
        while state.declarations.len() > MAX_DECLARATIONS
            || state
                .declarations
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                > MAX_DECLARATION_BYTES
        {
            state.declarations.pop_front();
        }
    }
    pub fn code(
        &self,
        document: &Weak<Document>,
        data: bool,
    ) -> Option<crate::symbols::expl3::Analysis> {
        let mut state = self.0.lock().unwrap();
        let index = state
            .code
            .iter()
            .position(|entry| entry.data == data && entry.document.ptr_eq(document))?;
        let entry = state.code.remove(index).unwrap();
        let value = entry.value.clone();
        state.code.push_back(entry);
        Some(value)
    }
    pub fn insert_code(
        &self,
        document: Weak<Document>,
        data: bool,
        value: &crate::symbols::expl3::Analysis,
    ) {
        let bytes = value.cache_bytes() + std::mem::size_of::<Document>() + 128;
        if bytes > MAX_CODE_BYTES {
            return;
        }
        let mut state = self.0.lock().unwrap();
        state.code.retain(|entry| {
            entry.document.strong_count() > 0
                && !(entry.document.ptr_eq(&document) && entry.data == data)
        });
        state.code.push_back(Code {
            document,
            data,
            value: value.clone(),
            bytes,
        });
        while state.code.len() > MAX_CODE_TABLES
            || state.code.iter().map(|entry| entry.bytes).sum::<usize>() > MAX_CODE_BYTES
        {
            state.code.pop_front();
        }
    }
    fn inputs(ws: &Workspace, scope: &BTreeSet<Url>) -> Inputs {
        scope
            .iter()
            .filter(|uri| {
                ws.documents
                    .get(uri)
                    .is_some_and(|doc| doc.kind == DocumentKind::Latex)
            })
            .map(|uri| (uri.clone(), ws.documents.weak_revision(uri)))
            .collect()
    }
    fn same_inputs(left: &Inputs, right: &Inputs) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|((a, old), (b, new))| {
                a == b
                    && match (old, new) {
                        (Some(a), Some(b)) => a.ptr_eq(b),
                        (None, None) => true,
                        _ => false,
                    }
            })
    }
    fn proof_bytes(inputs: &Inputs) -> usize {
        inputs
            .iter()
            .map(|(uri, _)| uri.as_str().len() + std::mem::size_of::<Document>() + 128)
            .sum()
    }
    pub fn grammar(
        &self,
        ws: &Workspace,
        scope: &BTreeSet<Url>,
        source: bool,
    ) -> Option<Arc<CommandSignatures<'static>>> {
        let inputs = Self::inputs(ws, scope);
        let mut state = self.0.lock().unwrap();
        let index = state.grammars.iter().position(|entry| {
            entry.source == source
                && entry
                    .proofs
                    .iter()
                    .any(|proof| Self::same_inputs(&proof.inputs, &inputs))
        })?;
        let entry = state.grammars.remove(index).unwrap();
        let value = entry.value.clone();
        state.grammars.push_back(entry);
        Some(value)
    }
    pub fn intern(
        &self,
        ws: &Workspace,
        scope: &BTreeSet<Url>,
        source: bool,
        value: CommandSignatures<'static>,
        revision: u64,
    ) -> Arc<CommandSignatures<'static>> {
        let inputs = Self::inputs(ws, scope);
        if let Some(value) = Self::reuse_grammar(
            &mut self.0.lock().unwrap(),
            &inputs,
            source,
            &value,
            revision,
        ) {
            return value;
        }
        let bytes = value.cache_bytes() + Self::proof_bytes(&inputs);
        let value = Arc::new(value);
        if bytes <= MAX_GRAMMAR_BYTES {
            let mut state = self.0.lock().unwrap();
            // Sizing initializes lazy tables outside the lock. Another worker
            // may have installed this same grammar in the meantime.
            if let Some(existing) =
                Self::reuse_grammar(&mut state, &inputs, source, &value, revision)
            {
                return existing;
            }
            state.grammars.push_back(Grammar {
                source,
                proofs: vec![Proof { inputs, revision }],
                value: value.clone(),
                bytes,
            });
            Self::trim_grammars(&mut state);
        }
        value
    }

    fn reuse_grammar(
        state: &mut State,
        inputs: &Inputs,
        source: bool,
        value: &CommandSignatures<'static>,
        revision: u64,
    ) -> Option<Arc<CommandSignatures<'static>>> {
        if let Some(index) = state
            .grammars
            .iter()
            .position(|entry| entry.source == source && entry.value.same_meanings(value))
        {
            let mut entry = state.grammars.remove(index).unwrap();
            let old_keys: usize = entry
                .proofs
                .iter()
                .map(|proof| Self::proof_bytes(&proof.inputs))
                .sum();
            if let Some(proof) = entry
                .proofs
                .iter_mut()
                .find(|proof| Self::same_inputs(&proof.inputs, inputs))
            {
                proof.revision = proof.revision.max(revision);
            } else {
                entry.proofs.push(Proof {
                    inputs: inputs.clone(),
                    revision,
                });
            }
            entry
                .proofs
                .sort_by_key(|proof| std::cmp::Reverse(proof.revision));
            entry.proofs.truncate(MAX_PROOFS);
            entry.bytes = entry.bytes - old_keys
                + entry
                    .proofs
                    .iter()
                    .map(|proof| Self::proof_bytes(&proof.inputs))
                    .sum::<usize>();
            let value = entry.value.clone();
            if entry.bytes <= MAX_GRAMMAR_BYTES {
                state.grammars.push_back(entry);
                Self::trim_grammars(state);
            }
            return Some(value);
        }
        None
    }

    fn trim_grammars(state: &mut State) {
        while state.grammars.len() > MAX_GRAMMARS
            || state
                .grammars
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                > MAX_GRAMMAR_BYTES
        {
            state.grammars.pop_front();
        }
    }
}

fn same_prefixes(left: &PrefixInputs, right: &PrefixInputs) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|((a, end_a), (b, end_b))| a.ptr_eq(b) && end_a == end_b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{analysis::AnalysisContext, symbols::query_cache};

    #[test]
    fn declaration_facts_are_shared_only_for_the_exact_immutable_syntax() {
        let source = r"\newcommand\one[1]{#1}\def\alias{\one}\let\copy\alias\newenvironment{box}[1]{}{}\edef\expanded{\one}";
        let (mut ws, uri, scope) = workspace(source);
        let cache = Arc::new(Cache::default());
        let first = AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            let doc = &ws.documents[&uri];
            let facts = crate::symbols::commands::Declarations::get(&doc.text.source, &doc.syntax);
            let cached = query_cache::source_signatures(&ws, &scope);
            let direct =
                CommandSignatures::source_context([(doc.text.source.as_str(), &doc.syntax)]);
            assert!(cached.same_meanings(&direct));
            facts
        });
        AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            let doc = &ws.documents[&uri];
            let facts = crate::symbols::commands::Declarations::get(&doc.text.source, &doc.syntax);
            assert!(Arc::ptr_eq(&first, &facts));
            let scratch = doc.syntax.clone();
            let scratch = crate::symbols::commands::Declarations::get(&doc.text.source, &scratch);
            assert!(!Arc::ptr_eq(&facts, &scratch));
        });
        // Even a syntax-only copy-on-write mutation must lose its proof.
        ws.documents
            .get_mut(&uri)
            .unwrap()
            .syntax
            .definitions
            .clear();
        AnalysisContext::for_workspace(&ws, 0, 3, cache.clone()).run(|_| {
            let doc = &ws.documents[&uri];
            let facts = crate::symbols::commands::Declarations::get(&doc.text.source, &doc.syntax);
            assert!(!Arc::ptr_eq(&first, &facts));
            let cached = query_cache::source_signatures(&ws, &scope);
            let direct =
                CommandSignatures::source_context([(doc.text.source.as_str(), &doc.syntax)]);
            assert!(cached.same_meanings(&direct));
        });
    }

    #[test]
    fn execution_prefixes_include_order_endpoints_and_complete_source_revisions() {
        let (mut ws, uri, _) = workspace(r"\newcommand\one[1]{#1}\one{Text}");
        let other = Url::parse("untitled:second.tex").unwrap();
        ws.documents.insert(
            other.clone(),
            Document::new(&other, r"\newcommand\two[2]{#1#2}".into(), Some(1)),
        );
        let cache = Arc::new(Cache::default());
        let first = AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            AnalysisContext::lexical().run(|_| {
                let doc = &ws.documents[&uri];
                let first = CommandSignatures::at_prefixes(&[(doc, 0)]);
                let again = CommandSignatures::at_prefixes(&[(doc, 0)]);
                assert!(Arc::ptr_eq(&first, &again));
                let whole = CommandSignatures::at_prefixes(&[(doc, doc.text.source.len())]);
                assert!(!Arc::ptr_eq(&first, &whole));
                assert!(!first.same_meanings(&whole));
                let full =
                    CommandSignatures::source_context([(doc.text.source.as_str(), &doc.syntax)]);
                assert!(whole.same_meanings(&full));
                let second = &ws.documents[&other];
                let a = CommandSignatures::at_prefixes(&[(doc, 0), (second, 0)]);
                let b = CommandSignatures::at_prefixes(&[(second, 0), (doc, 0)]);
                assert!(!Arc::ptr_eq(&a, &b));
                first
            })
        });
        AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            assert!(Arc::ptr_eq(
                &first,
                &CommandSignatures::at_prefixes(&[(&ws.documents[&uri], 0)])
            ));
        });
        let changed =
            ws.documents[&uri].with_text(&uri, r"\newcommand\one[1]{#1}\one{Hook}".into(), Some(2));
        ws.documents.insert(uri.clone(), changed);
        AnalysisContext::for_workspace(&ws, 0, 3, cache).run(|_| {
            let changed = CommandSignatures::at_prefixes(&[(&ws.documents[&uri], 0)]);
            assert!(
                !Arc::ptr_eq(&first, &changed),
                "suffix-only changes affect source-wide budgets"
            );
        });
    }

    #[test]
    fn declaration_and_execution_prefix_storage_obey_entry_and_byte_limits() {
        let mut ws = Workspace::default();
        for i in 0..MAX_DECLARATIONS + 8 {
            let uri = Url::parse(&format!("untitled:{i}.tex")).unwrap();
            ws.documents.insert(
                uri.clone(),
                Document::new(&uri, r"\def\one#1{#1}".into(), Some(1)),
            );
        }
        let cache = Arc::new(Cache::default());
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            for doc in ws.documents.values() {
                crate::symbols::commands::Declarations::get(&doc.text.source, &doc.syntax);
                CommandSignatures::at_prefixes(&[(doc, 0)]);
            }
        });
        let state = cache.0.lock().unwrap();
        assert!(!state.declarations.is_empty());
        assert!(state.declarations.len() <= MAX_DECLARATIONS);
        assert!(
            state
                .declarations
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                <= MAX_DECLARATION_BYTES
        );
        assert!(!state.prefixes.is_empty());
        assert!(state.prefixes.len() <= MAX_PREFIXES);
        assert!(
            state
                .prefixes
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                <= MAX_PREFIX_BYTES
        );
    }

    fn workspace(source: &str) -> (Workspace, Url, BTreeSet<Url>) {
        let uri = Url::parse("untitled:persistent.tex").unwrap();
        let mut ws = Workspace::default();
        ws.documents
            .insert(uri.clone(), Document::new(&uri, source.into(), Some(1)));
        (ws, uri.clone(), BTreeSet::from([uri]))
    }
    fn grammar(
        cache: &Arc<Cache>,
        ws: &Workspace,
        scope: &BTreeSet<Url>,
        revision: u64,
    ) -> Arc<CommandSignatures<'static>> {
        AnalysisContext::for_workspace(ws, 0, revision, cache.clone()).run(|_| {
            query_cache::project_signatures(ws, scope);
        });
        cache.grammar(ws, scope, false).unwrap()
    }

    #[test]
    fn unchanged_queries_and_equal_grammars_reuse_across_text_revisions() {
        let cache = Arc::new(Cache::default());
        let source = "\\newcommand{\\reader}[1]{Text}\nPlain text\n\\input{tail}\n";
        let (mut ws, uri, scope) = workspace(source);
        let first = grammar(&cache, &ws, &scope, 1);
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            assert!(first
                .source_command_positions(source, &ws.documents[&uri].syntax)
                .contains(&source.find("\\input").unwrap()));
        });
        assert!(Arc::ptr_eq(&first, &grammar(&cache, &ws, &scope, 2)));
        ws.documents.insert(
            uri.clone(),
            Document::new(
                &uri,
                source.replace("Plain text", "Changed 😀 prose"),
                Some(2),
            ),
        );
        assert!(Arc::ptr_eq(&first, &grammar(&cache, &ws, &scope, 3)));
        AnalysisContext::for_workspace(&ws, 0, 3, cache.clone()).run(|_| {
            let doc = &ws.documents[&uri];
            let positions = first.source_command_positions(&doc.text.source, &doc.syntax);
            assert!(positions.contains(&doc.text.source.find("\\input").unwrap()));
            assert!(!positions.contains(&source.find("\\input").unwrap()));
        });
        let changed = source.replace("[1]", "[2]");
        ws.documents
            .insert(uri.clone(), Document::new(&uri, changed, Some(3)));
        assert!(!Arc::ptr_eq(&first, &grammar(&cache, &ws, &scope, 4)));
    }

    #[test]
    fn memoized_file_operands_refresh_when_a_different_file_changes_arity() {
        let cache = Arc::new(Cache::default());
        let (mut ws, provider, mut scope) = workspace(r"\newcommand{\reader}[1]{Text}");
        let consumer = Url::parse("untitled:consumer.cls").unwrap();
        let source = r"\reader{first}\input{visible}";
        ws.documents.insert(
            consumer.clone(),
            Document::new(&consumer, source.into(), None),
        );
        scope.insert(consumer.clone());
        let at = source.find(r"\input").unwrap();
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            let signature = query_cache::project_signatures(&ws, &scope);
            assert!(signature
                .source_command_positions(source, &ws.documents[&consumer].syntax)
                .contains(&at));
        });
        let first = cache.grammar(&ws, &scope, false).unwrap();
        ws.documents.insert(
            provider.clone(),
            Document::new(&provider, r"\newcommand{\reader}[2]{Text}".into(), Some(2)),
        );
        AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            let signature = query_cache::project_signatures(&ws, &scope);
            assert!(!Arc::ptr_eq(&first, &signature));
            assert!(!signature
                .source_command_positions(source, &ws.documents[&consumer].syntax)
                .contains(&at));
        });
    }

    #[test]
    fn document_copy_on_write_invalidates_operand_sets_without_a_text_edit() {
        let cache = Arc::new(Cache::default());
        let source = r"\input{first}\input{second}";
        let (mut ws, uri, scope) = workspace(source);
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            let signature = query_cache::project_signatures(&ws, &scope);
            let parsed = &ws.documents[&uri].syntax;
            assert!(signature
                .source_command_positions(source, parsed)
                .contains(&0));
        });
        let first = cache.grammar(&ws, &scope, false).unwrap();
        ws.documents.get_mut(&uri).unwrap().syntax.commands.clear();
        AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            let signature = query_cache::project_signatures(&ws, &scope);
            assert!(Arc::ptr_eq(&first, &signature));
            let parsed = &ws.documents[&uri].syntax;
            let uncached = CommandSignatures::new([(source, parsed)]);
            assert!(signature
                .source_command_positions(source, parsed)
                .is_empty());
            assert_eq!(
                signature.source_command_positions(source, parsed),
                uncached.source_command_positions(source, parsed)
            );
        });
    }

    #[test]
    fn filesystem_proofs_invalidate_missing_source_paths_and_keep_observers_complete() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("main.tex");
        let source =
            "\\documentclass{article}\n\\begin{document}\n\\input{late}\n\\end{document}\n";
        std::fs::write(&path, source).unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.open(uri.clone(), source.into(), 1);
        let cache = Arc::new(Cache::default());
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            source_inputs::Index::new(&ws, &uri);
        });
        let binding = Binding::new(&ws, 0, 2, cache.clone());
        assert!(binding.index(&ws, &uri).is_some());
        let (_, files) = AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            Files::capture(|| AnalysisContext::new().run(|_| source_inputs::Index::new(&ws, &uri)))
        });
        let late = root.join("late.tex");
        std::fs::write(&late, "New source").unwrap();
        assert!(!files.current());
        assert!(binding.index(&ws, &uri).is_none());
        AnalysisContext::for_workspace(&ws, 0, 3, cache).run(|_| {
            assert!(ws
                .project_documents(&uri)
                .contains(&Url::from_file_path(late).unwrap()));
        });
    }

    #[test]
    fn metadata_scratch_workspaces_and_old_background_work_cannot_replace_new_indexes() {
        let cache = Arc::new(Cache::default());
        let (ws, uri, _) = workspace("Text");
        let (_, files) = Files::capture(|| ());
        let index = source_inputs::Index::default();
        let current = Binding::new(&ws, 4, 20, cache.clone());
        current.insert_index(&ws, &uri, &index, files.clone());
        assert!(current.index(&ws, &uri).is_some());
        assert!(Binding::new(&ws, 5, 21, cache.clone())
            .index(&ws, &uri)
            .is_none());
        let scratch = ws.clone();
        assert!(current.index(&scratch, &uri).is_none());
        Binding::new(&ws, 3, 10, cache.clone()).insert_index(&ws, &uri, &index, files);
        assert!(current.index(&ws, &uri).is_some());
        assert_eq!(cache.0.lock().unwrap().indexes[0].revision, 20);
    }

    #[test]
    fn retention_is_bounded_and_does_not_keep_obsolete_source_documents_alive() {
        let cache = Arc::new(Cache::default());
        let mut weak = None;
        for revision in 0..20 {
            let (ws, _, scope) = workspace(&format!(
                r"\newcommand{{\macro{}}}{{Text}}",
                (b'a' + revision) as char
            ));
            weak = Some(ws.documents.weak_revisions()[0].1.clone());
            grammar(&cache, &ws, &scope, revision as u64);
        }
        assert!(weak.unwrap().upgrade().is_none());
        let state = cache.0.lock().unwrap();
        assert!(state.grammars.len() <= MAX_GRAMMARS);
        assert!(
            state
                .grammars
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                <= MAX_GRAMMAR_BYTES
        );
    }

    #[test]
    fn cancelled_queries_publish_no_persistent_analyses() {
        let cache = Arc::new(Cache::default());
        let (ws, uri, scope) = workspace(r"\newcommand{\reader}[1]{Text}");
        AnalysisContext::cancellable(|| true).run(|_| {
            AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
                query_cache::project_signatures(&ws, &scope);
                source_inputs::Index::new(&ws, &uri);
            });
        });
        let state = cache.0.lock().unwrap();
        assert!(state.grammars.is_empty());
        assert!(state.indexes.is_empty());
    }

    #[test]
    fn equivalent_scope_proofs_coexist_and_ignore_bibliography_only_edits() {
        let cache = Arc::new(Cache::default());
        let (mut ws, _, scope) = workspace(r"\newcommand{\reader}[1]{Text}");
        let other = Url::parse("untitled:orphan.tex").unwrap();
        let bib = Url::parse("untitled:references.bib").unwrap();
        ws.documents
            .insert(other.clone(), Document::new(&other, "Prose".into(), None));
        ws.documents.insert(
            bib.clone(),
            Document::new(&bib, "@book{first}".into(), None),
        );
        let mut connected = scope.clone();
        connected.extend([other, bib.clone()]);
        let first = grammar(&cache, &ws, &scope, 1);
        assert!(Arc::ptr_eq(&first, &grammar(&cache, &ws, &connected, 1)));
        assert!(cache.grammar(&ws, &scope, false).is_some());
        assert!(cache.grammar(&ws, &connected, false).is_some());
        ws.documents.insert(
            bib.clone(),
            Document::new(&bib, "@book{changed}".into(), None),
        );
        assert!(cache.grammar(&ws, &connected, false).is_some());
        assert_eq!(cache.0.lock().unwrap().grammars[0].proofs.len(), 2);
    }

    #[test]
    fn equal_meaning_proofs_are_bounded_without_retaining_old_text() {
        let cache = Arc::new(Cache::default());
        let (mut ws, uri, scope) = workspace("Text");
        let first = grammar(&cache, &ws, &scope, 1);
        for revision in 2..30 {
            ws.documents.insert(
                uri.clone(),
                Document::new(&uri, format!("Changed text {revision}"), Some(revision)),
            );
            assert!(Arc::ptr_eq(
                &first,
                &grammar(&cache, &ws, &scope, revision as u64)
            ));
        }
        let state = cache.0.lock().unwrap();
        assert_eq!(state.grammars.len(), 1);
        assert_eq!(state.grammars[0].proofs.len(), MAX_PROOFS);
    }

    #[test]
    fn lexical_updates_reuse_only_pure_grammars_and_validate_each_current_document_set() {
        let cache = Arc::new(Cache::default());
        let (mut ws, uri, scope) = workspace(r"\newcommand{\reader}[1]{Text}");
        let context = AnalysisContext::for_workspace(&ws, 0, 1, cache.clone());
        context.run(|_| {
            AnalysisContext::lexical().run(|_| {
                assert!(AnalysisContext::current_query().is_none());
                let first = query_cache::source_signatures(&ws, &scope);
                ws.documents.insert(
                    uri.clone(),
                    Document::new(&uri, r"\newcommand{\reader}[2]{Text}".into(), Some(2)),
                );
                let second = query_cache::source_signatures(&ws, &scope);
                assert!(!Arc::ptr_eq(&first, &second));
                source_inputs::Index::new(&ws, &uri);
            })
        });
        assert!(cache.0.lock().unwrap().indexes.is_empty());
    }

    #[test]
    fn immutable_file_code_tables_are_shared_without_retaining_source_trees() {
        let cache = Arc::new(Cache::default());
        let source = r"\ExplSyntaxOn\cs_new:Npn \example_func:n #1 {#1}\ExplSyntaxOff";
        let (mut ws, uri, _) = workspace(source);
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            let doc = &ws.documents[&uri];
            assert!(!crate::symbols::expl3::occurrences(doc).bodies.is_empty());
            crate::symbols::expl3::math_data_spans(doc);
        });
        assert_eq!(cache.0.lock().unwrap().code.len(), 2);
        ws.documents.insert(
            uri.clone(),
            Document::new(&uri, "Plain text".into(), Some(2)),
        );
        AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            assert!(crate::symbols::expl3::occurrences(&ws.documents[&uri])
                .bodies
                .is_empty());
        });
        let weak = ws.documents.weak_revisions()[0].1.clone();
        drop(ws);
        assert!(weak.upgrade().is_none());
        let state = cache.0.lock().unwrap();
        assert!(state.code.len() <= MAX_CODE_TABLES);
        assert!(state.code.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_CODE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_proofs_detect_symlink_retargets() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let main = root.join("main.tex");
        let source =
            "\\documentclass{article}\n\\begin{document}\n\\input{selected}\n\\end{document}\n";
        std::fs::write(&main, source).unwrap();
        std::fs::write(root.join("first.tex"), "First").unwrap();
        std::fs::write(root.join("second.tex"), "Second").unwrap();
        symlink("first.tex", root.join("selected.tex")).unwrap();
        let uri = Url::from_file_path(main).unwrap();
        let cache = Arc::new(Cache::default());
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.open(uri.clone(), source.into(), 1);
        AnalysisContext::for_workspace(&ws, 0, 1, cache.clone()).run(|_| {
            source_inputs::Index::new(&ws, &uri);
        });
        let binding = Binding::new(&ws, 0, 2, cache);
        assert!(binding.index(&ws, &uri).is_some());
        std::fs::remove_file(root.join("selected.tex")).unwrap();
        symlink("second.tex", root.join("selected.tex")).unwrap();
        assert!(binding.index(&ws, &uri).is_none());
    }

    #[test]
    fn file_call_memos_distinguish_target_kinds_and_execution_positions() {
        let cache = Arc::new(Cache::default());
        let source = r"\csuse{includegraphics}{image}\csuse{input}{child}";
        let (ws, uri, scope) = workspace(source);
        AnalysisContext::for_workspace(&ws, 0, 1, cache).run(|_| {
            let signatures = query_cache::project_signatures(&ws, &scope);
            let parsed = &ws.documents[&uri].syntax;
            let positions = signatures.source_command_positions(source, parsed);
            for _ in 0..2 {
                let images = signatures.indirect_graphics_invocations(source, parsed, &positions);
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].command.name, "includegraphics");
                let sources = signatures.indirect_source_invocations(source, parsed, &positions);
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].command.name, "input");
                assert!(signatures
                    .indirect_graphics_invocations(source, parsed, &BTreeSet::new())
                    .is_empty());
            }
        });
    }

    #[test]
    fn concurrent_workers_intern_one_owned_grammar() {
        let cache = Arc::new(Cache::default());
        let source: String = (0..500)
            .map(|n| {
                let suffix: String = [n / 26, n % 26]
                    .into_iter()
                    .map(|n| (b'a' + n as u8) as char)
                    .collect();
                format!("\\newcommand{{\\macro{suffix}}}{{Text}}\n")
            })
            .collect();
        let (ws, uri, scope) = workspace(&source);
        let ws = Arc::new(ws);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (cache, ws, scope, barrier) =
                    (cache.clone(), ws.clone(), scope.clone(), barrier.clone());
                let uri = uri.clone();
                std::thread::spawn(move || {
                    let doc = &ws.documents[&uri];
                    let value = CommandSignatures::new([(doc.text.source.as_str(), &doc.syntax)])
                        .into_owned();
                    barrier.wait();
                    cache.intern(&ws, &scope, false, value, 1)
                })
            })
            .collect();
        let values: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(values.iter().all(|value| Arc::ptr_eq(value, &values[0])));
        assert_eq!(cache.0.lock().unwrap().grammars.len(), 1);
    }

    #[test]
    fn nested_queries_inside_mutations_do_not_reuse_persistent_workspace_indexes() {
        let cache = Arc::new(Cache::default());
        let (ws, uri, _) = workspace("Text");
        AnalysisContext::for_workspace(&ws, 0, 0, cache.clone()).run(|_| {
            source_inputs::Index::new(&ws, &uri);
        });
        assert_eq!(cache.0.lock().unwrap().indexes[0].revision, 0);
        AnalysisContext::for_workspace(&ws, 0, 2, cache.clone()).run(|_| {
            AnalysisContext::lexical().run(|_| {
                AnalysisContext::new().run(|context| {
                    assert!(AnalysisContext::current_query().is_some());
                    assert!(!context.persistent_indexes());
                    source_inputs::Index::new(&ws, &uri);
                })
            });
        });
        assert_eq!(cache.0.lock().unwrap().indexes[0].revision, 0);
    }
}
