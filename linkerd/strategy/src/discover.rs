use futures::{prelude::*, ready};
use linkerd2_error::Error;
use linkerd2_proxy_core::Resolve;
use linkerd2_service_profiles as profiles;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct Discover<P, E> {
    get_profiles: P,
    get_endpoints: E,
}

pub struct Strategy {}

impl<P, E> tower::Service<SocketAddr> for Discover<P, E>
where
    P: profiles::GetRoutes<SocketAddr>,
    P::Future: Send + 'static,
    E: Resolve<SocketAddr>,
{
    type Response = Strategy;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Strategy, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(ready!(self.get_profiles.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, addr: SocketAddr) -> Self::Future {
        let get_profiles = self.get_profiles.get_routes(addr);

        Box::pin(async move {
            let mut profiles = get_profiles.err_into::<Error>().await?;

            let dst_overrides = match profiles.recv().await {
                Some(profiles::Routes { dst_overrides, .. }) => dst_overrides,
                None => return Ok(Strategy {}),
            };
            for _dst in dst_overrides {
                unimplemented!();
            }

            Ok(Strategy {})
        })
    }
}
