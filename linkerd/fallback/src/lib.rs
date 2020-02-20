#![deny(warnings, rust_2018_idioms)]

use futures::{Future, Poll};
use linkerd2_error::Error;
use tower::util::{Either, Oneshot, ServiceExt};
use tracing::debug;

mod layer;

pub use self::layer::FallbackLayer;

#[derive(Clone, Debug)]
pub struct Fallback<I, F, P = fn(&Error) -> bool> {
    inner: I,
    fallback: F,
    predicate: P,
}

pub enum MakeFuture<A, B, P> {
    A { a: A, b: Option<B>, predicate: P },
    B(B),
}

// === impl Fallback ===

impl<A, B, P, T> tower::Service<T> for Fallback<A, B, P>
where
    T: Clone,
    A: tower::Service<T>,
    A::Error: Into<Error>,
    B: tower::Service<T> + Clone,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
    P: Clone,
{
    type Response = Either<A::Response, B::Response>;
    type Error = Error;
    type Future = MakeFuture<A::Future, Oneshot<B, T>, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture::A {
            a: self.inner.call(target.clone()),
            b: Some(self.fallback.clone().oneshot(target)),
            predicate: self.predicate.clone(),
        }
    }
}

// === impl MakeFuture ===

impl<A, B, P> Future for MakeFuture<A, B, P>
where
    A: Future,
    A::Error: Into<Error>,
    B: Future,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Item = Either<A::Item, B::Item>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                MakeFuture::A {
                    ref mut a,
                    ref mut b,
                    ref predicate,
                } => match a.poll() {
                    Ok(ok) => return Ok(ok.map(Either::A)),
                    Err(e) => {
                        let error = e.into();
                        if !(predicate)(&error) {
                            return Err(error);
                        }

                        debug!(%error, "Falling back");
                        MakeFuture::B(b.take().unwrap())
                    }
                },
                MakeFuture::B(ref mut b) => {
                    return b.poll().map(|ok| ok.map(Either::B)).map_err(Into::into);
                }
            };
        }
    }
}
