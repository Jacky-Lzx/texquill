//! Citation candidates at every numeric execution of a pending edit.
use crate::{
    bibliography::{bib_section_keys, bib_source_catalog::Catalog},
    project::workspace::{DocumentKind, Workspace},
};
use lsp_types::*;
struct Context {
    span: std::ops::Range<usize>,
    removals: Vec<std::ops::Range<usize>>,
    candidates:
        std::collections::BTreeMap<String, crate::bibliography::bib_source_catalog::Candidate>,
}
fn context(ws: &Workspace, uri: &Url, offset: usize) -> Option<Option<Context>> {
    let doc = ws
        .documents
        .get(uri)
        .filter(|doc| doc.kind == DocumentKind::Latex)?;
    if !ws.documents.values().any(|doc| {
        doc.kind == DocumentKind::Latex
            && doc.syntax.commands.iter().any(|command| {
                matches!(command.name.as_str(), "defbibentryset" | "csuse" | "csname")
            })
    }) {
        return None;
    }
    let (selected, projects) = match bib_section_keys::source_completion_projects(ws, uri, offset) {
        Some(projects) => projects,
        None if bib_section_keys::unproved_citation_completion(ws, uri, offset) => {
            return Some(None)
        }
        None => return None,
    };
    if !selected
        || !projects
            .iter()
            .any(|project| bib_section_keys::has_source_sets(&project.docs, &project.signatures))
        || !projects.iter().any(|project| {
            bib_section_keys::source_completion_candidate(doc, &project.signatures, offset)
        })
    {
        return None;
    }
    Some((|| {
        if !ws.index_truncated.is_empty() {
            return None;
        }
        let mut catalog = Catalog::default();
        let mut context = None;
        for project in &projects {
            let completion = bib_section_keys::source_completion_trace(
                ws,
                &project.root,
                &project.docs,
                &project.signatures,
                uri,
                offset,
            )?;
            let identity = (
                completion.span.clone(),
                completion.removals.clone(),
                completion.member,
                completion.nonloading,
            );
            if context
                .as_ref()
                .is_some_and(|previous| *previous != identity)
            {
                return None;
            }
            context = Some(identity);
            catalog.trace_completion(&completion).ok()?;
        }
        let (span, removals, member, _) = context?;
        let candidates = catalog
            .candidates(ws, uri, if member { "member" } else { "cite" })
            .ok()?;
        Some(Context {
            span,
            removals,
            candidates,
        })
    })())
}

pub(crate) fn completion(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<CompletionItem>> {
    let context = context(ws, uri, offset)?;
    Some(
        (|| {
            let Context {
                span,
                removals,
                candidates,
            } = context?;
            let doc = ws.documents.get(uri)?;
            let mut items = crate::bibliography::citation_search::candidate_items(
                ws,
                doc,
                offset,
                span,
                candidates.iter().map(|(name, candidate)| {
                    (&candidate.uri, name.as_str(), candidate.detail.as_str())
                }),
                ws.completion_budget(),
            )?;
            for item in &mut items {
                if !removals.is_empty() {
                    item.additional_text_edits
                        .get_or_insert_with(Vec::new)
                        .extend(removals.iter().map(|span| TextEdit {
                            range: doc.text.range(span.start, span.end),
                            new_text: String::new(),
                        }));
                }
            }
            Some(items)
        })()
        .unwrap_or_default(),
    )
}

/// Repair candidates are certified as new requests at the actual edit position.
/// They are independent of completion limits and metadata search filtering.
pub(crate) fn repairs(
    ws: &Workspace,
    uri: &Url,
    diagnostic: &Diagnostic,
) -> Vec<crate::parser::syntax::QuickFix> {
    (|| {
        let doc = ws.documents.get(uri)?;
        let start = doc.text.offset(diagnostic.range.start)?;
        let end = doc.text.offset(diagnostic.range.end)?;
        let key = &doc.text.source[start..end];
        let length = key.chars().count();
        if !(3..=128).contains(&length) || !crate::parser::syntax::is_static_citation_key(key) {
            return None;
        }
        let context = context(ws, uri, end)??;
        if context.span != (start..end)
            || context.candidates.len() > 4096
            || context.candidates.contains_key(key)
        {
            return None;
        }
        let mut ranked: Vec<_> = context
            .candidates
            .keys()
            .filter_map(|name| {
                crate::editing::spelling_fixes::distance(key, name, if length < 8 { 1 } else { 2 })
                    .map(|score| (score, name))
            })
            .collect();
        ranked.sort();
        Some(
            ranked
                .into_iter()
                .take(5)
                .map(|(_, name)| crate::parser::syntax::QuickFix {
                    title: format!("Replace with citation `{name}`"),
                    diagnostic: diagnostic.clone(),
                    edit: TextEdit {
                        range: diagnostic.range,
                        new_text: name.clone(),
                    },
                    additional_edits: context
                        .removals
                        .iter()
                        .map(|span| TextEdit {
                            range: doc.text.range(span.start, span.end),
                            new_text: String::new(),
                        })
                        .collect(),
                })
                .collect(),
        )
    })()
    .unwrap_or_default()
}
