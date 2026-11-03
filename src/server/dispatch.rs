//! Preserve wire order until a handler has captured its input snapshot.
use std::{cell::RefCell, future::Future};
use tokio::sync::{oneshot, OwnedSemaphorePermit};

tokio::task_local! {
    static TURN: RefCell<Option<oneshot::Sender<()>>>;
    static CAPACITY: RefCell<Option<OwnedSemaphorePermit>>;
}

pub(crate) async fn with_capacity<F: Future>(
    permit: Option<OwnedSemaphorePermit>,
    future: F,
) -> F::Output {
    CAPACITY.scope(RefCell::new(permit), future).await
}

#[derive(Default)]
pub struct RequestQueue {
    tail: Option<oneshot::Receiver<()>>,
}

pub(crate) struct Turn(oneshot::Sender<()>);

impl Turn {
    pub(crate) async fn run<F: Future>(self, future: F) -> F::Output {
        TURN.scope(RefCell::new(Some(self.0)), future).await
    }
}

impl RequestQueue {
    /// Keep this future alive until it yields a turn, even if its request is
    /// cancelled. Dropping it early would let successors overtake its predecessor.
    pub(crate) fn turn(&mut self) -> impl Future<Output = Turn> + Send {
        let previous = self.tail.take();
        let (done, next) = oneshot::channel();
        self.tail = Some(next);
        async move {
            let _timing = crate::performance::Span::new("dispatch.wait");
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            Turn(done)
        }
    }

    pub fn ordered<F: Future + Send + 'static>(
        &mut self,
        future: F,
    ) -> impl Future<Output = F::Output> + Send {
        let turn = self.turn();
        async move { turn.await.run(future).await }
    }
}

/// Long-running handlers call this only after capturing and validating inputs.
/// Their eventual result must be checked against the current workspace again.
pub fn release() {
    let _ = CAPACITY.try_with(|permit| permit.borrow_mut().take());
    let _ = TURN.try_with(|turn| {
        if let Some(done) = turn.borrow_mut().take() {
            let _ = done.send(());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn captured_queries_release_dispatch_capacity_before_finishing() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let available = slots.clone();
        with_capacity(Some(permit), async move {
            assert_eq!(available.available_permits(), 0);
            release();
            assert_eq!(available.available_permits(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn snapshots_release_the_next_request_while_external_work_remains_pending() {
        let mut queue = RequestQueue::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (finish, finished) = oneshot::channel();
        let first_events = events.clone();
        let first = queue.ordered(async move {
            first_events.lock().unwrap().push(1);
            release();
            finished.await.unwrap();
            first_events.lock().unwrap().push(3);
        });
        let second_events = events.clone();
        let second = queue.ordered(async move {
            second_events.lock().unwrap().push(2);
        });
        let first = tokio::spawn(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), second)
            .await
            .unwrap();
        assert!(!first.is_finished());
        assert_eq!(*events.lock().unwrap(), vec![1, 2]);
        finish.send(()).unwrap();
        first.await.unwrap();
        assert_eq!(*events.lock().unwrap(), vec![1, 2, 3]);
    }
}
