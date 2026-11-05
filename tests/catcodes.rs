use texquill::{parser::syntax::Syntax, text::Text};

const IEEE_BACKSLASH: &str = "{\\catcode`\\|=0 \\catcode`\\\\=12\n|xdef|@IEEEbackslash{\\}}";

#[test]
fn ieeetran_backslash_definition_balances_without_suppressing_real_errors() {
    for ending in ["", "\n", "\r\n"] {
        let source = format!("{IEEE_BACKSLASH}{ending}\\newcommand{{\\ordinary}}{{Body}}");
        let parsed = Syntax::latex(&Text::new(source.clone()));
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
        assert!(parsed.fixes.is_empty(), "{source}: {:?}", parsed.fixes);
        assert!(parsed
            .definitions
            .iter()
            .any(|entry| entry.name == "ordinary"));
    }
    for (tail, message) in [("{", "Unclosed brace"), ("}", "Unexpected closing brace")] {
        let source = format!("{IEEE_BACKSLASH}\n{tail}");
        let text = Text::new(source.clone());
        let parsed = Syntax::latex(&text);
        assert_eq!(
            parsed.diagnostics.len(),
            1,
            "{source}: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.diagnostics[0].message, message);
        assert_eq!(
            parsed.diagnostics[0].range,
            text.range(source.len() - 1, source.len())
        );
        assert_eq!(parsed.fixes.len(), 1);
    }
}

#[test]
fn literal_escape_assignments_accept_tex_integer_forms_and_restore_nested_scopes() {
    for (target, value) in [
        ("92", "12"),
        ("'134", "'14"),
        ("\"5C", "\"C"),
        ("`\\\\", "12"),
    ] {
        let source =
            format!("{{\\catcode`\\|=0 \\catcode{target}={value}\n{{\\}} |relax}}\\label{{kept}}");
        let parsed = Syntax::latex(&Text::new(source.clone()));
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.labels.len(), 1, "{source}");
        assert_eq!(parsed.labels[0].name, "kept");
    }
    let source = "{\\catcode`\\|=0 {\\catcode`\\\\=12 |relax{\\}}\\label{kept}}\\label{after}";
    let parsed = Syntax::latex(&Text::new(source.into()));
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert_eq!(
        parsed
            .labels
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["kept", "after"]
    );
}

#[test]
fn tokenized_copied_redefined_and_literal_catcode_commands_do_not_change_lexing() {
    for source in [
        r"\def\wrapper{\catcode`\|=0 \catcode`\\=12 \}}",
        r"\newcommand{\wrapper}{\catcode`\\=12 \}}",
        r"\textbf{\catcode`\\=12 \}}",
        r"\let\saved\catcode`\\=12 \}",
        r"\def\catcode{}\catcode`\\=12 \}",
        r"\verb|\catcode`\\=12|\}",
        "% \\catcode`\\=12\n\\}",
    ] {
        let parsed = Syntax::latex(&Text::new(source.into()));
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
    }
}

#[test]
fn explicit_group_commands_restore_escape_codes_and_global_assignments_survive() {
    for (open, close) in [(r"\begingroup", "|endgroup"), (r"\bgroup", "|egroup")] {
        let source =
            format!("{open}\\catcode`\\|=0 \\catcode`\\\\=12\n{{\\}}{close}\\label{{after}}");
        let parsed = Syntax::latex(&Text::new(source.clone()));
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.labels.len(), 1, "{source}");
    }
    let source = "{{\\global\\catcode`\\|=0}}|catcode92=12\n{\\}";
    let parsed = Syntax::latex(&Text::new(source.into()));
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn alternate_escape_tokens_do_not_turn_their_operands_into_groups_or_comments() {
    for source in [
        "{\\catcode`\\|=0 |{ |} |%}",
        "{\\catcode`\\|=0 |catcode`\\\\=12\n{\\}}",
        "{\\catcode`\\|=0 \\catcode`\\\\=12\n|catcode`|\\=0 \\label{inside}}\\label{after}",
    ] {
        let parsed = Syntax::latex(&Text::new(source.into()));
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
    }
}

#[test]
fn dynamic_incomplete_and_unsupported_assignments_keep_existing_error_checks() {
    for assignment in [
        r"\catcode92=\category",
        r"\catcode92=1\digit",
        r"\catcode92=",
        r"\catcode-92=12",
        r"\catcode128=12",
        r"\catcode92=16",
    ] {
        let source = format!("{assignment}\n\\}}\\label{{kept}}{{");
        let parsed = Syntax::latex(&Text::new(source.clone()));
        assert_eq!(
            parsed.diagnostics.len(),
            1,
            "{source}: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.diagnostics[0].message, "Unclosed brace");
        assert_eq!(parsed.labels.len(), 1, "{source}");
    }
}
