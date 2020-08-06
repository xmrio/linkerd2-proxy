use crate::endpoint::TcpEndpoint;
use futures::prelude::*;
use linkerd2_app_core::{
    buffer,
    config::ProxyConfig,
    drain,
    duplex::Duplex,
    proxy::{http, identity},
    svc,
    transport::{tls, BoxedIo},
    Error, Never, ProxyMetrics,
};
use linkerd2_strategy::{Concrete, Detect, Endpoint, Logical, Strategy};
use rand::{distributions::Distribution, rngs::SmallRng};
use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{io, net::TcpStream, sync::watch};
use tower::{util::ServiceExt, Service};
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct Router<S> {
    get_strategy: S,
    inner: Inner,
}

type BufferedHttp = buffer::Buffer<http::Request<http::Body>, http::Response<http::boxed::Payload>>;

#[derive(Clone)]
pub struct Accept {
    strategy: watch::Receiver<Strategy>,
    inner: Inner,
}

#[derive(Clone)]
struct Inner {
    config: ProxyConfig,
    identity: tls::Conditional<identity::Local>,
    metrics: ProxyMetrics,
    rng: SmallRng,
    drain: drain::Watch,
}

#[allow(dead_code)]
#[derive(Copy, Clone, Debug)]
enum Protocol {
    Unknown,
    Http(http::Version),
}

// The router is shared and its responses are buffered/cached so that multiple
// connections use the same accept object.
impl<S> Service<SocketAddr> for Router<S>
where
    S: Service<SocketAddr, Response = watch::Receiver<Strategy>> + Clone + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    type Response = Accept;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Accept, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: SocketAddr) -> Self::Future {
        let get_strategy = self.get_strategy.clone().oneshot(target);

        let inner = self.inner.clone();
        Box::pin(async move {
            let strategy = get_strategy.await?;
            return Ok(Accept { inner, strategy });
        })
    }
}

impl Service<TcpStream> for Accept {
    type Response = ();
    type Error = Never;
    type Future = Pin<Box<dyn Future<Output = Result<(), Never>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: TcpStream) -> Self::Future {
        let accept = self.clone();

        Box::pin(async move {
            // TODO metrics...

            match res {
                Err(error) => info!(%error, "Connection closed"),
                Ok(()) => debug!("Connection closed"),
            }

            Ok(())
        })
    }
}

impl Accept {
    async fn detect(&self, tcp: TcpStream) -> Result<(Protocol, BoxedIo), Error> {
        use linkerd2_app_core::transport::io::Peekable;

        let (buffer_capacity, timeout) = match self.strategy.borrow().detect {
            Detect::Opaque => return Ok((Protocol::Unknown, BoxedIo::new(tcp))),
            Detect::Client {
                buffer_capacity,
                timeout,
            } => (buffer_capacity, timeout),
        };

        // TODO sniff SNI?
        let peek = tcp.peek(buffer_capacity);
        let io = tokio::time::timeout(timeout, peek).await??;

        let proto = http::Version::from_prefix(io.prefix().as_ref())
            .map(Protocol::Http)
            .unwrap_or(Protocol::Unknown);

        Ok((proto, BoxedIo::new(io)))
    }

    async fn proxy_tcp(mut self, io: BoxedIo) -> Result<(), Error> {
        // There's no need to watch for updates with TCP streams, since the routing decision is made
        // instantaneously.
        let Strategy {
            addr, mut logical, ..
        } = self.strategy.borrow().clone();

        loop {
            logical = match logical {
                Logical::Split(split) => {
                    let idx = split.weights.sample(&mut self.inner.rng);
                    debug_assert!(idx < split.targets.len());
                    split.targets[idx].clone()
                }

                Logical::Concrete(Concrete::Balance(authority, _)) => {
                    // TODO TCP discovery/balancing.
                    warn!(%authority, "TCP load balancing not supported yet; forwarding");
                    Logical::Concrete(Concrete::Forward(addr, Endpoint::default()))
                }

                Logical::Concrete(Concrete::Forward(addr, endpoint)) => {
                    let id = endpoint
                        .identity
                        .clone()
                        .map(tls::Conditional::Some)
                        .unwrap_or_else(|| {
                            tls::Conditional::None(
                                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                            )
                        });

                    let dst_io = self.connect(addr, id, endpoint.connect_timeout).await?;
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
        mut timeout: Duration,
    ) -> Result<impl io::AsyncRead + io::AsyncWrite + Send, Error> {
        let Inner {
            config,
            identity,
            metrics,
            ..
        } = self.inner;
        if timeout == Duration::default() {
            timeout = config.connect.timeout;
        }

        let connect = svc::connect(config.connect.keepalive)
            // Initiates mTLS if the target is configured with identity.
            .push(tls::client::ConnectLayer::new(identity))
            // Limits the time we wait for a connection to be established.
            .push_timeout(timeout)
            .push(metrics.transport.layer_connect(crate::TransportLabels))
            .into_inner();

        let endpoint = TcpEndpoint {
            addr,
            identity: peer_identity,
        };
        let io = connect.oneshot(endpoint).await?;

        Ok(io)
    }
}

#[derive(Clone, Debug)]
struct HttpService {
    rx: watch::Receiver<Strategy>,
    concretes: HashMap<String, ()>,
    endpoints: HashMap<String, ()>,
}

impl Service<http::Request<http::Body>> for HttpService {
    type Response = http::Response<http::boxed::Payload>;
    type Error = Error;
    type Future = Pin<
        Box<
            dyn Future<Output = Result<http::Response<http::boxed::Payload>, Error>>
                + Send
                + 'static,
        >,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if let Poll::Ready(Some(strategy)) = self.rx.poll_recv_ref(cx) {
            let _logical = strategy.logical.clone();
            // loop {
            //     target = match target {
            //         Target::Concrete(concrete) => unimplemented!("LB"),
            //         Target::Endpoint(endpoint) => unimplemented!("endpoint"),
            //         Target::LogicalSplit(split) => unimplemented!("split"),
            //     };
            // }
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<http::Body>) -> Self::Future {
        Box::pin(async move {
            let _ = req;
            unimplemented!();
        })
    }
}
