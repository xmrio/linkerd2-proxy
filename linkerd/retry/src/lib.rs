#![deny(warnings, rust_2018_idioms)]

use futures::{Future, Poll};
use linkerd2_error::Error;
use linkerd2_stack::{proxy, NewService, Proxy};
use tower::retry;
pub use tower::retry::{budget::Budget, Policy};
use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

pub trait NewPolicy<T> {
    type Policy;

    fn new_policy(&self, target: &T) -> Option<Self::Policy>;
}

#[derive(Clone, Debug)]
pub struct Layer<P> {
    new_policy: P,
}

#[derive(Clone, Debug)]
pub struct NewRetry<P, N> {
    new_policy: P,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Retry<P, S> {
    policy: Option<P>,
    inner: S,
}

pub enum ResponseFuture<R, P, S, Req>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    Disabled(P::Future),
    Retry(Oneshot<retry::Retry<R, proxy::Service<P, S>>, Req>),
}

// === impl Layer ===

impl<P> Layer<P> {
    pub fn new(new_policy: P) -> Self {
        Self { new_policy }
    }
}

impl<P: Clone, N> tower::layer::Layer<N> for Layer<P> {
    type Service = NewRetry<P, N>;

    fn layer(&self, inner: N) -> Self::Service {
        Self::Service {
            inner,
            new_policy: self.new_policy.clone(),
        }
    }
}

// === impl Stack ===

impl<T, N, P> NewService<T> for NewRetry<P, N>
where
    N: NewService<T>,
    P: NewPolicy<T>,
{
    type Service = Retry<P::Policy, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let policy = self.new_policy.new_policy(&target);
        let inner = self.inner.new_service(target);
        Retry { policy, inner }
    }
}

// === impl Retry ===

impl<R, P, Req, S> Proxy<Req, S> for Retry<R, P>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = ResponseFuture<R, P, S, Req>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());
        if let Some(policy) = self.policy.clone() {
            let retry = retry::Retry::new(policy, self.inner.clone().into_service(svc.clone()));
            return ResponseFuture::Retry(retry.oneshot(req));
        }

        ResponseFuture::Disabled(self.inner.proxy(svc, req))
    }
}

impl<R, P, S, Req> Future for ResponseFuture<R, P, S, Req>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Item = P::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResponseFuture::Disabled(ref mut f) => f.poll().map_err(Into::into),
            ResponseFuture::Retry(ref mut f) => f.poll().map_err(Into::into),
        }
    }
}
