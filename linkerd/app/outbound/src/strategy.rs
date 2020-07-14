#![allow(warnings)]

use futures::prelude::*;
use linkerd2_app_core::Never;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::net::TcpStream;

#[derive(Clone, Debug)]
pub struct Router {}

#[derive(Clone, Debug)]
pub struct Accept {
    pub target: SocketAddr,
    detect: Detect,
}

#[derive(Clone, Debug)]
enum Detect {
    Opaque,
    Client,
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

    fn call(&mut self, tcp_stream: TcpStream) -> Self::Future {
        let detect = self.detect.clone();
        Box::pin(async move {
            let process: Pin<Box<dyn Future<Output = ()> + Send + 'static>> = match detect {
                Detect::Opaque => {
                    Box::pin(async move {
                        // TODO forward
                        drop(tcp_stream);
                    })
                }
                Detect::Client => {
                    Box::pin(async move {
                        // TODO sniff  SNI.
                        // TODO detect HTTP.
                        drop(tcp_stream);
                    })
                }
            };
            Ok(process)
        })
    }
}
