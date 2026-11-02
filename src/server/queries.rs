//! Analysis workers retain capacity until computation actually finishes.
use crate::rpc::{Error, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone)]
pub(crate) struct Queries {
    pub(crate) analyses: Arc<crate::analysis_cache::Cache>,
    slots: Arc<Semaphore>,
    background: Arc<Semaphore>,
    closing: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

#[derive(Clone)]
struct Cancellation {
    request: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
}

impl Cancellation {
    pub(crate) fn check(&self) -> Result<()> {
        if self.request.load(Ordering::Relaxed) || self.closing.load(Ordering::Relaxed) {
            Err(Error::request_cancelled())
        } else {
            Ok(())
        }
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct Active(Queries);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.idle.notify_waiters();
    }
}

impl Default for Queries {
    fn default() -> Self {
        Self::new(4)
    }
}

impl Queries {
    fn new(limit: usize) -> Self {
        Self {
            analyses: Arc::default(),
            slots: Arc::new(Semaphore::new(limit)),
            // Background work must leave capacity for completion/signatures.
            background: Arc::new(Semaphore::new(limit.saturating_sub(1).max(1))),
            closing: Arc::default(),
            active: Arc::default(),
            idle: Arc::default(),
        }
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&crate::analysis::AnalysisContext) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.run_with_priority(false, work).await
    }

    pub(crate) async fn run_background<T: Send + 'static>(
        &self,
        work: impl FnOnce(&crate::analysis::AnalysisContext) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.run_with_priority(true, work).await
    }

    async fn run_with_priority<T: Send + 'static>(
        &self,
        background: bool,
        work: impl FnOnce(&crate::analysis::AnalysisContext) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        // Acquire this first: queued background jobs must not take places in
        // the interactive semaphore's FIFO ahead of a completion request.
        let background = if background {
            Some(
                self.background
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::request_cancelled())?,
            )
        } else {
            None
        };
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::request_cancelled())?;
        let request = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(request.clone());
        let cancellation = Cancellation {
            request,
            closing: self.closing.clone(),
        };
        cancellation.check()?;
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = Active(self.clone());
        tokio::task::spawn_blocking(move || {
            // Cancelling the async waiter cannot release either permit while
            // its synchronous analysis is still using worker capacity.
            let (_background, _permit, _active) = (background, permit, active);
            cancellation.check()?;
            let signal = cancellation.clone();
            let result =
                crate::analysis::AnalysisContext::cancellable(move || signal.check().is_err())
                    .run(work)?;
            cancellation.check()?;
            Ok(result)
        })
        .await
        .map_err(|_| Error::internal_error())?
    }

    pub(crate) async fn shutdown(&self) {
        self.closing.store(true, Ordering::Relaxed);
        self.slots.close();
        self.background.close();
        loop {
            let idle = self.idle.notified();
            if self.active.load(Ordering::SeqCst) == 0 {
                break;
            }
            idle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn background_backlogs_reserve_interactive_capacity_and_eventually_finish() {
        let queries = Queries::new(4);
        let mut finishes = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..3 {
            let worker = queries.clone();
            let (started, start) = oneshot::channel();
            let (finish, finished) = oneshot::channel();
            workers.push(tokio::spawn(async move {
                worker
                    .run_background(move |_| {
                        let _ = started.send(());
                        let _ = finished.blocking_recv();
                        Ok(())
                    })
                    .await
            }));
            start.await.unwrap();
            finishes.push(finish);
        }
        let waiting = queries.clone();
        let (queued, queue_started) = oneshot::channel();
        let later = tokio::spawn(async move {
            let _ = queued.send(());
            waiting.run_background(|_| Ok(7)).await
        });
        queue_started.await.unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), queries.run(|_| Ok(42))).await;
        let was_queued = !later.is_finished();
        for finish in finishes {
            let _ = finish.send(());
        }
        for worker in workers {
            worker.await.unwrap().unwrap();
        }
        assert_eq!(
            result
                .expect("completion waited behind background work")
                .unwrap(),
            42
        );
        assert!(was_queued);
        assert_eq!(later.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn cancelled_background_workers_retain_both_permits_and_shutdown_wakes_waiters() {
        let queries = Queries::new(2);
        let worker = queries.clone();
        let (started, start) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let running = tokio::spawn(async move {
            worker
                .run_background(move |cancel| {
                    let _ = started.send(());
                    let _ = finished.blocking_recv();
                    cancel.check()
                })
                .await
        });
        start.await.unwrap();
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        assert_eq!(queries.background.available_permits(), 0);
        assert_eq!(queries.slots.available_permits(), 1);
        assert_eq!(queries.run(|_| Ok(3)).await.unwrap(), 3);
        let queued = queries.clone();
        let waiting = tokio::spawn(async move {
            queued
                .run_background::<()>(|_| panic!("queued work ran during shutdown"))
                .await
        });
        let stopping = queries.clone();
        let shutdown = tokio::spawn(async move { stopping.shutdown().await });
        let result = waiting.await.unwrap();
        let _ = finish.send(());
        shutdown.await.unwrap();
        assert_eq!(result.unwrap_err(), Error::request_cancelled());
    }

    #[tokio::test]
    async fn cancelled_workers_keep_capacity_and_shutdown_signals_running_work() {
        let queries = Queries::new(1);
        let (started, start) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let first_queries = queries.clone();
        let first = tokio::spawn(async move {
            first_queries
                .run(move |cancel| {
                    let _ = started.send(());
                    let _ = finished.blocking_recv();
                    cancel.check()
                })
                .await
        });
        start.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(queries.slots.available_permits(), 0);
        let waiting_queries = queries.clone();
        let waiting = tokio::spawn(async move {
            waiting_queries
                .run::<()>(|_| panic!("queued work ran after shutdown"))
                .await
        });
        let shutting_down = tokio::spawn({
            let queries = queries.clone();
            async move { queries.shutdown().await }
        });
        tokio::task::yield_now().await;
        assert!(!shutting_down.is_finished());
        let _ = finish.send(());
        assert_eq!(
            waiting.await.unwrap().unwrap_err(),
            Error::request_cancelled()
        );
        shutting_down.await.unwrap();
    }
}
