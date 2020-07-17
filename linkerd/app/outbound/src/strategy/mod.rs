use futures::prelude::*;
use linkerd2_app_core::{
    config::ProxyConfig,
    proxy::identity,
    transport::{tls, BoxedIo},
    Error, ProxyMetrics,
};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{net::TcpStream, sync::watch};

#[derive(Clone)]
pub struct Router<S> {
    get_strategy: S,
    config: Config,
}

#[derive(Clone)]
pub struct Accept {
    strategy: watch::Receiver<Strategy>,
    config: Config,
}

#[derive(Clone)]
struct Config {
    config: ProxyConfig,
    identity: tls::Conditional<identity::Local>,
    metrics: ProxyMetrics,
}

#[derive(Clone, Debug)]
pub struct Strategy {
    target: SocketAddr,
    detect: Detect,
}

#[allow(dead_code)]
#[derive(Copy, Clone, Debug)]
enum Detect {
    Opaque,
    Client,
}

#[allow(dead_code)]
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

        let config = self.config.clone();
        Box::pin(async move {
            let strategy = strategy.await?;

            return Ok(Accept { config, strategy });
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
        let config = self.config.clone();

        let strategy = self.strategy.borrow().clone();

        Box::pin(async move {
            let (protocol, io) = strategy.detect(tcp).await?;

            let rsp: Self::Response = Box::pin(async move {
                match protocol {
                    Protocol::Unknown => Self::proxy_tcp(config, strategy, io).await,
                    Protocol::Http => Self::proxy_http(config, strategy, io).await,
                    Protocol::H2 => Self::proxy_h2(config, strategy, io).await,
                }
            });
            Ok(rsp)
        })
    }
}

#[allow(warnings)]
impl Accept {
    async fn proxy_tcp(config: Config, strategy: Strategy, io: BoxedIo) {
        unimplemented!("TCP proxy")
    }

    async fn proxy_http(config: Config, strategy: Strategy, io: BoxedIo) {
        unimplemented!("HTTP/1 proxy")
    }

    async fn proxy_h2(config: Config, strategy: Strategy, io: BoxedIo) {
        unimplemented!("HTTP/2 proxy")
    }
}
