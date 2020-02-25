use futures::{future, Async, Future, Poll};
use linkerd2_error::Error;
use linkerd2_stack::{NewService, Proxy};
use tracing::{trace, Span};
use tracing_futures::{Instrument as _, Instrumented};

pub trait GetSpan<T> {
    fn get_span(&self, target: &T) -> tracing::Span;
}

impl<T, F> GetSpan<T> for F
where
    F: Fn(&T) -> tracing::Span,
{
    fn get_span(&self, target: &T) -> tracing::Span {
        (self)(target)
    }
}

impl<T: GetSpan<()>> GetSpan<T> for () {
    fn get_span(&self, t: &T) -> tracing::Span {
        t.get_span(&())
    }
}

impl<T> GetSpan<T> for tracing::Span {
    fn get_span(&self, _: &T) -> tracing::Span {
        self.clone()
    }
}

#[derive(Clone, Debug)]
pub struct MakeInstrumentLayer<G> {
    get_span: G,
}

#[derive(Clone, Debug)]
pub struct MakeInstrument<G, M> {
    get_span: G,
    make: M,
}

#[derive(Clone, Debug)]
pub struct Instrument<S> {
    span: Span,
    inner: S,
}

impl<G> MakeInstrumentLayer<G> {
    pub fn new(get_span: G) -> Self {
        Self { get_span }
    }
}

impl MakeInstrumentLayer<()> {
    pub fn from_target() -> Self {
        Self::new(())
    }
}

impl Default for MakeInstrumentLayer<Span> {
    fn default() -> Self {
        Self::new(Span::current())
    }
}

impl<G: Clone, M> tower::layer::Layer<M> for MakeInstrumentLayer<G> {
    type Service = MakeInstrument<G, M>;

    fn layer(&self, make: M) -> Self::Service {
        Self::Service {
            make,
            get_span: self.get_span.clone(),
        }
    }
}

impl<T, G, N> NewService<T> for MakeInstrument<G, N>
where
    T: std::fmt::Debug,
    G: GetSpan<T>,
    N: NewService<T>,
{
    type Service = Instrument<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        trace!(?target, "new_service");
        let span = self.get_span.get_span(&target);
        let inner = span.in_scope(move || self.make.new_service(target));
        Instrument { inner, span }
    }
}

impl<T, G, M> tower::Service<T> for MakeInstrument<G, M>
where
    T: std::fmt::Debug,
    G: GetSpan<T>,
    M: tower::Service<T>,
    M::Error: Into<Error>,
{
    type Response = Instrument<M::Response>;
    type Error = Error;
    type Future = Instrument<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        trace!("poll_ready");
        match self.make.poll_ready() {
            Err(e) => {
                let error = e.into();
                trace!(%error);
                Err(error)
            }
            Ok(ready) => {
                trace!(ready = ready.is_ready());

                Ok(ready)
            }
        }
    }

    fn call(&mut self, target: T) -> Self::Future {
        trace!(?target, "make_service");
        let span = self.get_span.get_span(&target);
        let inner = self.make.call(target);
        Instrument { inner, span }
    }
}

impl<F> Future for Instrument<F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = Instrument<F::Item>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let _enter = self.span.enter();

        trace!("making");
        match self.inner.poll() {
            Err(e) => {
                let error = e.into();
                trace!(%error);
                Err(error)
            }
            Ok(Async::NotReady) => {
                trace!(ready = false);
                Ok(Async::NotReady)
            }
            Ok(Async::Ready(inner)) => {
                trace!(ready = true);
                let svc = Instrument {
                    inner,
                    span: self.span.clone(),
                };
                Ok(svc.into())
            }
        }
    }
}

impl<Req, S, P> Proxy<Req, S> for Instrument<P>
where
    Req: std::fmt::Debug,
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = Instrumented<P::Future>;

    fn proxy(&self, svc: &mut S, request: Req) -> Self::Future {
        let _enter = self.span.enter();
        trace!(?request, "proxy");
        self.inner.proxy(svc, request).instrument(self.span.clone())
    }
}

impl<Req, S> tower::Service<Req> for Instrument<S>
where
    Req: std::fmt::Debug,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<Instrumented<S::Future>, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let _enter = self.span.enter();

        trace!("poll ready");
        match self.inner.poll_ready() {
            Err(e) => {
                let error = e.into();
                trace!(%error);
                Err(error)
            }
            Ok(ready) => {
                trace!(ready = ready.is_ready());
                Ok(ready)
            }
        }
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let _enter = self.span.enter();

        trace!(?request, "call");
        self.inner
            .call(request)
            .instrument(self.span.clone())
            .map_err(Into::into)
    }
}
