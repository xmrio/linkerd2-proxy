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

#[derive(Copy, Clone, Debug)]
enum Detect {
    Opaque,
    Client,
}

#[derive(Copy, Clone, Debug)]
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
                match protocol {
                    Protocol::Unknown => Self::proxy_tcp(&strategy, io).await,
                    Protocol::Http => Self::proxy_http(&strategy, io).await,
                    Protocol::H2 => Self::proxy_h2(&strategy, io).await,
                }
            });
            Ok(rsp)
        })
    }
}

impl Accept {
    async fn proxy_tcp(strategy: &Strategy, io: BoxedIo) {
        unimplemented!("TCP proxy")
    }

    async fn proxy_http(strategy: &Strategy, io: BoxedIo) {
        unimplemented!("HTTP/1 proxy")
    }

    async fn proxy_h2(strategy: &Strategy, io: BoxedIo) {
        unimplemented!("HTTP/2 proxy")
    }
}
