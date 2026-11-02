//! Declaration facts depend on one exact syntax, not on a project's meanings.
use super::*;
use crate::analysis::AnalysisContext;
use std::sync::Arc;

pub(crate) struct Declarations(pub(super) Vec<Declaration>);

pub(super) struct Declaration {
    pub signature: Option<Signature>,
    pub tex: Option<TexOperands>,
    pub copied: Option<String>,
    pub alias: Option<String>,
    pub ending: Option<Signature>,
    pub ending_alias: Option<String>,
    pub expanded: bool,
    pub identity_body: bool,
    pub position: Option<usize>,
}

impl Declarations {
    pub fn get(source: &str, parsed: &syntax::Syntax) -> Arc<Self> {
        let binding = AnalysisContext::current().and_then(|context| context.binding.clone());
        let document = binding
            .as_ref()
            .and_then(|binding| binding.document(source, parsed));
        if let Some(value) = binding
            .as_ref()
            .zip(document.as_ref())
            .and_then(|(binding, doc)| binding.cache.declarations(doc))
        {
            return value;
        }
        let value = Arc::new(Self(
            parsed
                .definitions
                .iter()
                .map(|entry| (entry, false))
                .chain(parsed.environments.iter().map(|entry| (entry, true)))
                .map(|(entry, environment)| Declaration::new(source, parsed, entry, environment))
                .collect(),
        ));
        if let Some((binding, document)) = binding.zip(document) {
            binding.cache.insert_declarations(document, value.clone());
        }
        value
    }

    pub fn cache_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.0.capacity() * std::mem::size_of::<Declaration>()
            + self
                .0
                .iter()
                .map(|fact| {
                    fact.signature.as_ref().map_or(0, grammar_signature_bytes)
                        + fact.ending.as_ref().map_or(0, grammar_signature_bytes)
                        + fact.tex.as_ref().map_or(0, grammar_tex_bytes)
                        + [&fact.copied, &fact.alias, &fact.ending_alias]
                            .into_iter()
                            .map(|value| value.as_ref().map_or(0, String::capacity))
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
}

impl Declaration {
    fn new(source: &str, parsed: &syntax::Syntax, entry: &Entry, environment: bool) -> Self {
        let mut signature = if environment {
            environment_start_signature(source, parsed, entry)
        } else {
            source_definition(source, parsed, entry, false)
        };
        let tex_definition = TexOperands::definition(source, parsed, entry);
        let tex_known = tex_definition.is_some();
        let tex = tex_definition
            .filter(|(operands, _)| operands.delimited() || signature.is_none())
            .map(|(operands, _)| operands);
        if TexOperands::declares(entry) && (tex.is_some() || !tex_known) {
            signature = None;
        }
        let bodies = environment
            .then(|| environment_bodies(source, parsed, entry))
            .flatten();
        let copied = literal_copy_target(source, parsed, entry, environment).map(|(name, _)| name);
        let alias = signature
            .as_ref()
            .filter(|s| s.parameters.is_empty())
            .and_then(|_| {
                if environment {
                    bodies
                        .as_ref()
                        .and_then(|(begin, _)| body_alias(source, begin))
                } else {
                    source_alias(source, parsed, entry)
                }
            });
        let ending = environment
            .then(|| {
                signature
                    .as_ref()
                    .filter(|_| bodies.is_some() || entry.detail == "Defined with \\newtheorem")
                    .map(|_| Signature {
                        name: format!("end{}", entry.name),
                        parameters: vec![],
                        documentation: entry.detail.clone(),
                        starred: false,
                        nested_brackets: true,
                    })
            })
            .flatten();
        let ending_alias = ending
            .as_ref()
            .and_then(|_| bodies.as_ref().and_then(|(_, end)| body_alias(source, end)));
        let expanded = matches!(
            entry.detail.strip_prefix("Defined with \\"),
            Some("edef" | "xdef" | "csedef" | "csxdef")
        );
        let identity_body = !environment
            && command_definition_body(source, parsed, entry)
                .is_some_and(|body| source[body.span].trim() == "#1");
        let declaration = entry.detail.strip_prefix("Defined with \\");
        let end = parsed
            .commands
            .partition_point(|command| command.span.start < entry.span.start);
        let position = parsed.commands[..end]
            .iter()
            .rev()
            .find(|command| Some(command.name.as_str()) == declaration)
            .map(|command| command.span.start);
        Self {
            signature,
            tex,
            copied,
            alias,
            ending,
            ending_alias,
            expanded,
            identity_body,
            position,
        }
    }
}
