#![allow(warnings)]

use futures::prelude::*;
use linkerd2_app_core::{transport::BoxedIo, Never};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::net::TcpStream;
use tracing::info;

#[derive(Clone, Debug)]
pub struct Router {}

#[derive(Clone, Debug)]
pub struct Accept {
    pub target: SocketAddr,
    detect: Detect,
}

#[derive(Copy, Clone, Debug)]
enum Detect {
    Opaque,
    Client,
}

enum Protocol {
    Unknown,
    Http,
    H2,
}

impl tower::Service<SocketAddr> for Router {
    type Response = Accept;
    type Error = Never;
    type Future = Pin<Box<dyn Future<Output = Result<Accept, Never>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: SocketAddr) -> Self::Future {
        Box::pin(async move {
            return Ok(Accept {
                target,
                detect: Detect::Client,
            });
        })
    }
}

impl tower::Service<TcpStream> for Accept {
    type Response = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
    type Error = Never;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Never>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: TcpStream) -> Self::Future {
        let target = self.target;
        let detect = self.detect.clone();

        Box::pin(async move {
            let response: Self::Response = match detect.detect(target, tcp).await {
                Ok((protocol, io)) => Box::pin(proxy(target, protocol, io)),
                Err(error) => {
                    info!(%error, "Failed to detect protocol");
                    Box::pin(future::ready(()))
                }
            };

            Ok(response)
        })
    }
}

impl Detect {
    pub async fn detect(
        &self,
        target: SocketAddr,
        tcp: TcpStream,
    ) -> io::Result<(Protocol, BoxedIo)> {
        match self {
            Self::Opaque => Ok((Protocol::Unknown, BoxedIo::new(tcp))),
            Self::Client => {
                // TODO sniff  SNI.
                // TODO detect HTTP.
                unimplemented!();
            }
        }
    }
}

async fn proxy(target: SocketAddr, protocol: Protocol, io: BoxedIo) {
    match protocol {
        Protocol::Unknown => proxy_tcp(target, io).await,
        Protocol::Http => proxy_http(target, io).await,
        Protocol::H2 => proxy_h2(target, io).await,
    }
}

async fn proxy_tcp(target: SocketAddr, io: BoxedIo) {
    unimplemented!("TCP proxy")
}

async fn proxy_http(target: SocketAddr, io: BoxedIo) {
    unimplemented!("HTTP/1 proxy")
}

async fn proxy_h2(target: SocketAddr, io: BoxedIo) {
    unimplemented!("HTTP/2 proxy")
}
