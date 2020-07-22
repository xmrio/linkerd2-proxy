use crate::endpoint::TcpEndpoint;
use futures::prelude::*;
use linkerd2_app_core::{
    config::ProxyConfig,
    proxy::identity,
    svc,
    transport::{tls, BoxedIo},
    Error, Never, ProxyMetrics,
};
use linkerd2_duplex::Duplex;
use linkerd2_strategy::{Detect, Endpoint, Strategy, Target};
use rand::{distributions::Distribution, rngs::SmallRng};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
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

// The router is shared and its responses are buffered/cached so that multiple
// connections use the same accept object.
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
        // TODO dispatch timeout
        let strategy = self.get_strategy.call(target);

        let config = self.config.clone();
        Box::pin(async move {
            let strategy = strategy.await?;

            return Ok(Accept { config, strategy });
        })
    }
}

impl tower::Service<TcpStream> for Accept {
    type Response = ();
    type Error = Never;
    type Future = Pin<Box<dyn Future<Output = Result<(), Never>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: TcpStream) -> Self::Future {
        let config = self.config.clone();
        let strategy_rx = self.strategy.clone();

        Box::pin(async move {
            let strategy = strategy_rx.borrow().clone();

            // TODO metrics...
            let (protocol, io) = match strategy.detect {
                Detect::Opaque => (Protocol::Unknown, BoxedIo::new(tcp)),
                Detect::Client => match config.detect(tcp).await {
                    Err(error) => {
                        info!(%error, "Protocol detection error");
                        return Ok(());
                    }
                    Ok((protocol, io)) => (protocol, io),
                },
            };

            match (protocol, io) {
                (Protocol::Unknown, io) => {
                    // There's no need to watch for updates with TCP
                    // streams, since the routing decision is made
                    // instantaneously.
                    match config.proxy_tcp(strategy, io).await {
                        Err(error) => info!(%error, "Connection closed"),
                        Ok(()) => debug!("Connection closed"),
                    }
                }
                (Protocol::Http1, io) => config.proxy_http1(strategy_rx, io).await,
                (Protocol::H2, _io) => unimplemented!("H2 proxy"),
            }

            Ok(())
        })
    }
}

impl Config {
    async fn detect(&self, tcp: TcpStream) -> Result<(Protocol, BoxedIo), Error> {
        use linkerd2_app_core::{proxy::http::Version, transport::io::Peekable};

        // TODO sniff  SNI.

        // TODO take advantage TcpStream::peek to avoid allocating a buf
        // per peek. A large buffer is needed, to fit the first line of
        // an arbitrary HTTP message (i.e. with a long URI).
        let io =
            tokio::time::timeout(self.config.detect_protocol_timeout, tcp.peek(8192)).await??;

        let proto = Version::from_prefix(io.prefix().as_ref())
            .map(|v| match v {
                Version::Http1 => Protocol::Http1,
                Version::H2 => Protocol::H2,
            })
            .unwrap_or(Protocol::Unknown);

        Ok((proto, BoxedIo::new(io)))
    }

    async fn proxy_tcp(
        mut self,
        Strategy {
            addr, mut target, ..
        }: Strategy,
        io: BoxedIo,
    ) -> Result<(), Error> {
        loop {
            target = match target {
                Target::LogicalSplit(split) => {
                    let idx = split.weights.sample(&mut self.rng);
                    debug_assert!(idx < split.targets.len());
                    split.targets[idx].clone()
                }

                Target::Concrete(concrete) => {
                    // TODO TCP discovery/balancing.
                    warn!(%concrete.authority, "TCP load balancing not supported yet; forwarding");
                    Target::Endpoint(Arc::new(Endpoint {
                        addr,
                        identity: None,
                        metric_labels: Default::default(),
                    }))
                }

                Target::Endpoint(endpoint) => {
                    let id = endpoint
                        .identity
                        .clone()
                        .map(tls::Conditional::Some)
                        .unwrap_or_else(|| {
                            tls::Conditional::None(
                                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                            )
                        });

                    let dst_io = self.connect(addr, id).await?;
                    Duplex::new(io, dst_io).await?;

                    return Ok(());
                }
            }
        }
    }

    async fn connect(
        self,
        addr: SocketAddr,
        peer_identity: tls::PeerIdentity,
    ) -> Result<impl io::AsyncRead + io::AsyncWrite + Send, Error> {
        use tower::{util::ServiceExt, Service};

        let Self {
            config,
            identity,
            metrics,
            ..
        } = self;

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

    #[allow(warnings)]
    async fn proxy_http1(mut self, strategy: watch::Receiver<Strategy>, io: BoxedIo) {
        // TODO
        // - create an HTTP server
        // - dispatches to a service that holds the strategy watch...
        // - buffered/cached...
        unimplemented!();
    }
}
