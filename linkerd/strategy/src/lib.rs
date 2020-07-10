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

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    service: DestinationClient<S>,
    recover: R,
    context_token: String,
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
    pub async fn strategy(
        &mut self,
        addr: SocketAddr,
    ) -> Result<watch::Receiver<api::StrategyResponse>, Error> {
        let req = api::StrategyRequest {
            target: Some(addr.into()),
            context_token: self.context_token.clone(),
        };

        let (strategy, mut stream) = match Self::init(&mut self.service, req.clone()).await {
            Ok(rsp) => rsp,
            Err(status) => {
                Self::recover(&mut self.service, req.clone(), &mut self.recover, status).await?
            }
        };

        let (mut tx, rx) = watch::channel(strategy);
        let mut service = self.service.clone();
        let mut recover = self.recover.clone();
        tokio::spawn(async move {
            loop {
                match Self::broadcast(&mut tx, &mut stream).await {
                    Ok(()) => return,
                    Err(status) => {
                        futures::select_biased! {
                            () = tx.closed().fuse() => { return; }
                            res = Self::recover(&mut service, req.clone(), &mut recover, status).fuse() => {
                                match res {
                                    Ok((strategy, new_stream)) => {
                                        let _ = tx.broadcast(strategy);
                                        new_stream
                                    }
                                    Err(_) => return,
                                }
                            }
                        }
                    }
                };
            }
        });

        Ok(rx)
    }

    async fn init(
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
    ) -> Result<
        (
            api::StrategyResponse,
            grpc::codec::Streaming<api::StrategyResponse>,
        ),
        grpc::Status,
    > {
        let mut stream = service.strategy(req.clone()).await?.into_inner();
        match stream.try_next().await {
            Ok(Some(s)) => Ok((s, stream)),
            Ok(None) => Err(grpc::Status::new(grpc::Code::Ok, "server closed stream")),
            Err(status) => Err(status),
        }
    }

    async fn recover(
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
        recover: &mut R,
        status: grpc::Status,
    ) -> Result<
        (
            api::StrategyResponse,
            grpc::codec::Streaming<api::StrategyResponse>,
        ),
        Error,
    > {
        let mut backoff = recover.recover(status)?;
        loop {
            if backoff.try_next().err_into::<Error>().await?.is_none() {
                return Err(grpc::Status::new(grpc::Code::Ok, "Backoff exhausted").into());
            }

            let res = Self::init(service, req.clone()).await;
            match res {
                Ok(rsp) => return Ok(rsp),
                Err(status) => {
                    let _ = recover.recover(status)?;
                }
            }
        }
    }

    async fn broadcast(
        tx: &mut watch::Sender<api::StrategyResponse>,
        stream: &mut grpc::codec::Streaming<api::StrategyResponse>,
    ) -> Result<(), grpc::Status> {
        loop {
            futures::select_biased! {
                () = tx.closed().fuse() => {
                    return Ok(());
                }
                res = stream.try_next().fuse() => {
                    match res? {
                        Some(strategy) => {
                            if tx.broadcast(strategy).is_err() {
                                return Ok(());
                            }
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
