use super::*;

pub(super) fn definition(
    uri: &Url,
    doc: &Document,
    entry: &Entry,
    environment: bool,
) -> Option<Signature> {
    let signature = source_definition(&doc.text.source, &doc.syntax, entry, environment)?;
    describe_definition(uri, doc, entry, environment, signature)
}

pub(super) fn describe_definition(
    uri: &Url,
    doc: &Document,
    entry: &Entry,
    environment: bool,
    mut signature: Signature,
) -> Option<Signature> {
    if signature.parameters.is_empty() || environment && signature.parameters.len() == 1 {
        return None;
    }
    signature.documentation = format!(
        "{}\n{}:{}",
        entry.detail,
        uri.path(),
        doc.text.position(entry.span.start).line + 1
    );
    if let Some(notes) = crate::symbols::command_docs::comments(doc, entry) {
        signature.documentation.push_str("\n\n");
        signature.documentation.push_str(&notes);
    }
    Some(signature)
}

pub(super) fn definition_arguments_start(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
    environment: bool,
) -> Option<usize> {
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    let mut i = syntax::skip_space(source, entry.span.end);
    if !environment
        && !crate::symbols::named_commands::declares(declaration)
        && source.as_bytes().get(entry.span.start) != Some(&b'\\')
    {
        let end = parsed
            .commands
            .iter()
            .find(|command| command.span.start == i && command.name == "endcsname")?;
        i = syntax::skip_space(source, end.span.end);
    }
    if source.as_bytes().get(i) == Some(&b'}') {
        i += 1;
    }
    i = syntax::skip_space(source, i);
    if matches!(declaration, "DeclareTextCommand" | "ProvideTextCommand") {
        let (_, end) = syntax::group(source, i, b'{', b'}')?;
        i = syntax::skip_space(source, end);
    }
    Some(i)
}

/// Comments remove their endline and indentation, but ordinary spaces between
/// digits remain significant. Do not expand macros or discard escaped tokens.
pub(super) fn literal_argument_count(source: &str) -> Option<usize> {
    if !source.contains('%') {
        return source.trim().parse().ok();
    }
    let mut literal = String::new();
    let mut at = 0;
    while at < source.len() {
        at = tex_operand_comments(source, at);
        let Some(ch) = source[at..].chars().next() else {
            break;
        };
        if ch == '\\' {
            return None;
        }
        literal.push(ch);
        at += ch.len_utf8();
    }
    literal.trim().parse().ok()
}

/// A class can bind the six layout operands while leaving LaTeX's section
/// reader to consume the star, optional short title and required title at use.
/// Require an exact tail call, so a fixed title or extra replacement tokens do
/// not turn an ordinary zero-argument macro into a title-taking command.
pub(super) fn startsection_wrapper(source: &str, at: usize, name: &str) -> Option<Signature> {
    let (body, _) = syntax::group(source, at, b'{', b'}')?;
    let source = body.value.as_str();
    let mut at = syntax::skip_space(source, 0);
    let command = syntax::control_word(source, at)?;
    if &source[command.clone()] != "\\@startsection" {
        return None;
    }
    at = command.end;
    for _ in 0..6 {
        let (_, end) = syntax::group(source, syntax::skip_space(source, at), b'{', b'}')?;
        at = end;
    }
    if syntax::skip_space(source, at) != source.len() {
        return None;
    }
    let mut signature = builtin("section")?;
    signature.name = name.into();
    Some(signature)
}

pub(super) fn source_definition(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
    environment: bool,
) -> Option<Signature> {
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    let mut i = definition_arguments_start(source, parsed, entry, environment)?;
    let mut parameters = Vec::new();
    let mut nested_brackets = true;
    match declaration {
        "DeclareTextFontCommand"
        | "DeclareMathAlphabet"
        | "DeclareSymbolFontAlphabet"
        | "DeclareTextAccent"
        | "DeclareMathAccent"
        | "DeclareMathRadical" => {
            parameters.push(Parameter {
                obey_spaces: false,
                kind: Kind::Required,
                label: "{#1}".into(),
            });
        }
        declaration if syntax::declares_paired_delimiter(declaration) => {
            let count = if declaration == "DeclarePairedDelimiter" {
                1
            } else {
                let (argument, _) = syntax::group(source, i, b'[', b']')?;
                let count = literal_argument_count(&argument.value)?;
                if !(1..=9).contains(&count) {
                    return None;
                }
                count
            };
            parameters.push(Parameter {
                obey_spaces: false,
                kind: Kind::Star,
                label: "*".into(),
            });
            parameters.push(Parameter {
                obey_spaces: false,
                kind: Kind::UnstarredOptional,
                label: "[size]".into(),
            });
            for number in 1..=count {
                parameters.push(Parameter {
                    obey_spaces: false,
                    kind: Kind::Required,
                    label: format!("{{#{number}}}"),
                });
            }
        }
        "newcommand"
        | "DeclareTextCommand"
        | "ProvideTextCommand"
        | "DeclareTextCommandDefault"
        | "ProvideTextCommandDefault"
        | "renewcommand"
        | "providecommand"
        | "newrobustcmd"
        | "renewrobustcmd"
        | "providerobustcmd"
        | "DeclareRobustCommand"
        | "newenvironment"
        | "renewenvironment"
        | "provideenvironment"
        | "NewEnviron"
        | "RenewEnviron" => {
            nested_brackets = false;
            // A traditional declaration without a count has no arguments.
            // Keep this information for operand analysis; signature help
            // still omits empty signatures.
            if source.as_bytes().get(i) == Some(&b'{') {
                return Some(Signature {
                    name: entry.name.clone(),
                    parameters,
                    documentation: entry.detail.clone(),
                    starred: false,
                    nested_brackets,
                });
            }
            let (count, end) = syntax::group(source, i, b'[', b']')?;
            let count = literal_argument_count(&count.value)?;
            if count > 9 {
                return None;
            }
            i = syntax::skip_space(source, end);
            let default = syntax::bracket_argument(source, i).map(|(argument, _)| argument.value);
            if count == 0 && default.is_some() {
                return None;
            }
            for number in 1..=count {
                let optional = number == 1 && default.is_some();
                let label = if optional {
                    format!("[#1={}]", short(default.as_deref().unwrap()))
                } else {
                    format!("{{#{number}}}")
                };
                parameters.push(Parameter {
                    obey_spaces: false,
                    kind: if optional {
                        Kind::Optional
                    } else {
                        Kind::Required
                    },
                    label,
                });
            }
        }
        declaration
            if syntax::declares_document_command(declaration)
                || matches!(
                    declaration,
                    "NewDocumentEnvironment"
                        | "RenewDocumentEnvironment"
                        | "ProvideDocumentEnvironment"
                        | "DeclareDocumentEnvironment"
                ) =>
        {
            let (arguments, _) = syntax::group(source, i, b'{', b'}')?;
            let source = &arguments.value;
            let mut i = 0;
            let mut obey_spaces = false;
            let mut seen_obey_spaces = false;
            while i < source.len() {
                i = syntax::skip_space(source, i);
                let Some(&kind) = source.as_bytes().get(i) else {
                    break;
                };
                i += 1;
                if kind == b'+' {
                    continue;
                }
                if kind == b'!' {
                    if obey_spaces || declaration.contains("ExpandableDocumentCommand") {
                        return None;
                    }
                    obey_spaces = true;
                    seen_obey_spaces = true;
                    continue;
                }
                // The kernel permits ! only on the final optional arguments.
                // A mandatory argument after any such modifier is invalid.
                if seen_obey_spaces && matches!(kind, b'm' | b'r' | b'R') {
                    return None;
                }
                let number = parameters.len() + 1;
                let (kind, label) = match kind {
                    b'm' => (Kind::Required, format!("{{#{number}}}")),
                    b'o' => (Kind::Optional, format!("[#{number}]")),
                    b'O' => {
                        i = syntax::skip_space(source, i);
                        let (default, end) = syntax::group(source, i, b'{', b'}')?;
                        i = end;
                        (
                            Kind::Optional,
                            format!("[#{number}={}]", short(&default.value)),
                        )
                    }
                    b's' => (Kind::Star, "*".into()),
                    b't' => {
                        let (flag, end) = delimiter_spec(source, i)?;
                        i = end;
                        (Kind::Flag(flag.clone()), flag)
                    }
                    b'd' | b'D' | b'r' | b'R' => {
                        let (open, end) = delimiter_spec(source, i)?;
                        let (close, end) = delimiter_spec(source, end)?;
                        i = end;
                        let default = if matches!(kind, b'D' | b'R') {
                            i = syntax::skip_space(source, i);
                            let (default, end) = syntax::group(source, i, b'{', b'}')?;
                            i = end;
                            format!("={}", short(&default.value))
                        } else {
                            String::new()
                        };
                        let label = format!("{open}#{number}{default}{close}");
                        (
                            Kind::Delimited {
                                open,
                                close,
                                optional: matches!(kind, b'd' | b'D'),
                            },
                            label,
                        )
                    }
                    _ => return None,
                };
                parameters.push(Parameter {
                    kind,
                    label,
                    obey_spaces,
                });
                obey_spaces = false;
                if parameters.len() > 9 {
                    return None;
                }
            }
            if obey_spaces {
                return None;
            }
        }
        "newtheorem" => {
            nested_brackets = false;
            parameters.push(Parameter {
                obey_spaces: false,
                kind: Kind::Optional,
                label: "[note]".into(),
            });
        }
        "def" | "gdef" | "edef" | "xdef" | "csdef" | "csgdef" | "csedef" | "csxdef" => {
            while source.as_bytes().get(i) == Some(&b'#') {
                let number = parameters.len() + 1;
                if number > 9 || source.as_bytes().get(i + 1) != Some(&(b'0' + number as u8)) {
                    return None;
                }
                parameters.push(Parameter {
                    obey_spaces: false,
                    kind: Kind::Required,
                    label: format!("{{#{number}}}"),
                });
                i += 2;
            }
            if source.as_bytes().get(i) != Some(&b'{') {
                return None;
            }
            if !environment && parameters.is_empty() {
                if let Some(signature) = startsection_wrapper(source, i, &entry.name) {
                    return Some(signature);
                }
            }
        }
        _ => return None,
    }
    if environment {
        parameters.insert(
            0,
            Parameter {
                obey_spaces: false,
                kind: Kind::Required,
                label: format!("{{{}}}", entry.name),
            },
        );
    }
    Some(Signature {
        name: if environment {
            "begin".into()
        } else {
            entry.name.clone()
        },
        parameters,
        documentation: entry.detail.clone(),
        starred: false,
        nested_brackets,
    })
}

/// The start control of an environment reads its arguments after the name
/// supplied to begin. Keep signature help's synthetic name parameter separate.
pub(super) fn environment_start_signature(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
) -> Option<Signature> {
    let mut signature = source_definition(source, parsed, entry, true)?;
    // A traditional declaration without a count returns an empty signature
    // before the synthetic name parameter is inserted.
    if signature.name == "begin" && !signature.parameters.is_empty() {
        signature.parameters.remove(0);
    }
    signature.name = entry.name.clone();
    Some(signature)
}

// These control symbols can be called literally by csuse/csname without
// widening the identifier grammar of labels, colors or other namespaces.
pub(super) fn static_invocation_name(name: &str) -> bool {
    syntax::is_static_key(name) || matches!(name, "(" | ")" | "[" | "]")
}
pub(crate) fn literal_invocation_name(
    source: &str,
    commands: &[syntax::Command],
    index: usize,
) -> Option<std::ops::Range<usize>> {
    let command = commands
        .get(index)
        .filter(|command| command.name == "csname")?;
    let end = commands
        .get(index + 1)
        .filter(|command| command.name == "endcsname")?;
    let name = syntax::skip_space(source, command.span.end).min(end.span.start)..end.span.start;
    static_invocation_name(&source[name.clone()]).then_some(name)
}

pub(super) fn body_alias(source: &str, body: &syntax::Argument) -> Option<String> {
    let start = syntax::skip_space(source, body.span.start);
    let token = syntax::token(source, start)?;
    let name = token.strip_prefix('\\')?;
    if syntax::control_word(source, start).is_none() && !matches!(name, "(" | ")" | "[" | "]") {
        return None;
    }
    (syntax::skip_space(source, start + token.len()) == body.span.end).then(|| name.into())
}

pub(crate) fn environment_bodies(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
) -> Option<(syntax::Argument, syntax::Argument)> {
    let mut at = definition_arguments_start(source, parsed, entry, true)?;
    match entry.detail.strip_prefix("Defined with \\")? {
        "newenvironment" | "renewenvironment" | "provideenvironment" => {
            if let Some((_, end)) = syntax::bracket_argument(source, at) {
                at = syntax::skip_space(source, end);
                if let Some((_, end)) = syntax::bracket_argument(source, at) {
                    at = syntax::skip_space(source, end);
                }
            }
        }
        "NewDocumentEnvironment"
        | "RenewDocumentEnvironment"
        | "ProvideDocumentEnvironment"
        | "DeclareDocumentEnvironment" => {
            at = syntax::group(source, at, b'{', b'}')?.1;
        }
        _ => return None,
    }
    let (begin, end) = syntax::group(source, syntax::skip_space(source, at), b'{', b'}')?;
    let (end, _) = syntax::group(source, syntax::skip_space(source, end), b'{', b'}')?;
    Some((begin, end))
}

/// A single control word or math delimiter forwards to its current meaning.
/// This recognizes lexical aliases without evaluating arbitrary replacement text.
pub(crate) fn source_alias(source: &str, parsed: &syntax::Syntax, entry: &Entry) -> Option<String> {
    body_alias(source, &source_alias_body(source, parsed, entry)?)
}

/// Read a static copy's source and operand end. A copy captures a meaning;
/// possible source shapes do not certify that meaning or a late-bound alias.
pub(crate) fn literal_copy_target(
    source: &str,
    parsed: &syntax::Syntax,
    entry: &Entry,
    environment: bool,
) -> Option<(String, usize)> {
    let declaration = entry.detail.strip_prefix("Defined with \\")?;
    let named = if environment {
        syntax::declares_environment_copy(declaration).then_some(true)?
    } else {
        match declaration {
            "let" | "cslet" => false,
            "letcs" | "csletcs" => true,
            name if syntax::declares_command_copy(name) => false,
            _ => return None,
        }
    };
    let mut at = if declaration == "let" {
        // Primitive let does not accept a braced destination or source. Do
        // not mistake a copied closing brace for the destination's delimiter.
        let mut at = syntax::skip_space(source, entry.span.end);
        if source.as_bytes().get(entry.span.start) != Some(&b'\\') {
            at = parsed
                .commands
                .iter()
                .find(|command| command.span.start == at && command.name == "endcsname")?
                .span
                .end;
        }
        at = syntax::skip_space(source, at);
        if source.as_bytes().get(at) == Some(&b'=') {
            at = syntax::skip_space(source, at + 1);
        }
        at
    } else {
        definition_arguments_start(source, parsed, entry, environment)?
    };
    if named {
        let (argument, end) = syntax::group(source, at, b'{', b'}')?;
        return syntax::is_static_key(&argument.value).then_some((argument.value, end));
    }
    if declaration != "let" {
        if let Some((argument, end)) = syntax::group(source, at, b'{', b'}') {
            at = syntax::skip_space(source, argument.span.start);
            let word = syntax::control_word(source, at)?;
            if syntax::skip_space(source, word.end) != argument.span.end {
                return None;
            }
            return Some((source[word.start + 1..word.end].into(), end));
        }
    }
    let word = syntax::control_word(source, at)?;
    Some((source[word.start + 1..word.end].into(), word.end))
}
