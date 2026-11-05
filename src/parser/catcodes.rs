//! Literal, scoped escape-code changes in class/style source. This is a
//! lexical reader, not a TeX evaluator: dynamic assignments and categories
//! other than escape/other retain the ordinary source grammar.
use super::syntax::{literal_integer, skip_space};

pub(crate) struct EscapeCodes {
    escapes: [bool; 128],
    groups: Vec<Vec<(u8, bool)>>,
}

pub(crate) struct Assignment {
    character: u8,
    escape: bool,
    pub(crate) end: usize,
}

impl Default for EscapeCodes {
    fn default() -> Self {
        let mut escapes = [false; 128];
        escapes[usize::from(b'\\')] = true;
        Self {
            escapes,
            groups: Vec::new(),
        }
    }
}

impl EscapeCodes {
    pub(crate) fn is_escape(&self, byte: u8) -> bool {
        self.escapes
            .get(usize::from(byte))
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn enter(&mut self) {
        self.groups.push(Vec::new());
    }

    pub(crate) fn leave(&mut self) {
        if let Some(changes) = self.groups.pop() {
            for (character, escape) in changes {
                self.escapes[usize::from(character)] = escape;
            }
        }
    }

    pub(crate) fn assign(&mut self, assignment: &Assignment, global: bool) {
        let character = assignment.character;
        if global {
            // A global assignment also cancels every pending local restore.
            for group in &mut self.groups {
                group.retain(|(saved, _)| *saved != character);
            }
        } else if let Some(group) = self.groups.last_mut() {
            if !group.iter().any(|(saved, _)| *saved == character) {
                group.push((character, self.escapes[usize::from(character)]));
            }
        }
        self.escapes[usize::from(character)] = assignment.escape;
    }

    pub(crate) fn assignment(&self, source: &str, after: usize) -> Option<Assignment> {
        let (character, end) = self.integer(source, after)?;
        let (category, end) = self.integer(source, end)?;
        let character = u8::try_from(character).ok().filter(u8::is_ascii)?;
        matches!(category, 0 | 12).then_some(Assignment {
            character,
            escape: category == 0,
            end,
        })
    }

    fn integer(&self, source: &str, after: usize) -> Option<(i32, usize)> {
        let bytes = source.as_bytes();
        let mut at = skip_space(source, after);
        if bytes.get(at) == Some(&b'=') {
            at = skip_space(source, at + 1);
        }
        let mut negative = false;
        while matches!(bytes.get(at), Some(b'+' | b'-')) {
            negative ^= bytes[at] == b'-';
            at = skip_space(source, at + 1);
        }
        if bytes.get(at) == Some(&b'`') {
            at += 1;
            let escaped = self.is_escape(*bytes.get(at)?);
            at += usize::from(escaped);
            let character = *bytes.get(at)?;
            if !character.is_ascii() {
                return None;
            }
            at += 1;
            // A control word in a character constant must have one letter.
            if escaped
                && (character.is_ascii_alphabetic() || character == b'@')
                && bytes
                    .get(at)
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'@')
            {
                return None;
            }
            let value = i32::from(character);
            return Some((if negative { -value } else { value }, at));
        }
        let (value, end) = literal_integer(source, after, numeric_boundary)?;
        // The shared reader knows ordinary backslashes. An alternate escape
        // can also introduce an expandable macro extending the number.
        if bytes.get(end).is_some_and(|byte| self.is_escape(*byte)) {
            let start = end + 1;
            let mut at = start;
            while bytes
                .get(at)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'@')
            {
                at += 1;
            }
            if !numeric_boundary(&source[start..at]) {
                return None;
            }
        }
        Some((value, end))
    }
}

fn numeric_boundary(name: &str) -> bool {
    matches!(
        name,
        "catcode"
            | "relax"
            | "global"
            | "begingroup"
            | "endgroup"
            | "bgroup"
            | "egroup"
            | "def"
            | "gdef"
            | "edef"
            | "xdef"
    )
}
