//! Ordered source traversal for environments spanning literal input calls.
use crate::{
    parser::syntax::{self, QuickFix},
    project::workspace::{resolve_file_path, DocumentKind, Workspace},
    symbols::commands::{definition_body_spans, CommandSignatures, EnvironmentCall},
};
use lsp_types::{DiagnosticRelatedInformation, Location, Url};
use std::collections::{BTreeMap, BTreeSet};

const MAX_VISITS: usize = 4096;
const MAX_EVENTS: usize = 65536;
const MAX_DEPTH: usize = 64;

#[derive(Clone)]
enum Operation {
    Environment(EnvironmentCall),
    Input(Option<Url>, bool),
    LinkedBarrier,
    UnverifiedRaw,
    UncertainName(String),
    Uncertain,
    Stop,
}
#[derive(Clone, Default)]
struct Source {
    operations: Vec<(usize, Operation)>,
}
#[derive(Clone)]
struct Event {
    uri: Url,
    occurrence: usize,
    call: EnvironmentCall,
}
struct LocatedFix {
    uri: Url,
    occurrence: usize,
    fix: QuickFix,
}
struct Walk<'a> {
    workspace: &'a Workspace,
    root: &'a Url,
    sources: BTreeMap<Url, Source>,
    active: BTreeSet<Url>,
    packages: BTreeSet<Url>,
    occurrences: BTreeMap<Url, usize>,
    uncertain_names: BTreeSet<String>,
    events: Vec<Event>,
    linked_barriers: BTreeSet<usize>,
    linked_complete: bool,
    complete: bool,
    has_inputs: bool,
    visits: usize,
}

impl<'a> Walk<'a> {
    fn new(
        workspace: &'a Workspace,
        root: &'a Url,
        scope: &BTreeSet<Url>,
        linked_editing: bool,
    ) -> Self {
        let inputs = crate::project::source_inputs::Index::new(workspace, root);
        let documents: Vec<_> = workspace
            .documents
            .iter()
            .filter(|(uri, doc)| scope.contains(*uri) && doc.kind == DocumentKind::Latex)
            .map(|(_, doc)| (doc.text.source.as_str(), &doc.syntax))
            .collect();
        let signatures = CommandSignatures::new(documents.iter().copied());
        let context =
            CommandSignatures::source_context_with(documents.iter().copied(), &signatures);
        Self {
            workspace,
            root,
            sources: scope
                .iter()
                .filter(|uri| {
                    workspace
                        .documents
                        .get(uri)
                        .is_some_and(|doc| doc.kind == DocumentKind::Latex)
                })
                .map(|uri| {
                    (
                        uri.clone(),
                        source(
                            workspace,
                            root,
                            uri,
                            &signatures,
                            &context,
                            &inputs,
                            linked_editing,
                        ),
                    )
                })
                .collect(),
            active: BTreeSet::new(),
            packages: BTreeSet::new(),
            occurrences: BTreeMap::new(),
            uncertain_names: BTreeSet::new(),
            events: vec![],
            linked_barriers: BTreeSet::new(),
            linked_complete: true,
            complete: signatures.environment_primitives_unchanged() && !inputs.incomplete,
            has_inputs: inputs.incomplete,
            visits: 0,
        }
    }

    fn visit(&mut self, uri: &Url, depth: usize) {
        if depth > MAX_DEPTH || self.visits >= MAX_VISITS || !self.active.insert(uri.clone()) {
            self.complete = false;
            return;
        }
        let Some(source) = self.sources.get(uri).cloned() else {
            self.complete = false;
            self.active.remove(uri);
            return;
        };
        self.visits += 1;
        let count = self.occurrences.entry(uri.clone()).or_default();
        let occurrence = *count;
        *count += 1;
        for (_, operation) in source.operations {
            if self.events.len() >= MAX_EVENTS {
                self.complete = false;
                break;
            }
            match operation {
                Operation::Environment(call) => self.events.push(Event {
                    uri: uri.clone(),
                    occurrence,
                    call,
                }),
                Operation::Input(target, package) => {
                    if !package {
                        self.has_inputs = true;
                    }
                    match target {
                        Some(target) if !package || self.packages.insert(target.clone()) => {
                            self.visit(&target, depth + 1)
                        }
                        Some(_) => {}
                        None if package => {}
                        None => self.complete = false,
                    }
                }
                Operation::Uncertain => self.complete = false,
                Operation::LinkedBarrier => {
                    self.linked_barriers.insert(self.events.len());
                }
                Operation::UnverifiedRaw => self.linked_complete = false,
                Operation::UncertainName(name) => {
                    self.uncertain_names.insert(name);
                }
                Operation::Stop => {
                    if uri == self.root {
                        self.complete = false;
                    }
                    break;
                }
            }
        }
        self.active.remove(uri);
    }

    fn linked_pairs(&self, uri: &Url) -> Vec<NamePair> {
        if !self.complete || !self.linked_complete {
            return vec![];
        }
        let mut stack: Vec<&Event> = vec![];
        let mut pairs: BTreeMap<NamePair, BTreeSet<usize>> = BTreeMap::new();
        for (index, event) in self.events.iter().enumerate() {
            if self.linked_barriers.contains(&index)
                || self.uncertain_names.contains(&event.call.argument.value)
            {
                stack.clear();
            }
            if self.uncertain_names.contains(&event.call.argument.value) {
                continue;
            }
            if event.call.begin {
                stack.push(event);
                continue;
            }
            let Some(open) = stack.pop() else {
                continue;
            };
            if open.call.argument.value != event.call.argument.value {
                stack.clear();
                continue;
            }
            if open.uri != *uri
                || event.uri != *uri
                || open.occurrence != event.occurrence
                // Linked editing cannot insert braces around a bare token.
                || open.call.argument.span == open.call.argument.outer_span
                || event.call.argument.span == event.call.argument.outer_span
            {
                continue;
            }
            pairs
                .entry([
                    (open.call.argument.span.start, open.call.argument.span.end),
                    (event.call.argument.span.start, event.call.argument.span.end),
                ])
                .or_default()
                .insert(event.occurrence);
        }
        pairs
            .into_iter()
            .filter(|(_, occurrences)| occurrences.len() == self.occurrences[uri])
            .map(|(pair, _)| pair)
            .collect()
    }

    fn pair(&self) -> BTreeMap<Url, Vec<QuickFix>> {
        if !self.complete {
            return BTreeMap::new();
        }
        let mut stack: Vec<&Event> = Vec::new();
        let mut issues = Vec::new();
        for event in &self.events {
            let name = &event.call.argument.value;
            if self.uncertain_names.contains(name) || syntax::is_verbatim_environment(name) {
                continue;
            }
            if event.call.begin {
                stack.push(event);
            } else if let Some(index) = stack
                .iter()
                .rposition(|open| open.call.argument.value == *name)
            {
                let missing = stack.split_off(index + 1);
                if !missing.is_empty() {
                    let doc = &self.workspace.documents[&event.uri];
                    let replacement: String = missing
                        .iter()
                        .rev()
                        .map(|open| {
                            format!(
                                "\\end{{{}}}{}",
                                open.call.argument.value,
                                doc.text.newline()
                            )
                        })
                        .collect();
                    for open in missing {
                        let missing_name = &open.call.argument.value;
                        let same_source =
                            open.uri == event.uri && open.occurrence == event.occurrence;
                        let span = if same_source {
                            open.call.argument.span.clone()
                        } else {
                            event.call.argument.span.clone()
                        };
                        let mut fix = syntax::syntax_fix(
                            &doc.text,
                            span,
                            format!("Unclosed environment `{missing_name}` before \\end{{{name}}}"),
                            format!("Close environment `{missing_name}` before `{name}`"),
                            lsp_types::TextEdit {
                                range: doc.text.range(event.call.span.start, event.call.span.start),
                                new_text: replacement.clone(),
                            },
                        );
                        if !same_source {
                            self.related(&mut fix, open);
                        }
                        issues.push(LocatedFix {
                            uri: event.uri.clone(),
                            occurrence: event.occurrence,
                            fix,
                        });
                    }
                }
                stack.pop();
            } else {
                let doc = &self.workspace.documents[&event.uri];
                let fix = syntax::syntax_fix(
                    &doc.text,
                    event.call.argument.span.clone(),
                    format!("Unexpected \\end{{{name}}}"),
                    format!("Remove unexpected \\end{{{name}}}"),
                    lsp_types::TextEdit {
                        range: doc.text.range(event.call.span.start, event.call.span.end),
                        new_text: String::new(),
                    },
                );
                issues.push(LocatedFix {
                    uri: event.uri.clone(),
                    occurrence: event.occurrence,
                    fix,
                });
            }
        }
        if !stack.is_empty() {
            let doc = &self.workspace.documents[self.root];
            let end = doc.text.source.len();
            let prefix = if doc.text.source.ends_with('\n') {
                ""
            } else {
                doc.text.newline()
            };
            let replacement = format!(
                "{prefix}{}",
                stack
                    .iter()
                    .rev()
                    .map(|open| format!(
                        "\\end{{{}}}{}",
                        open.call.argument.value,
                        doc.text.newline()
                    ))
                    .collect::<String>()
            );
            for open in stack {
                let name = &open.call.argument.value;
                let span = if open.uri == *self.root {
                    open.call.argument.span.clone()
                } else {
                    end..end
                };
                let mut fix = syntax::syntax_fix(
                    &doc.text,
                    span,
                    format!("Unclosed environment `{name}`"),
                    format!("Close environment `{name}`"),
                    lsp_types::TextEdit {
                        range: doc.text.range(end, end),
                        new_text: replacement.clone(),
                    },
                );
                if open.uri != *self.root {
                    self.related(&mut fix, open);
                }
                issues.push(LocatedFix {
                    uri: self.root.clone(),
                    occurrence: 0,
                    fix,
                });
            }
        }
        let mut result: BTreeMap<Url, Vec<QuickFix>> = BTreeMap::new();
        for issue in &issues {
            // An edit of a repeated source must be valid in every execution.
            let agreeing: BTreeSet<_> = issues
                .iter()
                .filter(|other| other.uri == issue.uri && same_fix(&other.fix, &issue.fix))
                .map(|other| other.occurrence)
                .collect();
            if agreeing.len() != self.occurrences[&issue.uri] {
                continue;
            }
            let fixes = result.entry(issue.uri.clone()).or_default();
            if !fixes.iter().any(|fix| same_fix(fix, &issue.fix)) {
                fixes.push(issue.fix.clone());
            }
        }
        result
    }

    fn related(&self, fix: &mut QuickFix, open: &Event) {
        let text = &self.workspace.documents[&open.uri].text;
        fix.diagnostic.related_information = Some(vec![DiagnosticRelatedInformation {
            location: Location::new(
                open.uri.clone(),
                text.range(open.call.argument.span.start, open.call.argument.span.end),
            ),
            message: format!("Environment `{}` starts here", open.call.argument.value),
        }]);
    }
}

fn source(
    workspace: &Workspace,
    root: &Url,
    uri: &Url,
    signatures: &CommandSignatures<'_>,
    context: &CommandSignatures<'_>,
    inputs: &crate::project::source_inputs::Index,
    linked_editing: bool,
) -> Source {
    let doc = &workspace.documents[uri];
    let mut result = Source::default();
    let mut boundaries =
        signatures.environment_boundaries_with_context(&doc.text.source, &doc.syntax, context);
    let bodies = definition_body_spans(&doc.text.source, &doc.syntax);
    // A balanced replacement body (e.g. a tabular cell helper) cannot consume
    // an enclosing opener. Keep its stored delimiters uneditable, but do not
    // make every executed environment with the same name uncertain.
    let balanced_bodies: Vec<_> = if linked_editing {
        bodies
            .iter()
            .filter(|body| {
                boundaries
                    .iter()
                    .any(|boundary| body.contains(&boundary.span.start))
            })
            .filter(|body| {
                if bodies
                    .iter()
                    .any(|inner| body.start < inner.start && inner.start < body.end)
                {
                    return false;
                }
                let mut stack = Vec::new();
                for boundary in boundaries
                    .iter()
                    .filter(|boundary| body.contains(&boundary.span.start))
                {
                    let Some(argument) = &boundary.argument else {
                        return false;
                    };
                    if boundary.operand
                        || !boundary.direct
                        || syntax::is_verbatim_environment(&argument.value)
                    {
                        return false;
                    }
                    if boundary.begin {
                        stack.push(&argument.value);
                    } else if stack.pop() != Some(&argument.value) {
                        return false;
                    }
                }
                stack.is_empty()
                    && !doc.syntax.commands.iter().any(|command| {
                        body.contains(&command.span.start)
                            && (command.name.starts_with("if")
                                || syntax::loader_extension(&command.name).is_some()
                                || matches!(
                                    command.name.as_str(),
                                    "else"
                                        | "fi"
                                        | "IfFileExists"
                                        | "InputIfFileExists"
                                        | "input"
                                        | "include"
                                        | "endinput"
                                ))
                    })
            })
            .collect()
    } else {
        vec![]
    };
    if linked_editing {
        // A change-word operation briefly leaves both linked names empty.
        // Preserve their zero-width insertion points without making empty
        // names declarations, references or diagnostic environment calls.
        for boundary in &mut boundaries {
            if boundary.argument.is_none() {
                let at = syntax::skip_space(&doc.text.source, boundary.span.end);
                if let Some((argument, end)) = syntax::group(&doc.text.source, at, b'{', b'}')
                    .filter(|(argument, _)| argument.span.is_empty())
                {
                    boundary.argument = Some(argument);
                    boundary.span.end = end;
                }
            }
        }
    }
    result.operations.extend(
        boundaries
            .iter()
            .filter(|call| call.argument.is_none() && !call.operand && !call.deferred)
            .map(|call| (call.span.start, Operation::LinkedBarrier)),
    );
    let calls: Vec<_> = boundaries
        .into_iter()
        .filter_map(|call| call.into_call())
        .collect();
    result.operations.extend(
        calls
            .iter()
            .filter(|call| {
                (call.operand || call.deferred)
                    && !balanced_bodies
                        .iter()
                        .any(|body| body.contains(&call.span.start))
            })
            .map(|call| {
                (
                    call.span.start,
                    Operation::UncertainName(call.argument.value.clone()),
                )
            }),
    );
    for call in calls
        .into_iter()
        .filter(|call| !call.operand && !call.deferred)
    {
        if call.begin && syntax::is_verbatim_environment(&call.argument.value) {
            // Only the parser's actual raw region proves a literal terminator.
            let ending = format!("\\end{{{}}}", call.argument.value);
            // VerbEnv can recover after consuming a code/closing line without
            // closing its group. A marker in that data is not a terminator.
            let verb_environment_end = (call.argument.value == "VerbEnv").then(|| {
                crate::parser::fancyvrb::verb_environment_end(
                    &doc.text.source,
                    call.argument.outer_span.end,
                )
                .closing
            });
            let terminator = doc.syntax.ignored.iter().find_map(|raw| {
                let start = raw.end.checked_sub(ending.len())?;
                (call.direct
                    && raw.start == call.argument.outer_span.end
                    && start >= raw.start
                    && verb_environment_end.is_none_or(|end| end == Some(raw.end))
                    && doc.text.source.get(start..raw.end) == Some(ending.as_str()))
                .then_some((start, raw.end))
            });
            if let Some((start, end)) = terminator {
                result.operations.push((
                    start,
                    Operation::Environment(EnvironmentCall {
                        argument: syntax::Argument {
                            value: call.argument.value.clone(),
                            span: start + "\\end{".len()..end - 1,
                            outer_span: start + "\\end".len()..end,
                        },
                        span: start..end,
                        begin: false,
                        direct: true,
                        operand: false,
                        deferred: false,
                    }),
                ));
            } else {
                // An indirect raw opener does not make the parser ignore its body.
                result
                    .operations
                    .push((call.span.start, Operation::UnverifiedRaw));
            }
        }
        result
            .operations
            .push((call.span.start, Operation::Environment(call)));
    }
    for call in inputs.calls.get(uri).into_iter().flatten() {
        let target = crate::project::source_inputs::target(uri, root, call).filter(|target| {
            workspace
                .documents
                .get(target)
                .is_some_and(|doc| doc.kind == DocumentKind::Latex)
        });
        result
            .operations
            .push((call.command.span.start, Operation::Input(target, false)));
    }
    let operands = signatures.source_argument_tokens(&doc.text.source, &doc.syntax);
    let indirect: Vec<_> = [&inputs.loaders, &inputs.checks, &inputs.selectors]
        .into_iter()
        .flat_map(|calls| calls.get(uri).into_iter().flatten())
        .collect();
    result.operations.extend(
        indirect
            .iter()
            .filter(|call| !call.certain)
            .map(|call| (call.command.span.start, Operation::Uncertain)),
    );
    for command in doc.syntax.commands.iter().chain(
        indirect
            .into_iter()
            .filter(|call| call.certain)
            .map(|call| &call.command),
    ) {
        if operands.contains(&command.span.start)
            || bodies.iter().any(|span| span.contains(&command.span.start))
        {
            continue;
        }
        let package = syntax::loader_extension(&command.name);
        let input = matches!(command.name.as_str(), "input" | "include");
        if !input && package.is_none() {
            if command.name == "endinput" {
                // TeX finishes the current input line before ending the file.
                // Do not infer a partial stack if that line still executes
                // tokens, or if endinput no longer has its primitive meaning.
                let remainder = doc.text.source[command.span.end..]
                    .split(['\r', '\n'])
                    .next()
                    .unwrap_or_default()
                    .trim_start();
                let operation = if signatures.command_unchanged("endinput")
                    && (remainder.is_empty() || remainder.starts_with('%'))
                {
                    Operation::Stop
                } else {
                    Operation::Uncertain
                };
                result.operations.push((command.span.start, operation));
                continue;
            }
            if matches!(command.name.as_str(), "InputIfFileExists" | "subfile") {
                result
                    .operations
                    .push((command.span.start, Operation::Input(None, false)));
                continue;
            }
            if command.name.starts_with("if")
                || matches!(
                    command.name.as_str(),
                    "else"
                        | "fi"
                        | "IfFileExists"
                        | "InputIfFileExists"
                        | "includeonly"
                        | "endinput"
                        | "subfile"
                )
            {
                // Never link across a conditional edge. Independent pairs
                // after (or wholly within) a branch can still be certified.
                let operation = if linked_editing
                    && (command.name.starts_with("if")
                        || matches!(command.name.as_str(), "else" | "fi"))
                {
                    Operation::LinkedBarrier
                } else {
                    Operation::Uncertain
                };
                result.operations.push((command.span.start, operation));
            }
            continue;
        }
        if !signatures.command_unchanged(&command.name) {
            result.operations.push((
                command.span.start,
                if input {
                    Operation::Input(None, false)
                } else {
                    Operation::Uncertain
                },
            ));
            continue;
        }
        let Some(argument) = syntax::file_argument(&doc.text.source, command) else {
            result.operations.push((
                command.span.start,
                if input {
                    Operation::Input(None, false)
                } else {
                    Operation::Uncertain
                },
            ));
            continue;
        };
        let values: Vec<_> = if package == Some("sty") {
            argument.value.split(',').map(str::trim).collect()
        } else {
            vec![argument.value.as_str()]
        };
        for value in values {
            let target = syntax::is_literal_file_path(value)
                .then(|| {
                    resolve_file_path(root, &command.name, value, &[package.unwrap_or("tex")])
                        .or_else(|| {
                            resolve_file_path(
                                uri,
                                &command.name,
                                value,
                                &[package.unwrap_or("tex")],
                            )
                        })
                        .filter(|target| {
                            workspace
                                .documents
                                .get(target)
                                .is_some_and(|doc| doc.kind == DocumentKind::Latex)
                        })
                })
                .flatten();
            if package.is_some() && !syntax::is_literal_file_path(value) {
                result
                    .operations
                    .push((command.span.start, Operation::Uncertain));
            } else {
                result.operations.push((
                    command.span.start,
                    Operation::Input(target, package.is_some()),
                ));
            }
        }
    }
    result.operations.sort_by_key(|(start, _)| *start);
    result
}

type Interpretations = Vec<(bool, Vec<QuickFix>)>;
type NamePair = [(usize, usize); 2];

/// Without project context, no pair may cross a source-loading boundary.
pub(crate) fn local_linked_pairs(workspace: &Workspace, uri: &Url) -> Vec<NamePair> {
    let mut walk = Walk::new(workspace, uri, &BTreeSet::from([uri.clone()]), true);
    for source in walk.sources.values_mut() {
        for (_, operation) in &mut source.operations {
            if matches!(operation, Operation::Input(..)) {
                *operation = Operation::LinkedBarrier;
            }
        }
    }
    walk.visit(uri, 0);
    walk.linked_pairs(uri)
}

/// Every execution of the file, and every consuming project, must agree.
pub(crate) fn linked_pairs(workspace: &Workspace, uri: &Url) -> Vec<NamePair> {
    let mut projects = workspace.saved_projects();
    projects.retain(|_, scope| scope.contains(uri));
    if projects.is_empty() {
        let root = workspace
            .root_document(uri)
            .ok()
            .unwrap_or_else(|| uri.clone());
        projects.insert(root, workspace.symbol_scope(uri));
    }
    let mut agreed: Option<Vec<NamePair>> = None;
    for (root, scope) in projects {
        let mut walk = Walk::new(workspace, &root, &scope, true);
        walk.visit(&root, 0);
        let pairs = walk.linked_pairs(uri);
        if let Some(agreed) = &mut agreed {
            agreed.retain(|pair| pairs.contains(pair));
        } else {
            agreed = Some(pairs);
        }
    }
    agreed.unwrap_or_default()
}

pub(crate) fn fixes(
    workspace: &Workspace,
    requested: &BTreeSet<Url>,
) -> BTreeMap<Url, Vec<QuickFix>> {
    let mut interpretations: BTreeMap<Url, Interpretations> = BTreeMap::new();
    for (root, scope) in workspace.saved_projects() {
        if scope.is_disjoint(requested) {
            continue;
        }
        let mut walk = Walk::new(workspace, &root, &scope, false);
        walk.visit(&root, 0);
        let mut found = walk.pair();
        for uri in scope.intersection(requested) {
            interpretations
                .entry(uri.clone())
                .or_default()
                .push((walk.has_inputs, found.remove(uri).unwrap_or_default()));
        }
    }
    let mut result = BTreeMap::new();
    for (uri, mut contexts) in interpretations {
        if !contexts.iter().any(|(inputs, _)| *inputs) {
            continue;
        }
        let (_, mut agreed) = contexts.pop().unwrap();
        agreed.retain(|fix| {
            contexts
                .iter()
                .all(|(_, fixes)| fixes.iter().any(|other| same_fix(fix, other)))
        });
        result.insert(uri, agreed);
    }
    result
}

fn same_fix(a: &QuickFix, b: &QuickFix) -> bool {
    a.diagnostic == b.diagnostic && a.edit == b.edit && a.additional_edits == b.additional_edits
}
