use std::marker::PhantomData;
pub use tower::util::BoxService;

pub struct BoxLayer<Req, Rsp, E> {
    _p: PhantomData<fn(Req, Rsp, E)>,
}

impl<Req, Rsp, E> BoxLayer<Req, Rsp, E> {
    pub fn new() -> Self {
        Self { _p: PhantomData }
    }
}

impl<S, Req, Rsp, E> tower::layer::Layer<S> for BoxLayer<Req, Rsp, E>
where
    S: tower::Service<Req, Response = Rsp, Error = E> + Send + 'static,
    S::Future: Send + 'static,
{
    type Service = BoxService<Req, Rsp, E>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxService::new(inner)
    }
}
