//! Static expl3 control sequences, with explicit syntax-mode boundaries.
use crate::{
    parser::syntax,
    project::workspace::{Document, DocumentKind, Workspace},
    symbols::extra_symbols::{Kind, Occurrence},
};
use lsp_types::*;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn valid(name: &str, variable: bool) -> bool {
    if name.len() > 256
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphabetic() || matches!(c, b'_' | b':'))
    {
        return false;
    }
    if variable {
        matches!(name.get(..2), Some("l_" | "g_" | "c_"))
            && !name.contains(':')
            && name[2..].contains('_')
            && !name.ends_with('_')
    } else {
        name.split_once(':').is_some_and(|(base, suffix)| {
            !base.is_empty()
                && base.contains('_')
                && !suffix.contains(':')
                && suffix.bytes().all(|c| b"nNcoVvexfwpTFD".contains(&c))
        })
    }
}
fn variable(name: &str) -> bool {
    valid(name, true)
}
fn word(source: &str, start: usize, mode: bool) -> usize {
    let mut end = start;
    while let Some(&c) = source.as_bytes().get(end) {
        if c.is_ascii_alphabetic() || c == b'@' || (mode && matches!(c, b'_' | b':')) {
            end += 1;
        } else {
            break;
        }
    }
    end
}
fn target(source: &str, at: usize) -> Option<(String, std::ops::Range<usize>)> {
    let start = syntax::skip_space(source, at);
    if source.as_bytes().get(start) != Some(&b'\\') {
        return None;
    }
    let end = word(source, start + 1, true);
    (end > start + 1).then(|| (source[start + 1..end].into(), start + 1..end))
}
fn function_declaration(name: &str) -> bool {
    let Some((base, suffix)) = name.split_once(':') else {
        return false;
    };
    matches!(
        base,
        "cs_new"
            | "cs_new_protected"
            | "cs_new_nopar"
            | "cs_new_protected_nopar"
            | "cs_set"
            | "cs_set_protected"
            | "cs_set_nopar"
            | "cs_set_protected_nopar"
            | "cs_gset"
            | "cs_gset_protected"
            | "cs_gset_nopar"
            | "cs_gset_protected_nopar"
    ) && matches!(suffix, "Npn" | "Npx" | "Npe" | "Nn" | "Nx" | "Ne")
}
fn alias_declaration(name: &str) -> bool {
    matches!(name, "cs_new_eq:NN" | "cs_set_eq:NN" | "cs_gset_eq:NN")
}
fn variable_declaration(name: &str) -> bool {
    let Some((base, suffix)) = name.split_once(':') else {
        return false;
    };
    let Some((kind, operation)) = base.rsplit_once('_') else {
        return false;
    };
    matches!(
        kind,
        "tl" | "str"
            | "int"
            | "seq"
            | "prop"
            | "bool"
            | "dim"
            | "skip"
            | "muskip"
            | "clist"
            | "fp"
            | "box"
            | "ior"
            | "iow"
            | "coffin"
    ) && ((operation == "new" && suffix == "N")
        || (operation == "const" && matches!(suffix, "Nn" | "Nx" | "Ne")))
}

#[derive(Clone)]
pub(crate) struct Analysis {
    pub entries: Vec<Occurrence>,
    pub bodies: Vec<std::ops::Range<usize>>,
    pub data: Vec<std::ops::Range<usize>>,
}
impl Analysis {
    pub(crate) fn cache_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Occurrence>()
            + self
                .entries
                .iter()
                .map(|entry| entry.name.capacity() + entry.detail.capacity())
                .sum::<usize>()
            + (self.bodies.capacity() + self.data.capacity())
                * std::mem::size_of::<std::ops::Range<usize>>()
    }
}
pub(crate) fn occurrences(doc: &Document) -> Analysis {
    analyze(doc, false)
}
pub(crate) fn math_data_spans(doc: &Document) -> Vec<std::ops::Range<usize>> {
    analyze(doc, true).data
}
fn analyze(doc: &Document, include_data: bool) -> Analysis {
    if let Some((context, binding, document)) = crate::analysis::AnalysisContext::current()
        .and_then(|context| {
            let binding = context.binding.clone()?;
            let document = binding.document(&doc.text.source, &doc.syntax)?;
            Some((context, binding, document))
        })
    {
        if let Some(value) = binding.cache.code(&document, include_data) {
            return value;
        }
        let value = compute_analysis(doc, include_data);
        if !context.cancelled() {
            binding.cache.insert_code(document, include_data, &value);
        }
        value
    } else {
        compute_analysis(doc, include_data)
    }
}

fn compute_analysis(doc: &Document, include_data: bool) -> Analysis {
    if doc.kind != DocumentKind::Latex {
        return Analysis {
            entries: vec![],
            bodies: vec![],
            data: vec![],
        };
    }
    let source = &doc.text.source;
    let mut deferred = crate::symbols::commands::definition_body_spans(source, &doc.syntax);
    let mut data = if include_data {
        crate::symbols::commands::definition_data_spans(source, &doc.syntax)
    } else {
        vec![]
    };
    if ![
        "ExplSyntaxOn",
        "ProvidesExplPackage",
        "ProvidesExplClass",
        "ProvidesExplFile",
    ]
    .iter()
    .any(|name| source.contains(name))
    {
        return Analysis {
            entries: vec![],
            bodies: deferred,
            data,
        };
    }
    let mut declarations = BTreeMap::new();
    let mut result = Vec::new();
    let mut at = 0;
    let mut mode = false;
    let mut groups = Vec::new();
    let mut ignored: Vec<_> = doc.syntax.ignored.iter().collect();
    ignored.sort_by_key(|span| span.start);
    let mut ignored_index = 0;
    while at < source.len() {
        while ignored
            .get(ignored_index)
            .is_some_and(|span| span.end <= at)
        {
            ignored_index += 1;
        }
        if let Some(span) = ignored.get(ignored_index).filter(|span| span.start <= at) {
            at = span.end;
            continue;
        }
        let byte = source.as_bytes()[at];
        if byte != b'\\' {
            // TeX restores syntax catcodes at group boundaries.
            if byte == b'{' {
                groups.push(mode);
            }
            if byte == b'}' {
                if let Some(previous) = groups.pop() {
                    mode = previous;
                }
            }
            at += 1;
            continue;
        }
        let slash = at;
        let start = at + 1;
        let end = word(source, start, mode);
        let name = &source[start..end];
        at = if end == start {
            start + source[start..].chars().next().map_or(0, char::len_utf8)
        } else {
            end
        };
        let active = !deferred.iter().any(|span| span.contains(&slash));
        if active {
            if name == "ExplSyntaxOn" {
                mode = true;
                continue;
            }
            if name == "ExplSyntaxOff" {
                mode = false;
                continue;
            }
            if matches!(
                name,
                "ProvidesExplPackage" | "ProvidesExplClass" | "ProvidesExplFile"
            ) {
                let mut header = end;
                for _ in 0..4 {
                    let Some((_, next)) =
                        syntax::group(source, syntax::skip_space(source, header), b'{', b'}')
                    else {
                        break;
                    };
                    header = next;
                }
                at = header;
                mode = true;
                continue;
            }
            if name == "endinput" {
                break;
            }
        }
        if !mode {
            continue;
        }
        if active
            && (function_declaration(name) || alias_declaration(name) || variable_declaration(name))
        {
            if let Some((target, span)) = target(source, end) {
                let kind = if variable_declaration(name) {
                    Kind::Expl3Variable
                } else {
                    Kind::Expl3Command
                };
                if kind.valid(&target) || include_data {
                    let valid_target = kind.valid(&target);
                    let mut detail = format!("Declared with \\{name}");
                    if function_declaration(name) {
                        let mut body = syntax::skip_space(source, span.end);
                        let mut ordinary = true;
                        if name
                            .split_once(':')
                            .is_some_and(|(_, suffix)| suffix.starts_with("Np"))
                        {
                            let mut count = 0usize;
                            while source.as_bytes().get(body) == Some(&b'#')
                                && source
                                    .as_bytes()
                                    .get(body + 1)
                                    .is_some_and(u8::is_ascii_digit)
                            {
                                count += 1;
                                ordinary &=
                                    count <= 9 && source.as_bytes()[body + 1] == b'0' + count as u8;
                                body = syntax::skip_space(source, body + 2);
                            }
                            ordinary &= target
                                .split_once(':')
                                .is_some_and(|(_, suffix)| suffix.len() == count);
                        }
                        if source.as_bytes().get(body) != Some(&b'{') {
                            ordinary = false;
                            let limit = source.len().min(body.saturating_add(4096));
                            while body < limit && source.as_bytes()[body] != b'{' {
                                if source.as_bytes()[body] == b'%' {
                                    body = source[body..]
                                        .find('\n')
                                        .map_or(source.len(), |offset| body + offset + 1);
                                } else if source.as_bytes()[body] == b'\\' {
                                    let end = word(source, body + 1, true);
                                    body = if end == body + 1 {
                                        end + source[end..].chars().next().map_or(0, char::len_utf8)
                                    } else {
                                        end
                                    };
                                } else {
                                    body += 1;
                                }
                            }
                        }
                        if let Some((body, _)) = syntax::group(source, body, b'{', b'}') {
                            if include_data {
                                data.push(slash..body.outer_span.end);
                            }
                            if valid_target {
                                deferred.push(body.span);
                            }
                        }
                        if !ordinary {
                            detail.push_str(" (unsupported parameter text)");
                        }
                    }
                    if include_data && variable_declaration(name) {
                        if let Some((value, _)) =
                            syntax::group(source, syntax::skip_space(source, span.end), b'{', b'}')
                        {
                            data.push(slash..value.outer_span.end);
                        }
                    }
                    if valid_target {
                        declarations.insert(span.start, (kind, detail));
                    }
                }
            }
        }
        let declaration = declarations.get(&start);
        result.push(Occurrence {
            name: name.into(),
            span: start..end,
            kind: declaration.map_or_else(
                || {
                    if variable(name) {
                        Kind::Expl3Variable
                    } else {
                        Kind::Expl3Command
                    }
                },
                |(kind, _)| *kind,
            ),
            declaration: declaration.is_some(),
            detail: declaration.map_or_else(String::new, |(_, detail)| detail.clone()),
        });
    }
    Analysis {
        entries: result,
        bodies: deferred,
        data,
    }
}

const BUILTINS: &[(&str, &str)] = &[
    ("cs_new:Npn", "Define a new function with a parameter text."),
    ("cs_new_protected:Npn", "Define a protected function."),
    ("cs_set:Npn", "Set a function definition."),
    ("cs_gset:Npn", "Set a global function definition."),
    (
        "cs_new:Nn",
        "Define a function with arguments inferred from its signature.",
    ),
    ("cs_new_eq:NN", "Declare a function alias."),
    (
        "cs_generate_variant:Nn",
        "Generate expansion variants of a function.",
    ),
    ("tl_new:N", "Declare a token list variable."),
    ("tl_set:Nn", "Set a token list variable."),
    ("tl_gset:Nn", "Set a global token list variable."),
    ("tl_use:N", "Use a token list variable."),
    ("tl_clear:N", "Clear a token list variable."),
    ("tl_if_empty:NTF", "Test whether a token list is empty."),
    ("int_new:N", "Declare an integer variable."),
    ("int_set:Nn", "Set an integer variable."),
    ("int_incr:N", "Increment an integer variable."),
    ("int_use:N", "Use an integer variable."),
    ("int_eval:n", "Evaluate an integer expression."),
    ("seq_new:N", "Declare a sequence variable."),
    ("seq_put_right:Nn", "Append an item to a sequence."),
    ("seq_map_inline:Nn", "Map inline code over a sequence."),
    ("prop_new:N", "Declare a property list."),
    ("prop_put:Nnn", "Set a property value."),
    ("prop_get:NnN", "Read a property value."),
    ("bool_new:N", "Declare a boolean variable."),
    ("bool_set_true:N", "Set a boolean to true."),
    ("bool_if:NTF", "Branch on a boolean variable."),
    ("dim_new:N", "Declare a dimension variable."),
    ("dim_set:Nn", "Set a dimension variable."),
    ("clist_new:N", "Declare a comma list variable."),
    ("clist_map_inline:nn", "Map inline code over a comma list."),
    ("str_new:N", "Declare a string variable."),
    ("str_if_eq:nnTF", "Compare two strings."),
    ("fp_new:N", "Declare a floating point variable."),
    ("fp_eval:n", "Evaluate a floating point expression."),
    (
        "exp_args:Nno",
        "Expand selected arguments before a function call.",
    ),
];
pub(crate) fn builtin(name: &str) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, detail)| *detail)
}
pub(crate) fn completion(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<CompletionItem>> {
    let doc = ws.documents.get(uri)?;
    let context = crate::symbols::extra_symbols::entries(doc)
        .iter()
        .find(|entry| {
            entry.kind.is_expl3() && entry.span.start <= offset && offset <= entry.span.end
        })?;
    let prefix = &doc.text.source[context.span.start..offset];
    let mut names = BTreeMap::new();
    for (name, detail) in BUILTINS {
        names.insert((*name).to_string(), (*detail).to_string());
    }
    let scope = ws.symbol_scope(uri);
    for (_, doc) in ws.documents.iter().filter(|(uri, _)| scope.contains(*uri)) {
        for entry in crate::symbols::extra_symbols::entries(doc)
            .iter()
            .filter(|entry| entry.kind.is_expl3() && entry.declaration)
        {
            names.insert(entry.name.clone(), entry.detail.clone());
        }
    }
    let items: Vec<_> = names
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .take(ws.completion_budget())
        .map(|(name, detail)| CompletionItem {
            label: name.clone(),
            kind: Some(if variable(&name) {
                CompletionItemKind::VARIABLE
            } else {
                CompletionItemKind::FUNCTION
            }),
            detail: Some(detail),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: doc.text.range(context.span.start, context.span.end),
                new_text: name,
            })),
            ..Default::default()
        })
        .collect();
    (!items.is_empty() || prefix.contains(['_', ':'])).then_some(items)
}
fn parameters(name: &str) -> Option<Vec<(char, String)>> {
    let suffix = name.split_once(':')?.1;
    if suffix.is_empty() || !suffix.bytes().all(|c| b"nNcoVvexfTF".contains(&c)) {
        return None;
    }
    Some(
        suffix
            .chars()
            .enumerate()
            .map(|(i, c)| (c, format!("{c}{}", i + 1)))
            .collect(),
    )
}
fn argument_end(source: &str, start: usize, kind: char) -> Option<usize> {
    if source.as_bytes().get(start) == Some(&b'{') {
        return Some(syntax::group(source, start, b'{', b'}').map_or(source.len(), |(_, end)| end));
    }
    if source.as_bytes().get(start) == Some(&b'\\') {
        let end = word(source, start + 1, true);
        return Some(if end == start + 1 {
            end + source[end..].chars().next()?.len_utf8()
        } else {
            end
        });
    }
    let c = source[start..].chars().next()?;
    // n-type arguments also accept a single unbraced TeX token.
    let _ = kind;
    Some(start + c.len_utf8())
}
pub(crate) fn help(ws: &Workspace, uri: &Url, cursor: usize) -> Option<SignatureHelp> {
    let doc = ws.documents.get(uri)?;
    if doc.syntax.is_ignored(cursor.saturating_sub(1)) {
        return None;
    }
    let scope = ws.symbol_scope(uri);
    let known: BTreeSet<_> = ws
        .documents
        .iter()
        .filter(|(uri, _)| scope.contains(*uri))
        .flat_map(|(_, doc)| {
            crate::symbols::extra_symbols::entries(doc)
                .iter()
                .filter(|entry| {
                    entry.kind == Kind::Expl3Command
                        && entry.declaration
                        && !entry.detail.contains("unsupported parameter text")
                })
                .map(|entry| entry.name.as_str())
        })
        .collect();
    for entry in crate::symbols::extra_symbols::entries(doc)
        .iter()
        .rev()
        .filter(|entry| {
            entry.kind == Kind::Expl3Command
                && !entry.declaration
                && entry.span.end <= cursor
                && (known.contains(entry.name.as_str()) || builtin(&entry.name).is_some())
        })
    {
        let Some(parameters) = parameters(&entry.name) else {
            continue;
        };
        let mut at = entry.span.end;
        for (index, (kind, _)) in parameters.iter().enumerate() {
            let start = syntax::skip_space(&doc.text.source, at);
            let end = argument_end(&doc.text.source, start, *kind).unwrap_or(start);
            if cursor <= start || cursor < end || end == doc.text.source.len() && cursor == end {
                let label = format!(
                    "\\{} {}",
                    entry.name,
                    parameters
                        .iter()
                        .map(|(_, label)| format!("{{{label}}}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                return Some(SignatureHelp { signatures:vec![SignatureInformation {label,documentation:Some(Documentation::String(builtin(&entry.name).unwrap_or("Project-defined expl3 function; argument types follow the name suffix.").into())),parameters:Some(parameters.iter().map(|(_,label)|ParameterInformation {label:ParameterLabel::Simple(label.clone()),documentation:None}).collect()),active_parameter:Some(index as u32)}],active_signature:Some(0),active_parameter:Some(index as u32) });
            }
            at = end;
        }
    }
    None
}
