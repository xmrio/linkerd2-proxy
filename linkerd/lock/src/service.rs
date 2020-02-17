use crate::error::{Error, Poisoned, ServiceError};
use crate::shared::{Shared, Wait};
use futures::{future, Async, Future, Poll};
use std::sync::{Arc, Mutex};
use tracing::trace;

/// Guards access to an inner service with a `tokio::sync::lock::Lock`.
///
/// As the service is polled to readiness, the lock is acquired and the inner
/// service is polled. If the sevice is cloned, the service's lock state is not
/// retained by the clone.
pub struct Lock<S> {
    state: State<S>,
    shared: Arc<Mutex<Shared<S>>>,
}

enum State<S> {
    Released,
    Waiting(Wait),
    Acquired(S),
    Failed(Arc<Error>),
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(service: S) -> Self {
        Self {
            state: State::Released,
            shared: Arc::new(Mutex::new(Shared::new(service))),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            // Clones have an independent local lock state.
            state: State::Released,
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
                State::Acquired(ref mut svc) => match svc.poll_ready() {
                    Ok(ok) => {
                        trace!(ready = ok.is_ready(), "Acquired");
                        return Ok(ok);
                    }
                    Err(inner) => {
                        // If the inner service fails to become ready, share
                        // that error with all other locks and update this
                        // lock's state to prevent trying to acquire the shared
                        // state again.
                        let error = Arc::new(inner.into());
                        trace!(%error, "Failing");
                        if let Ok(mut shared) = self.shared.lock() {
                            shared.fail(error.clone());
                        }
                        State::Failed(error)
                    }
                },

                State::Released => {
                    trace!("Released");
                    match self.shared.lock() {
                        Err(_) => return Err(Poisoned::new().into()),
                        Ok(mut shared) => match shared.try_acquire() {
                            Ok(None) => State::Waiting(Wait::default()),
                            Ok(Some(svc)) => State::Acquired(svc),
                            Err(error) => State::Failed(error),
                        },
                    }
                }

                State::Waiting(ref waiter) => {
                    trace!("Waiting");
                    match self.shared.lock() {
                        Err(_) => return Err(Poisoned::new().into()),
                        Ok(mut shared) => match shared.poll_acquire(waiter) {
                            Ok(Async::NotReady) => return Ok(Async::NotReady),
                            Ok(Async::Ready(svc)) => State::Acquired(svc),
                            Err(error) => State::Failed(error),
                        },
                    }
                }

                State::Failed(ref error) => {
                    trace!(%error, "Failed");
                    return Err(ServiceError::new(error.clone()).into());
                }
            };
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        // The service must have been acquired by poll_ready. Reset this lock's
        // state so that it must reacquire the service via poll_ready.
        let mut svc = match std::mem::replace(&mut self.state, State::Released) {
            State::Acquired(svc) => svc,
            _ => panic!("called before ready"),
        };

        let fut = svc.call(req);

        // Return the service to the shared state, notifying waiters as needed.
        //
        // The service is dropped if the inner mutex has been poisoned, and
        // subsequent calsl to poll_ready will return a Poisioned error.
        if let Ok(mut shared) = self.shared.lock() {
            trace!("Releasing acquired lock after use");
            shared.release_and_notify(svc);
        }

        // The inner service's error type is *not* wrapped with a ServiceError.
        fut.map_err(Into::into)
    }
}

impl<S> Drop for Lock<S> {
    fn drop(&mut self) {
        match std::mem::replace(&mut self.state, State::Released) {
            // If this lock was holding the service, return it back back to the
            // shared state so another lock may acquire it. Waiters are notified.
            State::Acquired(service) => {
                trace!("Dropping while acquired");
                if let Ok(mut shared) = self.shared.lock() {
                    shared.release_and_notify(service);
                }
            }

            // If this lock is waiting but the waiter isn't registered, it must
            // have been notified. Notify the next waiter to prevent deadlock.
            State::Waiting(wait) => {
                trace!("Dropping while waiting");
                if let Ok(mut shared) = self.shared.lock() {
                    if wait.is_not_waiting() {
                        shared.notify_next_waiter();
                    }
                }
            }

            // No state to cleanup.
            State::Released | State::Failed(_) => {}
        }
    }
}
