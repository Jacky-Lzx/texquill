//! Bounded hook fields with meanings shared by every consuming project.
use crate::{
    project::workspace::{Document, DocumentKind, Workspace},
    symbols::commands::CommandSignatures,
};
use lsp_types::Url;
use std::collections::BTreeMap;

/// A parsed field whose enclosing invocation still needs operand validation.
pub(super) struct Candidate<E> {
    pub entry: E,
    pub invocation: usize,
    /// An adjacent setting must also be an invocation, rather than data.
    pub prerequisite: Option<usize>,
}

pub(super) fn index<E: Clone + Eq>(
    ws: &Workspace,
    origin: &Url,
    offset: usize,
    roots: &[&str],
    read: impl Fn(&Document, &CommandSignatures<'_>) -> BTreeMap<usize, Candidate<E>>,
    selected: impl Fn(&E) -> bool,
) -> BTreeMap<Url, BTreeMap<usize, E>> {
    let Some(doc) = ws.documents.get(origin) else {
        return BTreeMap::new();
    };
    let before = doc
        .syntax
        .commands
        .partition_point(|command| command.span.start < offset);
    if before > 0 && offset < doc.syntax.commands[before - 1].span.end {
        // Static hook fields cannot contain a control word. Reject a cursor
        // inside its spelling before constructing any project interpretation;
        // named calls and forwarders still use the full analysis in fields.
        return BTreeMap::new();
    }
    if !ws.index_truncated.is_empty()
        || !ws.documents.values().any(|doc| {
            doc.kind == DocumentKind::Latex
                && roots.iter().any(|name| doc.text.source.contains(name))
        })
    {
        return BTreeMap::new();
    }
    let contexts = crate::symbols::environment_calls::project_contexts(ws);
    let mut connected = ws.symbol_scope(origin);
    loop {
        let before = connected.len();
        for scope in contexts.keys() {
            if !scope.is_disjoint(&connected) {
                connected.extend(scope.iter().cloned());
            }
        }
        if before == connected.len() {
            break;
        }
    }
    let mut interpretations = BTreeMap::<Url, Vec<BTreeMap<usize, E>>>::new();
    let (mut bytes, mut commands, mut definitions, mut entries) = (0usize, 0usize, 0usize, 0usize);
    for (scope, consumers) in contexts {
        if scope.is_disjoint(&connected) {
            continue;
        }
        let docs: Vec<_> = scope
            .iter()
            .filter_map(|uri| ws.documents.get(uri))
            .filter(|doc| doc.kind == DocumentKind::Latex)
            .collect();
        bytes = bytes.saturating_add(docs.iter().map(|doc| doc.text.source.len()).sum::<usize>());
        commands = commands.saturating_add(
            docs.iter()
                .map(|doc| doc.syntax.commands.len())
                .sum::<usize>(),
        );
        definitions = definitions.saturating_add(
            docs.iter()
                .map(|doc| doc.syntax.definitions.len() + doc.syntax.environments.len())
                .sum::<usize>(),
        );
        if bytes > 32 * 1024 * 1024 || commands > 100_000 || definitions > 8192 {
            return BTreeMap::new();
        }
        let signatures = super::query_cache::project_signatures(ws, &scope);
        // All callers need an entry at this cursor. Inspect its document
        // first so an ordinary label or command query does not index hook
        // fields in every other source. A shared source must still match in
        // each consumer interpretation before any result can be returned.
        for uri in consumers
            .iter()
            .filter(|uri| *uri == origin)
            .chain(consumers.iter().filter(|uri| *uri != origin))
        {
            let Some(doc) = ws
                .documents
                .get(uri)
                .filter(|doc| doc.kind == DocumentKind::Latex)
            else {
                continue;
            };
            let candidates = read(doc, &signatures);
            if uri == origin && !candidates.values().any(|call| selected(&call.entry)) {
                return BTreeMap::new();
            }
            // Ordinary queries need no operand table when their cursor does
            // not select a hook field. Candidates that survive this check
            // still undergo the same invocation-role validation as before.
            let calls = if candidates.is_empty() {
                BTreeMap::new()
            } else {
                let context = super::named_references::Context::new(doc, &signatures);
                candidates
                    .into_iter()
                    .filter(|(_, call)| {
                        !context.operand(doc, call.invocation)
                            && call.prerequisite.is_none_or(|at| !context.operand(doc, at))
                    })
                    .map(|(at, call)| (at, call.entry))
                    .collect()
            };
            if uri == origin && !calls.values().any(&selected) {
                return BTreeMap::new();
            }
            entries = entries.saturating_add(calls.len());
            if entries > 100_000 {
                return BTreeMap::new();
            }
            interpretations.entry(uri.clone()).or_default().push(calls);
        }
    }
    let mut result = BTreeMap::new();
    for (uri, interpretations) in interpretations {
        let Some(first) = interpretations.first() else {
            continue;
        };
        let entries = first
            .iter()
            .filter(|(at, entry)| {
                interpretations
                    .iter()
                    .all(|other| other.get(at) == Some(entry))
            })
            .map(|(at, entry)| (*at, entry.clone()))
            .collect();
        result.insert(uri, entries);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_control_words_do_not_construct_hook_interpretations() {
        let uri = Url::parse("untitled:ordinary.tex").unwrap();
        let source = r"\AtBeginDocument{Code}\maketitle";
        let mut ws = Workspace::default();
        ws.documents
            .insert(uri.clone(), Document::new(&uri, source.into(), Some(1)));
        let entries = index::<()>(
            &ws,
            &uri,
            source.find("maketitle").unwrap() + 2,
            &["AtBeginDocument"],
            |_, _| panic!("ordinary command entered hook analysis"),
            |_| true,
        );
        assert!(entries.is_empty());
    }
}
