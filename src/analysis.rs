//! Request-local context with revision-checked, connection-owned pure analyses.
use crate::{diagnostics::diagnostic_inputs::Files, project::source_inputs, symbols::query_cache};
use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc};

pub(crate) struct AnalysisContext {
    pub(crate) groups: Rc<RefCell<crate::parser::group_cache::Cache>>,
    pub(crate) symbols: RefCell<query_cache::Cache>,
    pub(crate) sources: RefCell<source_inputs::Query>,
    observed: Option<Rc<RefCell<Files>>>,
    cancellation: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    cache_enabled: bool,
    persistent_indexes: bool,
    pub(crate) binding: Option<Rc<crate::analysis_cache::Binding>>,
}

// Compatibility bridge for synchronous readers. The actual state is owned by
// the explicit context, never by separate subsystem thread-local caches.
thread_local! {
    static CURRENT: RefCell<Option<Rc<AnalysisContext>>> = const { RefCell::new(None) };
}

struct Restore(Option<Rc<AnalysisContext>>);
impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}

impl AnalysisContext {
    pub(crate) fn new() -> Rc<Self> {
        let parent = Self::current();
        Rc::new(Self {
            groups: parent
                .as_ref()
                .map(|parent| parent.groups.clone())
                .unwrap_or_default(),
            symbols: RefCell::default(),
            sources: RefCell::default(),
            observed: parent.as_ref().and_then(|parent| parent.observed.clone()),
            cancellation: parent
                .as_ref()
                .and_then(|parent| parent.cancellation.clone()),
            cache_enabled: true,
            binding: parent.as_ref().and_then(|parent| parent.binding.clone()),
            persistent_indexes: parent
                .as_ref()
                .is_some_and(|parent| parent.persistent_indexes),
        })
    }

    pub(crate) fn for_workspace(
        workspace: &crate::project::workspace::Workspace,
        epoch: u64,
        revision: u64,
        cache: Arc<crate::analysis_cache::Cache>,
    ) -> Rc<Self> {
        let mut context = Self::new();
        Rc::get_mut(&mut context).unwrap().binding = Some(Rc::new(
            crate::analysis_cache::Binding::new(workspace, epoch, revision, cache),
        ));
        Rc::get_mut(&mut context).unwrap().persistent_indexes = true;
        context
    }

    pub(crate) fn observing_files(&self) -> bool {
        self.observed.is_some()
    }

    pub(crate) fn persistent_indexes(&self) -> bool {
        self.persistent_indexes
    }

    pub(crate) fn source_document(
        source: &str,
        parsed: &crate::parser::syntax::Syntax,
    ) -> Option<std::sync::Weak<crate::project::workspace::Document>> {
        Self::current()?.binding.as_ref()?.document(source, parsed)
    }

    pub(crate) fn cancellable(cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Rc<Self> {
        let mut context = Self::new();
        let state = Rc::get_mut(&mut context).unwrap();
        let parent = state.cancellation.take();
        state.cancellation = Some(Arc::new(move || {
            parent.as_ref().is_some_and(|parent| parent()) || cancelled()
        }));
        state.cache_enabled = false;
        context
    }

    /// Mutating work may share certified document/grammar facts, never workspace indexes.
    pub(crate) fn lexical() -> Rc<Self> {
        let mut context = Self::new();
        Rc::get_mut(&mut context).unwrap().cache_enabled = false;
        Rc::get_mut(&mut context).unwrap().persistent_indexes = false;
        context
    }

    pub(crate) fn observing() -> Rc<Self> {
        let mut context = Self::new();
        let state = Rc::get_mut(&mut context).unwrap();
        state.observed = Some(Rc::default());
        state.cache_enabled = false;
        context
    }

    pub(crate) fn current() -> Option<Rc<Self>> {
        CURRENT.with(|current| current.borrow().clone())
    }

    /// Only explicit immutable analysis scopes cache workspace-derived values.
    /// Installing cancellation or file observation alone does not cache scratch
    /// workspaces which helpers may still be constructing or mutating.
    pub(crate) fn current_query() -> Option<Rc<Self>> {
        Self::current().filter(|context| context.cache_enabled)
    }

    /// A context is local to one worker and must never span an async suspension.
    /// Nested analyses get independent caches and restore their parent on panic.
    pub(crate) fn run<T>(self: &Rc<Self>, work: impl FnOnce(&Self) -> T) -> T {
        let _restore = Restore(CURRENT.with(|current| current.replace(Some(self.clone()))));
        work(self)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|cancelled| cancelled())
    }

    pub(crate) fn check(&self) -> crate::rpc::Result<()> {
        if self.cancelled() {
            Err(crate::rpc::Error::request_cancelled())
        } else {
            Ok(())
        }
    }

    pub(crate) fn observe(&self, path: &Path) {
        if let Some(files) = &self.observed {
            files.borrow_mut().observe(path);
        }
    }

    pub(crate) fn take_files(&self) -> Arc<Files> {
        Arc::new(std::mem::take(
            &mut *self.observed.as_ref().unwrap().borrow_mut(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn nested_queries_observe_files_without_retaining_state_between_captures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.tex");
        std::fs::write(&path, "first").unwrap();
        let (_, files) = Files::capture(|| {
            assert!(AnalysisContext::current_query().is_none());
            AnalysisContext::new().run(|context| {
                assert!(AnalysisContext::current_query().is_some());
                context.observe(&path);
            });
        });
        assert!(files.current());
        std::fs::write(&path, "changed contents").unwrap();
        assert!(!files.current());
        let (_, empty) = Files::capture(|| ());
        assert_eq!(empty.bytes, 0);
        assert!(AnalysisContext::current().is_none());
    }

    #[test]
    fn nested_contexts_restore_the_parent_and_share_its_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let parent = AnalysisContext::cancellable(move || signal.load(Ordering::Relaxed));
        parent.run(|context| {
            assert!(!context.cancelled());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                AnalysisContext::new().run(|nested| {
                    assert!(!std::ptr::eq(context, nested));
                    cancelled.store(true, Ordering::Relaxed);
                    assert!(nested.cancelled());
                    panic!("abandon nested analysis");
                });
            }));
            assert!(result.is_err());
            assert!(Rc::ptr_eq(&parent, &AnalysisContext::current().unwrap()));
            assert!(context.cancelled());
        });
        assert!(AnalysisContext::current().is_none());
    }

    #[test]
    fn updates_share_lexical_reads_without_caching_mutable_workspace_queries() {
        AnalysisContext::new().run(|parent| {
            AnalysisContext::lexical().run(|update| {
                assert!(AnalysisContext::current_query().is_none());
                assert!(Rc::ptr_eq(&parent.groups, &update.groups));
                AnalysisContext::new().run(|query| {
                    assert!(AnalysisContext::current_query().is_some());
                    assert!(Rc::ptr_eq(&update.groups, &query.groups));
                });
                assert!(AnalysisContext::current_query().is_none());
            });
            assert!(AnalysisContext::current_query().is_some());
        });
    }
}
