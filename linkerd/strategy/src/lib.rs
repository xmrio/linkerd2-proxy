#![recursion_limit = "256"]

use futures::prelude::*;
use http_body::Body as HttpBody;
use linkerd2_error::{Error, Recover};
use linkerd2_proxy_api::destination::{self as api, destination_client::DestinationClient};
use std::net::SocketAddr;
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

type Init = (
    api::StrategyResponse,
    grpc::codec::Streaming<api::StrategyResponse>,
);

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
    pub async fn watch(
        &mut self,
        addr: SocketAddr,
    ) -> Result<watch::Receiver<api::StrategyResponse>, Error> {
        let req = api::StrategyRequest {
            target: Some(addr.into()),
            context_token: self.context_token.clone(),
        };

        let (strategy, stream) = match Self::init(&mut self.service, req.clone()).await {
            Ok(rsp) => rsp,
            Err(status) => {
                Self::recover(&mut self.service, req.clone(), &mut self.recover, status).await?
            }
        };

        let (tx, rx) = watch::channel(strategy);
        tokio::spawn(Self::daemon(
            self.service.clone(),
            req,
            self.recover.clone(),
            tx,
            stream,
        ));

        Ok(rx)
    }

    /// Processes an initialized stream/watch, recovering as permitted.
    async fn daemon(
        mut service: DestinationClient<S>,
        req: api::StrategyRequest,
        mut recover: R,
        mut tx: watch::Sender<api::StrategyResponse>,
        mut stream: grpc::codec::Streaming<api::StrategyResponse>,
    ) {
        loop {
            match Self::broadcast(&mut stream, &mut tx).await {
                Ok(()) => {
                    trace!("Shutting down; all receivers dropped");
                    return;
                }
                Err(status) => {
                    futures::select_biased! {
                        () = tx.closed().fuse() => { return; }
                        res = Self::recover(&mut service, req.clone(), &mut recover, status).fuse() => {
                            match res {
                                Err(error) => {
                                    trace!(%error, "Watch failed");
                                    return;
                                }
                                Ok((strategy, new_stream)) => {
                                    // Broadcast the first update from the stream
                                    let _ = tx.broadcast(strategy);
                                    stream = new_stream;
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
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
    ) -> Result<Init, grpc::Status> {
        let mut stream = service.strategy(req.clone()).await?.into_inner();
        match stream.try_next().await {
            Ok(Some(s)) => Ok((s, stream)),
            Ok(None) => Err(grpc::Status::new(grpc::Code::Ok, "server closed stream")),
            Err(status) => Err(status),
        }
    }

    /// Publishes updates from `responses` to `tx` until either close.
    ///
    /// An error is returned if the `responses` stream terminates. Success is
    /// returned if `tx` is closed.
    async fn recover(
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
        recover: &mut R,
        status: grpc::Status,
    ) -> Result<Init, Error> {
        let mut backoff = recover.recover(status)?;

        loop {
            if backoff.try_next().err_into::<Error>().await?.is_none() {
                return Err(grpc::Status::new(grpc::Code::Ok, "Backoff exhausted").into());
            }

            let res = Self::init(service, req.clone()).await;
            match res {
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
        responses: &mut grpc::codec::Streaming<api::StrategyResponse>,
        tx: &mut watch::Sender<api::StrategyResponse>,
    ) -> Result<(), grpc::Status> {
        loop {
            futures::select_biased! {
                () = tx.closed().fuse() => {
                    return Ok(());
                }
                res = responses.try_next().fuse() => {
                    match res? {
                        Some(strategy) => {
                            let _ = tx.broadcast(strategy);
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
