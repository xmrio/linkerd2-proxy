//! A middleware for sharing an inner service via mutual exclusion.

#![deny(warnings, rust_2018_idioms)]

pub mod error;
mod layer;
#[cfg(test)]
mod test;

pub use self::layer::LockLayer;
use futures::task::AtomicTask;
use futures::{Async, Future, Poll};
use std::sync::{Arc, Mutex, Weak};
use tracing::trace;

/// Guards access to an inner service with a `tokio::sync::lock::Lock`.
///
/// As the service is polled to readiness, the lock is acquired and the inner
/// service is polled. If the sevice is cloned, the service's lock state is not
/// retained by the clone.
///
/// The inner service's errors are coerced to the cloneable `C`-typed error so
/// that the error may be returned to all clones of the lock. By default, errors
/// are propagated through the `Poisoned` type, but they may be propagated
/// through custom types as well.
pub struct Lock<S> {
    state: LockState<S>,
    shared: Arc<Mutex<Shared<S>>>,
}

pub struct ResponseFuture<F>(F);

enum LockState<S> {
    Released,
    Waiting(Arc<AtomicTask>),
    Acquired(S),
    Failed(Arc<error::Error>),
}

struct Shared<S> {
    state: SharedState<S>,
    waiters: Vec<Weak<AtomicTask>>,
}

enum SharedState<S> {
    /// A Lock is holding the service.
    Acquired,

    /// The inner service is available.
    Available(S),

    /// The lock has failed.
    Failed(Arc<error::Error>),
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(service: S) -> Self {
        Self {
            state: LockState::Released,
            shared: Arc::new(Mutex::new(Shared {
                waiters: Vec::new(),
                state: SharedState::Available(service),
            })),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            state: LockState::Released,
            shared: self.shared.clone(),
        }
    }
}

impl<S> Drop for Lock<S> {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, LockState::Released);
        match state {
            LockState::Acquired(service) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.release(service);
                }
            }

            LockState::Waiting(task) => {
                if let Ok(mut shared) = self.shared.lock() {
                    if Arc::weak_count(&task) == 0 {
                        if let SharedState::Available(_) = shared.state {
                            shared.notify_next_waiter();
                        }
                    }
                }
            }

            LockState::Released | LockState::Failed(_) => {}
        }
    }
}

impl<T, S> tower::Service<T> for Lock<S>
where
    S: tower::Service<T>,
    S::Error: Into<error::Error>,
{
    type Response = S::Response;
    type Error = error::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            self.state = match self.state {
                LockState::Acquired(ref mut svc) => match svc.poll_ready() {
                    Ok(ok) => {
                        trace!(acquired = true, ready = ok.is_ready(), "poll_ready");
                        return Ok(ok);
                    }
                    Err(inner) => {
                        let error = Arc::new(inner.into());
                        trace!(%error, "poll_ready");
                        if let Ok(mut shared) = self.shared.lock() {
                            shared.fail(error.clone());
                        }
                        LockState::Failed(error)
                    }
                },

                LockState::Released => match self.shared.lock() {
                    Err(_) => return Err(error::Poisoned(()).into()),
                    Ok(mut shared) => match shared.try_acquire() {
                        Ok(None) => LockState::Waiting(Arc::new(AtomicTask::new())),
                        Ok(Some(svc)) => LockState::Acquired(svc),
                        Err(error) => LockState::Failed(error),
                    },
                },

                LockState::Waiting(ref task) => match self.shared.lock() {
                    Err(_) => return Err(error::Poisoned(()).into()),
                    Ok(mut shared) => match shared.poll_acquire(task) {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(svc)) => LockState::Acquired(svc),
                        Err(error) => LockState::Failed(error),
                    },
                },

                LockState::Failed(ref err) => {
                    return Err(error::ServiceError(err.clone()).into());
                }
            };
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        let mut svc = match std::mem::replace(&mut self.state, LockState::Released) {
            LockState::Acquired(svc) => svc,
            _ => panic!("called before ready"),
        };

        let fut = ResponseFuture(svc.call(req));

        if let Ok(mut shared) = self.shared.lock() {
            shared.release(svc);
        }

        fut
    }
}

impl<F> Future for ResponseFuture<F>
where
    F: Future,
    F::Error: Into<error::Error>,
{
    type Item = F::Item;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll().map_err(Into::into)
    }
}

// === impl Shared ===

impl<S> Shared<S> {
    fn try_acquire(&mut self) -> Result<Option<S>, Arc<error::Error>> {
        match std::mem::replace(&mut self.state, SharedState::Acquired) {
            SharedState::Available(svc) => Ok(Some(svc)),
            SharedState::Acquired => Ok(None),
            SharedState::Failed(error) => {
                self.state = SharedState::Failed(error.clone());
                Err(error)
            }
        }
    }

    fn poll_acquire(&mut self, task: &Arc<AtomicTask>) -> Poll<S, Arc<error::Error>> {
        match self.try_acquire() {
            Ok(Some(svc)) => Ok(Async::Ready(svc)),
            Ok(None) => {
                task.register();
                if Arc::weak_count(&task) == 0 {
                    self.wait(&task);
                }
                debug_assert_eq!(Arc::weak_count(&task), 1);
                Ok(Async::NotReady)
            }
            Err(error) => Err(error),
        }
    }

    fn wait(&mut self, task: &Arc<AtomicTask>) {
        self.waiters.push(Arc::downgrade(task));
    }

    fn release(&mut self, service: S) {
        trace!(waiters = self.waiters.len(), "releasing");
        debug_assert!(match self.state {
            SharedState::Acquired => true,
            _ => false,
        });
        self.state = SharedState::Available(service);
        self.notify_next_waiter();
    }

    fn notify_next_waiter(&mut self) {
        while let Some(waiter) = self.waiters.pop() {
            if let Some(task) = waiter.upgrade() {
                task.notify();
                return;
            }
        }
    }

    fn fail(&mut self, error: Arc<error::Error>) {
        trace!(waiters = self.waiters.len(), %error, "failing");
        debug_assert!(match self.state {
            SharedState::Acquired => true,
            _ => false,
        });
        self.state = SharedState::Failed(error);

        while let Some(waiter) = self.waiters.pop() {
            if let Some(task) = waiter.upgrade() {
                task.notify();
            }
        }
    }
}
