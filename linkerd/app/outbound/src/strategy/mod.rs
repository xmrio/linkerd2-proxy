use crate::endpoint::TcpEndpoint;
use futures::prelude::*;
use linkerd2_app_core::{
    config::ProxyConfig,
    proxy::identity,
    svc,
    transport::{tls, BoxedIo},
    Error, ProxyMetrics,
};
use linkerd2_duplex::Duplex;
use linkerd2_strategy::{Detect, Strategy, Target};
use rand::{distributions::Distribution, rngs::SmallRng};
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{io, net::TcpStream, sync::watch};
use tracing::{debug, info, warn};

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
    rng: SmallRng,
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
                    Protocol::Unknown => {
                        match Self::proxy_tcp(config, strategy.addr, strategy.target, io).await {
                            Err(error) => info!(%error, "TCP stream completed"),
                            Ok(()) => debug!("TCP stream completed"),
                        }
                    }
                    Protocol::Http1 => unimplemented!("HTTP/1 proxy"),
                    Protocol::H2 => unimplemented!("H2 proxy"),
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

                let proto = Version::from_prefix(io.prefix().as_ref())
                    .map(|v| match v {
                        Version::Http1 => Protocol::Http1,
                        Version::H2 => Protocol::H2,
                    })
                    .unwrap_or(Protocol::Unknown);

                Ok((proto, BoxedIo::new(io)))
            }
        }
    }

    async fn proxy_tcp(
        mut config: Config,
        addr: SocketAddr,
        mut target: Target,
        io: BoxedIo,
    ) -> Result<(), Error> {
        loop {
            target = match target {
                Target::LogicalSplit {
                    metric_labels,
                    targets,
                    weights,
                } => {
                    let idx = weights.sample(&mut config.rng);
                    debug_assert!(idx < targets.len());
                    targets[idx].clone()
                }

                Target::Concrete {
                    authority,
                    metric_labels,
                } => {
                    // TODO TCP discovery/balancing.
                    warn!("TCP load balancing not supported yet; forwarding");
                    Target::Endpoint {
                        addr,
                        identity: None,
                    }
                }

                Target::Endpoint { addr, identity } => {
                    let id = identity.map(tls::Conditional::Some).unwrap_or_else(|| {
                        tls::Conditional::None(
                            tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                        )
                    });
                    let dst_io = Self::connect(config, addr, id).await?;
                    Duplex::new(io, dst_io).await?;
                    return Ok(());
                }
            }
        }
    }

    async fn connect(
        Config {
            config,
            identity,
            metrics,
            ..
        }: Config,
        addr: SocketAddr,
        peer_identity: tls::PeerIdentity,
    ) -> Result<impl io::AsyncRead + io::AsyncWrite + Send, Error> {
        use tower::{util::ServiceExt, Service};

        let mut connect = svc::connect(config.connect.keepalive)
            // Initiates mTLS if the target is configured with identity.
            .push(tls::client::ConnectLayer::new(identity))
            // Limits the time we wait for a connection to be established.
            .push_timeout(config.connect.timeout)
            .push(metrics.transport.layer_connect(crate::TransportLabels))
            .into_inner();

        let endpoint = TcpEndpoint {
            addr,
            identity: peer_identity,
        };
        let io = connect.ready_and().await?.call(endpoint).await?;
        Ok(io)
    }
}
