use futures::{Async, Future, Poll};
use indexmap::IndexMap;
use linkerd2_metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use linkerd2_stack::{NewService, Proxy};
use std::fmt;
use std::sync::{Arc, Mutex};

metrics! {
    stack_failure_total: Counter { "Total number stacks failures" },
    stack_create_total: Counter { "Total number stacks created" },
    stack_drop_total: Counter { "Total number stacks dropped" },
    stack_make_total: Counter { "Total number of services made" },
    stack_service_failure_total: Counter { "Total number of service failures" },
    stack_service_request_total: Counter { "Total number of requests processed by services" },
    stack_service_drop_total: Counter { "Total number of services dropped" }
}

type Registry = Arc<Mutex<IndexMap<Scope, Arc<Metrics>>>>;

#[derive(Copy, Clone, Debug, Default, Hash, PartialEq, Eq)]
struct Scope(&'static str);

#[derive(Clone, Debug, Default)]
pub struct NewLayer {
    registry: Registry,
}

/// Reports metrics for prometheus.
#[derive(Clone, Debug)]
pub struct Report {
    registry: Registry,
}

#[derive(Clone, Debug)]
pub struct Layer {
    metrics: Arc<Metrics>,
}

#[derive(Debug, Default)]
struct Metrics {
    create_total: Counter,
    drop_total: Counter,
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

#[derive(Clone, Debug)]
pub struct Service<S> {
    inner: S,
    metrics: Arc<Metrics>,
}

impl NewLayer {
    pub fn new_layer(&self, name: &'static str) -> Layer {
        let metrics = {
            let mut registry = self.registry.lock().expect("stack metrics lock poisoned");
            registry
                .entry(Scope(name))
                .or_insert_with(|| Default::default())
                .clone()
        };
        Layer { metrics }
    }

    pub fn report(&self) -> Report {
        Report {
            registry: self.registry.clone(),
        }
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = Make<S>;

    fn layer(&self, inner: S) -> Self::Service {
        self.metrics.create_total.incr();
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
    type Service = S::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.inner.new_service(target)
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

impl<S> Drop for Make<S> {
    fn drop(&mut self) {
        self.metrics.drop_total.incr();
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

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let registry = self.registry.lock().expect("metrics registry poisoned");
        if registry.is_empty() {
            return Ok(());
        }

        stack_failure_total.fmt_help(f)?;
        stack_failure_total.fmt_scopes(f, registry.iter(), |m| &m.failure_total)?;

        stack_create_total.fmt_help(f)?;
        stack_create_total.fmt_scopes(f, registry.iter(), |m| &m.create_total)?;

        stack_drop_total.fmt_help(f)?;
        stack_drop_total.fmt_scopes(f, registry.iter(), |m| &m.drop_total)?;

        stack_make_total.fmt_help(f)?;
        stack_make_total.fmt_scopes(f, registry.iter(), |m| &m.make_success_total)?;
        stack_make_total.fmt_scopes(f, registry.iter().map(|(s, m)| ((s, Failure), m)), |m| {
            &m.make_failure_total
        })?;

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

impl FmtLabels for Scope {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scope=\"{}\"", self.0)
    }
}
