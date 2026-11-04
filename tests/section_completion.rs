use lsp_types::*;
use texquill::{
    language::{features, signature},
    project::workspace::Workspace,
};

fn named_candidate(definition: &str, call: &str, name: &str) -> CompletionItem {
    let mut ws = Workspace::default();
    ws.config.distribution.enabled = false;
    ws.client.snippets = true;
    let uri = Url::parse("untitled:section-completion.tex").unwrap();
    let source = format!("{definition}\n{call}");
    let offset = source.find('¦').unwrap();
    ws.open(uri.clone(), source.replace('¦', ""), 1);
    let position = ws.documents[&uri].text.position(offset);
    features::completion(&ws, &uri, position)
        .into_iter()
        .find(|item| item.label == name)
        .unwrap()
}

fn candidate(definition: &str, call: &str) -> CompletionItem {
    named_candidate(definition, call, "subsubsection")
}

#[test]
fn startsection_wrapper_completion_reads_title_after_six_bound_arguments() {
    // IEEEtran declares these inside several class-option branches.
    let definition = "\\def\\subsubsection{\\@startsection{subsubsection}{3}{\\parindent}{0ex plus 0.1ex minus 0.1ex}%\n{0ex}{\\normalfont\\normalsize\\itshape}}";
    for definition in [definition.to_owned(), format!("{definition}\n{definition}")] {
        let item = candidate(&definition, r"\subsubs¦");
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let Some(CompletionTextEdit::Edit(edit)) = item.text_edit else {
            panic!("missing edit")
        };
        assert_eq!(edit.new_text, "subsubsection{${1:arg2}}");
    }
}

#[test]
fn startsection_wrappers_preserve_existing_titles_and_zero_argument_redefinitions() {
    let definition =
        r"\def\subsubsection{\@startsection{subsubsection}{3}{0pt}{0pt}{0pt}{\itshape}}";
    for call in [
        r"\subsubs¦{Existing}",
        r"\subsubs¦[Short]{Existing}",
        r"\subsubs¦*{Existing}",
    ] {
        let item = candidate(definition, call);
        let Some(CompletionTextEdit::Edit(edit)) = item.text_edit else {
            panic!("missing edit")
        };
        assert_eq!(edit.new_text, "subsubsection", "{call}");
    }
    for body in [
        r"Text",
        r"\@startsection{x}{3}{0pt}{0pt}{0pt}",
        r"\@startsection{x}{3}{0pt}{0pt}{0pt}{}{Fixed}",
    ] {
        let item = candidate(&format!(r"\def\subsubsection{{{body}}}"), r"\subsubs¦");
        assert_eq!(
            item.insert_text_format,
            Some(InsertTextFormat::PLAIN_TEXT),
            "{body}"
        );
    }
}

#[test]
fn startsection_wrapper_signature_supports_star_and_short_title() {
    let mut ws = Workspace::default();
    ws.config.distribution.enabled = false;
    let uri = Url::parse("untitled:section-signature.tex").unwrap();
    ws.open(uri.clone(), "\\def\\subsubsection{\\@startsection{subsubsection}{3}{0pt}{0pt}{0pt}{\\itshape}}\n\\subsubsection[Short]{Title}".into(), 1);
    let offset = ws.documents[&uri].text.source.rfind("Title").unwrap() + 2;
    let help = signature::help(&ws, &uri, ws.documents[&uri].text.position(offset)).unwrap();
    assert!(help.signatures[0].label.contains("short title"));
    assert_eq!(help.active_parameter, Some(1));
}

#[test]
fn deferred_redefinitions_do_not_hide_immediate_section_readers() {
    let definitions = r"\def\section{\@startsection{section}{1}{0pt}{0pt}{0pt}{\bfseries}}
\def\section{\@startsection{section}{1}{0pt}{1ex}{1ex}{\normalfont}}
\newcommand{\appendix}[1][]{\def\section{\@ifstar{\savedsection*}{\discardtitle}}}
\def\appendices{\def\section{\@ifstar{\savedsection*}{\refstepcounter{section}\processtitle}}}";
    for call in [r"\sect¦", r"\section¦"] {
        let item = named_candidate(definitions, call, "section");
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let Some(CompletionTextEdit::Edit(edit)) = item.text_edit else {
            panic!("missing edit")
        };
        assert_eq!(edit.new_text, "section{${1:arg2}}");
    }
}

#[test]
fn deferred_builtin_redefinitions_keep_native_completion_templates() {
    let definitions = r"\long\def\makecaption#1#2{\let\savedlabel\label
\def\label##1{\gdef\savedargs{##1}}\let\label\savedlabel}
\def\thebibliography#1{\let\savedbibitem\bibitem\def\bibitem{\prefix\savedbibitem}}
\def\disableglobals{\let\gdef\def\let\xdef\edef}";
    for name in ["label", "bibitem", "gdef", "xdef"] {
        let call = format!("\\{name}¦");
        let item = named_candidate(definitions, &call, name);
        assert_eq!(
            item.insert_text_format,
            Some(InsertTextFormat::SNIPPET),
            "{name}"
        );
        let Some(CompletionTextEdit::Edit(edit)) = item.text_edit else {
            panic!("missing edit")
        };
        let template = texquill::project::catalog::COMMANDS
            .iter()
            .find(|(n, _, _)| *n == name)
            .unwrap()
            .1;
        assert_eq!(edit.new_text, template.replace('\\', "\\\\"), "{name}");
    }
}

#[test]
fn immediate_overrides_and_conflicting_section_readers_remain_authoritative() {
    for name in ["section", "label", "bibitem"] {
        let definitions =
            format!("\\def\\{name}{{zero arguments}}\n\\def\\installer{{\\def\\{name}##1{{##1}}}}");
        let item = named_candidate(&definitions, &format!("\\{name}¦"), name);
        assert_eq!(
            item.insert_text_format,
            Some(InsertTextFormat::PLAIN_TEXT),
            "{name}"
        );
    }
    let definitions = r"\def\section{\@startsection{section}{1}{0pt}{0pt}{0pt}{}}
\def\section{zero arguments}";
    let item = named_candidate(definitions, r"\sect¦", "section");
    assert_eq!(item.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
}

#[test]
fn every_startsection_heading_preserves_existing_arguments() {
    for name in [
        "section",
        "subsection",
        "subsubsection",
        "paragraph",
        "subparagraph",
        "customheading",
    ] {
        let definitions =
            format!("\\def\\{name}{{\\@startsection{{{name}}}{{1}}{{0pt}}{{0pt}}{{0pt}}{{}}}}");
        let item = named_candidate(&definitions, &format!("\\{name}¦"), name);
        assert_eq!(
            item.insert_text_format,
            Some(InsertTextFormat::SNIPPET),
            "{name}"
        );
        for suffix in ["{Title}", "[Short]{Title}", "*{Title}"] {
            let item = named_candidate(&definitions, &format!("\\{name}¦{suffix}"), name);
            let Some(CompletionTextEdit::Edit(edit)) = item.text_edit else {
                panic!("missing edit")
            };
            assert_eq!(edit.new_text, name, "{name}{suffix}");
        }
    }
}

#[test]
fn deferred_variants_preserve_explicit_required_parameter_overrides() {
    let definitions = r"\newcommand{\label}[2]{#1#2}
\def\installer{\def\label{zero arguments}}";
    let item = named_candidate(definitions, r"\label¦", "label");
    let Some(CompletionTextEdit::Edit(edit)) = item.text_edit else {
        panic!("missing edit")
    };
    assert_eq!(edit.new_text, "label{${1:arg1}}{${2:arg2}}");
}
