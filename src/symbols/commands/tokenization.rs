//! Incremental lexical context for inline readers.
use super::*;

/// Incremental lexical context for catcode-changing inline commands. Keep the
/// resolved grammar until another declaration occurs, and scan each preceding
/// invocation once rather than rebuilding the document prefix for every call.
#[derive(Default)]
pub(crate) struct InlineTokenization {
    signatures: Option<CommandSignatures<'static>>,
    declarations: (usize, usize),
    scanned: usize,
    end: usize,
    aliases_seen: usize,
    aliases: BTreeMap<String, Vec<String>>,
}

impl InlineTokenization {
    /// Cheap lexical candidates keep ordinary local macro calls from forcing
    /// signature resolution. Only a path to an inline reader needs the full
    /// source-context proof of its current meaning.
    pub(crate) fn literal_name(
        &mut self,
        source: &str,
        parsed: &syntax::Syntax,
        name: &str,
    ) -> Option<&'static str> {
        self.builtin_name(
            source,
            parsed,
            name,
            &["lstinline", "mintinline", "mint", "Verb", "EscVerb"],
        )
    }

    fn builtin_name(
        &mut self,
        source: &str,
        parsed: &syntax::Syntax,
        name: &str,
        builtins: &[&'static str],
    ) -> Option<&'static str> {
        for entry in &parsed.definitions[self.aliases_seen..] {
            if let Some(target) = source_alias(source, parsed, entry).or_else(|| {
                literal_copy_target(source, parsed, entry, false).map(|(target, _)| target)
            }) {
                self.aliases
                    .entry(entry.name.clone())
                    .or_default()
                    .push(target);
            }
        }
        self.aliases_seen = parsed.definitions.len();
        let possible = {
            let mut pending = vec![name];
            let mut visited = BTreeSet::new();
            let mut found = false;
            while let Some(name) = pending.pop() {
                if builtins.contains(&name) {
                    found = true;
                    break;
                }
                if visited.insert(name) {
                    if let Some(targets) = self.aliases.get(name) {
                        pending.extend(targets.iter().map(String::as_str));
                    }
                }
            }
            found
        };
        if !possible {
            return None;
        }
        self.refresh(source, parsed);
        let signatures = self.signatures.as_ref().unwrap();
        builtins
            .iter()
            .copied()
            .find(|builtin| signatures.invokes_builtin(name, builtin))
    }

    /// File-verb resource lists change catcodes only when read from the input,
    /// never after an outer macro has already collected their tokens.
    pub(crate) fn resource_bracket(
        &mut self,
        source: &str,
        parsed: &syntax::Syntax,
        controls: &BTreeSet<usize>,
    ) -> Option<usize> {
        let command = parsed.commands.last()?;
        let (name, end, named) = match command.name.as_str() {
            "csuse" => {
                let argument = command.argument.as_ref()?;
                if !static_invocation_name(&argument.value) {
                    return None;
                }
                (argument.value.as_str(), argument.outer_span.end, true)
            }
            "endcsname" => {
                let index = parsed.commands.len().checked_sub(2)?;
                let span = literal_invocation_name(source, &parsed.commands, index)?;
                (&source[span], command.span.end, true)
            }
            name => (name, command.span.end, false),
        };
        let builtin = self.builtin_name(
            source,
            parsed,
            name,
            &["newrefsection", "refsection", "begin"],
        )?;
        let signatures = self.signatures.as_ref()?;
        if named
            && !(if command.name == "csuse" {
                signatures.command_unchanged("csuse")
            } else {
                signatures.command_unchanged("csname") && signatures.command_unchanged("endcsname")
            })
        {
            return None;
        }
        let end = if builtin == "begin" {
            let environment = syntax::environment_name_argument(source, end)?;
            if environment.value != "refsection" || !signatures.command_unchanged("refsection") {
                return None;
            }
            environment.outer_span.end
        } else {
            end
        };
        let at = syntax::skip_space(source, end);
        if source.as_bytes().get(at) != Some(&b'[')
            || self.contains(source, parsed, controls, command.span.start)
        {
            return None;
        }
        Some(at)
    }

    fn refresh(&mut self, source: &str, parsed: &syntax::Syntax) {
        let declarations = (parsed.definitions.len(), parsed.environments.len());
        if self.signatures.is_none() || self.declarations != declarations {
            let mut signatures = CommandSignatures::source_context([(source, parsed)]);
            signatures.inline_copy_operands(parsed);
            self.signatures = Some(CommandSignatures {
                common_grammars: Default::default(),
                local: signatures
                    .local
                    .into_iter()
                    .map(|(name, value)| (Cow::Owned(name.into_owned()), value))
                    .collect(),
                tex_operands: signatures
                    .tex_operands
                    .into_iter()
                    .map(|(name, value)| (Cow::Owned(name.into_owned()), value))
                    .collect(),
                literal_operands: signatures
                    .literal_operands
                    .into_iter()
                    .map(|(name, value)| (Cow::Owned(name.into_owned()), value))
                    .collect(),
                unverified_names: signatures
                    .unverified_names
                    .into_iter()
                    .map(|name| Cow::Owned(name.into_owned()))
                    .collect(),
                detokenize_boundary_known: signatures.detokenize_boundary_known,
                identity_parameters: signatures.identity_parameters,
                hook_arities: crate::symbols::hook_arguments::Arities::default(),
            });
            self.declarations = declarations;
            self.scanned = 0;
        }
    }

    pub(crate) fn contains(
        &mut self,
        source: &str,
        parsed: &syntax::Syntax,
        controls: &BTreeSet<usize>,
        at: usize,
    ) -> bool {
        if at < self.end {
            return true;
        }
        self.refresh(source, parsed);
        self.scanned = self.scanned.max(
            parsed
                .commands
                .partition_point(|command| command.span.start < self.end),
        );
        if let Some(end) = self.signatures.as_ref().unwrap().pretokenized_literal_end(
            source,
            parsed,
            controls,
            at,
            &mut self.scanned,
        ) {
            self.end = end;
            true
        } else {
            false
        }
    }
}
