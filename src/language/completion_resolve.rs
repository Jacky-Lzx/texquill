//! Resolve only documentation, retaining the completion's original edits.
use crate::rpc::{Error, ErrorCode, Result};
use crate::{parser::syntax, project::workspace::Workspace, symbols::command_docs};
use lsp_types::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    hash::{Hash, Hasher},
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    origin: Url,
    definition: Url,
    name: String,
    namespace: command_docs::Namespace,
    fingerprint: String,
}

pub(super) fn fingerprint(ws: &Workspace, scope: &BTreeSet<Url>) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    ws.index_truncated.hash(&mut hash);
    for uri in scope {
        uri.hash(&mut hash);
        if let Some(doc) = ws.documents.get(uri) {
            doc.text.source.hash(&mut hash);
            doc.version.hash(&mut hash);
        }
    }
    format!("{:016x}", hash.finish())
}

pub(crate) struct Commands<'a> {
    origin: &'a Url,
    index: command_docs::Index<'a>,
    fingerprint: Option<String>,
}

impl<'a> Commands<'a> {
    pub(crate) fn new(ws: &'a Workspace, origin: &'a Url, scope: &BTreeSet<Url>) -> Self {
        Self {
            origin,
            index: command_docs::Index::new(ws, scope),
            fingerprint: ws
                .client
                .completion_resolve_documentation
                .then(|| fingerprint(ws, scope)),
        }
    }

    pub(crate) fn apply(&self, item: &mut CompletionItem, entry: &syntax::Entry) {
        self.apply_name(item, &entry.name, command_docs::Namespace::Command);
    }

    pub(crate) fn apply_environment(&self, item: &mut CompletionItem, name: &str) {
        self.apply_name(item, name, command_docs::Namespace::Environment);
    }

    fn apply_name(
        &self,
        item: &mut CompletionItem,
        name: &str,
        namespace: command_docs::Namespace,
    ) {
        let Some(declaration) = self.index.get(name, namespace) else {
            return;
        };
        if let Some(fingerprint) = &self.fingerprint {
            item.documentation = None;
            item.data = Some(
                serde_json::to_value(Identity {
                    origin: self.origin.clone(),
                    definition: declaration.uri.clone(),
                    name: name.into(),
                    namespace,
                    fingerprint: fingerprint.clone(),
                })
                .unwrap(),
            );
        } else {
            item.documentation = self
                .index
                .documentation(name, namespace)
                .map(Documentation::String);
        }
    }
}

fn modified() -> Error {
    Error {
        code: ErrorCode::ServerError(-32801),
        message: "Completion item is outdated; request completions again".into(),
        data: None,
    }
}

pub(crate) fn resolve(ws: &Workspace, mut item: CompletionItem) -> Result<CompletionItem> {
    let Some(data) = &item.data else {
        return Ok(item);
    };
    let identity: Identity = serde_json::from_value(data.clone())
        .map_err(|_| Error::invalid_params("Invalid completion item identity"))?;
    if item.label != identity.name || !ws.documents.contains_key(&identity.origin) {
        return Err(modified());
    }
    let scope = ws.symbol_scope(&identity.origin);
    if fingerprint(ws, &scope) != identity.fingerprint {
        return Err(modified());
    }
    let index = command_docs::Index::new(ws, &scope);
    if index
        .get(&identity.name, identity.namespace)
        .is_none_or(|declaration| *declaration.uri != identity.definition)
    {
        return Err(modified());
    }
    item.documentation = index
        .documentation(&identity.name, identity.namespace)
        .map(Documentation::String);
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(deferred: bool) -> (Workspace, Url) {
        let uri = Url::parse("untitled:completion.tex").unwrap();
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.client.snippets = true;
        ws.client.completion_resolve_documentation = deferred;
        ws.open(
            uri.clone(),
            "% 中文😀 **plain** <tag>\r\n\\newcommand{\\example}[1]{#1}\r\n😀 \\exaBAD".into(),
            1,
        );
        (ws, uri)
    }

    fn candidate(ws: &Workspace, uri: &Url) -> CompletionItem {
        let doc = &ws.documents[uri];
        let offset = doc.text.source.rfind("\\exaBAD").unwrap() + 4;
        crate::language::features::completion(ws, uri, doc.text.position(offset))
            .into_iter()
            .find(|item| item.label == "example")
            .unwrap()
    }

    #[test]
    fn inline_and_deferred_documentation_agree_and_resolution_preserves_all_other_fields() {
        let (mut ws, uri) = fixture(false);
        let inline = candidate(&ws, &uri);
        assert!(inline.data.is_none());
        let Documentation::String(text) = inline.documentation.as_ref().unwrap() else {
            panic!()
        };
        assert!(text.ends_with("中文😀 **plain** <tag>"));
        ws.client.completion_resolve_documentation = true;
        let deferred = candidate(&ws, &uri);
        assert!(deferred.documentation.is_none());
        let resolved = resolve(&ws, deferred.clone()).unwrap();
        assert_eq!(resolved.documentation, inline.documentation);
        let mut expected = deferred;
        expected.documentation = inline.documentation;
        assert_eq!(resolved, expected);
        assert_eq!(resolve(&ws, resolved.clone()).unwrap(), resolved);
        assert_eq!(resolved.text_edit, inline.text_edit);
        assert_eq!(resolved.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn changed_source_versions_ambiguities_and_forged_identity_are_rejected() {
        let (mut ws, uri) = fixture(true);
        let item = candidate(&ws, &uri);
        for field in ["label", "fingerprint", "definition", "origin"] {
            let mut forged = serde_json::to_value(&item).unwrap();
            if field == "label" {
                forged["label"] = serde_json::json!("other");
            } else {
                forged["data"][field] = serde_json::json!(if field == "fingerprint" {
                    "invalid"
                } else {
                    "untitled:elsewhere.tex"
                });
            }
            assert_eq!(
                resolve(&ws, serde_json::from_value(forged).unwrap())
                    .unwrap_err()
                    .code,
                ErrorCode::ServerError(-32801)
            );
        }
        let source = ws.documents[&uri].text.source.clone();
        ws.open(uri.clone(), source.clone(), 2);
        assert_eq!(
            resolve(&ws, item).unwrap_err().code,
            ErrorCode::ServerError(-32801)
        );
        let item = candidate(&ws, &uri);
        ws.open(
            uri.clone(),
            format!("\\def\\example{{duplicate}}\n{source}"),
            3,
        );
        assert!(resolve(&ws, item).is_err());
        let ambiguous = candidate(&ws, &uri);
        assert!(ambiguous.data.is_none());
        let Documentation::String(text) = ambiguous.documentation.unwrap() else {
            panic!()
        };
        assert!(!text.contains("**plain**"));
    }

    #[test]
    fn unknown_item_without_identity_is_unchanged_and_malformed_identity_is_invalid() {
        let (ws, _) = fixture(true);
        let mut item = CompletionItem {
            label: "section".into(),
            ..Default::default()
        };
        assert_eq!(resolve(&ws, item.clone()).unwrap(), item);
        item.data = Some(serde_json::json!({"origin":"untitled:completion.tex"}));
        assert_eq!(
            resolve(&ws, item).unwrap_err().code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn project_scope_changes_invalidate_items_and_unrelated_projects_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = Url::from_file_path(dir.path().join("main.tex")).unwrap();
        let child = Url::from_file_path(dir.path().join("macros.tex")).unwrap();
        let other = Url::from_file_path(dir.path().join("other.tex")).unwrap();
        let source = "\\documentclass{article}\n\\input{macros}\n\\exaBAD";
        let definitions = "% A project macro\n\\newcommand{\\example}{x}";
        for (uri, source) in [
            (&root, source),
            (&child, definitions),
            (
                &other,
                "\\documentclass{article}\n% Other\n\\def\\example{other}",
            ),
        ] {
            std::fs::write(uri.to_file_path().unwrap(), source).unwrap();
        }
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        ws.client.completion_resolve_documentation = true;
        ws.open(root.clone(), source.into(), 1);
        ws.open(child.clone(), definitions.into(), 1);
        ws.open(
            other.clone(),
            std::fs::read_to_string(other.to_file_path().unwrap()).unwrap(),
            1,
        );
        let item = candidate(&ws, &root);
        ws.open(
            other.clone(),
            "\\documentclass{article}\n\\def\\example{changed}".into(),
            2,
        );
        let resolved = resolve(&ws, item.clone()).unwrap();
        assert!(
            matches!(resolved.documentation, Some(Documentation::String(ref text)) if text.contains("A project macro"))
        );
        ws.open(
            child,
            "% Updated project macro\n\\newcommand{\\example}{x}".into(),
            2,
        );
        assert!(resolve(&ws, item).is_err());
    }

    fn environment_item(ws: &Workspace, uri: &Url, needle: &str, name: &str) -> CompletionItem {
        let doc = &ws.documents[uri];
        let at = doc.text.source.rfind(needle).unwrap() + needle.len();
        crate::language::features::completion(ws, uri, doc.text.position(at))
            .into_iter()
            .find(|item| item.label == name)
            .unwrap()
    }

    fn documentation(item: &CompletionItem) -> &str {
        match item.documentation.as_ref().unwrap() {
            Documentation::String(value) => value,
            _ => panic!("source documentation must be plain text"),
        }
    }

    #[test]
    fn environment_documentation_keeps_macro_namespace_and_original_completion_edits() {
        let uri = Url::parse("untitled:environments.tex").unwrap();
        let source = "% Macro documentation\r\n\\newcommand{\\example}[1]{#1}\r\n% 中文😀 **Environment** <tag>\r\n\\NewDocumentEnvironment{example}{O{x} m}{}{}\r\n% Direct environment\r\n\\newenvironment{onlyenv}[1]{}{}\r\n😀 \\begin{exaBAD}\r\n\\end{exaBAD}\r\n\\exaBAD\r\n\\onlyBAD";
        let mut ws = Workspace {
            client: crate::project::state::ClientCapabilities {
                snippets: true,
                ..Default::default()
            },
            ..Default::default()
        };
        ws.open(uri.clone(), source.into(), 1);
        for (needle, name, expected, excluded) in [
            (
                "\\begin{exa",
                "example",
                "中文😀 **Environment** <tag>",
                "Macro documentation",
            ),
            (
                "\\end{exa",
                "example",
                "中文😀 **Environment** <tag>",
                "Macro documentation",
            ),
            ("\\exa", "example", "Macro documentation", "**Environment**"),
            (
                "\\only",
                "onlyenv",
                "Direct environment",
                "Macro documentation",
            ),
        ] {
            ws.client.completion_resolve_documentation = false;
            let inline = environment_item(&ws, &uri, needle, name);
            assert!(documentation(&inline).contains(expected));
            assert!(!documentation(&inline).contains(excluded));
            ws.client.completion_resolve_documentation = true;
            let deferred = environment_item(&ws, &uri, needle, name);
            assert!(deferred.documentation.is_none());
            let resolved = resolve(&ws, deferred.clone()).unwrap();
            let mut expected_item = deferred;
            expected_item.documentation = inline.documentation;
            assert_eq!(resolved, expected_item);
            assert_eq!(resolved.text_edit, inline.text_edit);
            assert_eq!(resolve(&ws, resolved.clone()).unwrap(), resolved);
        }
    }

    #[test]
    fn environment_documentation_resolution_rejects_updates_and_duplicate_declarations() {
        let uri = Url::parse("untitled:environment-updates.tex").unwrap();
        let mut ws = Workspace {
            client: crate::project::state::ClientCapabilities {
                completion_resolve_documentation: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let source = "% Environment notes\n\\newenvironment{example}{}{}\n\\begin{exa}";
        ws.open(uri.clone(), source.into(), 1);
        let item = environment_item(&ws, &uri, "\\begin{exa", "example");
        ws.open(
            uri.clone(),
            source.replace("Environment notes", "Updated notes"),
            2,
        );
        assert_eq!(
            resolve(&ws, item).unwrap_err().code,
            ErrorCode::ServerError(-32801)
        );
        let updated = environment_item(&ws, &uri, "\\begin{exa", "example");
        assert!(documentation(&resolve(&ws, updated.clone()).unwrap()).contains("Updated notes"));
        ws.open(
            uri.clone(),
            format!("{source}\n% Duplicate notes\n\\renewenvironment{{example}}{{}}{{}}"),
            3,
        );
        assert!(resolve(&ws, updated).is_err());
        let ambiguous = environment_item(&ws, &uri, "\\begin{exa", "example");
        assert!(ambiguous.data.is_none());
        assert!(ambiguous.documentation.is_none());
    }

    #[test]
    fn environment_name_completion_retains_multiline_edits_and_builtin_fallback() {
        let uri = Url::parse("untitled:multiline-environment.tex").unwrap();
        let mut ws = Workspace {
            client: crate::project::state::ClientCapabilities {
                completion_resolve_documentation: true,
                ..Default::default()
            },
            ..Default::default()
        };
        ws.open(uri.clone(), "% Environment notes\r\n\\newenvironment{example}{}{}\r\n\\begin{ \r\n  exaBAD \r\n }\r\n\\begin{ite}".into(), 1);
        let item = environment_item(&ws, &uri, "  exa", "example");
        assert!(!item.additional_text_edits.as_ref().unwrap().is_empty());
        let resolved = resolve(&ws, item.clone()).unwrap();
        assert!(documentation(&resolved).contains("Environment notes"));
        assert_eq!(resolved.additional_text_edits, item.additional_text_edits);
        assert_eq!(resolved.text_edit, item.text_edit);
        let builtin = environment_item(&ws, &uri, "\\begin{ite", "itemize");
        assert!(builtin.data.is_none());
        assert_eq!(resolve(&ws, builtin.clone()).unwrap(), builtin);
    }
}
