use crate::proxy::http::{
    boxed::Payload as BoxBody,
    glue::{Body, HyperServerSvc},
    h2::Settings as H2Settings,
    trace, upgrade, Version as HttpVersion,
};
use crate::transport::{
    io::{self, BoxedIo, Peekable},
    listen, tls,
};
use crate::{
    drain,
    proxy::{core::Accept, detect},
    svc::{NewService, Service, ServiceExt},
    Error,
};
use async_trait::async_trait;
use futures::prelude::*;
use http;
use hyper;
use indexmap::IndexSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Protocol<T> {
    pub http: Option<HttpVersion>,
    pub target: T,
}

#[derive(Clone, Debug)]
pub struct ProtocolDetect {
    capacity: usize,
    skip_ports: Arc<IndexSet<u16>>,
}

impl ProtocolDetect {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(skip_ports: Arc<IndexSet<u16>>) -> Self {
        ProtocolDetect {
            skip_ports,
            capacity: Self::PEEK_CAPACITY,
        }
    }
}

#[async_trait]
impl detect::Detect<tls::accept::Meta, BoxedIo> for ProtocolDetect {
    type Target = Protocol<tls::accept::Meta>;
    type Io = BoxedIo;
    type Error = io::Error;

    async fn detect(
        &self,
        target: tls::accept::Meta,
        io: BoxedIo,
    ) -> Result<(Self::Target, BoxedIo), Self::Error> {
        let port = target.addrs.target_addr().port();

        // Skip detection if the port is in the configured set.
        if self.skip_ports.contains(&port) {
            let proto = Protocol { target, http: None };
            return Ok::<_, Self::Error>((proto, io));
        }

        // Otherwise, attempt to peek the client connection to determine the protocol.
        // Currently, we only check for an HTTP prefix.
        let peek = io.peek(self.capacity).await?;

        let http = HttpVersion::from_prefix(peek.prefix().as_ref());
        let proto = Protocol { target, http };
        Ok((proto, BoxedIo::new(peek)))
    }
}

pub struct Server<D, F, H> {
    drain: drain::Watch,
    detect: D,
    forward_tcp: F,
    make_http: H,
    http: hyper::server::conn::Http<trace::Executor>,
}

impl<D, F, H> Server<D, F, H> {
    pub fn new(
        detect: D,
        forward_tcp: F,
        make_http: H,
        h2: H2Settings,
        drain: drain::Watch,
    ) -> Self {
        let mut http = hyper::server::conn::Http::new().with_executor(trace::Executor::new());
        http.http2_adaptive_window(true)
            .http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);

        Self {
            drain,
            detect,
            forward_tcp,
            make_http,
            http,
        }
    }
}

impl<D, F, H, T, S> Service<listen::Connection> for Server<D, F, H>
where
    T: Send + 'static,
    D: detect::Detect<listen::Addrs, TcpStream, Target = Protocol<T>>
        + Clone
        + Send
        + Sync
        + 'static,
    F: Accept<(T, D::Io)> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::ConnectionFuture: Send + 'static,
    H: NewService<T, Service = S> + Clone + Send + 'static,
    S: Service<http::Request<Body>, Response = http::Response<BoxBody>, Error = Error>
        + Unpin
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    /// Handle a new connection.
    ///
    /// This will peek on the connection for the first bytes to determine
    /// what protocol the connection is speaking. From there, the connection
    /// will be mapped into respective services, and spawned into an
    /// executor.
    fn call(&mut self, (addrs, tcp): listen::Connection) -> Self::Future {
        let mut http_server = self.http.clone();
        let make_http = self.make_http.clone();
        let forward_tcp = self.forward_tcp.clone();
        let drain = self.drain.clone();
        let detect = self.detect.clone();

        Box::pin(async move {
            let (Protocol { http, target }, io) =
                detect.detect(addrs, tcp).err_into::<Error>().await?;

            let rsp: Self::Response = Box::pin(async move {
                match http {
                    Some(http_version) => {
                        let svc = make_http.new_service(target);

                        match http_version {
                            HttpVersion::Http1 => {
                                // Enable support for HTTP upgrades (CONNECT and websockets).
                                let svc = upgrade::Service::new(svc, drain.clone());
                                let conn = http_server
                                    .http1_only(true)
                                    .serve_connection(io, HyperServerSvc::new(svc))
                                    .with_upgrades();
                                drain
                                    .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                                    .instrument(info_span!("h1"))
                                    .err_into::<Error>()
                                    .await
                            }

                            HttpVersion::H2 => {
                                let conn = http_server
                                    .http2_only(true)
                                    .serve_connection(io, HyperServerSvc::new(svc));
                                drain
                                    .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                                    .instrument(info_span!("h2"))
                                    .err_into::<Error>()
                                    .await
                            }
                        }
                    }

                    None => {
                        trace!("not HTTP; forwarding TCP");
                        let duplex = forward_tcp
                            .into_service()
                            .oneshot((target, io))
                            .err_into::<Error>()
                            .await?;

                        drain
                            .ignore_signal()
                            .release_after(duplex)
                            .err_into::<Error>()
                            .await
                    }
                }
            });

            Ok(rsp)
        })
    }
}
