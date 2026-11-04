use lsp_types::*;
use texquill::{
    editing,
    project::workspace::{Document, Workspace},
    text::Text,
};

fn document(source: &str, extension: &str) -> Document {
    let uri = Url::parse(&format!("file:///linked-editing/main.{extension}")).unwrap();
    Document::new(&uri, source.into(), Some(7))
}

#[test]
fn paired_environment_names_return_identical_utf16_ranges_from_either_end() {
    for name in ["quote", "align*", "环境"] {
        let source = format!("😀 \\begin{{{name}}}\r\nBody\r\n\\end{{{name}}}\r\n");
        let doc = document(&source, "tex");
        let starts: Vec<_> = source
            .match_indices(&format!("{{{name}}}"))
            .map(|(offset, _)| offset + 1)
            .collect();
        let expected: Vec<_> = starts
            .iter()
            .map(|start| doc.text.range(*start, start + name.len()))
            .collect();
        for start in starts {
            for offset in [
                start,
                start + name.chars().next().unwrap().len_utf8(),
                start + name.len(),
            ] {
                let linked =
                    editing::linked_editing_ranges(&doc, doc.text.position(offset)).unwrap();
                assert_eq!(linked.ranges, expected, "{source}");
                assert!(linked.word_pattern.is_some());
            }
        }
    }
}

#[test]
fn booktabs_rules_preserve_nested_table_environment_pairs() {
    for rule in ["toprule", "midrule", "bottomrule"] {
        for width in ["", "[0.8pt]"] {
            let source = format!(
                "\\documentclass{{article}}\n\\usepackage{{booktabs}}\n\\begin{{document}}\n\\begin{{table}}[H]\n  \\centering\n  \\begin{{tabular}}{{ccc}}\n    \\toprule\n    a & b & c \\\\\n    \\midrule\n    1 & 2 & 3 \\\\\n    \\{rule}{width}\n  \\end{{tabular}}\n\\end{{table}}\n\\end{{document}}\n"
            );
            let doc = document(&source, "tex");
            for name in ["table", "tabular", "document"] {
                let starts: Vec<_> = source
                    .match_indices(&format!("{{{name}}}"))
                    .map(|(at, _)| at + 1)
                    .collect();
                let expected: Vec<_> = starts
                    .iter()
                    .map(|at| doc.text.range(*at, *at + name.len()))
                    .collect();
                for at in starts {
                    assert_eq!(
                        editing::linked_editing_ranges(&doc, doc.text.position(at))
                            .map(|linked| linked.ranges),
                        Some(expected.clone()),
                        "{rule}{width}: {name}"
                    );
                }
            }
        }
    }
}

#[test]
fn redefined_booktabs_rule_still_consumes_its_declared_operand() {
    for rule in ["toprule", "midrule", "bottomrule"] {
        let source = format!(
            "\\renewcommand{{\\{rule}}}[1]{{}}\\begin{{table}}\\begin{{tabular}}{{c}}x\\{rule}\\end{{tabular}}\\end{{table}}"
        );
        let doc = document(&source, "tex");
        for name in ["table", "tabular"] {
            for (at, _) in source.match_indices(&format!("{{{name}}}")) {
                assert!(editing::linked_editing_ranges(&doc, doc.text.position(at + 1)).is_none());
            }
        }
    }
}

#[test]
fn nested_and_repeated_names_link_only_their_own_balanced_pair() {
    let source = r"\begin{panel}A\begin{panel}B\end{panel}C\end{panel}\begin{panel}D\end{panel}";
    let doc = document(source, "tex");
    let names: Vec<_> = source
        .match_indices("{panel}")
        .map(|(offset, _)| offset + 1)
        .collect();
    for (begin, end) in [(0, 3), (1, 2), (4, 5)] {
        let expected = vec![
            doc.text.range(names[begin], names[begin] + 5),
            doc.text.range(names[end], names[end] + 5),
        ];
        for index in [begin, end] {
            assert_eq!(
                editing::linked_editing_ranges(&doc, doc.text.position(names[index] + 2))
                    .unwrap()
                    .ranges,
                expected
            );
        }
    }
}

#[test]
fn empty_environment_names_keep_zero_width_linked_insertion_points() {
    for (begin, end) in [
        (r"\begin", r"\end"),
        (r"\csuse{begin}", r"\csname end\endcsname"),
        (r"\start", r"\finish"),
    ] {
        let source = format!(
            "\\def\\start{{\\begin}}\\def\\finish{{\\end}}😀 {begin}{{}}\r\nBody\r\n{end}{{}}"
        );
        let doc = document(&source, "tex");
        let offsets: Vec<_> = source.match_indices("{}").map(|(at, _)| at + 1).collect();
        let expected: Vec<_> = offsets.iter().map(|at| doc.text.range(*at, *at)).collect();
        for at in offsets {
            let linked = editing::linked_editing_ranges(&doc, doc.text.position(at)).unwrap();
            assert_eq!(linked.ranges, expected, "{source}");
            assert_eq!(linked.word_pattern.as_deref(), Some(r"[^\s{}\\%]*"));
        }
    }
}

#[test]
fn nested_and_repeated_empty_names_keep_separate_pairs() {
    let source = r"\begin{}A\begin{}B\end{}C\end{}\begin{}D\end{}";
    let doc = document(source, "tex");
    let names: Vec<_> = source.match_indices("{}").map(|(at, _)| at + 1).collect();
    for (begin, end) in [(0, 3), (1, 2), (4, 5)] {
        let expected = vec![
            doc.text.range(names[begin], names[begin]),
            doc.text.range(names[end], names[end]),
        ];
        for at in [names[begin], names[end]] {
            assert_eq!(
                editing::linked_editing_ranges(&doc, doc.text.position(at))
                    .unwrap()
                    .ranges,
                expected
            );
        }
    }
}

#[test]
fn empty_names_do_not_link_mismatched_dynamic_or_data_boundaries() {
    for marked in [
        r"\begin{¦}Body\end{quote}",
        r"\begin{quote}Body\end{¦}",
        r"\begin{¦}Body\end{\chosen}",
        r"\begin{¦}Body\end{ }",
        r"\begin{¦}Body\end{% comment
}",
        r"\begin{¦}Body\end",
        r"\begin{¦}\begin{inner}Body\end{}\end{inner}",
        r"\newcommand{\ignore}[1]{}\ignore{\begin{¦}Fake\end{}}",
        r"\def\saved{\begin{¦}Fake\end{}}",
        r"\renewcommand{\begin}[1]{#1}\begin{¦}Fake\end{}",
        r"% \begin{¦}Fake\end{}",
        r"\verb|\begin{¦}Fake\end{}|",
        "\\begin{verbatim}\n\\begin{¦}Fake\\end{}\n\\end{verbatim}",
        r"\begin{¦}\input{missing}\end{}",
    ] {
        let at = marked.find('¦').unwrap();
        let source = marked.replace('¦', "");
        let doc = document(&source, "tex");
        assert!(
            editing::linked_editing_ranges(&doc, doc.text.position(at)).is_none(),
            "{marked}"
        );
    }
}

#[test]
fn empty_project_pairs_still_require_agreeing_inputs_and_consumers() {
    for (child, expected) in [
        ("Body", true),
        (r"\end{}\begin{}", false),
        (r"\begin{inner}", false),
        (r"\iftrue Body\fi", false),
    ] {
        let source =
            r"\documentclass{article}\begin{document}\begin{}\input{child}\end{}\end{document}";
        let (temp, workspace) = project(&[("main.tex", source), ("child.tex", child)]);
        let uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
        let doc = &workspace.documents[&uri];
        for (at, _) in source.match_indices("{}") {
            assert_eq!(
                editing::linked_editing_ranges_in_workspace(
                    &workspace,
                    &uri,
                    doc.text.position(at + 1)
                )
                .is_some(),
                expected,
                "{child}"
            );
        }
    }
    for agree in [false, true] {
        let first = r"\documentclass{article}\def\start{\begin}\def\finish{\end}\input{shared}\input{shared}";
        let second = if agree {
            first
        } else {
            r"\documentclass{article}\newcommand{\start}[1]{#1}\newcommand{\finish}[1]{#1}\input{shared}"
        };
        let source = r"\start{}Body\finish{}";
        let (temp, workspace) =
            project(&[("a.tex", first), ("b.tex", second), ("shared.tex", source)]);
        let uri = Url::from_file_path(temp.path().join("shared.tex")).unwrap();
        let doc = &workspace.documents[&uri];
        for (at, _) in source.match_indices("{}") {
            assert_eq!(
                editing::linked_editing_ranges_in_workspace(
                    &workspace,
                    &uri,
                    doc.text.position(at + 1)
                )
                .is_some(),
                agree
            );
        }
    }
}

#[test]
fn raw_environment_delimiters_link_without_pairing_fake_commands_in_the_body() {
    for name in [
        "verbatim",
        "verbatim*",
        "lstlisting",
        "minted",
        "comment",
        "Verbatim",
    ] {
        let gap = if matches!(name, "Verbatim" | "minted") {
            "\r\n"
        } else {
            ""
        };
        let header = if name == "minted" { "{latex}" } else { "" };
        let source = format!("\\begin{{quote}}\\begin{{{name}}}{header}\r\n\\begin{{quote}}Fake\\end{{quote}}\r\n\\end{{{name}}}{gap}\\end{{quote}}");
        let doc = document(&source, "tex");
        let raw_begin = source.find(&format!("{{{name}}}")).unwrap() + 1;
        let raw_end = source.rfind(&format!("{{{name}}}")).unwrap() + 1;
        let expected = vec![
            doc.text.range(raw_begin, raw_begin + name.len()),
            doc.text.range(raw_end, raw_end + name.len()),
        ];
        for offset in [raw_begin, raw_end] {
            assert_eq!(
                editing::linked_editing_ranges(&doc, doc.text.position(offset))
                    .unwrap()
                    .ranges,
                expected,
                "{name}"
            );
        }
        let first_quote = source.find("{quote}").unwrap() + 1;
        let last_quote = source.rfind("{quote}").unwrap() + 1;
        assert_eq!(
            editing::linked_editing_ranges(&doc, doc.text.position(first_quote))
                .unwrap()
                .ranges,
            vec![
                doc.text.range(first_quote, first_quote + 5),
                doc.text.range(last_quote, last_quote + 5)
            ]
        );
        let fake_quote = source[first_quote + 5..].find("{quote}").unwrap() + first_quote + 5 + 1;
        assert!(
            editing::linked_editing_ranges(&doc, doc.text.position(fake_quote)).is_none(),
            "{name}"
        );
    }
}

#[test]
fn unbalanced_dynamic_declaration_comment_inline_raw_and_body_positions_have_no_link() {
    for marked in [
        r"\begin{pa¦nel}Unclosed",
        r"\begin*{pa¦nel}Body\end{panel}",
        r"\begin[Note]{pa¦nel}Body\end{panel}",
        r"\begin{panel}Body\end*{pa¦nel}",
        r"\begin{panel}Body\end[Note]{pa¦nel}",
        r"\begin{pa¦nel}Body\end*{panel}",
        r"\begin{outer}\begin{inner}x\end{out¦er}\end{inner}",
        r"\begin{out¦er}\begin{\chosen}\end{panel}\end{outer}",
        r"\begin{\chosen}Text\end{pa¦nel}",
        r"\begin{pa¦nel}x\end{other}",
        r"\begin{ panel }x\end{pa¦nel}",
        r"\begin{panel}x\end{ panel¦ }",
        r"\newenvironment{pa¦nel}{Body}{End}",
        r"\begin{panel}Bo¦dy\end{panel}",
        r"% \begin{pa¦nel}Text\end{panel}",
        r"\verb|\begin{pa¦nel}Text\end{panel}|",
        "\\begin{verbatim}\n\\begin{pa¦nel}Text\\end{panel}\\end{verbatim}",
        "\\begin{verbatim}\n\\begin{pa¦nel}Text\\end{panel}",
        r"\begin{verb¦atim}Unclosed",
    ] {
        let offset = marked.find('¦').unwrap();
        let source = marked.replace('¦', "");
        let doc = document(&source, "tex");
        assert!(
            editing::linked_editing_ranges(&doc, doc.text.position(offset)).is_none(),
            "{marked}"
        );
    }
}

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
    let temp = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(temp.path().join(name), text).unwrap();
    }
    let mut workspace = Workspace::default();
    workspace.add_root(temp.path().into());
    (temp, workspace)
}

fn assert_project_pair(workspace: &Workspace, uri: &Url, name: &str, expected: bool) {
    let doc = &workspace.documents[uri];
    let offsets: Vec<_> = doc
        .text
        .source
        .match_indices(name)
        .map(|(at, _)| at)
        .collect();
    assert_eq!(offsets.len(), 2, "{}", doc.text.source);
    let ranges: Vec<_> = offsets
        .iter()
        .map(|at| doc.text.range(*at, *at + name.len()))
        .collect();
    for at in offsets {
        let found =
            editing::linked_editing_ranges_in_workspace(workspace, uri, doc.text.position(at));
        if expected {
            assert_eq!(found.unwrap().ranges, ranges, "{}", doc.text.source);
        } else {
            assert!(found.is_none(), "{}", doc.text.source);
        }
    }
}

#[test]
fn table_pairs_survive_unrelated_style_conditionals_and_balanced_macro_bodies() {
    let source = r"\documentclass{article}\input{style}\begin{document}
\begin{table}[H]\centering\begin{tabular}{cc}test\end{tabular}\caption{}\label{tab:}\end{table}
\end{document}";
    for style in [
        r"\ifCLASSOPTIONcompsoc\usepackage[font=normalsize]{subfig}\else\usepackage[font=footnotesize]{subfig}\fi",
        r"\newcommand{\tabincell}[2]{\begin{tabular}{@{}#1@{}}#2\end{tabular}}",
        r"\ifCLASSOPTIONcompsoc\usepackage{subfig}\else\usepackage{subfig}\fi
\newcommand{\tabincell}[2]{\begin{tabular}{@{}#1@{}}#2\end{tabular}}",
        // IEEEtran locally captures labels while rendering a caption. The
        // doubled parameter marker belongs to that deferred definition.
        r"\long\def\@makecaption#1#2{\def\label##1{\gdef\saved{##1}}#2}",
    ] {
        let (temp, workspace) = project(&[("main.tex", source), ("style.tex", style)]);
        let uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
        let doc = &workspace.documents[&uri];
        for name in ["table", "tabular", "document"] {
            let starts: Vec<_> = source
                .match_indices(&format!("{{{name}}}"))
                .map(|(at, _)| at + 1)
                .collect();
            let expected: Vec<_> = starts
                .iter()
                .map(|at| doc.text.range(*at, at + name.len()))
                .collect();
            for at in starts {
                assert_eq!(
                    editing::linked_editing_ranges_in_workspace(
                        &workspace,
                        &uri,
                        doc.text.position(at)
                    )
                    .map(|linked| linked.ranges),
                    Some(expected.clone()),
                    "{name}: {style}"
                );
            }
        }
    }
}

#[test]
fn conditional_edges_only_block_pairs_that_cross_them() {
    for source in [
        r"\iftrue Text\else Other\fi\begin{quote}Body\end{quote}",
        r"\begin{quote}Body\end{quote}\iftrue Text\else Other\fi",
        r"\iftrue\begin{quote}Body\end{quote}\else Other\fi",
    ] {
        let doc = document(source, "tex");
        for (at, _) in source.match_indices("quote") {
            assert!(
                editing::linked_editing_ranges(&doc, doc.text.position(at)).is_some(),
                "{source}"
            );
        }
    }
}

#[test]
fn unbalanced_or_conditional_macro_effects_still_prevent_linked_pairs() {
    for body in [
        r"\begin{quote}",
        r"\end{quote}",
        r"\begin{quote}\begin{quote}\end{quote}",
        r"\begin{quote}\iftrue\end{quote}\fi",
        r"\begin{quote}\input{other}\end{quote}",
        r"\begin{quote}\usepackage{other}\end{quote}",
        r"\def\open{\begin{quote}}\end{quote}",
        r"\newcommand{\ignore}[1]{}\ignore{\begin{quote}}\end{quote}",
    ] {
        let source = format!(r"\def\helper{{{body}}}\begin{{quote}}Body\end{{quote}}");
        let doc = document(&source, "tex");
        let at = source.rfind("quote").unwrap();
        assert!(
            editing::linked_editing_ranges(&doc, doc.text.position(at)).is_none(),
            "{body}"
        );
    }
}

#[test]
fn static_named_calls_and_aliases_link_comment_bounded_environment_names() {
    for (begin, end) in [
        (r"\begin", r"\end"),
        (r"\csuse{begin}", r"\end"),
        (r"\begin", r"\csname end\endcsname"),
        (r"\start", r"\finish"),
        (r"\csuse{start}", r"\csuse{finish}"),
        (r"\csname start\endcsname", r"\csname finish\endcsname"),
    ] {
        for external in [false, true] {
            let aliases = r"\def\start{\begin}\def\finish{\end}";
            let preamble = if external {
                r"\usepackage{aliases}"
            } else {
                aliases
            };
            let source = format!("\\documentclass{{article}}{preamble}\\begin{{document}}😀 {begin}{{% before\r\npanel% after\r\n}}Body{end}{{panel% tail\r\n}}\\end{{document}}");
            let (temp, workspace) = project(&[
                ("main.tex", &source),
                ("aliases.sty", if external { aliases } else { "" }),
                (
                    "other.tex",
                    r"\documentclass{article}\renewcommand{\begin}[1]{#1}",
                ),
            ]);
            let uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
            assert_project_pair(&workspace, &uri, "panel", true);
        }
    }
}

#[test]
fn bare_names_and_dynamic_indirect_boundaries_never_borrow_an_outer_pair() {
    for marked in [
        r"\begin ¦x Body\end x",
        r"\begin{¦x}Body\end x",
        r"\begin x Body\end{¦x}",
        r"\csuse{begin}¦x Body\csuse{end}x",
        r"\def\start{\begin}\def\finish{\end}\start ¦x Body\finish x",
        r"\begin{out¦er}\csuse{begin}{\chosen}\end{panel}\end{outer}",
        r"\begin{out¦er}\csname end\endcsname{\chosen}\end{outer}",
        r"\def\start{\begin}\begin{out¦er}\start{\chosen}\end{panel}\end{outer}",
        r"\newcommand{\take}[1]{}\take{\csuse{begin}{pa¦nel}\csuse{end}{panel}}",
        r"\def\saved{\csname begin\endcsname{pa¦nel}\csname end\endcsname{panel}}",
    ] {
        let offset = marked.find('¦').unwrap();
        let source = marked.replace('¦', "");
        let doc = document(&source, "tex");
        assert!(
            editing::linked_editing_ranges(&doc, doc.text.position(offset)).is_none(),
            "{marked}"
        );
    }
}

#[test]
fn input_order_determines_pairs_without_returning_cross_document_ranges() {
    for (child, expected) in [
        (r"Body", true),
        (r"\begin{inner}Body\end{inner}", true),
        (r"\end{quote}\begin{quote}", false),
        (r"\end{quote}", false),
        (r"\begin{quote}", false),
        (r"\begin{\chosen}", false),
        (r"\iftrue Body\fi", false),
    ] {
        let main = r"\documentclass{article}\begin{document}\begin{quote}\input{child}\end{quote}\end{document}";
        let (temp, workspace) = project(&[("main.tex", main), ("child.tex", child)]);
        let uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
        assert_project_pair(&workspace, &uri, "quote", expected);
    }
}

#[test]
fn every_execution_and_consumer_must_agree_on_the_same_pair() {
    let aliases = r"\def\start{\begin}\def\finish{\end}";
    let a = r"\documentclass{article}\usepackage{aliases}\begin{document}\input{shared}\input{shared}\end{document}";
    for agree in [true, false] {
        let b = if agree {
            a
        } else {
            r"\documentclass{article}\newcommand{\start}[1]{#1}\newcommand{\finish}[1]{#1}\begin{document}\input{shared}\end{document}"
        };
        let (temp, workspace) = project(&[
            ("a.tex", a),
            ("b.tex", b),
            ("aliases.sty", aliases),
            ("shared.tex", r"\start{quote}Body\finish{quote}"),
        ]);
        let uri = Url::from_file_path(temp.path().join("shared.tex")).unwrap();
        assert_project_pair(&workspace, &uri, "quote", agree);
    }
    // The first execution closes an outer opener, and the second opens a new one.
    let (temp, workspace) = project(&[
        (
            "main.tex",
            r"\documentclass{article}\begin{document}\begin{quote}\input{shared}\input{shared}\end{quote}\end{document}",
        ),
        ("shared.tex", r"\end{quote}\begin{quote}"),
    ]);
    let uri = Url::from_file_path(temp.path().join("shared.tex")).unwrap();
    assert_project_pair(&workspace, &uri, "quote", false);
}

#[test]
fn uncertain_inputs_and_branches_do_not_produce_lexical_fallback_pairs() {
    for input in [
        r"\input{missing}",
        r"\input{\chosen}",
        r"\input{main}",
        r"\IfFileExists{missing}{Yes}{No}",
        r"\includeonly{child}",
        r"\iftrue Text\else Other\fi",
        r"\endinput",
        r"\csuse{IfFileExists}{missing}{Yes}{No}",
        r"\csname includeonly\endcsname{child}",
    ] {
        let source = format!(
            r"\documentclass{{article}}\begin{{document}}\begin{{quote}}{input}\end{{quote}}\end{{document}}"
        );
        let (temp, workspace) = project(&[("main.tex", &source)]);
        let uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
        assert_project_pair(&workspace, &uri, "quote", false);
    }
}

#[test]
fn indirect_package_loads_participate_in_environment_execution_order() {
    for loader in [
        r"\csuse{RequirePackage}",
        r"\csname RequirePackage\endcsname",
        r"\load",
    ] {
        for (package, expected) in [("Body", true), (r"\end{quote}\begin{quote}", false)] {
            let source = format!(
                r"\documentclass{{article}}\def\load{{\RequirePackage}}\begin{{quote}}{loader}{{flip}}\end{{quote}}\begin{{document}}Text\end{{document}}"
            );
            let (temp, workspace) = project(&[("main.tex", &source), ("flip.sty", package)]);
            let uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
            assert_project_pair(&workspace, &uri, "quote", expected);
        }
    }
}

#[test]
fn buffer_changes_recompute_aliases_and_discard_obsolete_pairs() {
    let source = r"\documentclass{article}\usepackage{aliases}\begin{document}\start{quote}Body\finish{quote}\end{document}";
    let aliases = r"\def\start{\begin}\def\finish{\end}";
    let (temp, mut workspace) = project(&[("main.tex", source), ("aliases.sty", aliases)]);
    let main = Url::from_file_path(temp.path().join("main.tex")).unwrap();
    let defs = Url::from_file_path(temp.path().join("aliases.sty")).unwrap();
    assert_project_pair(&workspace, &main, "quote", true);
    workspace.open(
        defs.clone(),
        r"\newcommand{\start}[1]{#1}\newcommand{\finish}[1]{#1}".into(),
        8,
    );
    assert_project_pair(&workspace, &main, "quote", false);
    workspace.open(defs, aliases.into(), 9);
    assert_project_pair(&workspace, &main, "quote", true);
}

#[test]
fn incomplete_indirect_raw_bodies_and_hidden_macro_effects_are_not_linked() {
    for marked in [
        r"\def\start{\begin}\start{verb¦atim}\begin{quote}Fake\end{quote}\end{verbatim}",
        r"\def\start{\begin}\start{verbatim}\begin{quo¦te}Fake\end{quote}\end{verbatim}",
        r"\newenvironment{wrapper}{\begin{quote}}{\end{quote}}\begin{quo¦te}\begin{wrapper}Text\end{quote}\end{wrapper}",
        r"\begin{quo¦te}\begin{verbatim}Unclosed",
    ] {
        let offset = marked.find('¦').unwrap();
        let source = marked.replace('¦', "");
        let doc = document(&source, "tex");
        assert!(
            editing::linked_editing_ranges(&doc, doc.text.position(offset)).is_none(),
            "{marked}"
        );
    }
}

#[test]
fn package_once_and_endinput_follow_the_same_order_as_source_inputs() {
    let main = "\\documentclass{article}\\begin{quote}\\usepackage{opening}\\usepackage{opening}\n\\end{box}\n\\input{child}\\end{quote}\\begin{document}Text\\end{document}";
    let (temp, workspace) = project(&[
        ("main.tex", main),
        ("opening.sty", "\\begin{box}\n"),
        (
            "child.tex",
            "Body\\endinput % no more tokens\n\\end{quote}\\begin{quote}",
        ),
    ]);
    let main_uri = Url::from_file_path(temp.path().join("main.tex")).unwrap();
    assert_project_pair(&workspace, &main_uri, "quote", true);
    let child = Url::from_file_path(temp.path().join("child.tex")).unwrap();
    assert_project_pair(&workspace, &child, "quote", false);
}

#[test]
fn linked_editing_refuses_non_tex_documents_and_invalid_utf16_positions() {
    let source = "😀 \\begin{quote}Body\\end{quote}";
    let doc = document(source, "tex");
    for at in [
        Position::new(0, 1),
        Position::new(100, 0),
        Position::new(0, u32::MAX),
    ] {
        assert!(editing::linked_editing_ranges(&doc, at).is_none());
    }
    for extension in ["bib", "bst"] {
        let doc = document(source, extension);
        assert!(editing::linked_editing_ranges(
            &doc,
            Text::new(source.into()).position(source.find("quote").unwrap())
        )
        .is_none());
    }
}

#[test]
fn environment_names_inside_macro_data_and_definitions_are_not_linked() {
    for source in [
        r"\newcommand{\ignore}[1]{}\ignore{\begin{quote}Fake\end{quote}}",
        r"\def\saved{\begin{quote}Fake\end{quote}}",
        r"\renewcommand{\begin}[1]{#1}\begin{quote}Fake\end{quote}",
    ] {
        let doc = document(source, "tex");
        for (offset, _) in source.match_indices("quote") {
            assert!(
                editing::linked_editing_ranges(&doc, doc.text.position(offset)).is_none(),
                "{source}"
            );
        }
    }
}

#[test]
fn document_only_pairs_never_cross_inputs_but_preserve_independent_pairs() {
    for input in [
        r"\input{part}",
        r"\csuse{input}{part}",
        r"\usepackage{part}",
    ] {
        for (source, expected) in [
            (format!(r"{input}\begin{{quote}}Text\end{{quote}}"), true),
            (format!(r"\begin{{quote}}Text\end{{quote}}{input}"), true),
            (format!(r"\begin{{quote}}{input}\end{{quote}}"), false),
        ] {
            let doc = document(&source, "tex");
            for (offset, _) in source.match_indices("quote") {
                assert_eq!(
                    editing::linked_editing_ranges(&doc, doc.text.position(offset)).is_some(),
                    expected,
                    "{source}"
                );
            }
        }
    }
}
