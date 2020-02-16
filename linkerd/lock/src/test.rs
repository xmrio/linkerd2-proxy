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
        let mut svc0 = LockLayer::default().layer(Decr::new(2, ready.clone()));

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
        let mut svc0 = LockLayer::default().layer(Decr::from(1));

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
                    .downcast_ref::<error::ServiceError>()
                    .expect("must fail with service error")
                    .inner()
                    .is::<Underflow>());

                // svc0 suffers the same fate.
                assert!(svc0
                    .poll_ready()
                    .expect_err("mut fail")
                    .downcast_ref::<error::ServiceError>()
                    .expect("must fail with service error")
                    .inner()
                    .is::<Underflow>());
            })
    }));
}

#[test]
fn dropping_releases_access() {
    use tower::util::ServiceExt;

    current_thread::run(future::lazy(|| {
        let ready = Arc::new(AtomicBool::new(false));
        let mut svc0 = LockLayer::default().layer(Decr::new(2, ready.clone()));

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
