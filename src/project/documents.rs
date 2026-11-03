//! Shared document revisions with copy-on-write mutation for workspace snapshots.
use super::workspace::Document;
use lsp_types::Url;
use std::{
    collections::btree_map,
    collections::BTreeMap,
    ops::Index,
    sync::{Arc, Weak},
};

#[derive(Clone, Debug, Default)]
pub struct Documents(BTreeMap<Url, Arc<Document>>);

/// Weak identities validate analyses without retaining obsolete source trees.
#[derive(Clone)]
pub(crate) struct Revisions(Vec<(Url, Weak<Document>)>);

impl Revisions {
    pub(crate) fn current_except(
        &self,
        documents: &Documents,
        pending: &std::collections::BTreeSet<Url>,
    ) -> bool {
        self.0.len() == documents.len()
            && self.0.iter().all(|(uri, revision)| {
                documents.0.get(uri).is_some_and(|doc| {
                    pending.contains(uri) || revision.as_ptr() == Arc::as_ptr(doc)
                })
            })
    }
    pub(crate) fn current(&self, documents: &Documents) -> bool {
        self.0.len() == documents.len()
            && self.0.iter().all(|(uri, revision)| {
                documents
                    .0
                    .get(uri)
                    .is_some_and(|doc| revision.as_ptr() == Arc::as_ptr(doc))
            })
    }
}

/// Conditional document replacements prepared outside the live workspace lock.
#[derive(Default)]
pub(crate) struct Updates(Vec<DocumentUpdate>);

struct DocumentUpdate {
    uri: Url,
    before: Option<Weak<Document>>,
    after: Option<Arc<Document>>,
}

impl Updates {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub type Iter<'a> = std::iter::Map<
    btree_map::Iter<'a, Url, Arc<Document>>,
    fn((&'a Url, &'a Arc<Document>)) -> (&'a Url, &'a Document),
>;

impl Documents {
    pub(crate) fn weak_revision(&self, uri: &Url) -> Option<Weak<Document>> {
        self.0.get(uri).map(Arc::downgrade)
    }

    pub(crate) fn weak_revisions(&self) -> Vec<(Url, Weak<Document>)> {
        self.revisions().0
    }
    pub(crate) fn revisions(&self) -> Revisions {
        Revisions(
            self.0
                .iter()
                .map(|(uri, doc)| (uri.clone(), Arc::downgrade(doc)))
                .collect(),
        )
    }
    pub(crate) fn same_revisions(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.0.iter().all(|(uri, doc)| {
                other
                    .0
                    .get(uri)
                    .is_some_and(|other| Arc::ptr_eq(doc, other))
            })
    }

    pub(crate) fn same_sources_for(
        &self,
        other: &Self,
        scope: &std::collections::BTreeSet<Url>,
    ) -> bool {
        scope
            .iter()
            .all(|uri| match (self.get(uri), other.get(uri)) {
                (Some(left), Some(right)) => {
                    std::ptr::eq(left, right)
                        || left.version == right.version
                            && left.kind == right.kind
                            && left.text.source == right.text.source
                }
                (None, None) => true,
                _ => false,
            })
    }

    /// Retain the ordered editor inputs of an interactive query while taking
    /// closed-file observations from a completed filesystem refresh.
    pub(crate) fn overlay_open(&mut self, before: &Self) -> std::collections::BTreeSet<Url> {
        let mut changed = std::collections::BTreeSet::new();
        for (uri, doc) in &before.0 {
            if doc.version.is_some()
                && !self
                    .0
                    .get(uri)
                    .is_some_and(|current| Arc::ptr_eq(current, doc))
            {
                self.0.insert(uri.clone(), doc.clone());
                changed.insert(uri.clone());
            }
        }
        changed
    }

    pub(crate) fn updates_since(&self, before: &Self) -> Updates {
        let mut changes = Vec::new();
        for uri in self
            .0
            .keys()
            .chain(before.0.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let old = before.0.get(uri);
            let new = self.0.get(uri);
            if !old.zip(new).is_some_and(|(old, new)| Arc::ptr_eq(old, new)) {
                changes.push(DocumentUpdate {
                    uri: uri.clone(),
                    before: old.map(Arc::downgrade),
                    after: new.cloned(),
                });
            }
        }
        Updates(changes)
    }

    /// Never replace an editor revision or a reload which arrived after capture.
    pub(crate) fn apply_updates(&mut self, updates: Updates) -> std::collections::BTreeSet<Url> {
        let mut changed = std::collections::BTreeSet::new();
        for DocumentUpdate { uri, before, after } in updates.0 {
            let current = self.0.get(&uri);
            if before.as_ref().map(Weak::as_ptr) != current.map(Arc::as_ptr) {
                continue;
            }
            match after {
                Some(doc) => {
                    self.0.insert(uri.clone(), doc);
                }
                None => {
                    self.0.remove(&uri);
                }
            }
            changed.insert(uri);
        }
        changed
    }

    pub fn get(&self, uri: &Url) -> Option<&Document> {
        self.0.get(uri).map(Arc::as_ref)
    }

    pub fn get_key_value(&self, uri: &Url) -> Option<(&Url, &Document)> {
        self.0
            .get_key_value(uri)
            .map(|(uri, doc)| (uri, doc.as_ref()))
    }

    pub fn get_mut(&mut self, uri: &Url) -> Option<&mut Document> {
        self.0.get_mut(uri).map(Arc::make_mut)
    }

    pub fn insert(&mut self, uri: Url, document: Document) -> Option<Arc<Document>> {
        self.0.insert(uri, Arc::new(document))
    }

    pub fn remove(&mut self, uri: &Url) -> Option<Arc<Document>> {
        self.0.remove(uri)
    }

    pub fn contains_key(&self, uri: &Url) -> bool {
        self.0.contains_key(uri)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn keys(&self) -> btree_map::Keys<'_, Url, Arc<Document>> {
        self.0.keys()
    }

    pub fn iter(&self) -> Iter<'_> {
        self.0
            .iter()
            .map(|(uri, document)| (uri, document.as_ref()))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Document> + ExactSizeIterator {
        self.0.values().map(Arc::as_ref)
    }

    pub fn values_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = &mut Document> + ExactSizeIterator {
        self.0.values_mut().map(Arc::make_mut)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&Url, &Document) -> bool) {
        self.0.retain(|uri, document| keep(uri, document));
    }
}

impl Index<&Url> for Documents {
    type Output = Document;

    fn index(&self, uri: &Url) -> &Self::Output {
        &self.0[uri]
    }
}

impl<'a> IntoIterator for &'a Documents {
    type Item = (&'a Url, &'a Document);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for Documents {
    type Item = (Url, Document);
    type IntoIter = std::iter::Map<
        btree_map::IntoIter<Url, Arc<Document>>,
        fn((Url, Arc<Document>)) -> (Url, Document),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0
            .into_iter()
            .map(|(uri, document)| (uri, Arc::unwrap_or_clone(document)))
    }
}

impl Extend<(Url, Document)> for Documents {
    fn extend<T: IntoIterator<Item = (Url, Document)>>(&mut self, documents: T) {
        self.0
            .extend(documents.into_iter().map(|(uri, doc)| (uri, Arc::new(doc))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::workspace::Workspace;

    #[test]
    fn weak_revisions_invalidate_in_place_updates_and_do_not_pin_old_documents() {
        let uri = Url::parse("untitled:revision.tex").unwrap();
        let mut documents = Documents::default();
        documents.insert(uri.clone(), Document::new(&uri, "old".into(), Some(1)));
        let revisions = documents.revisions();
        assert!(revisions.current(&documents));
        let snapshot = documents.clone();
        documents.get_mut(&uri).unwrap().version = Some(2);
        assert!(!revisions.current(&documents));
        assert!(revisions.current(&snapshot));
        drop(snapshot);
        assert!(revisions.0[0].1.upgrade().is_none());
        let revisions = documents.revisions();
        documents.remove(&uri);
        assert!(!revisions.current(&documents));
        assert!(revisions.0[0].1.upgrade().is_none());
        documents.insert(uri.clone(), Document::new(&uri, "new".into(), Some(2)));
        assert!(!revisions.current(&documents));
    }

    #[test]
    fn snapshots_share_revisions_and_isolate_mutation_replacement_and_removal() {
        let uri = Url::parse("untitled:snapshot.tex").unwrap();
        let mut workspace = Workspace::default();
        workspace
            .documents
            .insert(uri.clone(), Document::new(&uri, "old".into(), Some(1)));
        let snapshot = workspace.clone();
        assert!(std::ptr::eq(
            &workspace.documents[&uri],
            &snapshot.documents[&uri]
        ));
        workspace.documents.get_mut(&uri).unwrap().version = Some(2);
        assert_eq!(snapshot.documents[&uri].version, Some(1));
        assert!(!std::ptr::eq(
            &workspace.documents[&uri],
            &snapshot.documents[&uri]
        ));
        workspace
            .documents
            .insert(uri.clone(), Document::new(&uri, "new".into(), Some(3)));
        assert_eq!(snapshot.documents[&uri].text.source, "old");
        workspace.documents.remove(&uri);
        assert_eq!(snapshot.documents[&uri].text.source, "old");
    }
}
