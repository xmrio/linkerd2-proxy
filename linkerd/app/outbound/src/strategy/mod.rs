use futures::prelude::*;
use linkerd2_app_core::{
    config::ProxyConfig,
    proxy::identity,
    transport::{tls, BoxedIo},
    Error, ProxyMetrics,
};
use linkerd2_strategy::{Detect, Strategy};
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

#[allow(dead_code)]
#[derive(Copy, Clone, Debug)]
enum Protocol {
    Unknown,
    Http1,
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
            let (protocol, io) = Self::detect(strategy.detect, tcp).await?;

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

#[allow(unused_variables)]
impl Accept {
    async fn detect(detect: Detect, tcp: TcpStream) -> io::Result<(Protocol, BoxedIo)> {
        match detect {
            Detect::Opaque => Ok((Protocol::Unknown, BoxedIo::new(tcp))),
            Detect::Client => {
                use linkerd2_app_core::{proxy::http::Version, transport::io::Peekable};

                // TODO sniff  SNI.

                // TODO take advantage TcpStream::peek to avoid allocating a buf
                // per peek.
                let io = tcp.peek(8192).await?;

                let proto = match Version::from_prefix(io.prefix().as_ref()) {
                    None => Protocol::Unknown,
                    Some(Version::Http1) => Protocol::Http1,
                    Some(Version::Http1) => Protocol::H2,
                };

                Ok((proto, BoxedIo::new(io)))
            }
        }
    }

    async fn proxy_tcp(config: Config, strategy: Strategy, io: BoxedIo) {
        unimplemented!("TCP proxy")
    }

    async fn proxy_http1(config: Config, strategy: Strategy, io: BoxedIo) {
        unimplemented!("HTTP/1 proxy")
    }

    async fn proxy_h2(config: Config, strategy: Strategy, io: BoxedIo) {
        unimplemented!("HTTP/2 proxy")
    }
}
