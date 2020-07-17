use linkerd2_duplex::Duplex;
use linkerd2_error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tower::{Service, util::ServiceExt};

#[derive(Clone, Debug)]
pub struct Forward<C> {
    connect: C,
}

impl<C> Forward<C> {
    pub fn new(connect: C) -> Self {
        Self { connect }
    }
}

impl<C, T, I> Service<(T, I)> for Forward<C>
where
    T: Send + 'static,
    C: Service<T> + Clone + Send + 'static,
    C::Response: Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error> + Send,
    C::Response: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (meta, io): (T, I)) -> Self::Future {
        let mut connect = self.connect.clone();
        Box::pin(async move {
            forward(&mut connect, meta, io).await
        })
    }
}

pub async fn forward<C, T, I>(connect: &mut C, target: T, src_io: I) -> Result<(), Error>
where
    C: Service<T> + 'static,
    C::Response:  AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error> + Send,
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let dst_io = connect.ready_and().await.map_err(Into::into)?.call(target).await.map_err(Into::into)?;
    Duplex::new(src_io, dst_io).await.map_err(Into::into)
}
