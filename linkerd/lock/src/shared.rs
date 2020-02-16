use crate::error::Error;
use crate::waiter::{Notify, Wait};
use futures::{Async, Poll};
use std::sync::Arc;
use tracing::trace;

pub(crate) struct Shared<S> {
    state: State<S>,
    waiters: Vec<Notify>,
}

enum State<S> {
    /// A Lock is holding the service.
    Claimed,

    /// The inner service is available.
    Unclaimed(S),

    /// The lock has failed.
    Failed(Arc<Error>),
}

// === impl Shared ===

impl<S> Shared<S> {
    pub fn new(service: S) -> Self {
        Self {
            waiters: Vec::new(),
            state: State::Unclaimed(service),
        }
    }

    pub fn try_acquire(&mut self) -> Result<Option<S>, Arc<Error>> {
        match std::mem::replace(&mut self.state, State::Claimed) {
            // This lock has acquired the service.
            State::Unclaimed(svc) => Ok(Some(svc)),
            // The service is already claimed by a lock.
            State::Claimed => Ok(None),
            // The service failed, so reset the state immediately so that all
            // locks may be notified.
            State::Failed(error) => {
                self.state = State::Failed(error.clone());
                Err(error)
            }
        }
    }

    pub fn poll_acquire(&mut self, wait: &Wait) -> Poll<S, Arc<Error>> {
        match self.try_acquire() {
            Ok(Some(svc)) => Ok(Async::Ready(svc)),
            Ok(None) => {
                self.register(&wait);
                Ok(Async::NotReady)
            }
            Err(error) => Err(error),
        }
    }

    pub fn register(&mut self, wait: &Wait) {
        if let Some(notify) = wait.get_notify() {
            self.waiters.push(notify);
        }
        wait.register();
        debug_assert!(wait.is_waiting());
    }

    pub fn release(&mut self, service: S) {
        trace!(waiters = self.waiters.len(), "releasing");
        debug_assert!(match self.state {
            State::Claimed => true,
            _ => false,
        });
        self.state = State::Unclaimed(service);
        self.notify_next_waiter();
    }

    pub fn notify_next_waiter(&mut self) {
        while let Some(waiter) = self.waiters.pop() {
            if waiter.notify() {
                return;
            }
        }
    }

    pub fn fail(&mut self, error: Arc<Error>) {
        trace!(waiters = self.waiters.len(), %error, "failing");
        debug_assert!(match self.state {
            State::Claimed => true,
            _ => false,
        });
        self.state = State::Failed(error);

        while let Some(waiter) = self.waiters.pop() {
            waiter.notify();
        }
    }
}
