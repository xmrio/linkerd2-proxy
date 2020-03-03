use crate::{Dispatch, ProbeBuffer};
use futures::Future;
use linkerd2_error::Error;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing_futures::Instrument;

pub struct SpawnProbeBufferLayer<Req> {
    capacity: usize,
    probe_timeout: Duration,
    _marker: std::marker::PhantomData<fn(Req)>,
}

impl<Req> SpawnProbeBufferLayer<Req> {
    pub fn new(capacity: usize, probe_timeout: Duration) -> Self {
        Self {
            capacity,
            probe_timeout,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<Req> Clone for SpawnProbeBufferLayer<Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            probe_timeout: self.probe_timeout,
            _marker: self._marker,
        }
    }
}

impl<Req, S> tower::layer::Layer<S> for SpawnProbeBufferLayer<Req>
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Service = ProbeBuffer<Req, S::Response>;

    fn layer(&self, inner: S) -> Self::Service {
        let (tx, rx) = mpsc::channel(self.capacity);
        tokio::spawn(
            Dispatch::new(inner, rx, self.probe_timeout)
                .in_current_span()
                .map_err(|n| match n {}),
        );
        ProbeBuffer::new(tx)
    }
}
