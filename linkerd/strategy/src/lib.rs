#![recursion_limit = "256"]

use futures::prelude::*;
use http_body::Body as HttpBody;
use indexmap::IndexMap;
//use linkerd2_addr::Addr;
use linkerd2_error::{Error, Recover};
use linkerd2_identity as identity;
use linkerd2_proxy_api::destination::{self as api, destination_client::DestinationClient};
use rand::distributions::WeightedIndex;
use std::{
    //collections::HashMap,
    convert::TryFrom,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::watch;
use tonic::{
    self as grpc,
    body::{Body, BoxBody},
    client::GrpcService,
};
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    service: DestinationClient<S>,
    recover: R,
    context_token: String,
}

#[derive(Clone, Debug)]
pub struct Strategy {
    pub addr: SocketAddr,
    pub detect: Detect,   // TODO watch::Receiver<...>
    pub logical: Logical, // TODO watch::Receiver<...>
}

#[derive(Copy, Clone, Debug)]
pub enum Detect {
    Opaque,
    Client {
        buffer_capacity: usize,
        timeout: Duration,
    },
}

#[derive(Clone, Debug)]
pub enum Logical {
    Concrete(Concrete),
    Split(Arc<Split>),
    Fallback(Arc<Fallback>),
}

#[derive(Clone, Debug)]
pub enum Concrete {
    Forward(Endpoint),
    Balance {
        destination: http::uri::Authority,
        // Peak-EWMA settings, if applicable.
        peak_ewma_decay: Duration,
        peak_ewma_default_rtt: Duration,
    },
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub addr: SocketAddr,

    pub identity: Option<identity::Name>,

    // If set, indicates that the endpoint is running a proxy that supports HTTP
    // transport upgrading (i.e., using the l5d-orig-proto header).
    pub proxy_http_transport: Option<ProxyHttpTransport>,

    pub authority_override: Option<http::uri::Authority>,

    pub metric_labels: IndexMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum ProxyHttpTransport {
    H2,
    // TODO: H3?
}

#[derive(Clone, Debug)]
pub struct Fallback {
    primary: Logical,
    fallback: Logical,
}

#[derive(Clone, Debug)]
pub struct Split {
    targets: Vec<Logical>,
    weights: WeightedIndex<u32>,
}

pub type Init = (Strategy, grpc::codec::Streaming<api::StrategyResponse>);

impl<S, R> tower::Service<SocketAddr> for Client<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover<grpc::Status> + Clone + Send + 'static,
    R::Backoff: Send + Unpin,
{
    type Response = watch::Receiver<Strategy>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<watch::Receiver<Strategy>, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr: SocketAddr) -> Self::Future {
        let mut client = self.clone();
        Box::pin(async move { client.watch(addr).await })
    }
}

impl<S, R> Client<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover<grpc::Status> + Clone + Send + 'static,
    R::Backoff: Send + Unpin,
{
    async fn watch(&mut self, addr: SocketAddr) -> Result<watch::Receiver<Strategy>, Error> {
        let mut service = self.service.clone();
        let mut recover = self.recover.clone();
        let req = api::StrategyRequest {
            target: Some(addr.into()),
            context_token: self.context_token.clone(),
        };

        let (strategy, stream) = match Self::init(addr, &mut service, req.clone()).await {
            Ok(rsp) => rsp,
            Err(status) => {
                Self::recover(addr, &mut service, req.clone(), &mut recover, status).await?
            }
        };

        let (tx, rx) = watch::channel(strategy);
        tokio::spawn(Self::daemon(addr, service, req, recover, tx, stream));
        Ok(rx)
    }

    /// Processes an initialized stream/watch, recovering as permitted.
    async fn daemon(
        addr: SocketAddr,
        mut service: DestinationClient<S>,
        req: api::StrategyRequest,
        mut recover: R,
        mut tx: watch::Sender<Strategy>,
        mut responses: grpc::codec::Streaming<api::StrategyResponse>,
    ) {
        loop {
            match Self::broadcast(addr, &mut tx, &mut responses).await {
                Ok(()) => {
                    trace!("Shutting down; all receivers dropped");
                    return;
                }
                Err(status) => {
                    futures::select_biased! {
                        () = tx.closed().fuse() => { return; }
                        res = Self::recover(addr, &mut service, req.clone(), &mut recover, status).fuse() => {
                            match res {
                                Err(error) => {
                                    trace!(%error, "Watch failed");
                                    return;
                                }
                                Ok((strategy, stream)) => {
                                    // Broadcast the first update from the stream
                                    let _ = tx.broadcast(strategy);
                                    responses = stream;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Retrieves the initial strategy.
    async fn init(
        addr: SocketAddr,
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
    ) -> Result<Init, grpc::Status> {
        let mut stream = service.strategy(req.clone()).await?.into_inner();
        match stream.try_next().await {
            Ok(Some(rsp)) => Ok((Strategy::new(addr, rsp), stream)),
            Ok(None) => Err(grpc::Status::new(grpc::Code::Ok, "server closed stream")),
            Err(status) => Err(status),
        }
    }

    /// Publishes updates from `responses` to `tx` until either close.
    ///
    /// An error is returned if the `responses` stream terminates. Success is
    /// returned if `tx` is closed.
    async fn recover(
        addr: SocketAddr,
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
        recover: &mut R,
        status: grpc::Status,
    ) -> Result<Init, Error> {
        let mut backoff = recover.recover(status)?;

        loop {
            if backoff.next().await.is_none() {
                return Err(grpc::Status::new(grpc::Code::Ok, "Backoff exhausted").into());
            }

            match Self::init(addr, service, req.clone()).await {
                Ok(rsp) => return Ok(rsp),
                Err(status) => {
                    // Reuse the existing backoff instead of resetting.
                    let _ = recover.recover(status)?;
                }
            }
        }
    }

    /// Publishes updates from `responses` to `tx` until either close.
    ///
    /// An error is returned if the `responses` stream terminates. Success is
    /// returned if `tx` is closed.
    async fn broadcast(
        addr: SocketAddr,
        tx: &mut watch::Sender<Strategy>,
        responses: &mut grpc::codec::Streaming<api::StrategyResponse>,
    ) -> Result<(), grpc::Status> {
        loop {
            futures::select_biased! {
                () = tx.closed().fuse() => {
                    return Ok(());
                }
                res = responses.try_next().fuse() => {
                    match res? {
                        Some(s) => {
                            let _ = tx.broadcast(Strategy::new(addr, s));
                        }
                        None => {
                            return Err(grpc::Status::new(grpc::Code::Ok, "server closed stream"));
                        }
                    }
                }
            }
        }
    }
}

impl Strategy {
    fn new(
        addr: SocketAddr,
        api::StrategyResponse { detect, logical }: api::StrategyResponse,
    ) -> Self {
        use api::protocol_detection as detect;

        let detect = detect
            .and_then(|d| {
                d.strategy.map(|s| match s {
                    detect::Strategy::Opaque(_) => Detect::Opaque,
                    detect::Strategy::Client(detect::Client {
                        buffer_capacity,
                        timeout,
                    }) => Detect::Client {
                        buffer_capacity: buffer_capacity as usize,
                        timeout: timeout
                            .and_then(|t| Duration::try_from(t).ok())
                            .unwrap_or_else(|| Duration::from_secs(0)),
                    },
                })
            })
            .unwrap_or(Detect::Opaque);

        let logical = logical.and_then(Logical::from_api).unwrap_or_else(|| {
            Logical::Concrete(Concrete::Forward(Endpoint {
                addr,
                identity: None,
                proxy_http_transport: None,
                authority_override: None,
                metric_labels: Default::default(),
            }))
        });

        Strategy {
            addr,
            detect,
            logical,
        }
    }
}

impl Logical {
    fn from_api(api::Logical { kind }: api::Logical) -> Option<Self> {
        let target = match kind? {
            api::logical::Kind::Concrete(api::Concrete { kind }) => match kind? {
                api::concrete::Kind::Forward(api::concrete::Forward { addr }) => {
                    let addr = SocketAddr::try_from(addr?.addr?).ok()?;
                    unimplemented!()
                }
                api::concrete::Kind::Balance(api::concrete::Balance {
                    authority,
                    peak_ewma_decay,
                    peak_ewma_default_rtt,
                }) => unimplemented!(),
            },

            api::logical::Kind::Split(split) => {
                let mut targets = Vec::with_capacity(split.targets.len());
                let mut weights = Vec::with_capacity(split.targets.len());
                for api::logical::split::Weighted { target, weight } in split.targets.into_iter() {
                    if weight > 0 {
                        let target = Logical::from_api(target?)?;
                        targets.push(target);
                        weights.push(weight);
                    }
                }
                debug_assert_eq!(weights.len(), targets.len());

                // Returns None if weights is empty.
                let weights = WeightedIndex::new(weights).ok()?;
                Self::Split(Arc::new(Split { targets, weights }))
            }
        };

        Some(target)
    }
}
