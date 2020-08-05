use futures::prelude::*;
use linkerd2_error::Error;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::ServiceExt;

#[derive(Clone, Debug)]
pub struct Chain<A, B> {
    a: A,
    b: B,
}

impl<A, B> Chain<A, B> {
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<T, A, B> tower::Service<T> for Chain<A, B>
where
    A: tower::Service<T> + Send,
    A::Response: Send,
    A::Error: Into<Error> + Send,
    A::Future: Send + 'static,
    B: tower::Service<A::Response> + Clone + Send + 'static,
    B::Error: Into<Error>,
    B::Future: Send + 'static,
{
    type Response = B::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<B::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.a.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, a_req: T) -> Self::Future {
        let a_fut = self.a.call(a_req).err_into::<Error>();
        let b = self.b.clone();
        Box::pin(async move {
            let b_req = a_fut.await?;
            b.oneshot(b_req).await.map_err(Into::into)
        })
    }
}
