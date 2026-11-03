//! Search bibliography previews without changing the inserted citation key.
use crate::{
    parser::syntax,
    project::workspace::{Document, DocumentKind, Workspace},
};
use lsp_types::*;
use std::{collections::BTreeSet, ops::Range};

pub(crate) fn score(name: &str, metadata: &str, query: &str) -> Option<u8> {
    if name.starts_with(query) {
        return Some(0);
    }
    let query = query.to_lowercase();
    let name = name.to_lowercase();
    if name.starts_with(&query) {
        return Some(1);
    }
    if name.contains(&query) {
        return Some(2);
    }
    let words: Vec<_> = query.split_whitespace().collect();
    let metadata = metadata.to_lowercase();
    if !words.is_empty()
        && words
            .iter()
            .all(|word| name.contains(word) || metadata.contains(word))
    {
        return Some(3);
    }
    let mut chars = name.chars();
    if query.chars().count() >= 3
        && query
            .chars()
            .all(|ch| chars.by_ref().any(|candidate| candidate == ch))
    {
        return Some(4);
    }
    if query.chars().count() >= 4
        && crate::editing::spelling_fixes::distance(&name, &query, 2).is_some()
    {
        return Some(5);
    }
    None
}

pub(crate) fn search_span(
    source: &str,
    command: &syntax::Command,
    offset: usize,
) -> Option<Range<usize>> {
    let args: Vec<_> = if let Some(style) = crate::bibliography::citations::style(&command.name) {
        crate::bibliography::citations::arguments(source, command.span.end, style)
            .fields
            .into_iter()
            .filter(|field| field.role == crate::bibliography::citations::Role::Keys)
            .map(|field| field.argument)
            .collect()
    } else {
        let argument = command.argument.clone().or_else(|| {
            let start = syntax::citation_argument_start(source, &command.name, command.span.end)?;
            (source.as_bytes().get(start) == Some(&b'{')).then(|| syntax::Argument {
                value: source[start + 1..].into(),
                span: start + 1..source.len(),
                outer_span: start..source.len(),
            })
        });
        argument.into_iter().collect()
    };
    for arg in args {
        if !(arg.span.start <= offset && offset <= arg.span.end) {
            continue;
        }
        let multiple = !syntax::is_single_citation(&command.name);
        let segment = if multiple {
            syntax::key_list_segments(source, arg.span.clone())
                .into_iter()
                .find(|span| span.start <= offset && offset <= span.end)?
        } else {
            arg.span
        };
        // Comments and expansions retain the established literal-key editor.
        if source[segment.clone()].contains(['%', '\\', '{', '}']) {
            return None;
        }
        let mut span = syntax::trimmed_span(source, segment);
        span.start = span.start.min(offset);
        span.end = span.end.max(offset);
        return Some(span);
    }
    None
}

pub(crate) fn items(
    ws: &Workspace,
    doc: &Document,
    offset: usize,
    span: Range<usize>,
    scope: &BTreeSet<Url>,
    unique: bool,
) -> Option<Vec<CompletionItem>> {
    items_with_query(ws, doc, offset, span, scope, unique, false)
}

fn items_with_query(
    ws: &Workspace,
    doc: &Document,
    offset: usize,
    span: Range<usize>,
    scope: &BTreeSet<Url>,
    unique: bool,
    all: bool,
) -> Option<Vec<CompletionItem>> {
    let mut ambiguous = BTreeSet::new();
    if unique {
        let mut names = BTreeSet::new();
        for (uri, doc) in &ws.documents {
            if !scope.contains(uri) {
                continue;
            }
            for entry in doc
                .syntax
                .citations
                .iter()
                .chain(&doc.syntax.bibliography_aliases)
            {
                if !names.insert(&entry.name) {
                    ambiguous.insert(entry.name.clone());
                }
            }
        }
    }
    candidate_items_with_query(
        ws,
        doc,
        offset,
        span,
        ws.documents
            .iter()
            .filter(|(uri, _)| scope.contains(*uri))
            .flat_map(|(uri, doc)| {
                doc.syntax
                    .citations
                    .iter()
                    .chain(&doc.syntax.bibliography_aliases)
                    .filter(|entry| !ambiguous.contains(&entry.name))
                    .map(move |entry| (uri, entry.name.as_str(), entry.detail.as_str()))
            }),
        ws.completion_budget(),
        all,
    )
}
pub(crate) fn candidate_items<'a>(
    ws: &Workspace,
    doc: &Document,
    offset: usize,
    span: Range<usize>,
    candidates: impl IntoIterator<Item = (&'a Url, &'a str, &'a str)>,
    limit: usize,
) -> Option<Vec<CompletionItem>> {
    candidate_items_with_query(ws, doc, offset, span, candidates, limit, false)
}

#[allow(clippy::too_many_arguments)]
fn candidate_items_with_query<'a>(
    ws: &Workspace,
    doc: &Document,
    offset: usize,
    span: Range<usize>,
    candidates: impl IntoIterator<Item = (&'a Url, &'a str, &'a str)>,
    limit: usize,
    all: bool,
) -> Option<Vec<CompletionItem>> {
    let source = &doc.text.source;
    let query = if all {
        ""
    } else {
        source[span.start..offset].trim()
    };
    if query.chars().count() > 128 {
        return Some(vec![]);
    }
    let (range, extra) = syntax::name_completion_ranges(&doc.text, span, offset)?;
    let main_start = doc.text.offset(range.start)?;
    let filter_prefix = &source[main_start..offset];
    let mut ranked = std::collections::BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (entry_uri, name, detail) in candidates {
        if !syntax::is_static_citation_key(name) || !seen.insert(name.to_string()) {
            continue;
        }
        let Some(rank) = score(name, detail, query) else {
            continue;
        };
        ranked.insert((rank, name), (entry_uri, detail));
        if ranked.len() > limit {
            ranked.pop_last();
        }
    }
    Some(
        ranked
            .into_iter()
            .map(|((rank, name), (entry_uri, detail))| {
                let documentation =
                    super::reference_preview::documentation(ws, entry_uri, name, detail);
                CompletionItem {
                    label: name.into(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: documentation.is_none().then(|| detail.into()),
                    documentation,
                    filter_text: Some(format!("{filter_prefix} {} {}", name, detail)),
                    sort_text: Some(format!("{rank:02}{}", name)),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range,
                        new_text: name.into(),
                    })),
                    additional_text_edits: (!extra.is_empty()).then(|| extra.clone()),
                    ..Default::default()
                }
            })
            .collect(),
    )
}

pub(crate) fn completion(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
    scope: &BTreeSet<Url>,
) -> Option<Vec<CompletionItem>> {
    completion_with_query(ws, uri, offset, scope, false)
}

pub(crate) fn completion_unfiltered(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
    scope: &BTreeSet<Url>,
) -> Option<Vec<CompletionItem>> {
    completion_with_query(ws, uri, offset, scope, true)
}

fn completion_with_query(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
    scope: &BTreeSet<Url>,
    all: bool,
) -> Option<Vec<CompletionItem>> {
    let doc = ws.documents.get(uri)?;
    if doc.kind != DocumentKind::Latex {
        return None;
    }
    for command in doc.syntax.commands.iter().rev().filter(|command| {
        syntax::is_citation(&command.name)
            && !syntax::is_single_citation(&command.name)
            && !doc.syntax.control_operands.contains(&command.span.start)
    }) {
        let Some(span) = search_span(&doc.text.source, command, offset) else {
            continue;
        };
        if crate::symbols::named_references::completion_context(ws, uri, offset, |candidate| {
            (candidate.span == command.span).then_some(span.start..offset)
        })
        .is_none()
        {
            return Some(Vec::new());
        }
        return items_with_query(ws, doc, offset, span, scope, false, all);
    }
    None
}
