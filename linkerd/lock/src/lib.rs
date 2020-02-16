//! A middleware for sharing an inner service via mutual exclusion.

#![deny(warnings, rust_2018_idioms)]

pub mod error;
mod layer;

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

// === impl LockError ===

#[cfg(test)]
mod test {
    use super::*;
    use futures::future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::runtime::current_thread;
    use tower::layer::Layer as _Layer;
    use tower::Service as _Service;

    #[test]
    fn exclusive_access() {
        current_thread::run(future::lazy(|| {
            let ready = Arc::new(AtomicBool::new(false));
            let mut svc0 = Layer::default().layer(Decr::new(2, ready.clone()));

            // svc0 grabs the lock, but the inner service isn't ready.
            assert!(svc0.poll_ready().expect("must not fail").is_not_ready());

            // Cloning a locked service does not preserve the lock.
            let mut svc1 = svc0.clone();

            // svc1 can't grab the lock.
            assert!(svc1.poll_ready().expect("must not fail").is_not_ready());

            // svc0 holds the lock and becomes ready with the inner service.
            ready.store(true, Ordering::SeqCst);
            assert!(svc0.poll_ready().expect("must not fail").is_ready());

            // svc1 still can't grab the lock.
            assert!(svc1.poll_ready().expect("must not fail").is_not_ready());

            // svc0 remains ready.
            let fut0 = svc0.call(1);

            // svc1 grabs the lock and is immediately ready.
            assert!(svc1.poll_ready().expect("must not fail").is_ready());
            // svc0 cannot grab the lock.
            assert!(svc0.poll_ready().expect("must not fail").is_not_ready());

            let fut1 = svc1.call(1);

            fut0.join(fut1)
                .map(|_| ())
                .map_err(|_| panic!("must not fail"))
        }));
    }

    #[test]
    fn propagates_errors() {
        current_thread::run(future::lazy(|| {
            let mut svc0 = Layer::default().layer(Decr::from(1));

            // svc0 grabs the lock and we decr the service so it will fail.
            assert!(svc0.poll_ready().expect("must not fail").is_ready());
            // svc0 remains ready.
            svc0.call(1)
                .map_err(|_| panic!("must not fail"))
                .map(move |_| {
                    // svc1 grabs the lock and fails immediately.
                    let mut svc1 = svc0.clone();
                    assert!(svc1
                        .poll_ready()
                        .expect_err("mut fail")
                        .inner()
                        .expect("must fail")
                        .is::<Underflow>());

                    // svc0 suffers the same fate.
                    assert!(svc0
                        .poll_ready()
                        .expect_err("mut fail")
                        .inner()
                        .expect("must fail")
                        .is::<Underflow>());
                })
        }));
    }

    #[test]
    fn dropping_releases_access() {
        use tower::util::ServiceExt;

        current_thread::run(future::lazy(|| {
            let ready = Arc::new(AtomicBool::new(false));
            let mut svc0 = Layer::default().layer(Decr::new(2, ready.clone()));

            // svc0 grabs the lock, but the inner service isn't ready.
            assert!(svc0.poll_ready().expect("must not fail").is_not_ready());

            // Cloning a locked service does not preserve the lock.
            let mut svc1 = svc0.clone();

            // svc1 can't grab the lock.
            assert!(svc1.poll_ready().expect("must not fail").is_not_ready());

            // svc0 holds the lock and becomes ready with the inner service.
            ready.store(true, Ordering::SeqCst);
            assert!(svc0.poll_ready().expect("must not fail").is_ready());

            // svc1 still can't grab the lock.
            assert!(svc1.poll_ready().expect("must not fail").is_not_ready());

            let mut fut = svc1.oneshot(1);

            assert!(fut.poll().expect("must not fail").is_not_ready());

            drop(svc0);

            // svc1 grabs the lock and is immediately ready.
            assert_eq!(fut.poll().expect("must not fail"), Async::Ready(1));

            Ok(().into())
        }));
    }

    #[derive(Debug, Default)]
    struct Decr {
        value: usize,
        ready: Arc<AtomicBool>,
    }

    #[derive(Copy, Clone, Debug)]
    struct Underflow;

    impl From<usize> for Decr {
        fn from(value: usize) -> Self {
            Self::new(value, Arc::new(AtomicBool::new(true)))
        }
    }

    impl Decr {
        fn new(value: usize, ready: Arc<AtomicBool>) -> Self {
            Decr { value, ready }
        }
    }

    impl tower::Service<usize> for Decr {
        type Response = usize;
        type Error = Underflow;
        type Future = futures::future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
            if self.value == 0 {
                return Err(Underflow);
            }

            if !self.ready.load(Ordering::SeqCst) {
                return Ok(Async::NotReady);
            }

            Ok(().into())
        }

        fn call(&mut self, decr: usize) -> Self::Future {
            if self.value < decr {
                self.value = 0;
                return futures::future::err(Underflow);
            }

            self.value -= decr;
            futures::future::ok(self.value)
        }
    }

    impl std::fmt::Display for Underflow {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "underflow")
        }
    }

    impl std::error::Error for Underflow {}
}
