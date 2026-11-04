//! A command-name index assembled from independently reusable file contributions.
//! Context, signatures, documentation and cursor edits are resolved after search.
use crate::{
    analysis::AnalysisContext,
    project::{
        catalog, packages,
        workspace::{Document, Workspace},
    },
};
use lsp_types::*;
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, OnceLock, Weak},
};

const MAX_FILES: usize = 128;
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// Start asynchronous preparation when a declaration is edited. Waiting for
/// typing to pause would otherwise keep the old prepared view invalid through
/// the subsequent command prefix, even though its name index is already ready.
pub(crate) fn changed_declarations(before: &Document, after: &Document) -> bool {
    if before.text.source.len().max(after.text.source.len()) > 32 * 1024
        || before.text.source == after.text.source
        || (before.syntax.definitions.len() + before.syntax.environments.len())
            .max(after.syntax.definitions.len() + after.syntax.environments.len())
            > 512
    {
        return false;
    }
    let at = before
        .text
        .source
        .bytes()
        .zip(after.text.source.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let relevant = |doc: &Document| {
        crate::symbols::commands::definition_data_spans(&doc.text.source, &doc.syntax)
            .iter()
            .any(|span| span.start <= at && at <= span.end)
    };
    relevant(before)
        || relevant(after)
        || before.syntax.definitions.len() != after.syntax.definitions.len()
        || before.syntax.environments.len() != after.syntax.environments.len()
        || before.syntax.conditionals.len() != after.syntax.conditionals.len()
}

#[derive(PartialEq, Eq)]
enum Kind {
    User {
        environment: bool,
        variable: bool,
        detail: String,
    },
    Conditional {
        variable: bool,
        detail: String,
    },
    Package {
        package: usize,
        command: usize,
    },
    Builtin(usize),
}
#[derive(PartialEq, Eq)]
struct Candidate {
    name: String,
    kind: Kind,
}
#[derive(Default, PartialEq, Eq)]
struct Section {
    items: Vec<Candidate>,
    names: BTreeMap<String, Vec<usize>>,
}
impl Section {
    fn insert(&mut self, candidate: Candidate) {
        self.names
            .entry(candidate.name.clone())
            .or_default()
            .push(self.items.len());
        self.items.push(candidate);
    }
    fn matching<'a>(&'a self, prefix: &str) -> Box<dyn Iterator<Item = &'a Candidate> + 'a> {
        if prefix.is_empty() {
            return Box::new(self.items.iter());
        }
        let mut positions: Vec<_> = self
            .names
            .range(prefix.to_owned()..)
            .take_while(|(name, _)| name.starts_with(prefix))
            .flat_map(|(_, positions)| positions.iter().copied())
            .collect();
        positions.sort_unstable();
        Box::new(positions.into_iter().map(|at| &self.items[at]))
    }
    fn bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<Candidate>()
            + self
                .items
                .iter()
                .map(|item| {
                    item.name.capacity()
                        + match &item.kind {
                            Kind::User { detail, .. } | Kind::Conditional { detail, .. } => {
                                detail.capacity()
                            }
                            _ => 0,
                        }
                })
                .sum::<usize>()
            + self
                .names
                .iter()
                .map(|(name, positions)| {
                    name.capacity() + positions.capacity() * std::mem::size_of::<usize>() + 256
                })
                .sum::<usize>()
    }
}

#[derive(PartialEq, Eq)]
struct Fragment {
    sections: [Section; 3],
    packages: BTreeSet<String>,
}
impl Fragment {
    fn build(doc: &Document) -> Self {
        let mut result = Self {
            sections: Default::default(),
            packages: packages::direct(doc),
        };
        // A declaration inside replacement text runs when its owner is called,
        // not while the class/package is loaded. Keep such custom names in the
        // menu, but do not let them shadow native command completion templates.
        let deferred =
            crate::symbols::commands::definition_body_spans(&doc.text.source, &doc.syntax);
        for (phase, entries) in [&doc.syntax.definitions, &doc.syntax.environments]
            .into_iter()
            .enumerate()
        {
            for entry in entries {
                if phase == 0
                    && statics().1.names.contains_key(&entry.name)
                    && deferred.iter().any(|span| span.contains(&entry.span.start))
                {
                    continue;
                }
                if entry.name.is_empty()
                    || !entry
                        .name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphabetic() || byte == b'@')
                {
                    continue;
                }
                result.sections[phase].insert(Candidate {
                    name: entry.name.clone(),
                    kind: Kind::User {
                        environment: phase == 1,
                        variable: entry.is_variable,
                        detail: entry.detail.clone(),
                    },
                });
            }
        }
        for entry in &doc.syntax.conditionals {
            for (at, name) in crate::symbols::generated::conditional_names(&entry.name)
                .into_iter()
                .enumerate()
            {
                if name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphabetic() || byte == b'@')
                {
                    result.sections[2].insert(Candidate {
                        name,
                        kind: Kind::Conditional {
                            variable: at == 0,
                            detail: format!("Conditional `{}` — {}", entry.name, entry.detail),
                        },
                    });
                }
            }
        }
        result
    }
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.sections.iter().map(Section::bytes).sum::<usize>()
            + self
                .packages
                .iter()
                .map(|name| name.capacity() + 256)
                .sum::<usize>()
    }
}

struct File {
    document: Weak<Document>,
    fragment: Arc<Fragment>,
    bytes: usize,
}
#[derive(Default)]
pub(crate) struct Cache(Mutex<VecDeque<File>>);
impl Cache {
    fn fragment(&self, ws: &Workspace, uri: &Url) -> Arc<Fragment> {
        let doc = &ws.documents[uri];
        let document = ws.documents.weak_revision(uri).unwrap();
        {
            let mut files = self.0.lock().unwrap();
            if let Some(at) = files
                .iter()
                .position(|file| file.document.ptr_eq(&document))
            {
                let file = files.remove(at).unwrap();
                let value = file.fragment.clone();
                files.push_back(file);
                return value;
            }
        }
        // A text edit often changes offsets or parameter bodies without changing
        // the candidate names. Intern the contribution, never its source tree.
        let value = Fragment::build(doc);
        let bytes = value.bytes() + std::mem::size_of::<Document>() + 128;
        if bytes > MAX_BYTES {
            return Arc::new(value);
        }
        let mut files = self.0.lock().unwrap();
        let value = files
            .iter()
            .find(|file| *file.fragment == value)
            .map(|file| file.fragment.clone())
            .unwrap_or_else(|| Arc::new(value));
        files.retain(|file| file.document.strong_count() > 0 && !file.document.ptr_eq(&document));
        files.push_back(File {
            document,
            fragment: value.clone(),
            bytes,
        });
        while files.len() > MAX_FILES
            || files.iter().map(|file| file.bytes).sum::<usize>() > MAX_BYTES
        {
            files.pop_front();
        }
        value
    }
    pub(crate) fn prepare(&self, ws: &Workspace) {
        for uri in ws.documents.keys() {
            if AnalysisContext::current().is_some_and(|context| context.cancelled()) {
                break;
            }
            self.fragment(ws, uri);
        }
    }
    fn pool(&self, ws: &Workspace, scope: &BTreeSet<Url>) -> Pool {
        Pool::from_files(
            scope
                .iter()
                .filter(|uri| ws.documents.contains_key(uri))
                .map(|uri| self.fragment(ws, uri))
                .collect(),
        )
    }
}

struct Pool {
    files: Vec<Arc<Fragment>>,
    loaded: BTreeSet<String>,
}
impl Pool {
    fn from_files(files: Vec<Arc<Fragment>>) -> Self {
        let loaded = packages::expand(
            files
                .iter()
                .flat_map(|file| file.packages.iter().cloned())
                .collect(),
        );
        Self { files, loaded }
    }
    fn get(ws: &Workspace, scope: &BTreeSet<Url>) -> Self {
        if let Some(binding) =
            AnalysisContext::current().and_then(|context| context.binding.clone())
        {
            binding.cache.command_candidates().pool(ws, scope)
        } else {
            Self::from_files(
                scope
                    .iter()
                    .filter_map(|uri| ws.documents.get(uri))
                    .map(|doc| Arc::new(Fragment::build(doc)))
                    .collect(),
            )
        }
    }
    fn matching<'a>(
        &'a self,
        prefix: &'a str,
        math: bool,
    ) -> impl Iterator<Item = &'a Candidate> + 'a {
        let locals = (0..3).flat_map(move |phase| {
            self.files
                .iter()
                .flat_map(move |file| file.sections[phase].matching(prefix))
        });
        locals
            .chain(statics().0.matching(prefix).filter(move |item| {
                let Kind::Package { package, command } = item.kind else {
                    unreachable!()
                };
                let package = &packages::catalog()[package];
                self.loaded.contains(&package.name)
                    && (math || !package.commands[command].math_only)
            }))
            .chain(
                statics()
                    .1
                    .matching(prefix)
                    .filter(move |item| math || !catalog::math_command(&item.name)),
            )
    }
}

fn statics() -> &'static (Section, Section) {
    static SECTIONS: OnceLock<(Section, Section)> = OnceLock::new();
    SECTIONS.get_or_init(|| {
        let mut packages = Section::default();
        for (package, meta) in packages::catalog().iter().enumerate() {
            for (command, meta) in meta.commands.iter().enumerate() {
                packages.insert(Candidate {
                    name: meta.name.clone(),
                    kind: Kind::Package { package, command },
                });
            }
        }
        let mut builtins = Section::default();
        for (index, &(name, _, _)) in catalog::COMMANDS.iter().enumerate() {
            builtins.insert(Candidate {
                name: name.into(),
                kind: Kind::Builtin(index),
            });
        }
        (packages, builtins)
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn complete(
    ws: &Workspace,
    uri: &Url,
    scope: &BTreeSet<Url>,
    prefix: &str,
    following: &str,
    range: lsp_types::Range,
    math: bool,
    snippets: bool,
    limit: usize,
) -> Vec<CompletionItem> {
    let pool = Pool::get(ws, scope);
    let signatures = super::signature::CompletionSignatures::new(ws, scope);
    let docs = OnceCell::new();
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for candidate in pool.matching(prefix, math) {
        let name = &candidate.name;
        if !seen.insert(name.as_str()) {
            continue;
        }
        let mut item = CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FUNCTION),
            ..Default::default()
        };
        let (text, snippet) = match &candidate.kind {
            Kind::User {
                environment,
                variable,
                detail,
            } => {
                let signature = signatures.at(name, following);
                item.kind = Some(if *variable {
                    CompletionItemKind::VARIABLE
                } else {
                    CompletionItemKind::FUNCTION
                });
                item.detail = Some(
                    signature
                        .as_ref()
                        .map_or_else(|| detail.clone(), |(label, _)| label.clone()),
                );
                item.documentation = Some(Documentation::String(detail.clone()));
                let docs =
                    docs.get_or_init(|| super::completion_resolve::Commands::new(ws, uri, scope));
                if *environment {
                    docs.apply_environment(&mut item, name);
                } else {
                    docs.apply_command(&mut item, name);
                }
                let snippet = signature
                    .and_then(|(_, snippet)| snippet)
                    .filter(|_| snippets);
                (
                    snippet.clone().unwrap_or_else(|| name.clone()),
                    snippet.is_some(),
                )
            }
            Kind::Conditional { variable, detail } => {
                item.kind = Some(if *variable {
                    CompletionItemKind::VARIABLE
                } else {
                    CompletionItemKind::FUNCTION
                });
                item.detail = Some(detail.clone());
                (name.clone(), false)
            }
            Kind::Package { package, command } => {
                let package = &packages::catalog()[*package];
                let command = &package.commands[*command];
                item.detail = Some(format!("\\{} — {}", name, package.name));
                item.documentation = Some(Documentation::String(packages::documentation(
                    package, command,
                )));
                let snippet = signatures
                    .at(name, following)
                    .and_then(|(_, snippet)| snippet)
                    .filter(|_| snippets);
                (
                    snippet.clone().unwrap_or_else(|| name.clone()),
                    snippet.is_some(),
                )
            }
            Kind::Builtin(index) => {
                let (_, _, documentation) = catalog::COMMANDS[*index];
                let native =
                    name != "thefield" || crate::symbols::commands::uses_builtin(ws, name, scope);
                let snippet =
                    snippets && native && !signatures.has_existing_arguments(name, following);
                item.detail = Some(format!("\\{name}"));
                item.documentation = native.then(|| Documentation::String(documentation.into()));
                (
                    if snippet {
                        catalog::COMMANDS[*index].1.replace('\\', "\\\\")
                    } else {
                        name.clone()
                    },
                    snippet,
                )
            }
        };
        item.insert_text_format = Some(if snippet {
            InsertTextFormat::SNIPPET
        } else {
            InsertTextFormat::PLAIN_TEXT
        });
        item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: text,
        }));
        result.push(item);
        if result.len() >= limit {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(ws: &mut Workspace, uri: &Url, source: &str) {
        let version = ws
            .documents
            .get(uri)
            .and_then(|doc| doc.version)
            .unwrap_or(0)
            + 1;
        ws.documents.insert(
            uri.clone(),
            Document::new(uri, source.into(), Some(version)),
        );
    }
    fn names(pool: &Pool, prefix: &str) -> Vec<String> {
        let mut seen = BTreeSet::new();
        pool.matching(prefix, false)
            .filter(|item| seen.insert(item.name.as_str()))
            .map(|item| item.name.clone())
            .collect()
    }
    fn request(
        ws: &Workspace,
        uri: &Url,
        scope: &BTreeSet<Url>,
        cache: Arc<crate::analysis_cache::Cache>,
        prefix: &str,
        following: &str,
    ) -> Vec<CompletionItem> {
        AnalysisContext::for_workspace(ws, 0, 0, cache).run(|_| {
            complete(
                ws,
                uri,
                scope,
                prefix,
                following,
                lsp_types::Range::default(),
                false,
                true,
                512,
            )
        })
    }

    #[test]
    fn edited_files_replace_only_their_contribution_and_old_snapshots_remain_readable() {
        let a = Url::parse("untitled:a.tex").unwrap();
        let b = Url::parse("untitled:b.tex").unwrap();
        let scope = BTreeSet::from([a.clone(), b.clone()]);
        let mut ws = Workspace::default();
        document(&mut ws, &a, r"\newcommand\projectold{}");
        document(&mut ws, &b, r"\newcommand\projectstable{}");
        let old_workspace = ws.clone();
        let cache = Cache::default();
        let first = cache.pool(&ws, &scope);
        document(&mut ws, &a, r"\newcommand\projectnew{}");
        let next = cache.pool(&ws, &scope);
        assert!(!Arc::ptr_eq(&first.files[0], &next.files[0]));
        assert!(Arc::ptr_eq(&first.files[1], &next.files[1]));
        assert_eq!(names(&first, "project"), ["projectold", "projectstable"]);
        assert_eq!(names(&next, "project"), ["projectnew", "projectstable"]);
        cache.prepare(&old_workspace);
        assert_eq!(
            names(&cache.pool(&ws, &scope), "project"),
            ["projectnew", "projectstable"]
        );
        ws.documents.remove(&b);
        assert_eq!(names(&cache.pool(&ws, &scope), "project"), ["projectnew"]);
        assert_eq!(
            names(&cache.pool(&old_workspace, &BTreeSet::from([b])), "project"),
            ["projectstable"]
        );
    }

    #[test]
    fn candidate_identity_survives_arity_and_comment_changes_but_snippets_and_docs_are_current() {
        let uri = Url::parse("untitled:arity.tex").unwrap();
        let scope = BTreeSet::from([uri.clone()]);
        let mut ws = Workspace::default();
        ws.client.snippets = true;
        document(
            &mut ws,
            &uri,
            "% Old notes\n\\newcommand\\projectmacro[1]{#1}",
        );
        let cache = Arc::new(crate::analysis_cache::Cache::default());
        let first = cache.command_candidates().pool(&ws, &scope);
        let one = request(&ws, &uri, &scope, cache.clone(), "project", "");
        assert!(serde_json::to_string(&one[0])
            .unwrap()
            .contains("${1:arg1}"));
        document(
            &mut ws,
            &uri,
            "% New notes\n\\newcommand\\projectmacro[2]{#1#2}",
        );
        let next = cache.command_candidates().pool(&ws, &scope);
        assert!(
            Arc::ptr_eq(&first.files[0], &next.files[0]),
            "the name pool must not depend on arity or offsets"
        );
        let two = request(&ws, &uri, &scope, cache.clone(), "project", "");
        let two = serde_json::to_string(&two[0]).unwrap();
        assert!(two.contains("${2:arg2}"));
        assert!(two.contains("New notes") && !two.contains("Old notes"));
        let existing = request(&ws, &uri, &scope, cache, "project", " already");
        assert_eq!(
            existing[0].insert_text_format,
            Some(InsertTextFormat::PLAIN_TEXT)
        );
    }

    #[test]
    fn prefix_search_preserves_source_precedence_before_catalog_and_lexical_order() {
        let a = Url::parse("untitled:a.tex").unwrap();
        let b = Url::parse("untitled:b.tex").unwrap();
        let mut ws = Workspace::default();
        document(
            &mut ws,
            &a,
            r"\newcommand\projectz{}\newcommand\projecta{}\newenvironment{projectenv}{}{}\newcommand\input[2]{#1#2}",
        );
        document(&mut ws, &b, r"\newcommand\projectb{}\newcommand\projectz{}");
        let scope = BTreeSet::from([a.clone(), b]);
        let pool = Cache::default().pool(&ws, &scope);
        assert_eq!(
            names(&pool, "project"),
            ["projectz", "projecta", "projectb", "projectenv"]
        );
        ws.client.snippets = true;
        let result = request(&ws, &a, &scope, Arc::default(), "input", "");
        assert_eq!(
            result.iter().filter(|item| item.label == "input").count(),
            1
        );
        assert!(serde_json::to_string(&result[0])
            .unwrap()
            .contains("${2:arg2}"));
        let limited = complete(
            &ws,
            &a,
            &scope,
            "project",
            "",
            lsp_types::Range::default(),
            false,
            false,
            2,
        );
        assert_eq!(
            limited
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["projectz", "projecta"]
        );
    }

    #[test]
    fn package_and_conditional_contributions_follow_current_syntax_and_scope() {
        let uri = Url::parse("untitled:packages.tex").unwrap();
        let other = Url::parse("untitled:other.tex").unwrap();
        let mut ws = Workspace::default();
        document(&mut ws, &uri, r"\usepackage{amsmath}\newif\ifproject");
        document(&mut ws, &other, r"\usepackage{tikz}");
        let scope = BTreeSet::from([uri.clone()]);
        let cache = Cache::default();
        let first = cache.pool(&ws, &scope);
        assert_eq!(first.loaded, packages::loaded(&ws, &scope));
        assert!(names(&first, "ifproject").contains(&"ifproject".into()));
        document(&mut ws, &uri, r"\usepackage{siunitx}");
        let next = cache.pool(&ws, &scope);
        assert_eq!(next.loaded, packages::loaded(&ws, &scope));
        assert!(!next.loaded.contains("tikz"));
        assert!(names(&next, "ifproject").is_empty());
        assert!(!next.loaded.contains("amsmath"));
    }

    #[test]
    fn cache_limits_do_not_truncate_results_or_mix_workspaces() {
        let cache = Cache::default();
        let mut ws = Workspace::default();
        for index in 0..MAX_FILES + 5 {
            let uri = Url::parse(&format!("untitled:{index}.tex")).unwrap();
            document(
                &mut ws,
                &uri,
                &format!("\\newcommand\\project{}{{}}", "x".repeat(index + 1)),
            );
        }
        cache.prepare(&ws);
        let files = cache.0.lock().unwrap();
        assert!(files.len() <= MAX_FILES);
        assert!(files.iter().map(|file| file.bytes).sum::<usize>() <= MAX_BYTES);
        drop(files);
        let scope = ws.documents.keys().cloned().collect();
        assert_eq!(
            names(&cache.pool(&ws, &scope), "project").len(),
            MAX_FILES + 5
        );
        let uri = ws.documents.keys().next().unwrap().clone();
        let mut other = Workspace::default();
        document(&mut other, &uri, r"\newcommand\projectother{}");
        assert_eq!(
            names(&cache.pool(&other, &BTreeSet::from([uri])), "project"),
            ["projectother"]
        );
    }

    #[test]
    fn indexed_signature_lookup_keeps_alias_ambiguity_environment_and_generated_meanings() {
        let uri = Url::parse("untitled:meanings.tex").unwrap();
        let scope = BTreeSet::from([uri.clone()]);
        for source in [
            r"\newcommand\reader[2]{#1#2}\def\forward{\reader}",
            r"\newcommand\reader[1]{#1}\newcommand\reader[2]{#1#2}\def\forward{\reader}",
            r"\newenvironment{reader}[2]{}{}\newenvironment{forward}[1]{}{}",
            r"\newenvironment{reader}[1]{}{}\newenvironment{reader}[2]{}{}",
            r"\newif\ifreader\newcounter{field}\def\bibopenparen{custom}",
        ] {
            let mut ws = Workspace::default();
            document(&mut ws, &uri, source);
            let indexed = super::super::signature::CompletionSignatures::new(&ws, &scope);
            for name in [
                "reader",
                "forward",
                "input",
                "thefield",
                "ifreader",
                "mkbibparens",
                "lstinline",
                "num",
            ] {
                let direct = crate::symbols::commands::signature(&ws, name, &scope);
                assert!(indexed.resolved(name) == direct, "{source}: {name}");
                for following in ["", " text", "{arg}", "|literal|", "[opt]"] {
                    assert_eq!(
                        indexed.has_existing_arguments(name, following),
                        super::super::signature::has_existing_arguments(
                            &ws, name, &scope, following
                        ),
                        "{source}: {name}: {following}"
                    );
                }
            }
        }
    }

    #[test]
    fn declaration_edits_prepare_immediately_but_subsequent_prose_does_not() {
        let uri = Url::parse("untitled:preparation.tex").unwrap();
        let source = "\\newcommand\\project[1]{\n#1\n}\n\\begin{document}\nText\n\\end{document}";
        let before = Document::new(&uri, source.into(), Some(1));
        for (source, expected) in [
            (source.replace("[1]", "[2]"), true),
            (source.replace("#1", "#1#2"), true),
            (source.replace("project", "changed"), true),
            (
                source.replace(
                    "\\begin{document}",
                    "\\newcommand\\newmacro{}\n\\begin{document}",
                ),
                true,
            ),
            (
                source.replace("\\newcommand\\project[1]{\n#1\n}\n", ""),
                true,
            ),
            (source.replace("Text", "% typing\nText"), false),
            (source.replace("Text", "Text \\inp"), false),
            (source.to_owned(), false),
        ] {
            let after = Document::new(&uri, source.clone(), Some(2));
            assert_eq!(changed_declarations(&before, &after), expected, "{source}");
        }
    }
}
