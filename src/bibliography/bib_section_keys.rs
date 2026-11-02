//! Citation resource provenance for statically ordered reference sections.
//!
//! Citation references and renames still need the identity of every database
//! declaration and bibliography relation. Resource labels have a separate graph.
use crate::{
    bibliography::{
        bib_initializers::{
            certified_initializer, immediate_notes, initialized_call_argument,
            initialized_entrydata_spans, native_initializer, uses_initializer_forwards,
        },
        bib_section_files::Registry,
    },
    parser::syntax::{self, Command},
    project::workspace::{Document, DocumentKind, Workspace},
    symbols::commands::CommandSignatures,
    symbols::named_references::Context,
};
use lsp_types::{Hover, HoverContents, Location, Url};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::Arc,
};

#[derive(Clone, Copy)]
enum Purpose {
    Key,
    Completion,
}

const STATE: &[&str] = &[
    "newrefsection",
    "refsection",
    "endrefsection",
    "begin",
    "end",
    "addbibresource",
    "addglobalbib",
    "addsectionbib",
];
fn loader(name: &str) -> bool {
    matches!(name, "input" | "include" | "subfile") || syntax::loader_extension(name).is_some()
}
fn unsafe_control(name: &str) -> bool {
    name.starts_with("if")
        || name.starts_with("every")
        || matches!(
            name,
            "else"
                | "or"
                | "fi"
                | "unless"
                | "loop"
                | "repeat"
                | "whiledo"
                | "foreach"
                | "expandafter"
                | "noexpand"
                | "expanded"
                | "unexpanded"
                | "aftergroup"
                | "afterassignment"
                | "futurelet"
                | "let"
                | "edef"
                | "xdef"
                | "csedef"
                | "csxdef"
                | "cslet"
                | "csletcs"
                | "csundef"
                | "undef"
                | "catcode"
                | "makeatletter"
                | "globaldefs"
                | "count"
                | "countdef"
                | "advance"
                | "multiply"
                | "divide"
                | "scantokens"
                | "endlinechar"
                | "escapechar"
                | "ExplSyntaxOn"
                | "includeonly"
                | "InputIfFileExists"
                | "IfFileExists"
                | "import"
                | "subimport"
                | "bibliography"
                | "nobibliography"
        )
}

// These native predicates and initialized passive entrydata calls avoid Biber requests.
// Plain text branches cannot change section/resource state, regardless of the
// predicate result. Commands, parameter tokens and TeX-active syntax stay opaque.
fn textual_bibliography_test(
    source: &str,
    name: &str,
    end: usize,
    signatures: &CommandSignatures<'_>,
    pending: Option<&Range<usize>>,
    initialized_end: Option<usize>,
) -> bool {
    if !matches!(name, "ifentryinbib" | "ifentrycategory" | "entrydata")
        || !signatures.command_unchanged(name)
        || name == "entrydata" && initialized_end.is_none()
    {
        return false;
    }
    let style = crate::bibliography::citations::style(name).unwrap();
    let arguments = crate::bibliography::citations::arguments(source, end, style);
    let expected = match name {
        "ifentrycategory" => 4,
        "entrydata" => 2,
        _ => 3,
    };
    arguments.waiting.is_none()
        && arguments.fields.len() == expected
        && arguments.fields.iter().all(|field| {
            if !field.complete
                || name == "entrydata" && field.argument.outer_span.end > initialized_end.unwrap()
            {
                return false;
            }
            match field.role {
                crate::bibliography::citations::Role::Keys => {
                    if pending.is_some_and(|span| {
                        field.argument.span.start <= span.start
                            && span.end <= field.argument.span.end
                    }) {
                        return true;
                    }
                    let key = syntax::single_citation_argument(source, field.argument.clone());
                    key.value != "*" && syntax::is_static_citation_key(&key.value)
                }
                crate::bibliography::citations::Role::Category => {
                    syntax::is_static_reference_key(&field.argument.value)
                }
                crate::bibliography::citations::Role::Code(_) => {
                    let value = field.argument.value.as_bytes();
                    let mut at = 0;
                    while at < value.len() {
                        match value[at] {
                            b'%' => {
                                while at < value.len() && value[at] != b'\n' {
                                    at += 1;
                                }
                                continue;
                            }
                            b'\\' | b'{' | b'}' | b'#' | b'$' | b'&' | b'~' | b'^' | b'_' => {
                                return false
                            }
                            byte if byte.is_ascii_control() && !byte.is_ascii_whitespace() => {
                                return false
                            }
                            _ => at += 1,
                        }
                    }
                    true
                }
                _ => false,
            }
        })
}

#[derive(Clone)]
struct Query {
    span: Range<usize>,
    command: usize,
    call: Command,
    completion: bool,
}
fn query(
    doc: &Document,
    signatures: &CommandSignatures<'_>,
    offset: usize,
    purpose: Purpose,
) -> Option<Query> {
    let context = Context::new(doc, signatures);
    let source = &doc.text.source;
    for (index, lexical) in doc.syntax.commands.iter().enumerate() {
        if context.operand(doc, lexical.span.start) || context.literal(lexical.span.start) {
            continue;
        }
        let (name, span) = signatures
            .named_invocation(
                source,
                &doc.syntax,
                index,
                &BTreeSet::new(),
                &context.declarations,
            )
            .unwrap_or((&lexical.name, lexical.span.clone()));
        // Forwarded citation names require load-position meaning snapshots.
        if !syntax::is_citation(name) || !signatures.command_unchanged(name) {
            continue;
        }
        let command = crate::symbols::reference_calls::resolved_command(source, name, span);
        let span = match purpose {
            Purpose::Key => crate::symbols::reference_calls::keys(source, &command, false)
                .into_iter()
                .find(|(citation, span)| *citation && span.start <= offset && offset <= span.end)
                .map(|(_, span)| span),
            Purpose::Completion => completion_span(doc, &command, offset).map(|(span, _)| span),
        };
        if let Some(span) = span {
            return Some(Query {
                span,
                command: lexical.span.start,
                call: command,
                completion: matches!(purpose, Purpose::Completion),
            });
        }
    }
    None
}

fn completion_span(
    doc: &Document,
    command: &Command,
    offset: usize,
) -> Option<(Range<usize>, Vec<Range<usize>>)> {
    if !syntax::is_citation(&command.name) {
        return None;
    }
    if syntax::is_single_citation(&command.name) {
        crate::language::features::completion_name_argument(&doc.text.source, command, offset)
    } else {
        crate::bibliography::citation_search::search_span(&doc.text.source, command, offset)
            .map(|span| (span, Vec::new()))
    }
}

fn citation_candidate(
    doc: &Document,
    signatures: &CommandSignatures<'_>,
    offset: usize,
    purpose: Purpose,
) -> bool {
    let context = Context::new(doc, signatures);
    doc.syntax
        .commands
        .iter()
        .enumerate()
        .any(|(index, lexical)| {
            if context.literal(lexical.span.start) {
                return false;
            }
            let command = crate::symbols::reference_calls::named_command(
                &doc.text.source,
                &doc.syntax,
                index,
                signatures,
                &BTreeSet::new(),
                &context.declarations,
            )
            .or_else(|| {
                syntax::is_citation(&lexical.name).then(|| {
                    crate::symbols::reference_calls::resolved_command(
                        &doc.text.source,
                        &lexical.name,
                        lexical.span.clone(),
                    )
                })
            });
            command.is_some_and(|command| match purpose {
                Purpose::Key => {
                    crate::symbols::reference_calls::keys(&doc.text.source, &command, false)
                        .iter()
                        .any(|(citation, span)| {
                            *citation && span.start <= offset && offset <= span.end
                        })
                }
                Purpose::Completion => completion_span(doc, &command, offset).is_some(),
            })
        })
}

#[derive(Clone)]
pub(crate) struct ResourceChoice {
    pub uri: Url,
    pub span: Range<usize>,
    pub name: String,
}

pub(crate) struct LabelGraph {
    pub bindings: BTreeMap<String, crate::bibliography::bib_section_files::LabelBinding>,
    pub names: BTreeSet<String>,
    pub choices: Vec<ResourceChoice>,
}

struct LabelProof {
    command: Option<usize>,
    seen: bool,
    aliases: BTreeMap<String, String>,
    environments: Vec<String>,
    choices: Option<Vec<ResourceChoice>>,
}

struct Analysis<'a, 'b> {
    ws: &'a Workspace,
    root: &'a Url,
    signatures: &'b CommandSignatures<'a>,
    registry: Registry,
    origin: &'a Url,
    query: Option<Query>,
    // Label lookup proves execution without resolving unfinished selections.
    labels: Option<LabelProof>,
    resource_sets: BTreeSet<Arc<BTreeSet<Url>>>,
    alias_sections: Vec<Arc<BTreeSet<Url>>>,
    alias_current: usize,
    alias_uses: BTreeMap<usize, BTreeSet<&'a str>>,
    alias_processed: BTreeSet<usize>,
    collect_keys: bool,
    all_keys: Option<GraphKeys>,
    trace: Option<SourceTrace>,
    keys: BTreeMap<(usize, usize), Option<Arc<BTreeSet<Url>>>>,
    active: BTreeSet<Url>,
    answer: Option<BTreeSet<Url>>,
    visited: BTreeSet<Url>,
    stack: BTreeSet<Url>,
    visits: usize,
    commands: usize,
    work: usize,
    document: bool,
    ended: bool,
}
impl Analysis<'_, '_> {
    fn alias_resources_closed(
        &mut self,
        rename: Option<crate::bibliography::bib_source_aliases::PrimaryRename<'_>>,
    ) -> Option<()> {
        let mut resources = BTreeSet::new();
        for view in &self.resource_sets {
            for uri in view.iter() {
                self.work += uri.as_str().len() + 64;
                if self.work > 32 * 1024 * 1024 {
                    return None;
                }
                resources.insert(uri);
            }
        }
        let mut aliases = BTreeSet::new();
        for uri in resources {
            for alias in &self.ws.documents.get(uri)?.syntax.bibliography_aliases {
                self.work += alias.name.len() + 64;
                if self.work > 32 * 1024 * 1024 {
                    return None;
                }
                aliases.insert(alias.name.as_str());
            }
        }
        if aliases.is_empty() {
            return Some(());
        }
        fn record<'a>(
            uses: &mut BTreeMap<Arc<BTreeSet<Url>>, BTreeSet<&'a str>>,
            aliases: &BTreeSet<&str>,
            resources: &Arc<BTreeSet<Url>>,
            name: &'a str,
        ) {
            if aliases.contains(name) {
                uses.entry(resources.clone()).or_default().insert(name);
            }
        }
        let mut uses = BTreeMap::new();
        if let Some(keys) = &self.all_keys {
            for ((uri, start, end), views) in keys {
                for view in views {
                    record(
                        &mut uses,
                        &aliases,
                        view,
                        &self.ws.documents.get(uri)?.text.source[*start..*end],
                    );
                }
            }
        }
        for ((start, end), view) in &self.keys {
            if let Some(view) = view {
                record(
                    &mut uses,
                    &aliases,
                    view,
                    &self.ws.documents.get(self.origin)?.text.source[*start..*end],
                );
            }
        }
        if let (Some(resources), Some(query)) = (&self.answer, &self.query) {
            record(
                &mut uses,
                &aliases,
                self.resource_sets.get(resources)?,
                &self.ws.documents.get(self.origin)?.text.source[query.span.clone()],
            );
        }
        if let Some(trace) = &self.trace {
            for event in &trace.events {
                match event {
                    SourceEvent::Set(section, set) => {
                        for member in &set.members {
                            record(
                                &mut uses,
                                &aliases,
                                trace.resources.get(section)?,
                                &member.name,
                            );
                        }
                    }
                    SourceEvent::Request(section, key, _) | SourceEvent::Query(section, key) => {
                        record(
                            &mut uses,
                            &aliases,
                            trace.resources.get(section)?,
                            &key.name,
                        );
                    }
                }
            }
        }
        self.work += uses
            .values()
            .flatten()
            .map(|name| name.len() + 64)
            .sum::<usize>();
        if self.work > 32 * 1024 * 1024 {
            return None;
        }
        self.work += crate::bibliography::bib_source_aliases::closed_resources(
            self.ws,
            &self.resource_sets,
            &uses,
        )?;
        let mut consumed = BTreeMap::<usize, BTreeSet<&str>>::new();
        for (section, names) in &self.alias_uses {
            consumed
                .entry(*section)
                .or_default()
                .extend(names.iter().copied());
        }
        if let Some(trace) = &self.trace {
            for event in &trace.events {
                match event {
                    SourceEvent::Set(section, set) => {
                        consumed
                            .entry(*section as usize)
                            .or_default()
                            .extend(set.members.iter().map(|member| member.name.as_str()));
                    }
                    SourceEvent::Request(section, key, _) | SourceEvent::Query(section, key) => {
                        consumed
                            .entry(*section as usize)
                            .or_default()
                            .insert(key.name.as_str());
                    }
                }
            }
        }
        self.work += consumed
            .values()
            .flatten()
            .map(|name| name.len() + 64)
            .sum::<usize>();
        if self.work > 32 * 1024 * 1024 {
            return None;
        }
        // Ordinary database graphs and raw rename simulations retain the
        // original cache proof. Only an executed source trace can replace a
        // cached parent with its final per-section dynamic set.
        let source_overrides = if let Some(trace) = self.trace.as_ref().filter(|_| rename.is_none())
        {
            Some(crate::bibliography::bib_source_aliases::source_overrides(
                trace,
                &mut self.work,
            )?)
        } else {
            None
        };
        self.work += crate::bibliography::bib_source_aliases::cached_owners(
            self.ws,
            &self.alias_sections,
            &consumed,
            &aliases,
            &self.alias_processed,
            rename,
            source_overrides.as_ref(),
        )?;
        (self.work <= 32 * 1024 * 1024).then_some(())
    }
    // Share one retained resource set across all keys in that section.
    fn intern_resources(&mut self) -> Option<Arc<BTreeSet<Url>>> {
        if let Some(resources) = self.resource_sets.get(&self.active) {
            return Some(resources.clone());
        }
        self.work += self
            .active
            .iter()
            .map(|uri| uri.as_str().len() + std::mem::size_of::<Url>())
            .sum::<usize>();
        if self.work > 32 * 1024 * 1024 {
            return None;
        }
        let resources = Arc::new(self.active.clone());
        self.resource_sets.insert(resources.clone());
        Some(resources)
    }
    fn walk(&mut self, uri: &Url) -> Option<()> {
        if self.labels.is_none() && self.alias_sections.is_empty() {
            let resources = self.intern_resources()?;
            self.alias_sections.push(resources);
            self.work += 64;
        }
        self.visits += 1;
        if self.visits > 1024 || self.stack.len() >= 32 || !self.stack.insert(uri.clone()) {
            return None;
        }
        let doc = self
            .ws
            .documents
            .get(uri)
            .filter(|doc| doc.kind == DocumentKind::Latex)?;
        self.visited.insert(uri.clone());
        let source = &doc.text.source;
        let context = Context::new(doc, self.signatures);
        let positions = self.signatures.initialized_source_command_positions(
            source,
            &doc.syntax,
            self.trace
                .as_ref()
                .is_some_and(|trace| trace.immediate_notes),
        );
        let initialized = if self.trace.is_some() {
            initialized_entrydata_spans(
                doc,
                self.signatures,
                &positions,
                &context,
                self.trace.as_ref()?.immediate_notes,
                &mut self.work,
            )?
        } else {
            Vec::new()
        };
        let hint_assignments = self
            .trace
            .as_ref()
            .is_some_and(|trace| trace.assignments.is_some());
        let initializer_forwards = self
            .trace
            .as_ref()
            .is_some_and(|trace| trace.initializer_forwards);
        let source_forwards = hint_assignments || initializer_forwards;
        let mut forwarded_initialized_end = None::<usize>;
        let alias_declarations: BTreeMap<_, _> = if self.labels.is_some() || source_forwards {
            doc.syntax
                .definitions
                .iter()
                .filter_map(|entry| {
                    let builtin = self.signatures.forwarded_builtin(&entry.name)?;
                    if !(self.labels.is_some() && (STATE.contains(&builtin) || loader(builtin))
                        || hint_assignments
                            && crate::bibliography::bib_context_assignments::target(builtin)
                                .is_some()
                        || initializer_forwards && native_initializer(builtin))
                    {
                        return None;
                    }
                    let target =
                        crate::symbols::commands::source_alias(source, &doc.syntax, entry)?;
                    let body = crate::symbols::commands::command_definition_body(
                        source,
                        &doc.syntax,
                        entry,
                    )?;
                    let at =
                        crate::symbols::commands::command_declaration_start(&doc.syntax, entry)?;
                    Some((at, (entry.name.clone(), target, body.span)))
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        let forwarding_bodies: Vec<_> = alias_declarations
            .values()
            .map(|(_, _, body)| body)
            .collect();
        let mut scanner = if self
            .trace
            .as_ref()
            .is_some_and(|trace| trace.scope.is_some())
        {
            Some(crate::bibliography::bib_context_scope::Scan::new(
                doc,
                self.signatures,
                &mut self.work,
            )?)
        } else {
            None
        };
        for (index, lexical) in doc.syntax.commands.iter().enumerate() {
            if self.ended {
                break;
            }
            if let Some(scanner) = scanner.as_mut() {
                scanner.advance(
                    lexical.span.start,
                    self.trace.as_mut()?.scope.as_mut()?,
                    &mut self.work,
                )?;
                scanner.skip_command(lexical.span.end);
            }
            self.commands += 1;
            if self.commands > 100_000 {
                return None;
            }
            if let Some((name, target, _)) = alias_declarations.get(&lexical.span.start) {
                self.work += source.len();
                if self.work > 32 * 1024 * 1024 {
                    return None;
                }
                let executable = positions.contains(&lexical.span.start)
                    && !context.literal(lexical.span.start)
                    && !doc.syntax.control_operands.contains(&lexical.span.start)
                    && !crate::project::resource_context::grouped(
                        source,
                        &doc.syntax,
                        lexical.span.start,
                    );
                if let Some(labels) = self.labels.as_mut() {
                    if executable && labels.environments.is_empty() {
                        labels.aliases.insert(name.clone(), target.clone());
                    }
                } else if executable
                    && !self.document
                    && matches!(
                        lexical.name.as_str(),
                        "def"
                            | "gdef"
                            | "newcommand"
                            | "renewcommand"
                            | "NewDocumentCommand"
                            | "RenewDocumentCommand"
                            | "DeclareDocumentCommand"
                            | "csdef"
                            | "csgdef"
                    )
                    && self.signatures.command_unchanged(&lexical.name)
                    && !self.signatures.resource_is_tokenized(
                        source,
                        &doc.syntax,
                        lexical.span.start,
                    )
                    && self.trace.as_ref()?.scope.as_ref()?.global()
                {
                    self.work += name.len() + target.len() + 96;
                    if self.work > 32 * 1024 * 1024 {
                        return None;
                    }
                    self.trace
                        .as_mut()?
                        .source_aliases
                        .insert(name.clone(), target.clone());
                }
            }
            if context.literal(lexical.span.start)
                || (self.labels.is_some() || source_forwards)
                    && context
                        .declarations
                        .contains(&(lexical.span.start, lexical.span.end))
                || forwarding_bodies
                    .iter()
                    .any(|body| body.contains(&lexical.span.start))
            {
                continue;
            }
            let (name, span) = self
                .signatures
                .named_invocation(
                    source,
                    &doc.syntax,
                    index,
                    &BTreeSet::new(),
                    &context.declarations,
                )
                .unwrap_or((&lexical.name, lexical.span.clone()));
            if initializer_forwards
                && positions.contains(&lexical.span.start)
                && !context.operand(doc, lexical.span.start)
            {
                if let Some(builtin) = self.signatures.forwarded_builtin(name).filter(|name| {
                    certified_initializer(
                        name,
                        self.trace
                            .as_ref()
                            .is_some_and(|trace| trace.immediate_notes),
                    )
                }) {
                    // Every late-bound edge must already have executed, even
                    // when its replacement shape was visible in another file.
                    let mut current = name;
                    let mut confirmed = false;
                    for _ in 0..=64 {
                        if current == builtin {
                            confirmed = true;
                            break;
                        }
                        current = self.trace.as_ref()?.source_aliases.get(current)?;
                    }
                    if !confirmed {
                        return None;
                    }
                    let argument =
                        initialized_call_argument(source, lexical, &span, &mut self.work)?;
                    if let Some(argument) = argument {
                        forwarded_initialized_end =
                            Some(forwarded_initialized_end.unwrap_or(0).max(argument.end));
                    }
                }
            }
            let pending = self
                .query
                .as_ref()
                .filter(|query| {
                    query.completion && uri == self.origin && query.command == lexical.span.start
                })
                .map(|query| &query.span);
            let textual_test = |name: &str, end| {
                positions.contains(&lexical.span.start)
                    && !context.operand(doc, lexical.span.start)
                    && textual_bibliography_test(
                        source,
                        name,
                        end,
                        self.signatures,
                        pending,
                        initialized
                            .partition_point(|span| span.start <= lexical.span.start)
                            .checked_sub(1)
                            .and_then(|index| initialized.get(index))
                            .filter(|span| span.contains(&lexical.span.start))
                            .map(|span| span.end)
                            .max(forwarded_initialized_end.filter(|end| lexical.span.start < *end)),
                    )
            };
            if unsafe_control(&lexical.name) && !textual_test(&lexical.name, lexical.span.end) {
                return None;
            }
            if lexical.name == "endcsname" {
                continue;
            }
            let name = if let Some(labels) = self.labels.as_ref() {
                if let Some(builtin) = self
                    .signatures
                    .forwarded_builtin(name)
                    .filter(|builtin| STATE.contains(builtin) || loader(builtin))
                {
                    let mut current = name;
                    let mut confirmed = false;
                    for _ in 0..64 {
                        if current == builtin {
                            confirmed = true;
                            break;
                        }
                        current = labels.aliases.get(current)?;
                    }
                    if !confirmed {
                        return None;
                    }
                    builtin
                } else {
                    name
                }
            } else {
                name
            };
            let name = if hint_assignments {
                if let Some(builtin) = self.signatures.forwarded_builtin(name).filter(|builtin| {
                    crate::bibliography::bib_context_assignments::target(builtin).is_some()
                }) {
                    let mut current = name;
                    let mut confirmed = false;
                    // Keep immediate edges: late-bound inner aliases may be
                    // declared later, but every edge must exist at this call.
                    for _ in 0..=64 {
                        if current == builtin {
                            confirmed = true;
                            break;
                        }
                        current = self.trace.as_ref()?.source_aliases.get(current)?;
                    }
                    if !confirmed {
                        return None;
                    }
                    builtin
                } else {
                    name
                }
            } else {
                name
            };
            if matches!(name, "csuse" | "csname") {
                return None;
            }
            if unsafe_control(name) && !textual_test(name, span.end)
                || name.contains("refsection") && !STATE.contains(&name) && name != "therefsection"
            {
                return None;
            }
            if matches!(
                name,
                "setcounter"
                    | "addtocounter"
                    | "stepcounter"
                    | "refstepcounter"
                    | "defcounter"
                    | "counterwithin"
                    | "counterwithout"
                    | "@addtoreset"
                    | "@removefromreset"
            ) {
                let (counter, _) =
                    syntax::group(source, syntax::skip_space(source, span.end), b'{', b'}')?;
                if !syntax::is_static_key(&counter.value) || counter.value == "refsection" {
                    return None;
                }
            }
            let environment = if matches!(name, "begin" | "end") {
                Some(syntax::environment_name_argument(source, span.end)?)
            } else {
                None
            };
            if let Some(labels) = self.labels.as_mut().filter(|_| {
                positions.contains(&lexical.span.start) && !context.operand(doc, lexical.span.start)
            }) {
                if let Some(environment) =
                    environment.as_ref().filter(|env| env.value != "document")
                {
                    if name == "begin" {
                        if labels.environments.len() >= 64 {
                            return None;
                        }
                        labels.environments.push(environment.value.clone());
                    } else if labels.environments.pop().as_deref()
                        != Some(environment.value.as_str())
                    {
                        return None;
                    }
                }
            }
            let relevant = (STATE.contains(&name)
                && environment
                    .as_ref()
                    .is_none_or(|env| matches!(env.value.as_str(), "document" | "refsection")))
                || loader(name);
            if relevant {
                self.work += source.len() + doc.syntax.commands.len();
                if self.work > 32 * 1024 * 1024 {
                    return None;
                }
            }
            if relevant
                && (!self.signatures.command_unchanged(name)
                    || !positions.contains(&lexical.span.start)
                    || context.operand(doc, lexical.span.start)
                    || self.signatures.resource_is_tokenized(
                        source,
                        &doc.syntax,
                        lexical.span.start,
                    ))
            {
                // Deferred code and ordinary macro parameters can change the
                // state later; textual ordering cannot prove when they run.
                return None;
            }
            if uri == self.origin
                && self
                    .labels
                    .as_ref()
                    .is_some_and(|labels| labels.command == Some(lexical.span.start))
            {
                if !relevant {
                    return None;
                }
                self.labels.as_mut().unwrap().seen = true;
            }
            if hint_assignments && matches!(name, "DeclareBibliographyCategory" | "addtocategory") {
                self.work += source.len() + doc.syntax.commands.len();
                if self.work > 32 * 1024 * 1024
                    || !positions.contains(&lexical.span.start)
                    || context.operand(doc, lexical.span.start)
                    || !self.signatures.command_unchanged(name)
                    || self.signatures.resource_is_tokenized(
                        source,
                        &doc.syntax,
                        lexical.span.start,
                    )
                {
                    return None;
                }
                if name == "DeclareBibliographyCategory" {
                    if self.document {
                        return None;
                    }
                    let (argument, _) =
                        syntax::group(source, syntax::skip_space(source, span.end), b'{', b'}')?;
                    let name = crate::bibliography::bib_context_categories::name(source, argument)?;
                    let categories = &mut self.trace.as_mut()?.category_members;
                    if categories.len() >= 100_000
                        || categories.insert(name, BTreeSet::new()).is_some()
                    {
                        return None;
                    }
                } else {
                    let (category, span) =
                        crate::bibliography::bib_context_categories::membership(source, span.end)?;
                    let items = if source[span.clone()].trim().is_empty() {
                        Vec::new()
                    } else {
                        source_key_items(uri, source, span, None)?
                    };
                    let members = self.trace.as_mut()?.category_members.get_mut(&category)?;
                    for key in items.into_iter().flatten() {
                        self.work += key.name.len() + category.len() + 96;
                        if self.work > 32 * 1024 * 1024
                            || key.name == "*"
                            || members.len() >= 100_000
                        {
                            return None;
                        }
                        members.insert(key.name);
                    }
                }
                continue;
            }
            if crate::bibliography::bib_context_assignments::target(name).is_some()
                && self
                    .trace
                    .as_ref()
                    .is_some_and(|trace| trace.assignments.is_some())
            {
                if !positions.contains(&lexical.span.start)
                    || context.operand(doc, lexical.span.start)
                    || !self.signatures.command_unchanged(name)
                    || self.signatures.resource_is_tokenized(
                        source,
                        &doc.syntax,
                        lexical.span.start,
                    )
                {
                    return None;
                }
                let crate::bibliography::bib_context_assignments::Parsed {
                    target,
                    selection,
                    keys,
                    weak,
                } = crate::bibliography::bib_context_assignments::parse(source, span.end, name)?;
                self.work += keys.iter().map(|key| key.len() + 64).sum::<usize>()
                    + selection.prefix.len()
                    + selection.sorting.as_ref().map_or(0, String::len)
                    + 128;
                let trace = self.trace.as_mut()?;
                let assignments = trace.assignments.as_mut()?;
                if self.work > 32 * 1024 * 1024 || assignments.len() >= 100_000 {
                    return None;
                }
                assignments.push(crate::bibliography::bib_context_assignments::Assignment {
                    target,
                    before: trace.events.len(),
                    section: trace.current,
                    selection,
                    keys,
                    weak,
                });
                continue;
            }
            if let Some(scope) = self.trace.as_mut().and_then(|trace| trace.scope.as_mut()) {
                if crate::bibliography::bib_context_scope::control(name) {
                    if lexical.name != name
                        || !positions.contains(&lexical.span.start)
                        || context.operand(doc, lexical.span.start)
                        || self.signatures.resource_is_tokenized(
                            source,
                            &doc.syntax,
                            lexical.span.start,
                        )
                    {
                        return None;
                    }
                    scope.command(
                        name,
                        environment
                            .as_ref()
                            .map(|environment| environment.value.as_str()),
                    )?;
                }
            }
            if self.trace.is_some() {
                let declaration = self
                    .trace
                    .as_ref()?
                    .declarations
                    .get(&(uri.clone(), lexical.span.start))
                    .cloned();
                if let Some(declaration) = declaration {
                    if uri == self.origin
                        && self
                            .query
                            .as_ref()
                            .is_some_and(|query| query.command == lexical.span.start)
                    {
                        self.work += 64;
                        if self.work > 32 * 1024 * 1024 {
                            return None;
                        }
                        let trace = self.trace.as_mut()?;
                        trace.probes.push((trace.current, trace.events.len()));
                    }
                    // The declaration is shared, but processing its member list
                    // on every execution still consumes the analysis budget.
                    self.work += declaration.key.uri.as_str().len()
                        + declaration.key.name.len()
                        + 96
                        + declaration
                            .members
                            .iter()
                            .map(|member| member.uri.as_str().len() + member.name.len() + 96)
                            .sum::<usize>();
                    let trace = self.trace.as_mut()?;
                    if self.work > 32 * 1024 * 1024 || trace.events.len() >= 100_000 {
                        return None;
                    }
                    trace
                        .events
                        .push(SourceEvent::Set(trace.current, declaration));
                } else if syntax::is_citation(name) {
                    if !self.document
                        || !positions.contains(&lexical.span.start)
                        || context.operand(doc, lexical.span.start)
                        || !self.signatures.command_unchanged(name)
                        || (!syntax::warns_on_undefined_citation(name)
                            && !textual_test(name, span.end))
                    {
                        return None;
                    }
                    let command = crate::symbols::reference_calls::resolved_command(
                        source,
                        name,
                        span.clone(),
                    );
                    if !syntax::warns_on_undefined_citation(name) && pending.is_some() {
                        // A prospective predicate never requests a key. Its
                        // probe is resolved against the final section state.
                        self.work += 64;
                        if self.work > 32 * 1024 * 1024 {
                            return None;
                        }
                        let trace = self.trace.as_mut()?;
                        trace.probes.push((trace.current, trace.events.len()));
                    } else if !syntax::warns_on_undefined_citation(name) {
                        let argument = syntax::single_citation_argument(
                            source,
                            command.argument.as_ref()?.clone(),
                        );
                        let key = SourceKey {
                            uri: uri.clone(),
                            span: argument.span,
                            name: argument.value,
                        };
                        // Queries are physical uses, never backend requests:
                        // resolve them against the section's final namespace.
                        self.work += key.uri.as_str().len() + key.name.len() + 192;
                        let trace = self.trace.as_mut()?;
                        if self.work > 32 * 1024 * 1024 || trace.events.len() >= 100_000 {
                            return None;
                        }
                        trace.events.push(SourceEvent::Query(trace.current, key));
                    } else {
                        let pending = self
                            .query
                            .as_ref()
                            .filter(|query| {
                                uri == self.origin && query.command == lexical.span.start
                            })
                            .map(|query| &query.span);
                        let fields = if let Some(style) =
                            crate::bibliography::citations::style(name)
                        {
                            let arguments =
                                crate::bibliography::citations::arguments(source, span.end, style);
                            if arguments.fields.iter().any(|field| {
                                !field.complete
                                    && !pending.is_some_and(|span| {
                                        field.argument.span.start <= span.start
                                            && span.end <= field.argument.span.end
                                    })
                            }) {
                                return None;
                            }
                            arguments
                                .fields
                                .into_iter()
                                .filter(|field| {
                                    field.role == crate::bibliography::citations::Role::Keys
                                })
                                .map(|field| field.argument.span)
                                .collect::<Vec<_>>()
                        } else if let Some(argument) = command.argument.as_ref() {
                            vec![argument.span.clone()]
                        } else {
                            let pending = pending?;
                            let start = syntax::citation_argument_start(source, name, span.end)?;
                            if source.as_bytes().get(start) != Some(&b'{')
                                || pending.start < start + 1
                            {
                                return None;
                            }
                            std::iter::once(start + 1..source.len()).collect::<Vec<_>>()
                        };
                        if fields.is_empty() {
                            return None;
                        }
                        for field in fields {
                            let pending = pending
                                .filter(|span| field.start <= span.start && span.end <= field.end);
                            let keys = source_key_items(uri, source, field, pending)?;
                            if keys.iter().flatten().any(|key| key.name == "*")
                                && (name != "nocite" || keys.len() != 1)
                            {
                                return None;
                            }
                            for key in keys {
                                let Some(key) = key else {
                                    self.work += 64;
                                    if self.work > 32 * 1024 * 1024 {
                                        return None;
                                    }
                                    let trace = self.trace.as_mut()?;
                                    trace.probes.push((trace.current, trace.events.len()));
                                    continue;
                                };
                                self.work += key.uri.as_str().len()
                                    + key.name.len()
                                    + 96
                                    + if self.trace.as_ref()?.explicit_context() {
                                        32
                                    } else {
                                        0
                                    };
                                let trace = self.trace.as_mut()?;
                                if self.work > 32 * 1024 * 1024 || trace.events.len() >= 100_000 {
                                    return None;
                                }
                                let wildcard = key.name == "*";
                                if trace.explicit_context() {
                                    trace.explicit_requests.insert(trace.events.len());
                                }
                                trace.events.push(SourceEvent::Request(
                                    trace.current,
                                    key,
                                    wildcard,
                                ));
                            }
                        }
                    }
                } else if self
                    .signatures
                    .forwarded_builtin(name)
                    .is_some_and(syntax::is_citation)
                {
                    return None;
                }
            }
            if self.labels.is_none()
                && self.document
                && positions.contains(&lexical.span.start)
                && !context.operand(doc, lexical.span.start)
                && syntax::is_citation(name)
                && self.signatures.command_unchanged(name)
            {
                let command =
                    crate::symbols::reference_calls::resolved_command(source, name, span.clone());
                let resources = self.intern_resources()?;
                for (citation, key) in
                    crate::symbols::reference_calls::keys(source, &command, false)
                {
                    if !citation {
                        continue;
                    }
                    if syntax::warns_on_undefined_citation(name) {
                        self.alias_processed.insert(self.alias_current);
                    }
                    if self
                        .alias_uses
                        .entry(self.alias_current)
                        .or_default()
                        .insert(&source[key.clone()])
                    {
                        self.work += key.len() + 64;
                        if self.work > 32 * 1024 * 1024 {
                            return None;
                        }
                    }
                    let key = (key.start, key.end);
                    if let Some(all_keys) = &mut self.all_keys {
                        let visits = all_keys.entry((uri.clone(), key.0, key.1)).or_default();
                        if visits.insert(resources.clone()) {
                            self.work += uri.as_str().len() + 128;
                            if self.work > 32 * 1024 * 1024 || all_keys.len() > 100_000 {
                                return None;
                            }
                        }
                    }
                    if self.collect_keys && uri == self.origin {
                        if let Some(previous) = self.keys.get_mut(&key) {
                            if previous
                                .as_ref()
                                .is_some_and(|previous| !Arc::ptr_eq(previous, &resources))
                            {
                                *previous = None;
                            }
                        } else {
                            if self.keys.len() >= 100_000 {
                                return None;
                            }
                            self.keys.insert(key, Some(resources.clone()));
                        }
                    }
                }
            }
            if uri == self.origin
                && self
                    .query
                    .as_ref()
                    .is_some_and(|query| lexical.span.start == query.command)
            {
                if (!self.document && !(self.trace.is_some() && name == "defbibentryset"))
                    || !positions.contains(&lexical.span.start)
                {
                    return None;
                }
                if self.query.as_ref().is_some_and(|query| query.completion) {
                    if syntax::warns_on_undefined_citation(name) {
                        self.alias_processed.insert(self.alias_current);
                    }
                    for resource in &self.active {
                        for alias in &self.ws.documents.get(resource)?.syntax.bibliography_aliases {
                            if self
                                .alias_uses
                                .entry(self.alias_current)
                                .or_default()
                                .insert(alias.name.as_str())
                            {
                                self.work += alias.name.len() + 64;
                                if self.work > 32 * 1024 * 1024 {
                                    return None;
                                }
                            }
                        }
                    }
                }
                if self.trace.is_none() {
                    if self
                        .answer
                        .as_ref()
                        .is_some_and(|previous| *previous != self.active)
                    {
                        return None;
                    }
                    self.answer = Some(self.active.clone());
                }
            }
            if !relevant {
                continue;
            }
            let command = Command {
                name: name.into(),
                span: span.clone(),
                argument: if span == lexical.span {
                    lexical.argument.clone()
                } else {
                    syntax::source_input_argument(source, name, span.end)
                },
            };
            if crate::bibliography::bib_resources::references_file(name) {
                if self.document
                    || crate::project::resource_context::grouped(source, &doc.syntax, span.start)
                {
                    return None;
                }
            } else if matches!(name, "begin" | "end") {
                let environment = syntax::environment_name_argument(source, span.end)?;
                if environment.value == "document" {
                    if name == "begin" {
                        self.document = true;
                    } else {
                        self.ended = true;
                    }
                } else if environment.value == "refsection" {
                    if name == "begin" {
                        self.select(uri, doc, environment.outer_span.end)?;
                    } else if self.labels.is_none() {
                        self.active = self.registry.resources(self.root, None)?;
                        self.alias_current = 0;
                        if let Some(trace) = self.trace.as_mut() {
                            trace.current = 0;
                        }
                    }
                }
            } else if matches!(name, "newrefsection" | "refsection") {
                self.select(uri, doc, span.end)?;
            } else if name == "endrefsection" {
                if self.labels.is_none() {
                    self.active = self.registry.resources(self.root, None)?;
                    self.alias_current = 0;
                    if let Some(trace) = self.trace.as_mut() {
                        trace.current = 0;
                    }
                }
            } else if loader(name) {
                if self.labels.is_some()
                    && crate::project::resource_context::grouped(source, &doc.syntax, span.start)
                {
                    return None;
                }
                let command = doc.syntax.potential_file_command(source, &command);
                let static_name = if syntax::file_list(name) {
                    syntax::complete_file_list_spans(source, &command).is_some()
                } else {
                    syntax::file_argument(source, &command)
                        .is_some_and(|argument| syntax::is_literal_file_path(&argument.value))
                };
                if !static_name {
                    return None;
                }
                let dependencies = self
                    .ws
                    .literal_command_dependencies(uri, self.root, &command);
                if matches!(name, "input" | "include" | "subfile") && dependencies.is_empty() {
                    return None;
                }
                for (dependency, kind) in dependencies {
                    if kind == DocumentKind::Latex {
                        self.walk(&dependency)?;
                    }
                }
            }
        }
        if !self.ended {
            if let Some(scanner) = scanner.as_mut() {
                scanner.advance(
                    source.len(),
                    self.trace.as_mut()?.scope.as_mut()?,
                    &mut self.work,
                )?;
            }
        }
        self.stack.remove(uri);
        Some(())
    }
    fn select(&mut self, uri: &Url, doc: &Document, end: usize) -> Option<()> {
        if !self.document {
            return None;
        }
        let source = &doc.text.source;
        let at = syntax::skip_space(source, end);
        if crate::project::resource_context::ambiguous_header(&doc.syntax, at) {
            return None;
        }
        let selected = if source.as_bytes().get(at) == Some(&b'[') {
            let raw = doc.syntax.raw_resource_brackets.contains(&at);
            let parsed = if raw {
                syntax::verbatim_file_bracket(source, at)
            } else {
                syntax::bracket_argument(source, at)
            };
            let argument = if let Some((argument, _)) = parsed {
                argument
            } else if self
                .labels
                .as_ref()
                .is_some_and(|labels| labels.choices.is_none())
            {
                syntax::Argument {
                    value: source[at + 1..].into(),
                    span: at + 1..source.len(),
                    outer_span: at..source.len(),
                }
            } else {
                return None;
            };

            let segments = if raw {
                syntax::verbatim_list_segments(source, argument.span)
            } else {
                syntax::key_list_segments(source, argument.span)
            };
            let mut items = Vec::new();
            for span in segments {
                let span = syntax::trimmed_span(source, span);
                let value = &source[span.clone()];
                if value.is_empty() {
                    continue;
                }
                let supported = if self.labels.is_some() {
                    crate::bibliography::bib_section_files::literal_item(value)
                } else {
                    self.registry.is_label(value)
                        || crate::bibliography::bib_section_files::editable_item(value)
                };
                if !supported {
                    return None;
                }
                if let Some(choices) = self
                    .labels
                    .as_mut()
                    .and_then(|labels| labels.choices.as_mut())
                {
                    self.work +=
                        value.len() + uri.as_str().len() + std::mem::size_of::<ResourceChoice>();
                    if self.work > 32 * 1024 * 1024 || choices.len() >= 100_000 {
                        return None;
                    }
                    choices.push(ResourceChoice {
                        uri: uri.clone(),
                        span,
                        name: value.into(),
                    });
                }
                items.push(value.to_owned());
            }
            Some(items)
        } else {
            None
        };
        if self.labels.is_some() {
            return Some(());
        }
        self.active = self.registry.resources(self.root, selected.as_deref())?;
        {
            let resources = self.intern_resources()?;
            self.work += 64;
            if self.work > 32 * 1024 * 1024 || self.alias_sections.len() >= 100_000 {
                return None;
            }
            self.alias_current = self.alias_sections.len();
            self.alias_sections.push(resources.clone());
            if let Some(trace) = self.trace.as_mut() {
                self.work += 64;
                if self.work > 32 * 1024 * 1024 || trace.next >= 100_000 {
                    return None;
                }
                trace.next += 1;
                trace.current = trace.next;
                trace.resources.insert(trace.current, resources);
            }
        }
        Some(())
    }
}

pub(crate) fn signature_budget(docs: &[(Url, &Document)]) -> bool {
    docs.iter()
        .map(|(_, doc)| doc.syntax.commands.len())
        .sum::<usize>()
        <= 100_000
        && docs
            .iter()
            .map(|(_, doc)| doc.syntax.definitions.len() + doc.syntax.environments.len())
            .sum::<usize>()
            <= 4096
}

// All resource consumers share the same execution and path checks. A failed
// proof suppresses cross-library diagnostics rather than asserting a union.
fn valid_sources(docs: &[(Url, &Document)], signatures: &CommandSignatures<'_>) -> bool {
    !STATE.iter().any(|name| !signatures.command_unchanged(name))
        && docs.iter().all(|(_, doc)| {
            if doc
                .syntax
                .contextual_resource_readers
                .values()
                .flatten()
                .any(|reader| reader.opaque)
            {
                return false;
            }
            let compact: String = doc
                .text
                .source
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect();
            !compact.contains("refsection=") && !compact.contains("^^")
        })
}

// Dynamic sets need a separate model. Native handwritten bibliography entries
// belong to the traditional bibliography namespace, independent of biblatex keys.
fn database_only(docs: &[(Url, &Document)], signatures: &CommandSignatures<'_>) -> bool {
    let manual = crate::bibliography::bib_manual::items(docs, signatures);
    docs.iter().all(|(uri, doc)| {
        if doc.syntax.citations.iter().any(|entry| {
            !manual
                .as_ref()
                .and_then(|items| items.get(uri))
                .is_some_and(|spans| spans.contains(&(entry.span.start, entry.span.end)))
        }) {
            return false;
        }
        let context = Context::new(doc, signatures);
        !doc.syntax
            .commands
            .iter()
            .enumerate()
            .any(|(index, command)| {
                if context.literal(command.span.start) {
                    return false;
                }
                let (name, _) = signatures
                    .named_invocation(
                        &doc.text.source,
                        &doc.syntax,
                        index,
                        &BTreeSet::new(),
                        &context.declarations,
                    )
                    .unwrap_or((&command.name, command.span.clone()));
                ["defbibentryset", "bibitem"]
                    .iter()
                    .filter(|builtin| **builtin != "bibitem" || manual.is_none())
                    .any(|builtin| name == *builtin || signatures.invokes_builtin(name, builtin))
            })
    })
}

pub(crate) enum Coexistence {
    Union,
    Unknown,
    Sections(BTreeSet<Arc<BTreeSet<Url>>>),
}
impl Coexistence {
    pub(crate) fn permits(&self, a: &Url, b: &Url) -> bool {
        match self {
            Self::Union => true,
            Self::Unknown => false,
            Self::Sections(sections) => sections
                .iter()
                .any(|resources| resources.contains(a) && resources.contains(b)),
        }
    }
}

fn modern_databases(docs: &[(Url, &Document)], signatures: &CommandSignatures<'_>) -> bool {
    docs.iter().any(|(_, doc)| {
        let context = Context::new(doc, signatures);
        doc.syntax
            .commands
            .iter()
            .enumerate()
            .any(|(index, command)| {
                if context.literal(command.span.start) {
                    return false;
                }
                let (name, span) = signatures
                    .named_invocation(
                        &doc.text.source,
                        &doc.syntax,
                        index,
                        &BTreeSet::new(),
                        &context.declarations,
                    )
                    .unwrap_or((&command.name, command.span.clone()));
                ["addbibresource", "addglobalbib", "addsectionbib"]
                    .iter()
                    .any(|builtin| signatures.invokes_builtin(name, builtin))
                    || ["usepackage", "RequirePackage", "RequirePackageWithOptions"]
                        .iter()
                        .find(|builtin| signatures.invokes_builtin(name, builtin))
                        .and_then(|builtin| {
                            syntax::file_name_argument_start(&doc.text.source, builtin, span.end)
                        })
                        .and_then(|at| syntax::group(&doc.text.source, at, b'{', b'}'))
                        .is_some_and(|(argument, _)| {
                            argument
                                .value
                                .split(',')
                                .any(|name| syntax::loader_name(name) == "biblatex")
                        })
            })
    })
}

pub(crate) fn coexistence(ws: &Workspace, root: &Url, scope: &BTreeSet<Url>) -> Coexistence {
    let Some(docs) = crate::project::source_inputs::bounded_latex_sources(ws, scope) else {
        return Coexistence::Unknown;
    };
    if !signature_budget(&docs) {
        return Coexistence::Unknown;
    }
    let signatures = CommandSignatures::new(
        docs.iter()
            .map(|(_, doc)| (doc.text.source.as_str(), &doc.syntax)),
    );
    let selected = crate::bibliography::bib_sections::selects_resources(
        &docs.iter().map(|(_, doc)| *doc).collect::<Vec<_>>(),
        &signatures,
    );
    let modern = modern_databases(&docs, &signatures) || selected;
    if !modern {
        return Coexistence::Union;
    }
    if selected && (!database_only(&docs, &signatures) || !static_databases(ws, root)) {
        return Coexistence::Unknown;
    }
    let sections = (|| {
        if !ws.index_truncated.is_empty() || !valid_sources(&docs, &signatures) {
            return None;
        }
        let registry = Registry::new(root, &docs, &signatures);
        let active = registry.resources(root, None)?;
        let mut analysis = Analysis {
            ws,
            root,
            signatures: &signatures,
            registry,
            origin: root,
            query: None,
            labels: None,
            resource_sets: BTreeSet::from([Arc::new(active.clone())]),
            alias_sections: Vec::new(),
            alias_current: 0,
            alias_uses: BTreeMap::new(),
            alias_processed: BTreeSet::new(),
            collect_keys: false,
            all_keys: None,
            trace: None,
            keys: BTreeMap::new(),
            active,
            answer: None,
            visited: BTreeSet::new(),
            stack: BTreeSet::new(),
            visits: 0,
            commands: 0,
            work: 0,
            document: false,
            ended: false,
        };
        analysis.walk(root)?;
        analysis.alias_resources_closed(None)?;
        if docs.iter().any(|(uri, _)| !analysis.visited.contains(uri)) {
            return None;
        }
        Some(analysis.resource_sets)
    })();
    sections.map_or(Coexistence::Unknown, Coexistence::Sections)
}

struct Target {
    span: Range<usize>,
    resources: BTreeSet<Url>,
    call: Command,
}
// If a resource analysis limit is reached, do not fall back to a union
// target that the ordinary key index would otherwise expose.
fn incomplete(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
    purpose: Purpose,
) -> Option<Option<Target>> {
    let supported = match purpose {
        Purpose::Key => crate::symbols::named_references::Index::new(ws, uri)
            .target(uri, offset)
            .is_some_and(|(citation, _)| citation),
        Purpose::Completion => {
            crate::symbols::named_references::completion_context(ws, uri, offset, |command| {
                if !syntax::is_citation(&command.name) {
                    return None;
                }
                let doc = ws.documents.get(uri)?;
                completion_span(doc, command, offset).map(|(span, _)| span.start..offset)
            })
            .is_some()
        }
    };
    supported.then_some(None)
}
pub(crate) fn unproved_citation_completion(ws: &Workspace, uri: &Url, offset: usize) -> bool {
    incomplete(ws, uri, offset, Purpose::Completion).is_some()
}

pub(crate) struct Project<'a> {
    pub root: Url,
    pub docs: Vec<(Url, &'a Document)>,
    pub signatures: Arc<CommandSignatures<'a>>,
}
pub(crate) fn section_projects<'a>(
    ws: &'a Workspace,
    uri: &Url,
) -> Option<(bool, Vec<Project<'a>>)> {
    section_projects_for_cursor(ws, uri, None)
}

fn section_projects_for_cursor<'a>(
    ws: &'a Workspace,
    uri: &Url,
    cursor: Option<usize>,
) -> Option<(bool, Vec<Project<'a>>)> {
    let mut roots: BTreeSet<_> = ws.source_roots(uri).into_iter().collect();
    // Opened roots consume shared files even before their own path is saved.
    for candidate in ws
        .documents
        .keys()
        .filter(|candidate| ws.is_root(candidate))
    {
        if ws.project_documents(candidate).contains(uri) {
            roots.insert(candidate.clone());
        }
        if roots.len() > 64 {
            return None;
        }
    }
    let mut projects = Vec::new();
    let mut bytes = 0usize;
    for root in roots {
        let mut scope = ws.project_documents(&root);
        scope.insert(root.clone());
        // Rootless snippets retain the ordinary database symbol model.
        // With real roots, an unlinked database may still be consumed by an
        // uncertain section selection and must keep its provenance guard.
        if root == *uri
            && ws.index_truncated.is_empty()
            && ws
                .documents
                .get(&root)
                .is_some_and(|doc| doc.kind == DocumentKind::Bibtex)
            && !ws.documents.keys().any(|uri| ws.is_root(uri))
            && !scope.iter().any(|uri| {
                ws.documents
                    .get(uri)
                    .is_some_and(|doc| doc.kind == DocumentKind::Latex)
            })
        {
            continue;
        }
        let docs = crate::project::source_inputs::bounded_latex_sources(ws, &scope)?;
        if !signature_budget(&docs) {
            return None;
        }
        bytes += docs
            .iter()
            .map(|(_, doc)| doc.text.source.len())
            .sum::<usize>();
        if bytes > 16 * 1024 * 1024 {
            return None;
        }
        let signatures = crate::symbols::query_cache::project_signatures(ws, &scope);
        projects.push(Project {
            root,
            docs,
            signatures,
        });
    }
    // Completion first proves that this cursor can name a source-set key.
    // Ordinary file/command queries must not trace bibliography execution in
    // every project merely because a class contains csname declarations.
    if cursor.is_some_and(|offset| {
        ws.documents.get(uri).is_none_or(|doc| {
            !projects
                .iter()
                .any(|project| source_completion_candidate(doc, &project.signatures, offset))
        })
    }) {
        return Some((false, Vec::new()));
    }
    let selected = projects.iter().any(|project| {
        let docs = &project.docs;
        let signatures = &project.signatures;
        crate::bibliography::bib_sections::selects_resources(
            &docs.iter().map(|(_, doc)| *doc).collect::<Vec<_>>(),
            signatures,
        ) || modern_databases(docs, signatures)
            && ws
                .project_documents(&project.root)
                .iter()
                .filter_map(|uri| ws.documents.get(uri))
                .any(database_execution)
    });
    Some((selected, projects))
}

/// A normal project also needs execution identities when it declares source
/// sets, even if it never selects a separate bibliography resource section.
/// Loose snippets keep their existing project-wide editing behavior.
pub(crate) fn source_projects<'a>(
    ws: &'a Workspace,
    uri: &Url,
) -> Option<(bool, Vec<Project<'a>>)> {
    source_projects_for_cursor(ws, uri, None)
}

pub(crate) fn source_completion_projects<'a>(
    ws: &'a Workspace,
    uri: &Url,
    offset: usize,
) -> Option<(bool, Vec<Project<'a>>)> {
    source_projects_for_cursor(ws, uri, Some(offset))
}

fn source_projects_for_cursor<'a>(
    ws: &'a Workspace,
    uri: &Url,
    cursor: Option<usize>,
) -> Option<(bool, Vec<Project<'a>>)> {
    // This document necessarily belongs to its own context. Reject a locally
    // exhausted budget before constructing its consumer scopes and signatures.
    if ws.documents.get(uri).is_some_and(|doc| {
        doc.text.source.len() > 16 * 1024 * 1024
            || doc.syntax.commands.len() > 100_000
            || doc.syntax.definitions.len() + doc.syntax.environments.len() > 4096
    }) {
        return None;
    }
    let (selected, projects) = section_projects_for_cursor(ws, uri, cursor)?;
    let sources = !selected
        && projects.iter().any(|project| {
            ws.is_root(&project.root)
                && project.docs.iter().any(|(_, doc)| {
                    let context = Context::new(doc, &project.signatures);
                    doc.syntax
                        .commands
                        .iter()
                        .enumerate()
                        .any(|(index, command)| {
                            if context.literal(command.span.start) {
                                return false;
                            }
                            let (name, span) = project
                                .signatures
                                .named_invocation(
                                    &doc.text.source,
                                    &doc.syntax,
                                    index,
                                    &BTreeSet::new(),
                                    &context.declarations,
                                )
                                .unwrap_or((&command.name, command.span.clone()));
                            name == "begin"
                                && syntax::environment_name_argument(&doc.text.source, span.end)
                                    .is_some_and(|environment| environment.value == "document")
                        })
                })
                && has_source_sets(&project.docs, &project.signatures)
        });
    Some((selected || sources, projects))
}

/// Own potential key queries when signature or project budgets prevented
/// building a complete context. This cannot certify their declaration identity;
/// other symbol namespaces remain independent.
pub(crate) fn unproved_citation_query(ws: &Workspace, uri: &Url, offset: usize) -> bool {
    let Some(doc) = ws.documents.get(uri) else {
        return false;
    };
    if doc
        .syntax
        .citations
        .iter()
        .chain(&doc.syntax.citation_references)
        .chain(&doc.syntax.bibliography_aliases)
        .any(|entry| entry.span.start <= offset && offset <= entry.span.end)
    {
        return true;
    }
    let signatures = CommandSignatures::new(std::iter::empty());
    let empty = BTreeSet::new();
    let literal = doc
        .syntax
        .commands
        .iter()
        .enumerate()
        .any(|(index, command)| {
            let (name, span) = signatures
                .named_invocation(
                    &doc.text.source,
                    &doc.syntax,
                    index,
                    &empty,
                    &BTreeSet::new(),
                )
                .unwrap_or((&command.name, command.span.clone()));
            if name == "defbibentryset" {
                return crate::bibliography::citations::arguments(
                    &doc.text.source,
                    span.end,
                    crate::bibliography::citations::Style::Set,
                )
                .fields
                .iter()
                .any(|field| {
                    syntax::citation_list_spans(&doc.text.source, field.argument.span.clone())
                        .into_iter()
                        .any(|span| span.start <= offset && offset <= span.end)
                });
            }
            if !syntax::is_citation(name) {
                return false;
            }
            let command =
                crate::symbols::reference_calls::resolved_command(&doc.text.source, name, span);
            crate::symbols::reference_calls::keys(&doc.text.source, &command, false)
                .into_iter()
                .any(|(citation, span)| citation && span.start <= offset && offset <= span.end)
        });
    literal || incomplete(ws, uri, offset, Purpose::Key).is_some()
}

/// None leaves ordinary projects to the existing symbol model; Some(None)
/// owns a section-dependent citation whose provenance cannot be certified.
fn target(ws: &Workspace, uri: &Url, offset: usize, purpose: Purpose) -> Option<Option<Target>> {
    let doc = ws
        .documents
        .get(uri)
        .filter(|doc| doc.kind == DocumentKind::Latex)?;
    if !needs_namespace_proof(ws) || !ws.index_truncated.is_empty() {
        return None;
    }
    let Some((selected, projects)) = section_projects(ws, uri) else {
        return incomplete(ws, uri, offset, purpose);
    };
    if selected
        && doc
            .syntax
            .citations
            .iter()
            .any(|entry| entry.span.start <= offset && offset <= entry.span.end)
    {
        return Some(None);
    }
    if !selected
        || !projects
            .iter()
            .any(|project| citation_candidate(doc, &project.signatures, offset, purpose))
    {
        return None;
    }
    Some((|| {
        let mut result: Option<Target> = None;
        for Project {
            root,
            docs,
            signatures,
        } in &projects
        {
            let query = query(doc, signatures, offset, purpose)?;
            if !valid_sources(docs, signatures)
                || !database_only(docs, signatures)
                || !static_databases(ws, root)
            {
                return None;
            }
            let registry = Registry::new(root, docs, signatures);
            let active = registry.resources(root, None)?;
            let mut analysis = Analysis {
                ws,
                root,
                signatures,
                registry,
                origin: uri,
                query: Some(query.clone()),
                labels: None,
                resource_sets: BTreeSet::from([Arc::new(active.clone())]),
                alias_sections: Vec::new(),
                alias_current: 0,
                alias_uses: BTreeMap::new(),
                alias_processed: BTreeSet::new(),
                collect_keys: false,
                all_keys: None,
                trace: None,
                keys: BTreeMap::new(),
                active,
                answer: None,
                visited: BTreeSet::new(),
                stack: BTreeSet::new(),
                visits: 0,
                commands: 0,
                work: 0,
                document: false,
                ended: false,
            };
            analysis.walk(root)?;
            analysis.alias_resources_closed(None)?;
            // A potential input that was never reached must not contribute a
            // resource registration from the project's discovery graph.
            if docs.iter().any(|(uri, _)| !analysis.visited.contains(uri)) {
                return None;
            }
            let resources = analysis.answer?;
            if resources.iter().any(|uri| {
                !ws.documents
                    .get(uri)
                    .is_some_and(|doc| doc.kind == DocumentKind::Bibtex)
            }) {
                return None;
            }
            if result.as_ref().is_some_and(|previous| {
                previous.span != query.span
                    || previous.resources != resources
                    || previous.call.name != query.call.name
                    || previous.call.span != query.call.span
            }) {
                return None;
            }
            result = Some(Target {
                span: query.span,
                resources,
                call: query.call,
            });
        }
        result
    })())
}

pub(crate) fn definitions(ws: &Workspace, uri: &Url, offset: usize) -> Option<Vec<Location>> {
    let Some(target) = target(ws, uri, offset, Purpose::Key)? else {
        return Some(Vec::new());
    };
    let name = &ws.documents[uri].text.source[target.span];
    Some(
        target
            .resources
            .iter()
            .flat_map(|uri| {
                let doc = &ws.documents[uri];
                doc.syntax
                    .citations
                    .iter()
                    .chain(&doc.syntax.bibliography_aliases)
                    .filter(move |entry| entry.name == name)
                    .map(move |entry| Location {
                        uri: uri.clone(),
                        range: doc.text.range(entry.span.start, entry.span.end),
                    })
            })
            .collect(),
    )
}
pub(crate) fn hover(ws: &Workspace, uri: &Url, offset: usize) -> Option<Option<Hover>> {
    let target = target(ws, uri, offset, Purpose::Key)?;
    Some(target.and_then(|target| {
        let source = &ws.documents[uri];
        let name = &source.text.source[target.span.clone()];
        let mut entries = target.resources.iter().flat_map(|uri| {
            let doc = &ws.documents[uri];
            doc.syntax
                .citations
                .iter()
                .chain(&doc.syntax.bibliography_aliases)
                .filter(move |entry| entry.name == name)
                .map(move |entry| (uri, doc, entry))
        });
        let (_, doc, entry) = entries.next()?;
        if entries.next().is_some() {
            return None;
        }
        Some(Hover {
            contents: HoverContents::Markup(super::reference_preview::hover(doc, entry)),
            range: Some(source.text.range(target.span.start, target.span.end)),
        })
    }))
}

pub(crate) fn completion(
    ws: &Workspace,
    uri: &Url,
    offset: usize,
) -> Option<Vec<lsp_types::CompletionItem>> {
    if let Some(items) = crate::bibliography::bib_source_citations::completion(ws, uri, offset) {
        return Some(items);
    }
    let Some(target) = target(ws, uri, offset, Purpose::Completion)? else {
        return Some(Vec::new());
    };
    let doc = ws.documents.get(uri)?;
    let Some((_, removals)) = completion_span(doc, &target.call, offset) else {
        return Some(Vec::new());
    };
    let mut items = crate::bibliography::citation_search::items(
        ws,
        doc,
        offset,
        target.span,
        &target.resources,
        true,
    )
    .unwrap_or_default();
    if !removals.is_empty() {
        let removals: Vec<_> = removals
            .into_iter()
            .map(|span| lsp_types::TextEdit {
                range: doc.text.range(span.start, span.end),
                new_text: String::new(),
            })
            .collect();
        for item in &mut items {
            item.additional_text_edits
                .get_or_insert_with(Vec::new)
                .extend(removals.iter().cloned());
        }
    }
    Some(items)
}

pub(crate) type ResourceMap = BTreeMap<(usize, usize), Arc<BTreeSet<Url>>>;
/// None uses ordinary project semantics. Some owns section-dependent calls;
/// missing spans are unproved and must not fall back to a bibliography union.
pub(crate) fn occurrence_resources(ws: &Workspace, uri: &Url) -> Option<ResourceMap> {
    let doc = ws.documents.get(uri)?;
    if doc.kind != DocumentKind::Latex || !needs_namespace_proof(ws) {
        return None;
    }
    let Some((selected, projects)) = section_projects(ws, uri) else {
        return Some(BTreeMap::new());
    };
    if !selected {
        return None;
    }
    Some(
        (|| {
            if !ws.index_truncated.is_empty() {
                return None;
            }
            let mut result: Option<ResourceMap> = None;
            for Project {
                root,
                docs,
                signatures,
            } in &projects
            {
                if !valid_sources(docs, signatures)
                    || !database_only(docs, signatures)
                    || !static_databases(ws, root)
                {
                    return None;
                }
                let registry = Registry::new(root, docs, signatures);
                let active = registry.resources(root, None)?;
                let mut analysis = Analysis {
                    ws,
                    root,
                    signatures,
                    registry,
                    origin: uri,
                    query: None,
                    labels: None,
                    resource_sets: BTreeSet::from([Arc::new(active.clone())]),
                    alias_sections: Vec::new(),
                    alias_current: 0,
                    alias_uses: BTreeMap::new(),
                    alias_processed: BTreeSet::new(),
                    collect_keys: true,
                    all_keys: None,
                    trace: None,
                    keys: BTreeMap::new(),
                    active,
                    answer: None,
                    visited: BTreeSet::new(),
                    stack: BTreeSet::new(),
                    visits: 0,
                    commands: 0,
                    work: 0,
                    document: false,
                    ended: false,
                };
                analysis.walk(root)?;
                analysis.alias_resources_closed(None)?;
                if docs.iter().any(|(uri, _)| !analysis.visited.contains(uri))
                    || analysis.resource_sets.iter().any(|resources| {
                        resources.iter().any(|uri| {
                            !ws.documents
                                .get(uri)
                                .is_some_and(|doc| doc.kind == DocumentKind::Bibtex)
                        })
                    })
                {
                    return None;
                }
                let keys: ResourceMap = analysis
                    .keys
                    .into_iter()
                    .filter_map(|(span, resources)| resources.map(|resources| (span, resources)))
                    .collect();
                if let Some(previous) = &mut result {
                    let mut agreement = BTreeMap::new();
                    previous.retain(|span, resources| {
                        keys.get(span).is_some_and(|candidate| {
                            let identity = (
                                Arc::as_ptr(resources) as usize,
                                Arc::as_ptr(candidate) as usize,
                            );
                            *agreement
                                .entry(identity)
                                .or_insert_with(|| resources.as_ref() == candidate.as_ref())
                        })
                    });
                } else {
                    result = Some(keys);
                }
            }
            result
        })()
        .unwrap_or_default(),
    )
}

/// Certify the registration dictionary separately from the selected resources.
/// Incomplete names can be completed without pretending they already resolve.
pub(crate) fn label_bindings(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
    uri: &Url,
    command: usize,
) -> Option<BTreeMap<String, crate::bibliography::bib_section_files::LabelBinding>> {
    Some(analyze_labels(ws, root, docs, signatures, uri, Some(command), false)?.bindings)
}

pub(crate) fn label_graph(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
) -> Option<LabelGraph> {
    analyze_labels(ws, root, docs, signatures, root, None, true)
}

fn analyze_labels(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
    uri: &Url,
    command: Option<usize>,
    complete: bool,
) -> Option<LabelGraph> {
    if !valid_sources(docs, signatures) {
        return None;
    }
    let registry = Registry::new(root, docs, signatures);
    let mut analysis = Analysis {
        ws,
        root,
        signatures,
        registry,
        origin: uri,
        query: None,
        labels: Some(LabelProof {
            command,
            seen: false,
            aliases: BTreeMap::new(),
            environments: Vec::new(),
            choices: complete.then(Vec::new),
        }),
        resource_sets: BTreeSet::new(),
        alias_sections: Vec::new(),
        alias_current: 0,
        alias_uses: BTreeMap::new(),
        alias_processed: BTreeSet::new(),
        collect_keys: false,
        all_keys: None,
        trace: None,
        keys: BTreeMap::new(),
        active: BTreeSet::new(),
        answer: None,
        visited: BTreeSet::new(),
        stack: BTreeSet::new(),
        visits: 0,
        commands: 0,
        work: 0,
        document: false,
        ended: false,
    };
    analysis.walk(root)?;
    let labels = analysis.labels?;
    if command.is_some() && !labels.seen
        || docs.iter().any(|(uri, _)| !analysis.visited.contains(uri))
    {
        return None;
    }
    Some(LabelGraph {
        bindings: analysis.registry.label_bindings()?,
        names: if complete {
            analysis.registry.label_names()
        } else {
            BTreeSet::new()
        },
        choices: labels.choices.unwrap_or_default(),
    })
}

pub(crate) type GraphKeys = BTreeMap<(Url, usize, usize), BTreeSet<Arc<BTreeSet<Url>>>>;
pub(crate) struct KeyGraph {
    pub sections: BTreeSet<Arc<BTreeSet<Url>>>,
    pub occurrences: GraphKeys,
}

/// Recheck actual numeric cache loads after moving one real primary spelling.
/// Resources and request sections stay fixed; aliases follow their moved owner.
pub(crate) fn validate_primary_rename(
    ws: &Workspace,
    root: &Url,
    scope: &BTreeSet<Url>,
    rename: crate::bibliography::bib_source_aliases::PrimaryRename<'_>,
) -> Option<usize> {
    let docs = crate::project::source_inputs::bounded_latex_sources(ws, scope)?;
    if !signature_budget(&docs) {
        return None;
    }
    let signatures = CommandSignatures::new(
        docs.iter()
            .map(|(_, doc)| (doc.text.source.as_str(), &doc.syntax)),
    );
    if has_source_sets(&docs, &signatures) {
        source_trace_query(
            ws,
            root,
            &docs,
            &signatures,
            root,
            None,
            TracePurpose::Keys(Some(rename)),
        )
        .map(|trace| trace.work)
    } else {
        key_graph_after_rename(ws, root, &docs, &signatures, Some(rename)).map(|(_, work)| work)
    }
}

/// Collect every source occurrence once per root, preserving all resource
/// views of a repeatedly loaded source for later declaration-identity checks.
pub(crate) fn key_graph(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
) -> Option<KeyGraph> {
    key_graph_after_rename(ws, root, docs, signatures, None).map(|(graph, _)| graph)
}
fn key_graph_after_rename(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
    rename: Option<crate::bibliography::bib_source_aliases::PrimaryRename<'_>>,
) -> Option<(KeyGraph, usize)> {
    if !ws.index_truncated.is_empty()
        || !valid_sources(docs, signatures)
        || !database_only(docs, signatures)
        || !static_databases(ws, root)
    {
        return None;
    }
    let registry = Registry::new(root, docs, signatures);
    let active = registry.resources(root, None)?;
    let mut analysis = Analysis {
        ws,
        root,
        signatures,
        registry,
        origin: root,
        query: None,
        labels: None,
        resource_sets: BTreeSet::from([Arc::new(active.clone())]),
        alias_sections: Vec::new(),
        alias_current: 0,
        alias_uses: BTreeMap::new(),
        alias_processed: BTreeSet::new(),
        collect_keys: false,
        all_keys: Some(BTreeMap::new()),
        trace: None,
        keys: BTreeMap::new(),
        active,
        answer: None,
        visited: BTreeSet::new(),
        stack: BTreeSet::new(),
        visits: 0,
        commands: 0,
        work: 0,
        document: false,
        ended: false,
    };
    analysis.walk(root)?;
    analysis.alias_resources_closed(rename)?;
    if docs.iter().any(|(uri, _)| !analysis.visited.contains(uri))
        || analysis.resource_sets.iter().any(|resources| {
            resources.iter().any(|uri| {
                !ws.documents
                    .get(uri)
                    .is_some_and(|doc| doc.kind == DocumentKind::Bibtex)
            })
        })
    {
        return None;
    }
    Some((
        KeyGraph {
            sections: analysis.resource_sets,
            occurrences: analysis.all_keys?,
        },
        analysis.work,
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceKey {
    pub uri: Url,
    pub span: std::ops::Range<usize>,
    pub name: String,
}
#[derive(Clone, Debug)]
pub(crate) struct SourceSet {
    pub key: SourceKey,
    pub members: Vec<SourceKey>,
}
type SourceDeclarations = BTreeMap<(Url, usize), Arc<SourceSet>>;

#[derive(Clone, Debug)]
pub(crate) enum SourceEvent {
    Set(u32, Arc<SourceSet>),
    Request(u32, SourceKey, bool),
    Query(u32, SourceKey),
}
pub(crate) struct SourceTrace {
    pub work: usize,
    pub resources: BTreeMap<u32, Arc<BTreeSet<Url>>>,
    pub events: Vec<SourceEvent>,
    pub probes: Vec<(u32, usize)>,
    pub assignments: Option<Vec<crate::bibliography::bib_context_assignments::Assignment>>,
    pub explicit_requests: BTreeSet<usize>,
    pub category_members: BTreeMap<String, BTreeSet<String>>,
    scope: Option<crate::bibliography::bib_context_scope::Scope>,
    source_aliases: BTreeMap<String, String>,
    initializer_forwards: bool,
    immediate_notes: bool,
    declarations: SourceDeclarations,
    current: u32,
    next: u32,
}
impl SourceTrace {
    fn explicit_context(&self) -> bool {
        self.scope.as_ref().is_some_and(|scope| scope.active)
    }
}

pub(crate) fn has_source_sets(
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
) -> bool {
    docs.iter().any(|(_, doc)| {
        let context = Context::new(doc, signatures);
        let positions = std::cell::OnceCell::new();
        doc.syntax
            .commands
            .iter()
            .enumerate()
            .any(|(index, command)| {
                if context.literal(command.span.start) {
                    return false;
                }
                let (name, _) = signatures
                    .named_invocation(
                        &doc.text.source,
                        &doc.syntax,
                        index,
                        &BTreeSet::new(),
                        &context.declarations,
                    )
                    .unwrap_or((&command.name, command.span.clone()));
                name == "defbibentryset"
                    || signatures.invokes_builtin(name, "defbibentryset")
                    // An unresolved executed name can construct a source set.
                    // Names stored in macro bodies or consumed as declaration
                    // operands do not execute merely because a theme is loaded.
                    || matches!(name, "csuse" | "csname")
                        && !doc.syntax.control_operands.contains(&command.span.start)
                        && positions
                            .get_or_init(|| {
                                signatures.source_command_positions(&doc.text.source, &doc.syntax)
                            })
                            .contains(&command.span.start)
            })
    })
}

// Only the pending completion item may be absent or contain search words.
// Other keys retain the normal complete source-trace validation.
fn source_key_items(
    uri: &Url,
    source: &str,
    span: std::ops::Range<usize>,
    pending: Option<&std::ops::Range<usize>>,
) -> Option<Vec<Option<SourceKey>>> {
    if source[span.clone()]
        .bytes()
        .filter(|byte| *byte == b',')
        .take(100_001)
        .count()
        > 100_000
    {
        return None;
    }
    let mut result = Vec::new();
    let mut stored = 0usize;
    let mut probed = false;
    for segment in syntax::key_list_segments(source, span) {
        if pending.is_some_and(|span| segment.start <= span.start && span.end <= segment.end) {
            if probed {
                return None;
            }
            probed = true;
            result.push(None);
            continue;
        }
        let mut names = syntax::citation_list_spans(source, segment).into_iter();
        let span = names.next()?;
        if names.next().is_some() {
            return None;
        }
        stored += uri.as_str().len() + span.len() + 96;
        if stored > 32 * 1024 * 1024 || result.len() >= 100_000 {
            return None;
        }
        result.push(Some(SourceKey {
            uri: uri.clone(),
            name: source[span.clone()].into(),
            span,
        }));
    }
    if pending.is_some() && !probed {
        return None;
    }
    (!result.is_empty()).then_some(result)
}

fn source_declarations(
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
    pending: Option<(&Url, &Query)>,
) -> Option<(SourceDeclarations, usize)> {
    if !signatures.command_unchanged("defbibentryset") {
        return None;
    }
    let mut result = BTreeMap::new();
    let mut work = 0usize;
    for (uri, doc) in docs {
        let source = &doc.text.source;
        let context = Context::new(doc, signatures);
        let positions = signatures.source_command_positions(source, &doc.syntax);
        let mut declared = BTreeSet::new();
        for (index, command) in doc.syntax.commands.iter().enumerate() {
            if context.literal(command.span.start) {
                continue;
            }
            let (name, span) = signatures
                .named_invocation(
                    source,
                    &doc.syntax,
                    index,
                    &BTreeSet::new(),
                    &context.declarations,
                )
                .unwrap_or((&command.name, command.span.clone()));
            if name != "defbibentryset" {
                if signatures.invokes_builtin(name, "defbibentryset") {
                    return None;
                }
                continue;
            }
            if !positions.contains(&command.span.start) || context.operand(doc, command.span.start)
            {
                return None;
            }
            let arguments = crate::bibliography::citations::arguments(
                source,
                span.end,
                crate::bibliography::citations::Style::Set,
            );
            let pending = pending
                .filter(|(origin, query)| *origin == uri && query.command == command.span.start)
                .map(|(_, query)| &query.span);
            if arguments.fields.len() != 2
                || !arguments.fields[0].complete
                || (!arguments.fields[1].complete && pending.is_none())
            {
                return None;
            }
            let key =
                syntax::single_citation_argument(source, arguments.fields[0].argument.clone());
            if !syntax::is_static_citation_key(&key.value) || key.value == "*" {
                return None;
            }
            let members = source_key_items(
                uri,
                source,
                arguments.fields[1].argument.span.clone(),
                pending,
            )?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if members.iter().any(|member| member.name == "*") {
                return None;
            }
            work += 2 * uri.as_str().len()
                + key.value.len()
                + 192
                + members
                    .iter()
                    .map(|member| member.uri.as_str().len() + member.name.len() + 96)
                    .sum::<usize>();
            if work > 32 * 1024 * 1024 || result.len() >= 100_000 {
                return None;
            }
            declared.insert((key.span.start, key.span.end));
            result.insert(
                (uri.clone(), command.span.start),
                Arc::new(SourceSet {
                    key: SourceKey {
                        uri: uri.clone(),
                        span: key.span,
                        name: key.value,
                    },
                    members,
                }),
            );
        }
        // Any other source bibliography declaration has an unproved namespace.
        if doc
            .syntax
            .citations
            .iter()
            .any(|entry| !declared.contains(&(entry.span.start, entry.span.end)))
        {
            return None;
        }
    }
    Some((result, work))
}

pub(crate) fn source_trace(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
) -> Option<SourceTrace> {
    source_trace_query(
        ws,
        root,
        docs,
        signatures,
        root,
        None,
        TracePurpose::Keys(None),
    )
}
/// Context assignment certification belongs only to displayed labels. Source
/// sets and edit graphs keep the ordinary trace's nonloading-consumer guards.
pub(crate) fn hint_trace(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
) -> Option<SourceTrace> {
    source_trace_query(ws, root, docs, signatures, root, None, TracePurpose::Hints)
}
pub(crate) struct SourceCompletion {
    pub trace: SourceTrace,
    pub span: std::ops::Range<usize>,
    pub removals: Vec<std::ops::Range<usize>>,
    pub member: bool,
    pub nonloading: bool,
}
pub(crate) fn source_completion_candidate(
    doc: &Document,
    signatures: &CommandSignatures<'_>,
    offset: usize,
) -> bool {
    query(doc, signatures, offset, Purpose::Completion).is_some()
}
pub(crate) fn source_completion_trace(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
    origin: &Url,
    offset: usize,
) -> Option<SourceCompletion> {
    let doc = ws.documents.get(origin)?;
    let query = query(doc, signatures, offset, Purpose::Completion)?;
    let (span, removals) = completion_span(doc, &query.call, offset)?;
    let nonloading = matches!(
        query.call.name.as_str(),
        "ifentryinbib" | "ifentrycategory" | "entrydata"
    );
    if !syntax::warns_on_undefined_citation(&query.call.name)
        && query.call.name != "defbibentryset"
        && !nonloading
    {
        return None;
    }
    let member = query.call.name == "defbibentryset";
    let trace = source_trace_query(
        ws,
        root,
        docs,
        signatures,
        origin,
        Some(query),
        TracePurpose::Keys(None),
    )?;
    if trace.probes.is_empty() {
        return None;
    }
    Some(SourceCompletion {
        trace,
        span,
        removals,
        member,
        nonloading,
    })
}
enum TracePurpose<'a> {
    Keys(Option<crate::bibliography::bib_source_aliases::PrimaryRename<'a>>),
    Hints,
}
fn source_trace_query(
    ws: &Workspace,
    root: &Url,
    docs: &[(Url, &Document)],
    signatures: &CommandSignatures<'_>,
    origin: &Url,
    query: Option<Query>,
    purpose: TracePurpose<'_>,
) -> Option<SourceTrace> {
    let (rename, hint_assignments) = match purpose {
        TracePurpose::Keys(rename) => (rename, false),
        TracePurpose::Hints => (None, true),
    };
    if !ws.index_truncated.is_empty()
        || !valid_sources(docs, signatures)
        || !static_databases(ws, root)
    {
        return None;
    }
    let (declarations, mut work) = source_declarations(
        docs,
        signatures,
        query.as_ref().map(|query| (origin, query)),
    )?;
    let registry = Registry::new(root, docs, signatures);
    let active = registry.resources(root, None)?;
    work += active
        .iter()
        .map(|uri| uri.as_str().len() + 96)
        .sum::<usize>();
    if work > 32 * 1024 * 1024 {
        return None;
    }
    let resources = Arc::new(active.clone());
    let immediate_notes = immediate_notes(docs, signatures, &mut work)?;
    let initializer_forwards =
        uses_initializer_forwards(docs, signatures, immediate_notes, &mut work)?;
    let trace = SourceTrace {
        work: 0,
        resources: BTreeMap::from([(0, resources.clone())]),
        events: Vec::new(),
        probes: Vec::new(),
        assignments: hint_assignments.then(Vec::new),
        explicit_requests: BTreeSet::new(),
        category_members: BTreeMap::new(),
        source_aliases: BTreeMap::new(),
        initializer_forwards,
        immediate_notes,
        scope: (initializer_forwards
            || hint_assignments
                && docs.iter().any(|(_, doc)| {
                    crate::bibliography::bib_context_assignments::has_weak(doc, signatures)
                        || doc.syntax.definitions.iter().any(|entry| {
                            signatures
                                .forwarded_builtin(&entry.name)
                                .and_then(crate::bibliography::bib_context_assignments::target)
                                .is_some()
                        })
                }))
        .then(Default::default),
        declarations,
        current: 0,
        next: 0,
    };
    let nonloading_completion = query.as_ref().is_some_and(|query| {
        query.completion
            && matches!(
                query.call.name.as_str(),
                "ifentryinbib" | "ifentrycategory" | "entrydata"
            )
    });
    let mut analysis = Analysis {
        ws,
        root,
        signatures,
        registry,
        origin,
        query,
        labels: None,
        resource_sets: BTreeSet::from([resources]),
        alias_sections: Vec::new(),
        alias_current: 0,
        alias_uses: BTreeMap::new(),
        alias_processed: BTreeSet::new(),
        collect_keys: false,
        all_keys: None,
        trace: Some(trace),
        keys: BTreeMap::new(),
        active,
        answer: None,
        visited: BTreeSet::new(),
        stack: BTreeSet::new(),
        visits: 0,
        commands: 0,
        work,
        document: false,
        ended: false,
    };
    analysis.walk(root)?;
    analysis.alias_resources_closed(rename)?;
    if docs.iter().any(|(uri, _)| !analysis.visited.contains(uri))
        || analysis.resource_sets.iter().any(|view| {
            view.iter().any(|uri| {
                !ws.documents
                    .get(uri)
                    .is_some_and(|doc| doc.kind == DocumentKind::Bibtex)
            })
        })
    {
        return None;
    }
    let mut trace = analysis.trace?;
    if trace.scope.as_ref().is_some_and(|scope| !scope.complete()) {
        return None;
    }
    let assigned = trace
        .assignments
        .as_ref()
        .is_some_and(|assignments| !assignments.is_empty());
    if assigned && !trace.declarations.is_empty() {
        return None;
    }
    if assigned
        || nonloading_completion
        || trace
            .events
            .iter()
            .any(|event| matches!(event, SourceEvent::Query(..)))
    {
        // Alias loading has its own cache semantics. Keep the intersection
        // opaque until nonloading consumers have a complete owner proof.
        for resources in &analysis.resource_sets {
            for uri in resources.iter() {
                for entry in &ws
                    .documents
                    .get(uri)?
                    .syntax
                    .bibliography_completion
                    .entries
                {
                    for field in &entry.fields {
                        analysis.work += 64;
                        if analysis.work > 32 * 1024 * 1024 || field.name == "ids" {
                            return None;
                        }
                    }
                }
            }
        }
    }
    trace.work = analysis.work;
    Some(trace)
}

// Recognize a potential declaration for read-only lookup. source_trace still
// proves its execution and every consuming project before returning a target.
pub(crate) fn source_set_key(
    doc: &Document,
    signatures: &CommandSignatures<'_>,
    offset: usize,
) -> Option<(String, std::ops::Range<usize>)> {
    let context = Context::new(doc, signatures);
    for (index, command) in doc.syntax.commands.iter().enumerate() {
        if context.literal(command.span.start) {
            continue;
        }
        let (name, span) = signatures
            .named_invocation(
                &doc.text.source,
                &doc.syntax,
                index,
                &BTreeSet::new(),
                &context.declarations,
            )
            .unwrap_or((&command.name, command.span.clone()));
        if name != "defbibentryset" {
            continue;
        }
        let arguments = crate::bibliography::citations::arguments(
            &doc.text.source,
            span.end,
            crate::bibliography::citations::Style::Set,
        );
        let key = syntax::single_citation_argument(
            &doc.text.source,
            arguments.fields.first()?.argument.clone(),
        );
        if syntax::is_static_citation_key(&key.value)
            && key.span.start <= offset
            && offset <= key.span.end
        {
            return Some((key.value, key.span));
        }
    }
    None
}

// Bibliography preambles and entry execute fields run TeX from BBL input.
// Both can introduce sets absent from the TeX source's execution trace.
pub(crate) fn database_execution(doc: &Document) -> bool {
    doc.kind == DocumentKind::Bibtex
        && (doc
            .syntax
            .bibliography_completion
            .types
            .iter()
            .any(|entry| doc.text.source[entry.span.clone()].eq_ignore_ascii_case("preamble"))
            || doc
                .syntax
                .bibliography_completion
                .entries
                .iter()
                .any(|entry| entry.fields.iter().any(|field| field.name == "execute")))
}

// Cheap workspace gate only; section_projects proves actual project membership
// and biblatex use, leaving unrelated papers and classic BibTeX independent.
pub(crate) fn needs_namespace_proof(ws: &Workspace) -> bool {
    ws.documents.values().any(|doc| {
        database_execution(doc)
            || doc.kind == DocumentKind::Latex
                && (doc.text.source.contains("refsection") || doc.text.source.contains("bibitem"))
    })
}

// Until generated execution is proved, no source graph certifies its effects.
fn static_databases(ws: &Workspace, root: &Url) -> bool {
    let mut bytes = 0usize;
    let mut entries = 0usize;
    for uri in ws.project_documents(root) {
        let Some(doc) = ws.documents.get(&uri) else {
            return false;
        };
        if doc.kind != DocumentKind::Bibtex {
            continue;
        }
        bytes += doc.text.source.len();
        entries += doc.syntax.bibliography_completion.types.len();
        if bytes > 16 * 1024 * 1024 || entries > 100_000 || database_execution(doc) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod completion_project_tests {
    use super::*;

    #[test]
    fn completion_skips_unrelated_source_traces_and_keeps_named_citation_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("main.tex");
        let uri = Url::from_file_path(&path).unwrap();
        let source = "\\def\\citer{\\cite}\n\\defbibentryset{chosen}{member}\n\\begin{document}\n\\input{Tex/}\n\\csuse{cite}{cho}\n\\end{document}\n";
        let mut ws = Workspace::default();
        ws.config.distribution.enabled = false;
        std::fs::write(path, source).unwrap();
        ws.open(uri.clone(), source.into(), 1);
        crate::analysis::AnalysisContext::new().run(|_| {
            let path = source.find("Tex/").unwrap() + 4;
            let (selected, projects) = source_completion_projects(&ws, &uri, path).unwrap();
            assert!(!selected);
            assert!(
                projects.is_empty(),
                "file queries entered bibliography traces"
            );
            let citation = source.find("{cho}").unwrap() + 4;
            let (selected, projects) = source_completion_projects(&ws, &uri, citation).unwrap();
            assert!(!projects.is_empty());
            let (ordinary_selected, ordinary_projects) = source_projects(&ws, &uri).unwrap();
            assert_eq!(selected, ordinary_selected);
            assert_eq!(projects.len(), ordinary_projects.len());
            assert!(projects.iter().all(|project| source_completion_candidate(
                &ws.documents[&uri],
                &project.signatures,
                citation
            )));
        });
    }
}
