//! Static hook arities bound data operands; they do not simulate hook execution.
use super::commands::{self as signature, CommandSignatures};
use crate::parser::syntax;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const USES: &[&str] = &[
    "UseHook",
    "UseOneTimeHook",
    "UseHookWithArguments",
    "UseOneTimeHookWithArguments",
];
const DECLARATIONS: &[&str] = &[
    "NewHook",
    "NewReversedHook",
    "NewMirroredHookPair",
    "NewHookWithArguments",
    "NewReversedHookWithArguments",
    "NewMirroredHookPairWithArguments",
];

fn field(source: &str, after: usize) -> Option<(&str, usize)> {
    let at = syntax::skip_space(source, after);
    if source.as_bytes().get(at) == Some(&b'{') {
        let (argument, end) = syntax::group(source, at, b'{', b'}')?;
        let argument = syntax::single_key_argument(source, argument);
        Some((&source[argument.span], end))
    } else {
        let value = syntax::required_token(source, at)?;
        Some((value, at + value.len()))
    }
}

fn integer(value: &str) -> Option<usize> {
    value
        .trim_matches([' ', '\t', '\r', '\n'])
        .parse()
        .ok()
        .filter(|&n| n <= 9)
}

fn literal(name: &str) -> bool {
    syntax::is_static_key(name) && name != "." && !name.starts_with("./")
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Arities {
    declared: BTreeMap<String, Option<usize>>,
    one_time: BTreeSet<String>,
    dynamic: bool,
}

impl Arities {
    pub(super) fn cache_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .declared
                .keys()
                .map(|name| {
                    name.capacity() + 12 * std::mem::size_of::<(String, Option<usize>)>() + 256
                })
                .sum::<usize>()
            + self
                .one_time
                .iter()
                .map(|name| name.capacity() + 12 * std::mem::size_of::<String>() + 256)
                .sum::<usize>()
    }

    pub(crate) fn collect(
        documents: &[(&str, &syntax::Syntax)],
        signatures: &CommandSignatures<'_>,
    ) -> Self {
        let mut result = Self::default();
        if !documents.iter().any(|(source, _)| source.contains("Hook")) {
            return result;
        }
        let roots: Vec<_> = DECLARATIONS.iter().chain(USES).copied().collect();
        let Some(possible) = signature::possible_builtin_aliases(
            documents.iter().copied(),
            roots.iter().copied(),
            false,
        ) else {
            result.dynamic = true;
            return result;
        };
        let mut work = 0usize;
        for &(source, parsed) in documents {
            work = work.saturating_add(source.len() + parsed.commands.len() * 64);
            if work > 32 * 1024 * 1024 {
                result.dynamic = true;
                break;
            }
            if !parsed.commands.iter().any(|command| {
                possible.contains_key(&command.name)
                    || matches!(command.name.as_str(), "csuse" | "csname")
            }) {
                continue;
            }
            let active = signatures.source_command_positions(source, parsed);
            let alias_bodies = signatures.builtin_alias_body_positions(source, parsed, &roots);
            let declarations: BTreeSet<_> = parsed
                .definitions
                .iter()
                .map(|entry| (entry.span.start, entry.span.end))
                .collect();
            let empty = BTreeSet::new();
            for (index, command) in parsed.commands.iter().enumerate() {
                if parsed.control_operands.contains(&command.span.start)
                    || declarations.contains(&(command.span.start, command.span.end))
                    || alias_bodies.contains(&command.span.start)
                {
                    continue;
                }
                let (name, invocation) = signatures
                    .named_invocation(source, parsed, index, &empty, &declarations)
                    .unwrap_or((&command.name, command.span.clone()));
                for &builtin in possible.get(name).into_iter().flatten() {
                    if !signatures.invokes_builtin(name, builtin)
                        && signatures.has_custom_signature(name, builtin)
                    {
                        continue;
                    }
                    if !DECLARATIONS.contains(&builtin) && !builtin.starts_with("UseOneTime") {
                        continue;
                    }
                    let Some((raw, mut after)) = field(source, invocation.end) else {
                        result.dynamic = true;
                        continue;
                    };
                    let raw = raw.trim_matches([' ', '\t', '\r', '\n']);
                    if !literal(raw) {
                        result.dynamic = true;
                        continue;
                    }
                    if builtin.starts_with("UseOneTime") {
                        result.one_time.insert(raw.into());
                        continue;
                    }
                    let mut names = vec![raw];
                    if builtin.starts_with("NewMirrored") {
                        let Some((raw, end)) = field(source, after) else {
                            result.dynamic = true;
                            continue;
                        };
                        after = end;
                        let raw = raw.trim_matches([' ', '\t', '\r', '\n']);
                        if !literal(raw) {
                            result.dynamic = true;
                            continue;
                        }
                        names.push(raw);
                    }
                    let count = if builtin.ends_with("WithArguments") {
                        field(source, after).and_then(|(value, _)| integer(value))
                    } else {
                        Some(0)
                    };
                    let count = count.filter(|_| {
                        active.contains(&invocation.start)
                            && signatures.invokes_builtin(name, builtin)
                    });
                    for name in names {
                        result
                            .declared
                            .entry(name.into())
                            .and_modify(|n| *n = None)
                            .or_insert(count);
                    }
                }
            }
        }
        result
    }

    /// An exact structural arity is useful for help and for preserving the
    /// command after a known zero-argument call. Lifecycle ambiguity stays out.
    pub(crate) fn count(
        &self,
        source: &str,
        after: usize,
        builtin: &str,
        upper: bool,
    ) -> Option<usize> {
        let (raw, after) = field(source, after)?;
        let name = if builtin.starts_with("UseOneTime") {
            raw.trim_matches([' ', '\t', '\r', '\n'])
        } else {
            raw
        };
        let supplied = if builtin.ends_with("WithArguments") {
            field(source, after).and_then(|(value, _)| integer(value))
        } else {
            Some(0)
        };
        if self.dynamic || !literal(name) {
            // An unresolved name does not establish a structural grammar.
            // The independent edit validator still protects possible data.
            return None;
        }
        if self.one_time.contains(name) {
            return upper.then(|| {
                self.declared
                    .get(name)
                    .copied()
                    .unwrap_or(Some(usize::from(name == "label")))
                    .unwrap_or(9)
                    .max(supplied.unwrap_or(9))
            });
        }
        if let Some(count) = self.declared.get(name) {
            return count.or_else(|| upper.then_some(9));
        }
        if name == "label" {
            return Some(1);
        }
        supplied.or_else(|| upper.then_some(9))
    }
}
