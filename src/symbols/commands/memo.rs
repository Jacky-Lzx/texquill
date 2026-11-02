//! Pure token and call analyses tied to immutable documents and one grammar.
use super::SourceCall;
use crate::project::workspace::Document;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Mutex, Weak},
};

pub(super) const MAX_BYTES: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 64;
enum Value {
    Tokens {
        mode: u8,
        tokens: BTreeSet<usize>,
    },
    Calls {
        roots: Vec<String>,
        positions: BTreeSet<usize>,
        calls: Vec<SourceCall>,
    },
}
struct Entry {
    doc: Weak<Document>,
    value: Value,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct Memo(Mutex<VecDeque<Entry>>);
impl Memo {
    pub fn get(&self, doc: &Weak<Document>, mode: u8) -> Option<BTreeSet<usize>> {
        let mut entries = self.0.lock().unwrap();
        let at = entries.iter().position(|entry| {
            entry.doc.ptr_eq(doc)
                && matches!(&entry.value, Value::Tokens { mode: saved, .. } if *saved == mode)
        })?;
        let entry = entries.remove(at).unwrap();
        let Value::Tokens { tokens, .. } = &entry.value else {
            unreachable!()
        };
        let result = tokens.clone();
        entries.push_back(entry);
        Some(result)
    }
    pub fn get_calls(
        &self,
        doc: &Weak<Document>,
        roots: &[&str],
        positions: &BTreeSet<usize>,
    ) -> Option<Vec<SourceCall>> {
        let mut entries = self.0.lock().unwrap();
        let at = entries.iter().position(|entry| entry.doc.ptr_eq(doc) && matches!(&entry.value, Value::Calls { roots: saved, positions: saved_positions, .. }
            if saved.iter().map(String::as_str).eq(roots.iter().copied()) && saved_positions == positions))?;
        let entry = entries.remove(at).unwrap();
        let Value::Calls { calls, .. } = &entry.value else {
            unreachable!()
        };
        let result = calls.clone();
        entries.push_back(entry);
        Some(result)
    }
    pub fn insert(&self, doc: Weak<Document>, mode: u8, tokens: &BTreeSet<usize>) {
        let bytes = tokens.len() * 64 + std::mem::size_of::<Document>() + 128;
        if bytes > MAX_BYTES {
            return;
        }
        self.insert_entry(Entry {
            doc,
            value: Value::Tokens {
                mode,
                tokens: tokens.clone(),
            },
            bytes,
        });
    }
    pub fn insert_calls(
        &self,
        doc: Weak<Document>,
        roots: &[&str],
        positions: &BTreeSet<usize>,
        calls: &[SourceCall],
    ) {
        let bytes = positions.len() * 64
            + std::mem::size_of::<Document>()
            + 256
            + roots.iter().map(|name| name.len() + 32).sum::<usize>()
            + calls
                .iter()
                .map(|call| {
                    std::mem::size_of::<SourceCall>()
                        + call.command.name.capacity()
                        + call
                            .command
                            .argument
                            .as_ref()
                            .map_or(0, |arg| arg.value.capacity() + 128)
                })
                .sum::<usize>();
        if bytes > MAX_BYTES {
            return;
        }
        self.insert_entry(Entry {
            doc,
            value: Value::Calls {
                roots: roots.iter().map(|name| (*name).to_owned()).collect(),
                positions: positions.clone(),
                calls: calls.to_vec(),
            },
            bytes,
        });
    }
    fn insert_entry(&self, entry: Entry) {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|entry| entry.doc.strong_count() > 0);
        entries.push_back(entry);
        while entries.len() > MAX_ENTRIES
            || entries.iter().map(|entry| entry.bytes).sum::<usize>() > MAX_BYTES
        {
            entries.pop_front();
        }
    }
}
