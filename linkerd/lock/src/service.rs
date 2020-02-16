use crate::error::{Error, Poisoned, ServiceError};
use futures::{future, Async, Future, Poll};
use std::sync::{Arc, Mutex};
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

enum LockState<S> {
    Released,
    Waiting(waiter::Wait),
    Acquired(S),
    Failed(Arc<Error>),
}

struct Shared<S> {
    state: SharedState<S>,
    waiters: Vec<waiter::Notify>,
}

enum SharedState<S> {
    /// A Lock is holding the service.
    Claimed,

    /// The inner service is available.
    Unclaimed(S),

    /// The lock has failed.
    Failed(Arc<Error>),
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(service: S) -> Self {
        Self {
            state: LockState::Released,
            shared: Arc::new(Mutex::new(Shared::new(service))),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            // Clones have an independent local lock state.
            state: LockState::Released,
            shared: self.shared.clone(),
        }
    }
}

impl<T, S> tower::Service<T> for Lock<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            self.state = match self.state {
                // This lock has exlcusive access to the inner service.
                LockState::Acquired(ref mut svc) => match svc.poll_ready() {
                    Ok(ok) => {
                        trace!(acquired = true, ready = ok.is_ready(), "poll_ready");
                        return Ok(ok);
                    }
                    Err(inner) => {
                        // If the inner service fails to become ready, share
                        // that error with all other locks and update this
                        // lock's state to prevent trying to acquire the shared
                        // state again.
                        let error = Arc::new(inner.into());
                        trace!(%error, "poll_ready");
                        if let Ok(mut shared) = self.shared.lock() {
                            shared.fail(error.clone());
                        }
                        LockState::Failed(error)
                    }
                },

                LockState::Released => match self.shared.lock() {
                    Err(_) => return Err(Poisoned::new().into()),
                    Ok(mut shared) => match shared.try_acquire() {
                        Ok(None) => LockState::Waiting(waiter::Wait::default()),
                        Ok(Some(svc)) => LockState::Acquired(svc),
                        Err(error) => LockState::Failed(error),
                    },
                },

                LockState::Waiting(ref waiter) => match self.shared.lock() {
                    Err(_) => return Err(Poisoned::new().into()),
                    Ok(mut shared) => match shared.poll_acquire(waiter) {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(svc)) => LockState::Acquired(svc),
                        Err(error) => LockState::Failed(error),
                    },
                },

                LockState::Failed(ref err) => {
                    return Err(ServiceError::new(err.clone()).into());
                }
            };
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        // The service must have been acquired by poll_ready. Reset this lock's
        // state so that it must reacquire the service via poll_ready.
        let mut svc = match std::mem::replace(&mut self.state, LockState::Released) {
            LockState::Acquired(svc) => svc,
            _ => panic!("called before ready"),
        };

        let fut = svc.call(req);

        // Return the service to the shared state, notifying waiters as needed.
        //
        // The service is dropped if the inner mutex has been poisoned, and
        // subsequent calsl to poll_ready will return a Poisioned error.
        if let Ok(mut shared) = self.shared.lock() {
            shared.release(svc);
        }

        // The inner service's error type is *not* wrapped with a ServiceError.
        fut.map_err(Into::into)
    }
}

impl<S> Drop for Lock<S> {
    fn drop(&mut self) {
        match std::mem::replace(&mut self.state, LockState::Released) {
            // If this lock was holding the service, return it back back to the
            // shared state so another lock may acquire it. Waiters are notified.
            LockState::Acquired(service) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.release(service);
                }
            }

            // If this lock is waiting but the waiter isn't registered, it must
            // have been notified. Notify the next waiter to prevent deadlock.
            LockState::Waiting(wait) => {
                if let Ok(mut shared) = self.shared.lock() {
                    if wait.is_not_waiting() {
                        shared.notify_next_waiter();
                    }
                }
            }

            // No state to cleanup.
            LockState::Released | LockState::Failed(_) => {}
        }
    }
}

// === impl Shared ===

impl<S> Shared<S> {
    fn new(service: S) -> Self {
        Self {
            waiters: Vec::new(),
            state: SharedState::Unclaimed(service),
        }
    }

    fn try_acquire(&mut self) -> Result<Option<S>, Arc<Error>> {
        match std::mem::replace(&mut self.state, SharedState::Claimed) {
            // This lock has acquired the service.
            SharedState::Unclaimed(svc) => Ok(Some(svc)),
            // The service is already claimed by a lock.
            SharedState::Claimed => Ok(None),
            // The service failed, so reset the state immediately so that all
            // locks may be notified.
            SharedState::Failed(error) => {
                self.state = SharedState::Failed(error.clone());
                Err(error)
            }
        }
    }

    fn poll_acquire(&mut self, wait: &waiter::Wait) -> Poll<S, Arc<Error>> {
        match self.try_acquire() {
            Ok(Some(svc)) => Ok(Async::Ready(svc)),
            Ok(None) => {
                self.register(&wait);
                Ok(Async::NotReady)
            }
            Err(error) => Err(error),
        }
    }

    fn register(&mut self, wait: &waiter::Wait) {
        if let Some(notify) = wait.get_notify() {
            self.waiters.push(notify);
        }
        wait.register();
        debug_assert!(wait.is_waiting());
    }

    fn release(&mut self, service: S) {
        trace!(waiters = self.waiters.len(), "releasing");
        debug_assert!(match self.state {
            SharedState::Claimed => true,
            _ => false,
        });
        self.state = SharedState::Unclaimed(service);
        self.notify_next_waiter();
    }

    fn notify_next_waiter(&mut self) {
        while let Some(waiter) = self.waiters.pop() {
            if waiter.notify() {
                return;
            }
        }
    }

    fn fail(&mut self, error: Arc<Error>) {
        trace!(waiters = self.waiters.len(), %error, "failing");
        debug_assert!(match self.state {
            SharedState::Claimed => true,
            _ => false,
        });
        self.state = SharedState::Failed(error);

        while let Some(waiter) = self.waiters.pop() {
            waiter.notify();
        }
    }
}

mod waiter {
    use futures::task::AtomicTask;
    use std::sync::{Arc, Weak};

    #[derive(Default)]
    pub struct Wait(Arc<AtomicTask>);

    pub struct Notify(Weak<AtomicTask>);

    impl Wait {
        pub fn get_notify(&self) -> Option<Notify> {
            if self.is_not_waiting() {
                let n = Notify(Arc::downgrade(&self.0));
                debug_assert!(self.is_waiting());
                Some(n)
            } else {
                None
            }
        }

        pub fn register(&self) {
            self.0.register();
        }

        pub fn is_waiting(&self) -> bool {
            Arc::weak_count(&self.0) == 1
        }

        pub fn is_not_waiting(&self) -> bool {
            Arc::weak_count(&self.0) == 0
        }
    }

    impl Notify {
        pub fn notify(self) -> bool {
            if let Some(task) = self.0.upgrade() {
                task.notify();
                true
            } else {
                false
            }
        }
    }
}
