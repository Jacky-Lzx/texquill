//! Curated, extensible package metadata; distribution sources stay read-only.
use crate::{parser::syntax, project::workspace::Workspace};
use lsp_types::Url;
use serde::Deserialize;
use std::{collections::BTreeSet, sync::OnceLock};

#[derive(Deserialize)]
pub(crate) struct Package {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub requires: Vec<String>,
    pub commands: Vec<Command>,
}

#[derive(Deserialize)]
pub(crate) struct Command {
    pub name: String,
    pub parameters: Vec<String>,
    pub description: String,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub math_only: bool,
}

pub(crate) fn catalog() -> &'static [Package] {
    static CATALOG: OnceLock<Vec<Package>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/packages.json"
        )))
        .expect("valid bundled package metadata")
    })
}

pub(crate) fn loaded(workspace: &Workspace, scope: &BTreeSet<Url>) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    for (uri, doc) in &workspace.documents {
        if !scope.contains(uri) {
            continue;
        }
        result.extend(direct(doc));
    }
    expand(result)
}

pub(crate) fn direct(doc: &crate::project::workspace::Document) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    for command in &doc.syntax.commands {
        if doc.syntax.control_operands.contains(&command.span.start)
            || syntax::loader_extension(&command.name) != Some("sty")
        {
            continue;
        }
        if let Some(arg) = syntax::file_argument(&doc.text.source, command) {
            for name in arg.value.split(',').map(syntax::loader_name) {
                if syntax::is_static_key(&name) {
                    result.insert(name);
                }
            }
        }
    }
    result
}

pub(crate) fn expand(mut result: BTreeSet<String>) -> BTreeSet<String> {
    loop {
        let before = result.len();
        for package in catalog() {
            if result.contains(&package.name) {
                result.extend(package.requires.iter().cloned());
            }
        }
        if result.len() == before {
            return result;
        }
    }
}

pub(crate) fn command(
    workspace: &Workspace,
    scope: &BTreeSet<Url>,
    name: &str,
) -> Option<(&'static Package, &'static Command)> {
    let loaded = loaded(workspace, scope);
    catalog()
        .iter()
        .filter(|package| loaded.contains(&package.name))
        .find_map(|package| {
            package
                .commands
                .iter()
                .find(|command| command.name == name)
                .map(|command| (package, command))
        })
}

pub(crate) fn documentation(package: &Package, command: &Command) -> String {
    format!(
        "{}\n\nPackage: {}\nDocumentation: https://texdoc.org/pkg/{}",
        command.description, package.name, package.name
    )
}
