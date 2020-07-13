use futures::prelude::*;
use linkerd2_app_core::Never;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

pub struct Router {}

pub struct Accept {}

impl tower::Service<SocketAddr> for Router {
    type Response = Accept;
    type Error = Never;
    type Future = Pin<Box<dyn Future<Output = Result<Accept, Never>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr: SocketAddr) -> Self::Future {
        Box::pin(async move {
            unimplemented!("{:?}", addr);
        })
    }
}
