//! Source comments document a unique user declaration, without markup execution.
use crate::{
    parser::syntax,
    project::workspace::{Document, Workspace},
};
use lsp_types::Url;
use std::collections::{BTreeMap, BTreeSet};

const MAX_LINES: usize = 32;
const MAX_BYTES: usize = 8192;

fn directive(text: &str) -> bool {
    text.split_whitespace().next().is_some_and(|word| {
        word.eq_ignore_ascii_case("!tex")
            || word.eq_ignore_ascii_case("!bib")
            || word.eq_ignore_ascii_case("arara:")
            || word.eq_ignore_ascii_case("vim:")
    })
}

pub(crate) fn for_command(ws: &Workspace, scope: &BTreeSet<Url>, name: &str) -> Option<String> {
    Index::new(ws, scope).notes(name, Namespace::Command)
}

pub(crate) fn for_environment(ws: &Workspace, scope: &BTreeSet<Url>, name: &str) -> Option<String> {
    Index::new(ws, scope).notes(name, Namespace::Environment)
}

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Namespace {
    Command,
    Environment,
}

pub(crate) struct Declaration<'a> {
    pub(crate) uri: &'a Url,
    pub(crate) doc: &'a Document,
    pub(crate) entry: &'a syntax::Entry,
}

/// Build once for a completion list, instead of scanning the project per item.
pub(crate) struct Index<'a> {
    definitions: BTreeMap<&'a str, Option<Declaration<'a>>>,
    environments: BTreeMap<&'a str, Option<Declaration<'a>>>,
}

impl<'a> Index<'a> {
    pub(crate) fn declaration(
        &self,
        name: &str,
        namespace: Namespace,
    ) -> Result<Option<&Declaration<'a>>, ()> {
        let index = match namespace {
            Namespace::Command => &self.definitions,
            Namespace::Environment => &self.environments,
        };
        match index.get(name) {
            None => Ok(None),
            Some(Some(declaration)) => Ok(Some(declaration)),
            Some(None) => Err(()),
        }
    }
    pub(crate) fn new(ws: &'a Workspace, scope: &BTreeSet<Url>) -> Self {
        let mut definitions = BTreeMap::new();
        let mut environments = BTreeMap::new();
        for (uri, doc) in ws.documents.iter().filter(|(uri, _)| scope.contains(*uri)) {
            for (entries, index) in [
                (&doc.syntax.definitions, &mut definitions),
                (&doc.syntax.environments, &mut environments),
            ] {
                for entry in entries {
                    index
                        .entry(entry.name.as_str())
                        .and_modify(|entry| *entry = None)
                        .or_insert(Some(Declaration { uri, doc, entry }));
                }
            }
        }
        Self {
            definitions,
            environments,
        }
    }

    pub(crate) fn get(&self, name: &str, namespace: Namespace) -> Option<&Declaration<'a>> {
        match namespace {
            Namespace::Command => &self.definitions,
            Namespace::Environment => &self.environments,
        }
        .get(name)?
        .as_ref()
    }

    fn notes(&self, name: &str, namespace: Namespace) -> Option<String> {
        let declaration = self.get(name, namespace)?;
        comments(declaration.doc, declaration.entry)
    }

    pub(crate) fn documentation(&self, name: &str, namespace: Namespace) -> Option<String> {
        let declaration = self.get(name, namespace)?;
        let mut text = declaration.entry.detail.clone();
        if let Some(notes) = self.notes(name, namespace) {
            text.push_str("\n\n");
            text.push_str(&notes);
        }
        Some(text)
    }
}

pub(crate) fn comments(doc: &Document, entry: &syntax::Entry) -> Option<String> {
    let source = &doc.text.source;
    let declaration = crate::symbols::commands::command_declaration_start(&doc.syntax, entry)?;
    let line_start = source[..declaration].rfind('\n').map_or(0, |at| at + 1);
    let mut prefix = &source[line_start..declaration];
    loop {
        prefix = prefix.trim_start_matches([' ', '\t']);
        if prefix.is_empty() {
            break;
        }
        let token = syntax::token(prefix, 0)?;
        if !matches!(token, "\\long" | "\\outer" | "\\global" | "\\protected") {
            return None;
        }
        prefix = &prefix[token.len()..];
    }
    let mut end = line_start;
    let mut lines = Vec::new();
    let mut bytes = 0;
    while end > 0 {
        let previous_end = end - 1;
        let start = source[..previous_end].rfind('\n').map_or(0, |at| at + 1);
        let line = source[start..previous_end]
            .strip_suffix('\r')
            .unwrap_or(&source[start..previous_end]);
        let contents = line.trim_start_matches([' ', '\t']);
        if !contents.starts_with('%') {
            break;
        }
        let at = start + line.len() - contents.len();
        if doc
            .syntax
            .comments
            .binary_search_by_key(&at, |span| span.start)
            .is_err()
        {
            break;
        }
        let contents = contents.trim_start_matches('%');
        let contents = contents.strip_prefix(' ').unwrap_or(contents);
        if directive(contents.trim()) {
            break;
        }
        bytes += line.len();
        if lines.len() >= MAX_LINES || bytes > MAX_BYTES {
            return None;
        }
        lines.push(contents);
        end = start;
    }
    lines.reverse();
    let start = lines.iter().position(|line| !line.trim().is_empty())?;
    let end = lines.iter().rposition(|line| !line.trim().is_empty())? + 1;
    Some(lines[start..end].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn notes(source: &str) -> Option<String> {
        let uri = Url::parse("untitled:docs.tex").unwrap();
        let doc = Document::new(&uri, source.into(), Some(1));
        comments(
            &doc,
            doc.syntax
                .definitions
                .iter()
                .find(|entry| entry.name == "example")
                .unwrap(),
        )
    }
    #[test]
    fn adjacent_real_comments_keep_paragraphs_unicode_and_literal_markup() {
        assert_eq!(notes("%% A 中文😀 command.\r\n%\r\n% **literal** <script>text</script> [link](https://example.test)\r\n\\newcommand{\\example}[1]{#1}"), Some("A 中文😀 command.\n\n**literal** <script>text</script> [link](https://example.test)".into()));
        assert_eq!(
            notes("%\n% Note\n%%\n\\def\\example{}"),
            Some("Note".into())
        );
    }
    #[test]
    fn blank_lines_code_inline_comments_and_magic_directives_end_the_block() {
        for source in [
            "% Old note\n\n\\newcommand{\\example}{}",
            "Code % not documentation\n\\newcommand{\\example}{}",
            "% Old note\nCode\n\\newcommand{\\example}{}",
            "% !TeX program = xelatex\n\\newcommand{\\example}{}",
            "% arara: pdflatex\n\\newcommand{\\example}{}",
            "% Note\nText \\newcommand{\\example}{}",
        ] {
            assert_eq!(notes(source), None, "{source}");
        }
        assert_eq!(
            notes("% !TeX root = main.tex\n% Actual documentation\n\\newcommand{\\example}{}"),
            Some("Actual documentation".into())
        );
    }
    #[test]
    fn declaration_comments_work_with_multiline_headers_modifiers_and_named_forms() {
        for declaration in [
            "\\newcommand\n  {\\example}[1]{#1}",
            "\\NewDocumentCommand{\\example}{m}{#1}",
            "\\long\\def\\example#1{#1}",
            "\\global \\protected \\def\\example{}",
            "\\csdef{example}{}",
            "\\let\\example\\relax",
        ] {
            assert_eq!(
                notes(&format!("% Documentation\n{declaration}")),
                Some("Documentation".into()),
                "{declaration}"
            );
        }
    }
    #[test]
    fn preceding_raw_material_and_over_budget_blocks_do_not_become_documentation() {
        assert_eq!(
            notes("\\begin{verbatim}\n% Raw note\n\\end{verbatim}\n\\newcommand{\\example}{}"),
            None
        );
        assert_eq!(
            notes(&format!(
                "{}\\newcommand{{\\example}}{{}}",
                "% Notes\n".repeat(MAX_LINES + 1)
            )),
            None
        );
        assert_eq!(
            notes(&format!(
                "% {}\n\\newcommand{{\\example}}{{}}",
                "文".repeat(MAX_BYTES)
            )),
            None
        );
    }
    #[test]
    fn project_resolution_requires_a_unique_user_definition() {
        let mut ws = Workspace::default();
        let a = Url::parse("untitled:a.tex").unwrap();
        let b = Url::parse("untitled:b.tex").unwrap();
        ws.open(a.clone(), "% First\n\\newcommand{\\example}{}".into(), 1);
        ws.open(b.clone(), "% Second\n\\newcommand{\\example}{}".into(), 1);
        assert_eq!(
            for_command(&ws, &[a.clone()].into(), "example"),
            Some("First".into())
        );
        assert_eq!(for_command(&ws, &[a, b].into(), "example"), None);
        assert_eq!(for_command(&ws, &BTreeSet::new(), "textbf"), None);
    }

    #[test]
    fn environment_documentation_uses_its_own_unique_declaration_and_comment_rules() {
        let uri = Url::parse("untitled:environments.tex").unwrap();
        for declaration in [
            "\\newenvironment{example}[1]{}{}",
            "\\NewDocumentEnvironment{example}{m}{}{}",
            "\\NewEnviron{example}{}",
            "\\DeclareEnvironmentCopy{example}{itemize}",
        ] {
            let source = format!("% Macro notes\r\n\\newcommand{{\\example}}{{}}\r\n%% 中文😀 **plain**\r\n{declaration}\r\n\\begin{{example}}body\\end{{example}}");
            let mut ws = Workspace::default();
            ws.open(uri.clone(), source.clone(), 1);
            let scope = [uri.clone()].into();
            assert_eq!(
                for_command(&ws, &scope, "example"),
                Some("Macro notes".into())
            );
            assert_eq!(
                for_environment(&ws, &scope, "example"),
                Some("中文😀 **plain**".into()),
                "{declaration}"
            );
            let doc = &ws.documents[&uri];
            for needle in ["\\begin{example", "\\end{example"] {
                let at = source.rfind(needle).unwrap() + needle.len() - 2;
                let hover =
                    crate::language::features::hover(&ws, &uri, doc.text.position(at)).unwrap();
                let lsp_types::HoverContents::Markup(value) = hover.contents else {
                    panic!()
                };
                assert_eq!(value.kind, lsp_types::MarkupKind::PlainText);
                assert!(value.value.contains("中文😀 **plain**"), "{}", value.value);
                assert!(!value.value.contains("Macro notes"));
            }
            ws.open(
                uri.clone(),
                format!("{source}\n% Duplicate notes\n\\renewenvironment{{example}}{{}}{{}}"),
                2,
            );
            assert_eq!(for_environment(&ws, &scope, "example"), None);
            assert_eq!(
                for_command(&ws, &scope, "example"),
                Some("Macro notes".into())
            );
        }
    }
}
