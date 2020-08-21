use futures::prelude::*;
use linkerd2_app_core::{profiles, Error};
use linkerd2_strategy::{Concrete, Detect, Endpoint, Logical, Strategy};
use std::{
    collections::HashSet,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tower::{util::ServiceExt, Service};

pub type Receiver = watch::Receiver<Strategy>;

#[derive(Clone)]
pub struct Router<S, M> {
    get_strategy: S,
    make_accept: M,
}

#[derive(Clone, Debug)]
pub struct FromProfiles<P> {
    opaque_ports: Arc<HashSet<u16>>,
    get_profiles: P,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub addr: SocketAddr,

    pub strategy: watch::Receiver<Strategy>,

    // TODO this should be lazy if/when strategy resolution is its own API.
    pub profile: profiles::Receiver,
}

impl<S, M> Router<S, M> {
    pub fn new(get_strategy: S, make_accept: M) -> Self {
        Self {
            get_strategy,
            make_accept,
        }
    }
}

impl<P, M> Router<FromProfiles<P>, M> {
    pub fn from_profiles(
        opaque_ports: impl IntoIterator<Item = u16>,
        get_profiles: P,
        make_accept: M,
    ) -> Self {
        let profiles = FromProfiles {
            get_profiles,
            opaque_ports: Arc::new(opaque_ports.into_iter().collect()),
        };
        Router::new(profiles, make_accept)
    }
}

impl<T, S, M> Service<T> for Router<S, M>
where
    T: Into<SocketAddr>,
    S: Service<SocketAddr, Response = Target> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::Future: Send + 'static,
    M: Service<Target> + Clone + Send + 'static,
    M::Error: Into<Error> + Send,
    M::Future: Send + 'static,
{
    type Response = M::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<M::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(futures::ready!(self.get_strategy.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let addr: SocketAddr = target.into();
        let get_strategy = self.get_strategy.call(addr).err_into::<Error>();
        let make = self.make_accept.clone();

        Box::pin(async move {
            let target = get_strategy.await?;
            make.oneshot(target).err_into::<Error>().await
        })
    }
}

impl<P> Service<SocketAddr> for FromProfiles<P>
where
    P: Service<SocketAddr, Response = profiles::Receiver> + Clone + Send + 'static,
    P::Error: Into<Error> + Send,
    P::Future: Send + 'static,
{
    type Response = Target;
    type Error = P::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Target, P::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), P::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr: SocketAddr) -> Self::Future {
        let is_opaque = self.opaque_ports.contains(&addr.port());
        let get_profiles = self.get_profiles.clone();

        Box::pin(async move {
            if is_opaque {
                let (mut stx, strategy) = watch::channel(Strategy {
                    addr,
                    detect: Detect::Opaque,
                    logical: Logical::Concrete(Concrete::Forward(addr, Endpoint::default())),
                });
                let (mut ptx, profile) = watch::channel(profiles::Routes::default());
                // Hold the sender until all receivers have dropped.
                tokio::spawn(async move {
                    futures::join!(stx.closed(), ptx.closed());
                });
                return Ok(Target {
                    addr,
                    profile,
                    strategy,
                });
            }

            let mut profile_rx = get_profiles.oneshot(addr).await?;
            let init = Strategy::from_profile(addr, profile_rx.borrow().clone());
            let (mut tx, rx) = watch::channel(init);
            let profile = profile_rx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = tx.closed() => { return; }
                        p = profile_rx.recv() => {
                            if let Some(profile) = p {
                                let _ = tx.broadcast(Strategy::from_profile(addr, profile));
                            }
                        }
                    }
                }
            });

            Ok(Target {
                addr,
                profile,
                strategy: rx,
            })
        })
    }
}
