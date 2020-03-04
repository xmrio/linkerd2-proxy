use crate::error::ServiceError;
use crate::InFlight;
use futures::{Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::timer::Delay;
use tracing::trace;

/// A future that drives the inner service.
pub struct Dispatch<S, Req, Rsp> {
    inner: Option<S>,
    rx: mpsc::Receiver<InFlight<Req, Rsp>>,
    probe_timeout: Duration,
    probe: Option<Delay>,
    ready: watch::Sender<Poll<(), ServiceError>>,
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
        ready: watch::Sender<Poll<(), ServiceError>>,
        probe_timeout: Duration,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            ready,
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
        trace!(probe = self.probe.is_some(), "Polling");

        // Clear any existing probes, since we're about to poll the inner
        // service's readiness again.
        self.probe = None;

        // Complete the task when all services have dropped.
        if self.ready.poll_close().expect("must not fail").is_ready() {
            return Ok(Async::Ready(()));
        }

        // Drive requests from the queue to the inner service, ensuring that the
        // inner service is plled at least once per `probe_timeout`.
        loop {
            let ready = match self.inner.as_mut() {
                Some(inner) => inner.poll_ready(),
                None => {
                    // This is safe because ready.poll_close has returned NotReady.
                    return Ok(Async::NotReady);
                }
            };

            match ready {
                // If it's not ready, wait for it..
                Ok(Async::NotReady) => {
                    if self.ready.broadcast(Ok(Async::NotReady)).is_err() {
                        return Ok(Async::Ready(()));
                    }
                    trace!("Waiting for inner service");
                    return Ok(Async::NotReady);
                }

                // If the service fails, propagate the failure to all pending
                // requests and then complete.
                Err(error) => {
                    let shared = ServiceError(Arc::new(error.into()));
                    trace!(%shared, "Inner service failed");

                    if self.ready.broadcast(Err(shared.clone())).is_err() {
                        return Ok(Async::Ready(()));
                    }

                    while let Ok(Async::Ready(Some(InFlight { tx, .. }))) = self.rx.poll() {
                        let _ = tx.send(Err(shared.clone().into()));
                    }

                    // Drop the inner Service but keep the task alive until all consumers have
                    // dropped.
                    //
                    // This is safe because ready.poll_close has returned NotReady.
                    self.inner = None;
                    return Ok(Async::NotReady);
                }

                // If inner service can receive requests, start polling the channel.
                Ok(Async::Ready(())) => {
                    if self.ready.broadcast(Ok(Async::Ready(()))).is_err() {
                        return Ok(Async::Ready(()));
                    }
                    trace!("Ready for requests");
                }
            }

            // The inner service is ready, so poll for new requests.
            match self.rx.poll() {
                // The sender has been dropped, complete (notifying in-flight requests).
                Err(_) | Ok(Async::Ready(None)) => return Ok(Async::Ready(())),

                // If a request was ready, spawn its response future
                Ok(Async::Ready(Some(InFlight { request, tx }))) => {
                    trace!("Dispatching a request");
                    let inner = self.inner.as_mut().expect("Service must not be dropped");
                    tokio::spawn(inner.call(request).then(move |res| {
                        let _ = tx.send(res.map_err(Into::into));
                        Ok(())
                    }));
                }

                // If the inner service is ready but no requests are
                // available, schedule a probe to trigger periodic checks of
                // the inner service to allow idle timeouts, etc.
                Ok(Async::NotReady) => {
                    trace!(probe.timeout = ?self.probe_timeout, "No requests available");
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
