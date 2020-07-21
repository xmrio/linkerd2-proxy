use futures::prelude::*;
use linkerd2_app_core::{
    config::ProxyConfig,
    proxy::identity,
    timeout,
    transport::{connect::connect, tls, BoxedIo},
    Error, ProxyMetrics,
};
use linkerd2_duplex::Duplex;
use linkerd2_strategy::{Detect, Strategy, Target};
use rand::{distributions::Distribution, rngs::SmallRng};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{net::TcpStream, sync::watch};
use tracing::{debug, warn};

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
                        if let Err(error) =
                            Self::proxy_tcp(config, strategy.addr, strategy.target, io).await
                        {
                            debug!(%error, "TCP stream completed");
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
        let connect_timeout = config.config.connect.timeout;
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
                    let dst_io = Self::connect(config, addr, identity).await?
                    Duplex::new(io, dst_io).await?;
                    return Ok(());
                }
            }
        }
    }

    async fn connect(
        Config {
            config, identity, ..
        }: Config,
        addr: SocketAddr,
        peer_identity: tls::PeerIdentity,
    ) -> Result<BoxedIo, Error> {
        let connect = {
            let tcp = connect(addr, config.connect.keepalive).await?;

            // TODO .push(metrics.transport.layer_connect(TransportLabels))
            match (identity, peer_identity) {
                (tls::Conditional::Some(key), tls::Conditional::Some(id)) => {
                    let io = tls::client::handshake(&key, &id, tcp).await?;
                    BoxedIo::new(io)
                }
                _ => BoxedIo::new(tcp),
            }
        };

        futures::select_biased! {
            () = tokio::time::delay_for(connect_timeout).fuse() => {
                Err(timeout::error::ConnectTimeout::from(connect_timeout).into())
            }
            res = connect.fuse() => { res }
        }
    }
}
