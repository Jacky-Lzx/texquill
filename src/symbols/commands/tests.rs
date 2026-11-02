use super::*;

#[cfg(test)]
mod tex_operand_tests {
    use super::*;
    use crate::text::Text;

    #[test]
    fn common_grammars_cannot_certify_unverified_external_declarations() {
        let source = r"\newcommand\one[1]{#1}\renewcommand\one[1]{#1}\def\bridge{\one}\bridge{Text}\ref{same}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let at = source.rfind(r"\ref").unwrap();
        let ordinary = CommandSignatures::new([(source, &parsed)]);
        assert!(!ordinary.argument_tokens(source, &parsed).contains(&at));
        for name in ["one", "bridge"] {
            let paused = CommandSignatures::with_ambiguous_names([(source, &parsed)], [name]);
            assert!(
                paused.argument_tokens(source, &parsed).contains(&at),
                "{name}"
            );
        }
    }

    #[test]
    fn completion_can_prove_zero_parameters_without_proving_a_unique_meaning() {
        let source = r"\def\maketitle{Title text}\let\maketitle\relax";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let signatures = CommandSignatures::new([(source, &parsed)]);
        assert!(signatures.local["maketitle"].is_none());
        assert!(signatures.completion_shapes()["maketitle"]
            .as_ref()
            .unwrap()
            .takes_no_arguments());
        let unverified =
            CommandSignatures::with_ambiguous_names([(source, &parsed)], ["maketitle"]);
        assert!(unverified.completion_shapes()["maketitle"].is_none());

        for source in [
            r"\def\maketitle#1{}\let\maketitle\relax",
            r"\def\relax#1{}\def\maketitle{}\let\maketitle\relax",
            r"\def\original{}\def\maketitle{}\let\maketitle\original\def\original#1{}",
            r"\def\maketitle{}\let\maketitle\unknown",
            r"\def\maketitle{}\let\maketitle\noexpand",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            assert!(
                signatures.completion_shapes()["maketitle"].is_none(),
                "{source}"
            );
        }
    }

    #[test]
    fn copied_literal_candidates_capture_builtin_and_previous_source_shapes() {
        for (source, expected) in [
            (
                r"\def\original#1#2{}\let\bad\original\def\original{}\bad x&",
                true,
            ),
            (
                r"\newenvironment{bad}[2]{}{}\let\start\begin\def\begin{}\let\begin\start\start{bad}x&",
                true,
            ),
            (
                r"\def\consume#1#2{}\def\bad{}\def\let{}\let\bad\consume x&",
                true,
            ),
            (r"\let\first\second\let\second\first\first x&", false),
            (r"\def\original{}\let\bad\original\bad{x}{y}&", false),
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            assert_eq!(
                signatures.literal_is_argument(source, &parsed, source.rfind('&').unwrap()),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn copied_sources_are_operands_and_do_not_execute_file_loads() {
        for copy in [
            r"\let\bad\input",
            r"\let\bad=\input",
            r"\NewCommandCopy{\bad}{\input}",
            r"\cslet{bad}\input",
            r"\letcs\bad{input}",
            r"\csletcs{bad}{input}",
        ] {
            let source = format!(r"{copy}{{wrong}}\input{{right}}");
            let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
            let signatures = CommandSignatures::source_context([(source.as_str(), &parsed)]);
            let active = signatures.source_command_positions(&source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
            {
                assert_eq!(
                    active.contains(&command.span.start),
                    command
                        .argument
                        .as_ref()
                        .is_some_and(|argument| argument.value == "right"),
                    "{source}: {command:?}"
                );
            }
        }
    }

    #[test]
    fn copy_sources_require_one_static_operand_and_keep_primitive_braces_literal() {
        for source in [
            r"\let\bad{\source}",
            r"\let\bad}\source",
            r"\NewCommandCopy{\bad}{\source\other}",
            r"\csletcs{bad}{\dynamic}",
            r"\letcs\bad{source\dynamic}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let entry = parsed
                .definitions
                .iter()
                .find(|entry| entry.name == "bad")
                .unwrap();
            assert!(
                literal_copy_target(source, &parsed, entry, false).is_none(),
                "{source}"
            );
        }
    }

    #[test]
    fn paused_literal_candidates_preserve_complete_arguments_without_certifying_meanings() {
        for (declaration, call, consumed) in [
            (
                r"\newenvironment{bad}[2][default]{}{}",
                r"\begin{bad}{x}&",
                false,
            ),
            (
                r"\newenvironment{bad}[2][default]{}{}",
                r"\begin{bad}&",
                true,
            ),
            (r"\def\bad#1#2{}", r"\bad{x}{y}&", false),
            (r"\def\bad#1#2{}", r"\bad{x}&", true),
            (r"\def\bad#2{}", r"\bad{x}&", true),
            (r"\def\bad{\undefined}", r"\bad{x}&", true),
            (r"\def\bad{\halign}", r"\bad{#&#\cr A&B&C\cr}", false),
        ] {
            let definitions = syntax::Syntax::latex(&Text::new(declaration.into()));
            let parsed = syntax::Syntax::latex(&Text::new(call.into()));
            let signatures = CommandSignatures::with_ambiguous_names([(call, &parsed)], ["bad"])
                .with_literal_sources([(declaration, &definitions)]);
            assert!(signatures.local["bad"].is_none());
            assert_eq!(
                signatures.literal_is_argument(call, &parsed, call.rfind('&').unwrap()),
                consumed,
                "{declaration}/{call}"
            );
        }
    }

    #[test]
    fn possible_literal_arguments_follow_long_aliases_and_terminate_cycles() {
        let name = |index: usize| format!("alias{}", "a".repeat(index + 1));
        let mut source = String::from(r"\def\bad{}\def\bad#1#2{&}");
        for index in 0..512 {
            source.push_str(&format!("\\def\\{}{{\\{}}}", name(index), name(index + 1)));
        }
        source.push_str(&format!("\\def\\{}{{\\bad}}\\{} x&", name(512), name(0)));
        let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
        let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
        assert!(signatures.local["bad"].is_none());
        assert!(signatures.literal_is_argument(&source, &parsed, source.len() - 1));
        for (definitions, expected) in [
            (r"\def\first{\second}\def\second{\first}", false),
            (
                r"\def\first{\second}\def\second{\first}\def\second{\bad}\def\bad#1#2{&}",
                true,
            ),
        ] {
            let source = format!("{definitions}\\first x&");
            let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
            let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
            assert_eq!(
                signatures.literal_is_argument(&source, &parsed, source.len() - 1),
                expected
            );
        }
    }

    #[test]
    fn possible_literal_arguments_preserve_completed_calls_and_deferred_declarations() {
        for source in [
            r"\def\bad{}\def\bad{}\bad x&",
            r"\def\bad#1#2{}\def\bad#1#2{}\bad xy&",
            r"\def\bad#1;{}\def\bad#1;{}\bad x;&",
            r"\def\bad#1&#2{}\def\provider{\bad x}&",
            r"\def\bad#1&#2{}\def\provider#1\bad#2{}x&",
            r"\def\bad#1#2{}\def\bad#1#2{}\def\show{\textbf}\show{x}&",
            r"\newenvironment{bad}[2]{}{}\renewenvironment{bad}[2]{}{}\begin{bad}xy&\end{bad}",
        ] {
            let at = source.rfind('&').unwrap();
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            assert!(
                !signatures.possible_literal_argument(source, &parsed, at),
                "{source}"
            );
            assert!(
                !signatures.literal_is_argument(source, &parsed, at),
                "{source}"
            );
        }
    }

    #[test]
    fn graphicspath_consumes_only_its_data_argument_and_preserves_following_calls() {
        for call in [
            r"\graphicspath",
            r"\csuse{graphicspath}",
            r"\csname graphicspath\endcsname",
            r"\setpaths",
            r"\csuse{setpaths}",
            r"\csname setpaths\endcsname",
        ] {
            for argument in [r"{\input{opaque}}", r"\input{opaque}"] {
                let source = format!(
                    r"\def\setpaths{{\graphicspath}}{call}{argument}\includegraphics{{picture}}\input{{right}}"
                );
                let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
                let signatures = CommandSignatures::source_context([(source.as_str(), &parsed)]);
                let operands = signatures.argument_tokens(&source, &parsed);
                for command in parsed
                    .commands
                    .iter()
                    .filter(|command| matches!(command.name.as_str(), "input" | "includegraphics"))
                {
                    assert_eq!(
                        operands.contains(&command.span.start),
                        command
                            .argument
                            .as_ref()
                            .is_some_and(|argument| argument.value == "opaque"),
                        "{source}: {command:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn bare_environment_names_dispatch_the_actual_environment_parameters() {
        for call in [
            r"\begin x",
            r"\csuse{begin}x",
            r"\csname begin\endcsname x",
            r"\start x",
            r"\csuse{start}x",
            r"\csname start\endcsname x",
            "\\begin% name\r\nx",
        ] {
            for (definition, prefix, tail) in [
                (r"\newenvironment{x}[3]{}{}", r"\relax", ""),
                (r"\newenvironment{x}[4][default]{}{}", r"[note]\relax", ""),
                (
                    r"\NewDocumentEnvironment{x}{O{default} m m m}{}{}",
                    r"[note]\relax",
                    "",
                ),
                (r"\newenvironment{x}[1]{}{}", "{", "}"),
                (r"\NewDocumentEnvironment{x}{d()}{}{}", "(", ")"),
                (r"\NewDocumentEnvironment{x}{t\mark m}{}{}", r"\mark", ""),
            ] {
                let source = format!(
                    r"{definition}\def\start{{\begin}}{call}{prefix}\input{{wrong}}{tail}\input{{right}}"
                );
                let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
                let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
                let operands = signatures.argument_tokens(&source, &parsed);
                for command in parsed
                    .commands
                    .iter()
                    .filter(|command| command.name == "input")
                {
                    assert_eq!(
                        operands.contains(&command.span.start),
                        command.argument.as_ref().unwrap().value == "wrong",
                        "{source}"
                    );
                }
            }
        }
    }

    #[test]
    fn bare_environment_names_require_an_engine_independent_literal_token() {
        for value in ["x", "7", "-", "[", "]"] {
            let source = format!("% name\r\n{value}tail");
            let argument = syntax::environment_name_argument(&source, 0).unwrap();
            assert_eq!(argument.value, value);
            assert_eq!(argument.span, argument.outer_span);
            assert_eq!(&source[argument.span], value);
        }
        for value in [r"\which", "é", "😀", "^^78", "{x", "}", "#1", "", "% end"] {
            assert!(
                syntax::environment_name_argument(value, 0).is_none(),
                "{value}"
            );
        }
        let source = "{% name\r\nx% keep\r\n}";
        let argument = syntax::environment_name_argument(source, 0).unwrap();
        assert_eq!(argument.value, "x");
        assert_eq!(&source[argument.span.clone()], "x");
        assert_ne!(argument.span, argument.outer_span);
    }

    #[test]
    fn environment_arguments_are_operands_through_begin_direct_and_named_calls() {
        for call in [
            r"\begin{wrapped}",
            r"\csuse{begin}{wrapped}",
            r"\csname begin\endcsname{wrapped}",
            r"\start{wrapped}",
            r"\csuse{start}{wrapped}",
            r"\csname start\endcsname{wrapped}",
            "\\csuse{begin}{% name\r\nwrapped% keep\r\n}",
            r"\wrapped",
            r"\csuse{wrapped}",
            r"\csname wrapped\endcsname",
        ] {
            for (definition, prefix, tail) in [
                (r"\newenvironment{wrapped}[3]{}{}", r"\relax", ""),
                (
                    r"\newenvironment{wrapped}[4][default]{}{}",
                    r"[note]\relax",
                    "",
                ),
                (
                    r"\NewDocumentEnvironment{wrapped}{O{default} m m m}{}{}",
                    r"[note]\relax",
                    "",
                ),
                (r"\newenvironment{wrapped}[1]{}{}", "{", "}"),
                (r"\NewDocumentEnvironment{wrapped}{d()}{}{}", "(", ")"),
                (
                    r"\NewDocumentEnvironment{wrapped}{t\mark m}{}{}",
                    r"\mark",
                    "",
                ),
            ] {
                let source = format!(
                    r"{definition}\def\start{{\begin}}{call}{prefix}\input{{wrong}}{tail}\input{{right}}"
                );
                let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
                let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
                let operands = signatures.argument_tokens(&source, &parsed);
                for command in parsed
                    .commands
                    .iter()
                    .filter(|command| command.name == "input")
                {
                    assert_eq!(
                        operands.contains(&command.span.start),
                        command.argument.as_ref().unwrap().value == "wrong",
                        "{source}"
                    );
                }
            }
        }
    }

    #[test]
    fn indirect_begins_preserve_completed_arguments_and_builtin_environment_bodies() {
        for begin in [
            r"\begin",
            r"\csuse{begin}",
            r"\csname begin\endcsname",
            r"\start",
            r"\csuse{start}",
            r"\csname start\endcsname",
        ] {
            for (declaration, call) in [
                (r"\newenvironment{wrapped}{}{}", "{wrapped}"),
                (r"\newenvironment{wrapped}[1]{}{}", "{wrapped}{Unused}"),
                (r"\newenvironment{wrapped}[1][default]{}{}", "{wrapped}"),
                (
                    r"\NewDocumentEnvironment{wrapped}{d()}{}{}",
                    "{wrapped}(note)",
                ),
                (r"\NewDocumentEnvironment{wrapped}{!s}{}{}", "{wrapped} "),
                ("", "{document}"),
                ("", "{minipage}{3cm}"),
                ("", "{figure}[ht]"),
                // A replacement begin consumes only its own arguments.
                (
                    r"\newenvironment{wrapped}[3]{}{}\renewcommand{\begin}[1]{}",
                    "{wrapped}",
                ),
            ] {
                let source =
                    format!(r"{declaration}\def\start{{\begin}}{begin}{call}\input{{right}}");
                let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
                let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
                let operands = signatures.argument_tokens(&source, &parsed);
                for command in parsed
                    .commands
                    .iter()
                    .filter(|command| command.name == "input")
                {
                    assert!(!operands.contains(&command.span.start), "{source}");
                }
            }
        }
        let source = r"\newenvironment{wrapped}[3]{}{}\def\start{\begin}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        for ambiguous in ["begin", "start"] {
            let signatures =
                CommandSignatures::with_ambiguous_names([(source, &parsed)], [ambiguous]);
            assert!(signatures.local["start"].is_none(), "{ambiguous}");
        }
    }

    #[test]
    fn completed_environment_arguments_preserve_the_body_and_following_commands() {
        for source in [
            r"\newenvironment{wrapped}{}{}\begin{wrapped}\input{right}\end{wrapped}",
            r"\newenvironment{wrapped}[0]{}{}\begin{wrapped}\input{right}\end{wrapped}",
            r"\newenvironment{wrapped}[1]{}{}\begin{wrapped}{Unused}\input{right}\end{wrapped}",
            r"\newenvironment{wrapped}[1][default]{}{}\begin{wrapped}\input{right}\end{wrapped}",
            r"\NewDocumentEnvironment{wrapped}{d()}{}{}\begin{wrapped}(Unused)\input{right}\end{wrapped}",
            r"\newenvironment{wrapped}{}{}\def\bridge{\wrapped}\bridge\input{right}",
            r"\begin{document}{\input{right}}\end{document}",
            r"\begin{minipage}{3cm}\input{right}\end{minipage}",
            r"\begin{figure}[ht]\input{right}\end{figure}",
            r"\newcommand{\begin}[1]{}\begin{wrapped}\input{right}",
            r"\newenvironment{begin}{}{}\input{right}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            let operands = signatures.argument_tokens(source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
            {
                assert!(!operands.contains(&command.span.start), "{source}");
            }
        }
    }

    #[test]
    fn generated_environment_end_controls_and_aliases_keep_real_following_commands_visible() {
        for source in [
            r"\newenvironment{wrapped}{}{}\wrapped\endwrapped\input{right}",
            r"\newenvironment{wrapped}{}{}\csuse{wrapped}\csuse{endwrapped}\input{right}",
            r"\newenvironment{wrapped}{}{}\csname wrapped\endcsname\csname endwrapped\endcsname\input{right}",
            r"\newenvironment{wrapped}{}{}\def\finish{\endwrapped}\wrapped\finish\input{right}",
            r"\newenvironment{wrapped}{\consume}{}\newcommand{\consume}[1]{}\wrapped\input{wrong}\input{right}",
            r"\newenvironment{wrapped}{}{\consume}\newcommand{\consume}[1]{}\endwrapped\input{wrong}\input{right}",
            r"\NewDocumentEnvironment{wrapped}{}{\consume}{}\newcommand{\consume}[1]{}\wrapped\input{wrong}\input{right}",
            r"\NewDocumentEnvironment{wrapped}{}{}{\consume}\newcommand{\consume}[1]{}\endwrapped\input{wrong}\input{right}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            let operands = signatures.argument_tokens(source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
            {
                assert_eq!(
                    operands.contains(&command.span.start),
                    command.argument.as_ref().unwrap().value == "wrong",
                    "{source}"
                );
            }
        }
        let source = r"\newenvironment{wrapped}{}{}\def\finish{\endwrapped}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let paused =
            CommandSignatures::with_ambiguous_environments([(source, &parsed)], [], ["wrapped"]);
        assert!(paused.local["wrapped"].is_none());
        assert!(paused.local["endwrapped"].is_none());
        assert!(paused.local["finish"].is_none());
        for source in [
            r"\newenvironment{wrapped}{}{}\renewcommand{\endwrapped}[1]{}\def\finish{\endwrapped}",
            r"\newenvironment{wrapped}{\endwrapped}{\wrapped}\def\finish{\endwrapped}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            assert!(signatures.local["endwrapped"].is_none(), "{source}");
            assert!(signatures.local["finish"].is_none(), "{source}");
        }
    }

    #[test]
    fn environment_declaration_metadata_is_data_while_start_and_end_code_stays_visible() {
        for source in [
            r"\newenvironment{wrapped}[1][\input{wrong}]{\input{right}}{\input{right}}",
            r"\NewDocumentEnvironment{wrapped}{O{\input{wrong}}}{\input{right}}{\input{right}}",
            r"\NewEnviron{wrapped}[1][\input{wrong}]{\input{right}}[\input{right}]",
            r"\def\declare{\NewDocumentEnvironment}\declare{wrapped}{O{\input{wrong}}}{\input{right}}{\input{right}}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            let operands = signatures.argument_tokens(source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
            {
                assert_eq!(
                    operands.contains(&command.span.start),
                    command.argument.as_ref().unwrap().value == "wrong",
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn environment_start_signatures_preserve_namespace_conflicts_and_paused_aliases() {
        let source = r"\newenvironment{wrapped}[3]{}{}\def\bridge{\wrapped}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let signatures = CommandSignatures::new([(source, &parsed)]);
        assert_eq!(
            signatures.local["wrapped"]
                .as_ref()
                .unwrap()
                .parameters
                .len(),
            3
        );
        assert_eq!(
            signatures.local["bridge"]
                .as_ref()
                .unwrap()
                .parameters
                .len(),
            3
        );
        let paused = CommandSignatures::with_ambiguous_names([(source, &parsed)], ["wrapped"]);
        assert!(paused.local["wrapped"].is_none());
        assert!(paused.local["bridge"].is_none());
        for source in [
            r"\newenvironment{wrapped}[3]{}{}\renewcommand{\wrapped}[1]{}\def\bridge{\wrapped}",
            r"\newenvironment{wrapped}[3]{}{}\renewenvironment{wrapped}[1]{}{}\def\bridge{\wrapped}",
            r"\NewDocumentEnvironment{wrapped}{b}{}{}\def\bridge{\wrapped}",
            r"\NewEnvironmentCopy{wrapped}{other}\def\bridge{\wrapped}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            assert!(signatures.local["wrapped"].is_none(), "{source}");
            assert!(signatures.local["bridge"].is_none(), "{source}");
        }
    }

    #[test]
    fn named_macro_invocations_preserve_operand_boundaries_and_following_commands() {
        for named in [r"\csuse{wrapped}", r"\csname wrapped\endcsname"] {
            for (definition, prefix, tail) in [
                (r"\newcommand{\wrapped}[3]{}", r"\relax", ""),
                (r"\newcommand{\wrapped}[4][default]{}", r"[note]\relax", ""),
                (r"\def\wrapped#1#2#3{}", r"\relax", ""),
                (r"\def\wrapped#1#2#3\stop{}", r"\relax", r"\stop"),
                (r"\def\wrapped#1 #2#3#4\stop{}", r" Unused \relax", r"\stop"),
                (
                    r"\NewDocumentCommand{\wrapped}{s t+ O{default} m m m}{}",
                    r"*+[note]\relax",
                    "",
                ),
                (r"\newcommand{\wrapped}[1]{}", "{", "}"),
                (r"\NewDocumentCommand{\wrapped}{d()}{}", "(", ")"),
            ] {
                let source =
                    format!(r"{definition}{named}{prefix}\input{{wrong}}{tail}\input{{right}}");
                let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
                let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
                let operands = signatures.argument_tokens(&source, &parsed);
                let calls: Vec<_> = parsed
                    .commands
                    .iter()
                    .filter(|command| command.name == "input")
                    .collect();
                assert_eq!(calls.len(), 2);
                assert!(operands.contains(&calls[0].span.start), "{source}");
                assert!(!operands.contains(&calls[1].span.start), "{source}");
            }
        }
    }

    #[test]
    fn named_invocations_keep_name_group_spaces_and_literal_definitions_distinct() {
        for source in [
            r"\csdef{wrapped} #1\stop{}\csuse{wrapped} \input{wrong}\stop\input{right}",
            r"\expandafter\def\csname wrapped\endcsname#1{\input{right}}",
            r"\newcommand{\wrapped}[1]{}\renewcommand{\csuse}[1]{}\csuse{wrapped}\input{right}",
            r"\newcommand{\wrapped}[1]{}\newcommand{\discard}[1]{}\discard\csname wrapped\endcsname\relax\input{right}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::new([(source, &parsed)]);
            let operands = signatures.argument_tokens(source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
            {
                assert_eq!(
                    operands.contains(&command.span.start),
                    command.argument.as_ref().unwrap().value == "wrong",
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn tex_delimiters_skip_groups_match_whole_tokens_and_preserve_completed_calls() {
        for (definition, call) in [
            (
                r"\def\wrapped#1 #2#3#4\stop{}",
                r"\wrapped Unused \relax\input{wrong}\stop",
            ),
            (
                "\\def\\wrapped#1 \t  #2#3#4\\stop{}",
                "\\wrapped Unused\t\\relax\\input{wrong}\\stop",
            ),
            (
                "\\def\\wrapped#1\r\n #2#3#4\\stop{}",
                "\\wrapped Unused\r\n \\relax\\input{wrong}\\stop",
            ),
            (
                "\\def\\wrapped#1 % keep\r\n #2#3#4\\stop{}",
                "\\wrapped Unused % keep\r\n \\relax\\input{wrong}\\stop",
            ),
            (
                r"\def\wrapped#1a #2\stop{}",
                r"\wrapped\input{wrong}a \relax\stop",
            ),
            (
                r"\def\wrapped#1\marker   #2\stop{}",
                r"\wrapped\input{wrong}\marker payload\stop",
            ),
            (
                r"\def\wrapped#1\% #2\stop{}",
                r"\wrapped\input{wrong}\% payload\stop",
            ),
            (
                r"\def\wrapped#1\    #2\stop{}",
                r"\wrapped\input{wrong}\ payload\stop",
            ),
            (
                r"\def\wrapped#1#2#3\stop{}",
                r"\wrapped\relax\input{wrong}\stop",
            ),
            (
                r"\def\wrapped#1\stop{}",
                r"\wrapped\relax\input{wrong}\stop",
            ),
            (
                r"\def\wrapped#1\stop{}",
                r"\wrapped{\stop\input{wrong}}\stopper\input{wrong}\stop",
            ),
            (
                r"\def\wrapped\start#1\mid#2\stop{}",
                r"\wrapped\start\relax\mid\input{wrong}\stop",
            ),
            (r"\def\wrapped#1aba{}", r"\wrapped ab\input{wrong}aaba"),
            (r"\def\wrapped#1;!{}", r"\wrapped; !\input{wrong};!"),
            (
                "\\def\\wrapped#1a% keep\r\n b{}",
                "\\wrapped a% keep\r\n \\input{wrong}ab",
            ),
            (
                r"\def\wrapped#1\stop{}\def\bridge{\wrapped}",
                r"\bridge\input{wrong}\stop",
            ),
        ] {
            let source = format!("{definition}\n{call}\\input{{right}}");
            let text = Text::new(source.clone());
            let parsed = syntax::Syntax::latex(&text);
            let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
            let operands = signatures.argument_tokens(&source, &parsed);
            let calls: Vec<_> = parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
                .collect();
            assert!(calls.len() >= 2, "{source}");
            for command in &calls[..calls.len() - 1] {
                assert!(operands.contains(&command.span.start), "{source}");
            }
            assert!(
                !operands.contains(&calls.last().unwrap().span.start),
                "{source}"
            );
        }
    }

    #[test]
    fn tex_operand_aliases_preserve_ambiguity_and_do_not_add_signature_help() {
        let source = r"\def\wrapped#1\stop{}\def\bridge{\wrapped}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let signatures = CommandSignatures::new([(source, &parsed)]);
        assert!(signatures.local["wrapped"].is_none());
        assert!(signatures.local["bridge"].is_none());
        assert!(signatures.tex_operands["wrapped"].is_some());
        assert!(signatures.tex_operands["bridge"].is_some());
        let paused = CommandSignatures::with_ambiguous_names([(source, &parsed)], ["wrapped"]);
        assert!(paused.tex_operands["wrapped"].is_none());
        assert!(paused.tex_operands["bridge"].is_none());
        let duplicate = format!("{source}\\def\\wrapped#1#2\\stop{{}}");
        let parsed = syntax::Syntax::latex(&Text::new(duplicate.clone()));
        let signatures = CommandSignatures::new([(duplicate.as_str(), &parsed)]);
        assert!(signatures.tex_operands["wrapped"].is_none());
        assert!(signatures.tex_operands["bridge"].is_none());
    }

    #[test]
    fn malformed_and_catcode_sensitive_tex_parameter_text_is_not_inferred() {
        for source in [
            r"\def\wrapped#2\stop{}",
            r"\def\wrapped#1#1\stop{}",
            "\\def\\wrapped#1\n\n#2\\stop{}",
            "\\def\\wrapped\n\n#1{}",
            r"\def\wrapped#1^^41{}",
            r"\def\wrapped#1é{}",
            r"\def\wrapped#1#{}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            assert!(!parsed.definitions.is_empty(), "{source}");
            assert!(
                TexOperands::definition(source, &parsed, &parsed.definitions[0]).is_none(),
                "{source}"
            );
        }
    }

    #[test]
    fn tex_space_tokens_do_not_confuse_named_prefixes_comments_and_paragraphs() {
        for (source, prefix, delimiters) in [
            (
                r"\def\wrapped#1   #2\stop{}",
                vec![],
                vec![vec![" "], vec![r"\stop"]],
            ),
            (
                r"\csdef{wrapped} #1\stop{}",
                vec![" "],
                vec![vec![r"\stop"]],
            ),
            (
                "\\def\\wrapped#1% join\r\n #2{}",
                vec![],
                vec![vec![], vec![]],
            ),
            (
                "\\def\\wrapped#% join\r\n 1 #2{}",
                vec![],
                vec![vec![" "], vec![]],
            ),
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let (operands, _) =
                TexOperands::definition(source, &parsed, &parsed.definitions[0]).unwrap();
            assert_eq!(operands.prefix, prefix, "{source}");
            assert_eq!(operands.delimiters, delimiters, "{source}");
        }
        let source = r"\def\wrapped#1 #2\stop{}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let (operands, _) =
            TexOperands::definition(source, &parsed, &parsed.definitions[0]).unwrap();
        for call in [
            " Unused\n\npayload\\stop",
            " Unused % keep\r\n \r\npayload\\stop",
        ] {
            assert!(operands.end(call, 0, true).is_none(), "{call}");
        }
        let source = r"\csdef{wrapped} #1\stop{}";
        let parsed = syntax::Syntax::latex(&Text::new(source.into()));
        let (operands, _) =
            TexOperands::definition(source, &parsed, &parsed.definitions[0]).unwrap();
        assert!(operands.end(r" Unused\stop", 0, true).is_none());
    }

    #[test]
    fn tex_definition_forms_share_bounded_operand_rules() {
        for declaration in [
            "def", "gdef", "edef", "xdef", "csdef", "csgdef", "csedef", "csxdef",
        ] {
            let target = if declaration.starts_with("cs") {
                "{wrapped}"
            } else {
                r"\wrapped"
            };
            let source = format!(
                r"\{declaration}{target}#1#2\stop{{}}\wrapped\relax\input{{wrong}}\stop\input{{right}}"
            );
            let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
            let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
            assert!(
                signatures.tex_operands["wrapped"].is_some(),
                "{declaration}"
            );
            let operands = signatures.argument_tokens(&source, &parsed);
            let calls: Vec<_> = parsed
                .commands
                .iter()
                .filter(|command| command.name == "input")
                .collect();
            assert_eq!(calls.len(), 2);
            assert!(operands.contains(&calls[0].span.start), "{declaration}");
            assert!(!operands.contains(&calls[1].span.start), "{declaration}");
        }
        for count in [64, 65] {
            let source = format!(r"\def\wrapped#1{}{{}}", "a".repeat(count));
            let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
            assert_eq!(
                TexOperands::definition(&source, &parsed, &parsed.definitions[0]).is_some(),
                count == 64
            );
        }
    }
}

#[cfg(test)]
mod code_configuration_operand_tests {
    use super::*;
    use crate::text::Text;

    #[test]
    fn code_settings_stop_at_one_data_argument_before_independent_file_calls() {
        for name in ["lstset", "fvset"] {
            for call in [
                format!("\\{name}"),
                format!("\\csuse{{{name}}}"),
                format!("\\csname {name}\\endcsname"),
                r"\settings".into(),
                r"\csuse{settings}".into(),
                r"\csname settings\endcsname".into(),
            ] {
                for argument in [r"{\input{opaque}}", r"\input{opaque}"] {
                    let source = format!(
                        r"\def\settings{{\{name}}}{call}{argument}\verbatiminput{{code.txt}}\lstinputlisting{{code.txt}}\VerbatimInput{{code.txt}}\input{{right}}"
                    );
                    let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
                    let signatures =
                        CommandSignatures::source_context([(source.as_str(), &parsed)]);
                    let operands = signatures.argument_tokens(&source, &parsed);
                    for command in parsed.commands.iter().filter(|command| {
                        syntax::code_input_command(&command.name) || command.name == "input"
                    }) {
                        assert_eq!(
                            operands.contains(&command.span.start),
                            command
                                .argument
                                .as_ref()
                                .is_some_and(|argument| argument.value == "opaque"),
                            "{source}: {command:?}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod chapter_selector_operand_boundaries {
    use super::*;
    use crate::text::Text;

    #[test]
    fn chapter_selection_ends_before_following_inputs_and_keeps_its_names_as_data() {
        for call in [
            r"\includeonly",
            r"\csuse{includeonly}",
            r"\csname includeonly\endcsname",
            r"\choose",
            r"\csuse{choose}",
            r"\csname choose\endcsname",
        ] {
            let source = format!(
                r"\def\choose{{\includeonly}}{call}{{\input{{opaque}}}}\input{{settings}}\include{{chapter}}"
            );
            let parsed = syntax::Syntax::latex(&Text::new(source.clone()));
            let signatures = CommandSignatures::source_context([(source.as_str(), &parsed)]);
            let operands = signatures.argument_tokens(&source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| matches!(command.name.as_str(), "input" | "include"))
            {
                assert_eq!(
                    operands.contains(&command.span.start),
                    command
                        .argument
                        .as_ref()
                        .is_some_and(|argument| argument.value == "opaque"),
                    "{source}: {command:?}"
                );
            }
        }
    }
}
#[cfg(test)]
mod quoted_source_token_tests {
    use super::*;
    use crate::text::Text;

    #[test]
    fn meaning_quotes_one_control_token_without_consuming_groups_or_forwarded_operands() {
        for source in [
            r"\meaning\input{quoted}\input{real}",
            "\\meaning% before 😀\r\n\\input{quoted}\\input{real}",
            r"\meaning{\input{real}}\input{real}",
            r"\meaning*\input{real}",
            r"\def\meaning#1{#1}\meaning\input{real}",
            r"\newcommand{\execute}[1]{#1}\execute{\meaning\input{real}}",
            r"\csuse{meaning}\input{quoted}\input{real}",
            r"\csname meaning\endcsname\input{quoted}\input{real}",
            r"\meaning\include{quoted}\include{real}",
        ] {
            let parsed = syntax::Syntax::latex(&Text::new(source.into()));
            let signatures = CommandSignatures::source_context([(source, &parsed)]);
            let quoted = signatures.quoted_command_tokens(source, &parsed);
            for command in parsed
                .commands
                .iter()
                .filter(|command| matches!(command.name.as_str(), "input" | "include"))
            {
                assert_eq!(
                    quoted.contains(&command.span.start),
                    // Fixture names identify expected operands independently
                    // of whether Syntax gives the control call arguments.
                    source[command.span.end..].starts_with("{quoted}"),
                    "{source}: {command:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod noexpand_tests {
    use super::*;

    #[test]
    fn noexpand_suppresses_one_control_token_with_the_invocations_real_space_rules() {
        for (invocation, tail, suppressed) in [
            (r"\noexpand", r"\leaf", true),
            (r"\noexpand", r" \leaf", true),
            (r"\csuse{noexpand}", r"\leaf", true),
            (r"\csuse{noexpand}", r" \leaf", false),
            (r"\csuse{noexpand}", "\n\\leaf", false),
            (r"\csuse{noexpand}", "% comment\r\n  \\leaf", true),
            (r"\csname noexpand\endcsname", r" \leaf", true),
            (r"\hold", r" \leaf", true),
            (r"\csuse{hold}", r" \leaf", false),
        ] {
            let source = format!(
                "\\newcommand{{\\leaf}}{{}}\n\\def\\hold{{\\noexpand}}\n{invocation}{tail}\n\\relax"
            );
            let parsed = syntax::Syntax::latex(&crate::text::Text::new(source.clone()));
            let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
            let operands = signatures.argument_tokens(&source, &parsed);
            let leaf = source.rfind("\\leaf").unwrap();
            assert_eq!(operands.contains(&leaf), suppressed, "{source}");
            assert!(
                !operands.contains(&source.rfind("\\relax").unwrap()),
                "{source}"
            );
        }
    }

    #[test]
    fn noexpand_does_not_turn_groups_or_literal_math_and_alignment_tokens_into_macro_data() {
        for invocation in [
            r"\noexpand",
            r"\csuse{noexpand}",
            r"\csname noexpand\endcsname",
            r"\hold",
        ] {
            for tail in [r"{\leaf}", r"[\leaf]", r"$\leaf$", r"&\leaf"] {
                let source = format!(
                    "\\newcommand{{\\leaf}}{{}}\n\\def\\hold{{\\noexpand}}\n{invocation}{tail}"
                );
                let parsed = syntax::Syntax::latex(&crate::text::Text::new(source.clone()));
                let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
                assert!(
                    !signatures
                        .argument_tokens(&source, &parsed)
                        .contains(&source.rfind("\\leaf").unwrap()),
                    "{source}"
                );
                let start = source.len() - tail.len();
                assert!(
                    !signatures.literal_is_argument(&source, &parsed, start),
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn user_redefinitions_keep_their_own_parameter_grammar() {
        for (definition, suppressed) in [(r"\def\noexpand{}", false), (r"\def\noexpand#1{}", true)]
        {
            let source = format!("\\newcommand{{\\leaf}}{{}}\n{definition}\n\\noexpand\\leaf");
            let parsed = syntax::Syntax::latex(&crate::text::Text::new(source.clone()));
            let signatures = CommandSignatures::new([(source.as_str(), &parsed)]);
            assert_eq!(
                signatures
                    .argument_tokens(&source, &parsed)
                    .contains(&source.rfind("\\leaf").unwrap()),
                suppressed,
                "{source}"
            );
        }
    }
    #[test]
    fn primitive_noexpand_only_hides_the_suppressed_loader_not_independent_grouped_inputs() {
        for (call, names) in [
            (r"\noexpand\input{skip}\input{after}", vec!["after"]),
            (
                r"\noexpand{\input{inside}}\input{after}",
                vec!["inside", "after"],
            ),
            (r"\csuse{noexpand} \input{inside}", vec!["inside"]),
            (r"\csuse{noexpand}\input{skip}\input{after}", vec!["after"]),
        ] {
            let parsed = syntax::Syntax::latex(&crate::text::Text::new(call.into()));
            let signatures = CommandSignatures::source_context([(call, &parsed)]);
            let positions = signatures.source_command_positions(call, &parsed);
            let actual: Vec<_> = parsed
                .commands
                .iter()
                .filter(|command| {
                    command.name == "input" && positions.contains(&command.span.start)
                })
                .map(|command| command.argument.as_ref().unwrap().value.as_str())
                .collect();
            assert_eq!(actual, names, "{call}");
        }
    }
}
