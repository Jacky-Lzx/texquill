//! Bounded reuse of pure lexical group reads during one analysis or update.
use super::syntax::Argument;
use std::collections::HashMap;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(super) struct Key {
    source: usize,
    length: usize,
    start: usize,
    open: u8,
    close: u8,
    comments: bool,
    nested: bool,
}

impl Key {
    pub(super) fn new(
        source: &str,
        start: usize,
        open: u8,
        close: u8,
        comments: bool,
        nested: bool,
    ) -> Self {
        Self {
            source: source.as_ptr() as usize,
            length: source.len(),
            start,
            open,
            close,
            comments,
            nested,
        }
    }
}

#[derive(Default)]
pub(crate) struct Cache {
    groups: HashMap<Key, (Argument, usize)>,
    bytes: usize,
}

impl Cache {
    pub(super) fn get(&self, key: Key, source: &str) -> Option<(Argument, usize)> {
        let (argument, end) = self.groups.get(&key)?;
        // Scratch sources can change or reuse an allocation during an update.
        // Pointer identity is only a lookup hint: certify the entire group,
        // including its delimiters, before reusing this pure lexical result.
        (source.as_bytes().get(key.start) == Some(&key.open)
            && source.as_bytes().get(end - 1) == Some(&key.close)
            && source.get(argument.span.clone()) == Some(argument.value.as_str()))
        .then(|| (argument.clone(), *end))
    }

    pub(super) fn insert(&mut self, key: Key, result: &(Argument, usize)) {
        let bytes = result.0.value.len() + std::mem::size_of::<(Key, Argument, usize)>() + 64;
        if self.groups.len() < MAX_ENTRIES && bytes <= MAX_BYTES - self.bytes {
            // Charge replacements too, keeping the bound conservative even
            // when a scratch allocation repeatedly changes its contents.
            self.groups.insert(key, result.clone());
            self.bytes += bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lexical_caches_fall_back_without_exceeding_the_budget() {
        let source = format!("{{{}}}", "x".repeat(MAX_BYTES));
        let key = Key::new(&source, 0, b'{', b'}', true, true);
        let result = (
            Argument {
                value: source[1..source.len() - 1].into(),
                span: 1..source.len() - 1,
                outer_span: 0..source.len(),
            },
            source.len(),
        );
        let mut cache = Cache::default();
        cache.insert(key, &result);
        assert!(cache.get(key, &source).is_none());
        assert_eq!(cache.bytes, 0);
        cache.bytes = MAX_BYTES;
        cache.insert(key, &result);
        assert!(cache.groups.is_empty());
        assert_eq!(cache.bytes, MAX_BYTES);
    }
}
