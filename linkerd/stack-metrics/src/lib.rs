use futures::{Async, Future, Poll};
use indexmap::IndexMap;
use linkerd2_metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use linkerd2_stack::{NewService, Proxy};
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

metrics! {
    stack_failure_total: Counter { "Total number stacks failures" },
    stack_make_service_total: Counter { "Total number of services made" },
    stack_service_failure_total: Counter { "Total number of service failures" },
    stack_service_request_total: Counter { "Total number of requests processed by services" },
    stack_service_drop_total: Counter { "Total number of services dropped" }
}

type Registry<L> = Arc<Mutex<IndexMap<L, Arc<Metrics>>>>;

#[derive(Debug)]
pub struct NewLayer<L: Hash + Eq> {
    registry: Registry<L>,
}

/// Reports metrics for prometheus.
#[derive(Debug)]
pub struct Report<L: Hash + Eq> {
    registry: Registry<L>,
}

#[derive(Clone, Debug)]
pub struct Layer {
    metrics: Arc<Metrics>,
}

#[derive(Debug, Default)]
struct Metrics {
    failure_total: Counter,
    make_success_total: Counter,
    make_failure_total: Counter,
    service_failure_total: Counter,
    service_request_total: Counter,
    service_drop_total: Counter,
}

#[derive(Clone, Debug)]
pub struct Make<S> {
    inner: S,
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
pub struct Service<S> {
    inner: S,
    metrics: Arc<Metrics>,
}

impl<L> NewLayer<L>
where
    L: Hash + Eq,
{
    pub fn new_layer(&self, labels: L) -> Layer {
        let metrics = {
            let mut registry = self.registry.lock().expect("stack metrics lock poisoned");
            registry
                .entry(labels.into())
                .or_insert_with(|| Default::default())
                .clone()
        };
        Layer { metrics }
    }

    pub fn report(&self) -> Report<L> {
        Report {
            registry: self.registry.clone(),
        }
    }
}

impl<L: Hash + Eq> Default for NewLayer<L> {
    fn default() -> Self {
        Self {
            registry: Registry::default(),
        }
    }
}

impl<L: Hash + Eq> Clone for NewLayer<L> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
        }
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = Make<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            metrics: self.metrics.clone(),
        }
    }
}

impl<T, S> NewService<T> for Make<S>
where
    S: NewService<T>,
{
    type Service = Service<S::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        self.metrics.make_success_total.incr();
        Self::Service {
            inner,
            metrics: self.metrics.clone(),
        }
    }
}

impl<T, S> tower::Service<T> for Make<S>
where
    S: tower::Service<T>,
{
    type Response = Service<S::Response>;
    type Error = S::Error;
    type Future = Service<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner.poll_ready() {
            Ok(ready) => Ok(ready),
            Err(e) => {
                self.metrics.failure_total.incr();
                Err(e)
            }
        }
    }

    fn call(&mut self, target: T) -> Self::Future {
        Service {
            inner: self.inner.call(target),
            metrics: self.metrics.clone(),
        }
    }
}

impl<F: Future> Future for Service<F> {
    type Item = Service<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(inner)) => {
                self.metrics.make_success_total.incr();
                let service = Self::Item {
                    inner,
                    metrics: self.metrics.clone(),
                };
                Ok(service.into())
            }
            Err(e) => {
                self.metrics.make_failure_total.incr();
                Err(e)
            }
        }
    }
}

impl<Req, S> tower::Service<Req> for Service<S>
where
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner.poll_ready() {
            Ok(ready) => Ok(ready),
            Err(e) => {
                self.metrics.service_failure_total.incr();
                Err(e)
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.metrics.service_request_total.incr();
        self.inner.call(req)
    }
}

impl<Req, S, P> Proxy<Req, S> for Service<P>
where
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        self.metrics.service_request_total.incr();
        self.inner.proxy(svc, req)
    }
}

impl<S> Drop for Service<S> {
    fn drop(&mut self) {
        self.metrics.service_drop_total.incr();
    }
}

impl<L: Hash + Eq> Clone for Report<L> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
        }
    }
}

impl<L: FmtLabels + Hash + Eq> FmtMetrics for Report<L> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let registry = self.registry.lock().expect("metrics registry poisoned");
        if registry.is_empty() {
            return Ok(());
        }

        stack_failure_total.fmt_help(f)?;
        stack_failure_total.fmt_scopes(f, registry.iter(), |m| &m.failure_total)?;

        stack_make_service_total.fmt_help(f)?;
        stack_make_service_total.fmt_scopes(f, registry.iter(), |m| &m.make_success_total)?;
        stack_make_service_total.fmt_scopes(
            f,
            registry.iter().map(|(s, m)| ((s, Failure), m)),
            |m| &m.make_failure_total,
        )?;

        stack_service_failure_total.fmt_help(f)?;
        stack_service_failure_total.fmt_scopes(f, registry.iter(), |m| &m.service_failure_total)?;

        stack_service_request_total.fmt_help(f)?;
        stack_service_request_total.fmt_scopes(f, registry.iter(), |m| &m.service_request_total)?;

        stack_service_drop_total.fmt_help(f)?;
        stack_service_drop_total.fmt_scopes(f, registry.iter(), |m| &m.service_drop_total)?;

        Ok(())
    }
}

struct Failure;

impl FmtLabels for Failure {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failure=\"true\"")
    }
}
