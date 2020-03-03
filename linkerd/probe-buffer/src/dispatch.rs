use crate::error::ServiceError;
use crate::InFlight;
use futures::{Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::timer::Delay;

/// A future that drives the inner service.
pub struct Dispatch<S, Req, Rsp> {
    inner: S,
    rx: mpsc::Receiver<InFlight<Req, Rsp>>,
    probe_timeout: Duration,
    probe: Option<Delay>,
}

impl<S, Req> Dispatch<S, Req, S::Response>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    pub(crate) fn new(
        inner: S,
        rx: mpsc::Receiver<InFlight<Req, S::Response>>,
        probe_timeout: Duration,
    ) -> Self {
        Self {
            inner,
            rx,
            probe_timeout,
            probe: None,
        }
    }
}

impl<S, Req> Future for Dispatch<S, Req, S::Response>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Clear any existing probes, since we're about to poll the inner
        // service's readiness again.
        self.probe = None;

        loop {
            match self.inner.poll_ready() {
                // If it's not ready, wait for it..
                Ok(Async::NotReady) => return Ok(Async::NotReady),

                // If the service fails, propagate the failure to all pending
                // requests and then complete.
                Err(error) => {
                    let shared = ServiceError(Arc::new(error.into()));
                    while let Ok(Async::Ready(Some(InFlight { tx, .. }))) = self.rx.poll() {
                        let _ = tx.send(Err(shared.clone().into()));
                    }
                    return Ok(Async::Ready(()));
                }

                // If inner service can receive requests, start polling the channel.
                Ok(Async::Ready(())) => {}
            }

            // The inner service is ready, so poll for new requests.
            match self.rx.poll() {
                // The sender has been dropped, complete (notifying in-flight requests).
                Err(_) | Ok(Async::Ready(None)) => return Ok(Async::Ready(())),

                // If a request was ready, spawn its response future
                Ok(Async::Ready(Some(InFlight { request, tx }))) => {
                    tokio::spawn(self.inner.call(request).then(move |res| {
                        let _ = tx.send(res.map_err(Into::into));
                        Ok(())
                    }));
                }

                // If the inner service is ready but no requests are
                // available, schedule a probe to trigger periodic checks of
                // the inner service to allow idle timeouts, etc.
                Ok(Async::NotReady) => {
                    let mut probe = Delay::new(Instant::now() + self.probe_timeout);
                    if let Ok(Async::NotReady) = probe.poll() {
                        self.probe = Some(probe);
                        return Ok(Async::NotReady);
                    }
                }
            }

            debug_assert!(self.probe.is_none());
        }
    }
}
