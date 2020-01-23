//! Layer to map HTTP service errors into appropriate `http::Response`s.

use crate::svc;
use futures::{Future, Poll};
use http::{header, Request, Response, StatusCode, Version};
use linkerd2_error::Error;
use linkerd2_proxy_http::HasH2Reason;
use tracing::{debug, warn};

/// Layer to map HTTP service errors into appropriate `http::Response`s.
#[derive(Clone, Debug)]
pub struct Layer;

#[derive(Clone, Debug)]
pub struct Errors<S>(S);

#[derive(Debug)]
pub struct ResponseFuture<F> {
    inner: F,
    is_http2: bool,
}

#[derive(Debug)]
pub struct StatusError {
    pub code: http::StatusCode,
    pub error: Error,
}

impl<S> svc::Layer<S> for Layer {
    type Service = Errors<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Errors(inner)
    }
}

impl<S, B1, B2> svc::Service<Request<B1>> for Errors<S>
where
    S: svc::Service<Request<B1>, Response = Response<B2>>,
    S::Error: Into<Error>,
    B2: Default,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Request<B1>) -> Self::Future {
        let is_http2 = req.version() == Version::HTTP_2;
        let inner = self.0.call(req);
        ResponseFuture { inner, is_http2 }
    }
}

impl<F, B> Future for ResponseFuture<F>
where
    F: Future<Item = Response<B>>,
    F::Error: Into<Error>,
    B: Default,
{
    type Item = Response<B>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.poll() {
            Ok(ok) => Ok(ok),
            Err(err) => {
                let err = err.into();

                if self.is_http2 {
                    if err.h2_reason().is_some() {
                        debug!("propagating http2 response error: {:?}", err);
                        return Err(err);
                    }
                }

                let response = Response::builder()
                    .status(map_err_to_5xx(err))
                    .header(header::CONTENT_LENGTH, "0")
                    .body(B::default())
                    .expect("app::errors response is valid");

                Ok(response.into())
            }
        }
    }
}

fn map_err_to_5xx(error: Error) -> StatusCode {
    use linkerd2_cache::error as cache;
    use tower::load_shed::error as shed;

    let status = if error.is::<cache::NoCapacity>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<shed::Overloaded>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<tower::timeout::error::Elapsed>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if let Some(StatusError { code, .. }) = error.downcast_ref() {
        *code
    } else {
        http::StatusCode::BAD_GATEWAY
    };

    warn!(%status, "Failed to proxy request: {}", error);
    status
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for StatusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}
