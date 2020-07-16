#![allow(warnings)]

use futures::prelude::*;
use linkerd2_app_core::{transport::BoxedIo, Error, Never};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{net::TcpStream, sync::watch};
use tracing::info;

#[derive(Clone, Debug)]
pub struct Router<S> {
    get_strategy: S,
}

#[derive(Clone, Debug)]
pub struct Accept {
    strategy: watch::Receiver<Strategy>,
}

#[derive(Clone, Debug)]
pub struct Strategy {
    target: SocketAddr,
    detect: Detect,
}

#[derive(Clone, Debug)]
enum Detect {
    Opaque,
    Client,
}

enum Protocol {
    Unknown,
    Http,
    H2,
}

impl<S> tower::Service<SocketAddr> for Router<S>
where
    S: tower::Service<SocketAddr, Response = watch::Receiver<Strategy>>,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    type Response = Accept;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Accept, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.get_strategy.poll_ready(cx)
    }

    fn call(&mut self, target: SocketAddr) -> Self::Future {
        let strategy = self.get_strategy.call(target);
        Box::pin(async move {
            let strategy = strategy.await?;
            return Ok(Accept { strategy });
        })
    }
}

impl tower::Service<TcpStream> for Accept {
    type Response = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: TcpStream) -> Self::Future {
        let strategy = self.strategy.borrow().clone();

        Box::pin(async move {
            let (protocol, io) = strategy.detect(tcp).await?;

            let rsp: Self::Response = Box::pin(async move {
                strategy.proxy(protocol, io).await;
            });
            Ok(rsp)
        })
    }
}

impl Strategy {
    async fn detect(&self, tcp: TcpStream) -> io::Result<(Protocol, BoxedIo)> {
        match self.detect {
            Detect::Opaque => Ok((Protocol::Unknown, BoxedIo::new(tcp))),
            Detect::Client => {
                // TODO sniff  SNI.
                // TODO detect HTTP.
                unimplemented!();
            }
        }
    }

    async fn proxy(self, protocol: Protocol, io: BoxedIo) {
        match protocol {
            Protocol::Unknown => self.proxy_tcp(io).await,
            Protocol::Http => self.proxy_http(io).await,
            Protocol::H2 => self.proxy_h2(io).await,
        }
    }

    async fn proxy_tcp(self, io: BoxedIo) {
        unimplemented!("TCP proxy")
    }

    async fn proxy_http(self, io: BoxedIo) {
        unimplemented!("HTTP/1 proxy")
    }

    async fn proxy_h2(self, io: BoxedIo) {
        unimplemented!("HTTP/2 proxy")
    }
}
