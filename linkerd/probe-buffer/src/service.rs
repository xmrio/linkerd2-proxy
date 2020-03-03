use crate::error::Closed;
use crate::InFlight;
use futures::{try_ready, Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::{mpsc, oneshot};

pub struct ProbeBuffer<Req, Rsp> {
    tx: mpsc::Sender<InFlight<Req, Rsp>>,
}

pub struct ResponseFuture<Rsp> {
    rx: oneshot::Receiver<Result<Rsp, Error>>,
}

// === impl ProbeBuffer ===

impl<Req, Rsp> ProbeBuffer<Req, Rsp> {
    pub(crate) fn new(tx: mpsc::Sender<InFlight<Req, Rsp>>) -> Self {
        Self { tx }
    }
}

impl<Req, Rsp> Clone for ProbeBuffer<Req, Rsp> {
    fn clone(&self) -> Self {
        Self::new(self.tx.clone())
    }
}

impl<Req, Rsp> tower::Service<Req> for ProbeBuffer<Req, Rsp> {
    type Response = Rsp;
    type Error = Error;
    type Future = ResponseFuture<Rsp>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.tx.poll_ready().map_err(|_| Closed(()).into())
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(InFlight { request, tx })
            .ok()
            .expect("poll_ready must be called");
        Self::Future { rx }
    }
}

// === impl ResponseFuture ===

impl<Rsp> Future for ResponseFuture<Rsp> {
    type Item = Rsp;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ret = try_ready!(self.rx.poll().map_err(|_| Error::from(Closed(()))));
        ret.map(Async::Ready)
    }
}
