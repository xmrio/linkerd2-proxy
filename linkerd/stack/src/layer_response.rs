//! A middleware that applies a layer to the inner service's response.
//!
//! This is typically used in the context of `tower::MakeService`.

use futures::{try_ready, Future, Poll};

/// Layers over services such that an `L`-typed Layer is applied to the responses
/// of the inner service.
#[derive(Clone, Debug)]
pub struct LayerResponseLayer<L>(L);

/// Applies `L`-typed layers to the reresponses of an `S`-typed service.
#[derive(Clone, Debug)]
pub struct LayerResponse<L, S> {
    inner: S,
    layer: L,
}

impl<L> LayerResponseLayer<L> {
    pub fn new(layer: L) -> LayerResponseLayer<L> {
        LayerResponseLayer(layer)
    }
}

impl<M, L: Clone> tower::layer::Layer<M> for LayerResponseLayer<L> {
    type Service = LayerResponse<L, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            layer: self.0.clone(),
        }
    }
}

impl<T, L, M> super::NewService<T> for LayerResponse<L, M>
where
    L: tower::layer::Layer<M::Service>,
    M: super::NewService<T>,
{
    type Service = L::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.layer.layer(self.inner.new_service(target))
    }
}

impl<T, L, M> tower::Service<T> for LayerResponse<L, M>
where
    L: tower::layer::Layer<M::Response> + Clone,
    M: tower::Service<T>,
{
    type Response = L::Service;
    type Error = M::Error;
    type Future = LayerResponse<L, M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);
        Self::Future {
            layer: self.layer.clone(),
            inner,
        }
    }
}

impl<L, F> Future for LayerResponse<L, F>
where
    L: tower::layer::Layer<F::Item>,
    F: Future,
{
    type Item = L::Service;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(self.layer.layer(inner).into())
    }
}
