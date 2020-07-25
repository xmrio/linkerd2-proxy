use super::{conditional_accept, ReasonForNoPeerName};
use crate::io::{BoxedIo, PrefixedIo};
use crate::listen::{self, Addrs};
use bytes::BytesMut;
use indexmap::IndexSet;
use linkerd2_conditional::Conditional;
use linkerd2_dns_name as dns;
use linkerd2_error::Error;
use linkerd2_identity as identity;
use linkerd2_proxy_core::Accept;
use linkerd2_proxy_detect as detect;
use linkerd2_stack::layer;
use pin_project::pin_project;
pub use rustls::ServerConfig as Config;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{self, AsyncReadExt},
    net::TcpStream,
};
use tracing::{debug, trace};

pub trait HasConfig {
    fn tls_server_name(&self) -> identity::Name;
    fn tls_server_config(&self) -> Arc<Config>;
}

/// Produces a server config that fails to handshake all connections.
pub fn empty_config() -> Arc<Config> {
    let verifier = rustls::NoClientAuth::new();
    Arc::new(Config::new(verifier))
}

#[derive(Clone, Debug)]
pub struct Meta {
    // TODO sni name
    pub peer_identity: super::PeerIdentity,
    pub addrs: Addrs,
}

pub type Connection = (Meta, BoxedIo);

#[derive(Clone)]
pub struct AcceptTls<A: Accept<Connection>, T> {
    accept: A,
    tls: super::Conditional<T>,
    skip_ports: Arc<IndexSet<u16>>,
}

#[derive(Clone, Debug)]
pub struct Detect<I> {
    local_identity: Option<I>,
    skip_ports: Arc<IndexSet<u16>>,
    capacity: usize,
}

#[async_trait::async_trait]
impl<I: HasConfig + Send + Sync> detect::Detect<Addrs, TcpStream> for Detect<I> {
    type Target = Meta;
    type Io = BoxedIo;
    type Error = io::Error;

    async fn detect(&self, addrs: Addrs, mut tcp: TcpStream) -> io::Result<(Meta, BoxedIo)> {
        let local_id = match self.local_identity.as_ref() {
            Some(local_id) => local_id,
            None => {
                let meta = Meta {
                    peer_identity: Conditional::None(ReasonForNoPeerName::LocalIdentityDisabled),
                    addrs,
                };
                return Ok((meta, BoxedIo::new(tcp)));
            }
        };

        if self.skip_ports.contains(&addrs.target_addr().port()) {
            let meta = Meta {
                peer_identity: Conditional::None(ReasonForNoPeerName::PortSkipped),
                addrs,
            };
            return Ok((meta, BoxedIo::new(tcp)));
        }

        let no_tls_meta = move |addrs: Addrs| Meta {
            peer_identity: Conditional::None(ReasonForNoPeerName::NoTlsFromRemote),
            addrs,
        };

        let mut buf = BytesMut::with_capacity(self.capacity);

        // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
        // This should avoid the need for PrefixedIo in most cases.
        tcp.peek(buf.as_mut()).await?;
        match conditional_accept::match_client_hello(buf.as_ref(), &local_id.tls_server_name()) {
            conditional_accept::Match::Matched => {
                // Terminate the TLS stream.
                let tls = tokio_rustls::TlsAcceptor::from(local_id.tls_server_config())
                    .accept(tcp)
                    .await?;

                // Determine the peer's identity, if it exist.
                let peer_identity = client_identity(&tls)
                    .map(Conditional::Some)
                    .unwrap_or_else(|| Conditional::None(ReasonForNoPeerName::NoPeerIdFromRemote));
                trace!(peer.identity=?peer_identity, "accepted TLS connection");

                let meta = Meta {
                    peer_identity,
                    addrs,
                };
                return Ok((meta, BoxedIo::new(tls)));
            }

            conditional_accept::Match::NotMatched => {
                return Ok((no_tls_meta(addrs), BoxedIo::new(tcp)));
            }

            conditional_accept::Match::Incomplete => {}
        }

        // Peeking didn't return enough data, so instead we'll reset the buffer
        // and try reading data from the socket.
        buf.clear();
        let mut sz = tcp.read(buf.as_mut()).await?;
        while sz > 0 && buf.capacity() > 0 {
            sz = tcp.read(buf.as_mut()).await?;
            match conditional_accept::match_client_hello(buf.as_ref(), &local_id.tls_server_name())
            {
                conditional_accept::Match::Matched => {
                    // Terminate the TLS stream.
                    let io = PrefixedIo::new(buf.freeze(), tcp);
                    let tls = tokio_rustls::TlsAcceptor::from(local_id.tls_server_config())
                        .accept(io)
                        .await?;

                    // Determine the peer's identity, if it exist.
                    let peer_identity = client_identity(&tls)
                        .map(Conditional::Some)
                        .unwrap_or_else(|| {
                            Conditional::None(ReasonForNoPeerName::NoPeerIdFromRemote)
                        });
                    trace!(peer.identity=?peer_identity, "accepted TLS connection");

                    let meta = Meta {
                        peer_identity,
                        addrs,
                    };
                    return Ok((meta, BoxedIo::new(tls)));
                }
                conditional_accept::Match::NotMatched => break,
                conditional_accept::Match::Incomplete => {}
            }
        }

        let io = BoxedIo::new(PrefixedIo::new(buf.freeze(), tcp));
        Ok((no_tls_meta(addrs), io))
    }
}

#[pin_project]
pub struct AcceptFuture<A: Accept<Connection>> {
    #[pin]
    state: AcceptState<A>,
}

#[pin_project(project = AcceptStateProj)]
enum AcceptState<A: Accept<Connection>> {
    TryTls(Option<TryTls<A>>),
    TerminateTls(
        #[pin] tokio_rustls::Accept<PrefixedIo<TcpStream>>,
        Option<AcceptMeta<A>>,
    ),
    ReadyAccept(A, Option<Connection>),
    Accept(#[pin] A::Future),
}

pub struct TryTls<A: Accept<Connection>> {
    meta: AcceptMeta<A>,
    server_name: identity::Name,
    config: Arc<Config>,
    peek_buf: BytesMut,
    socket: TcpStream,
}

pub struct AcceptMeta<A: Accept<Connection>> {
    accept: A,
    addrs: Addrs,
}

// === impl Listen ===

impl<A: Accept<Connection>, T: HasConfig> AcceptTls<A, T> {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(tls: super::Conditional<T>, accept: A) -> Self {
        Self {
            accept,
            tls,
            skip_ports: Default::default(),
        }
    }

    pub fn with_skip_ports(mut self, skip_ports: Arc<IndexSet<u16>>) -> Self {
        self.skip_ports = skip_ports;
        self
    }

    pub fn layer(
        tls: super::Conditional<T>,
        skip_ports: Arc<IndexSet<u16>>,
    ) -> impl tower::layer::Layer<A, Service = AcceptTls<A, T>>
    where
        T: Clone,
    {
        layer::mk(move |accept| {
            AcceptTls::new(tls.clone(), accept).with_skip_ports(skip_ports.clone())
        })
    }
}

impl<A, T> tower::Service<listen::Connection> for AcceptTls<A, T>
where
    A: Accept<Connection> + Clone,
    T: HasConfig + Send + 'static,
{
    type Response = A::ConnectionFuture;
    type Error = Error;
    type Future = AcceptFuture<A>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.accept
            .poll_ready(cx)
            .map(|poll| poll.map_err(Into::into))
    }

    fn call(&mut self, (addrs, socket): listen::Connection) -> Self::Future {
        // Protocol detection is disabled for the original port. Return a
        // new connection without protocol detection.
        let target_addr = addrs.target_addr();

        match &self.tls {
            // Tls is disabled. Return a new plaintext connection.
            Conditional::None(reason) => {
                debug!(%reason, "skipping TLS");
                let meta = Meta {
                    addrs,
                    peer_identity: Conditional::None(*reason),
                };
                let conn = (meta, BoxedIo::new(socket));
                AcceptFuture::accept(self.accept.accept(conn))
            }

            // Tls is enabled. Try to accept a Tls handshake.
            Conditional::Some(tls) => {
                if self.skip_ports.contains(&target_addr.port()) {
                    debug!("skipping protocol detection");
                    let meta = Meta {
                        peer_identity: Conditional::None(
                            super::ReasonForNoPeerName::NotHttp.into(),
                        ),
                        addrs,
                    };
                    let conn = (meta, BoxedIo::new(socket));
                    AcceptFuture::accept(self.accept.accept(conn))
                } else {
                    debug!("attempting TLS handshake");
                    let meta = AcceptMeta {
                        accept: self.accept.clone(),
                        addrs,
                    };
                    AcceptFuture::try_tls(TryTls {
                        meta,
                        socket,
                        peek_buf: BytesMut::with_capacity(Self::PEEK_CAPACITY),
                        config: tls.tls_server_config(),
                        server_name: tls.tls_server_name(),
                    })
                }
            }
        }
    }
}

impl<A: Accept<Connection>> AcceptFuture<A> {
    fn accept(f: A::Future) -> Self {
        Self {
            state: AcceptState::Accept(f),
        }
    }

    fn try_tls(try_tls: TryTls<A>) -> Self {
        Self {
            state: AcceptState::TryTls(Some(try_tls)),
        }
    }
}

impl<A: Accept<Connection>> Future for AcceptFuture<A> {
    type Output = Result<A::ConnectionFuture, Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            match this.state.as_mut().project() {
                AcceptStateProj::Accept(future) => return future.poll(cx).map_err(Into::into),
                AcceptStateProj::ReadyAccept(accept, conn) => {
                    futures::ready!(accept.poll_ready(cx).map_err(Into::into))?;
                    let conn = accept.accept(conn.take().expect("polled after complete"));
                    this.state.set(AcceptState::Accept(conn))
                }
                AcceptStateProj::TryTls(ref mut try_tls) => {
                    let match_ = futures::ready!(try_tls
                        .as_mut()
                        .expect("polled after complete")
                        .poll_match_client_hello(cx))?;
                    match match_ {
                        conditional_accept::Match::Matched => {
                            trace!("upgrading accepted connection to TLS");
                            let TryTls {
                                meta,
                                socket,
                                peek_buf,
                                config,
                                ..
                            } = try_tls.take().expect("polled after complete");
                            let io = PrefixedIo::new(peek_buf.freeze(), socket);
                            this.state.set(AcceptState::TerminateTls(
                                tokio_rustls::TlsAcceptor::from(config).accept(io),
                                Some(meta),
                            ));
                        }

                        conditional_accept::Match::NotMatched => {
                            trace!("passing through accepted connection without TLS");
                            let TryTls {
                                peek_buf,
                                socket,
                                meta: AcceptMeta { accept, addrs },
                                ..
                            } = try_tls.take().expect("polled after complete");
                            let meta = Meta {
                                addrs,
                                peer_identity: Conditional::None(
                                    ReasonForNoPeerName::NoTlsFromRemote.into(),
                                ),
                            };
                            let conn = (
                                meta,
                                BoxedIo::new(PrefixedIo::new(peek_buf.freeze(), socket)),
                            );
                            this.state.set(AcceptState::ReadyAccept(accept, Some(conn)))
                        }

                        conditional_accept::Match::Incomplete => {
                            continue;
                        }
                    }
                }
                AcceptStateProj::TerminateTls(future, meta) => {
                    let io = futures::ready!(future.poll(cx))?;
                    let peer_identity =
                        client_identity(&io)
                            .map(Conditional::Some)
                            .unwrap_or_else(|| {
                                Conditional::None(super::ReasonForNoPeerName::NoPeerIdFromRemote)
                            });
                    trace!(peer.identity=?peer_identity, "accepted TLS connection");

                    let AcceptMeta { accept, addrs } = meta.take().expect("polled after complete");
                    // FIXME the connection doesn't know about TLS connections
                    // that don't have a client id.
                    let meta = Meta {
                        addrs,
                        peer_identity,
                    };
                    this.state.set(AcceptState::ReadyAccept(
                        accept,
                        Some((meta, BoxedIo::new(io))),
                    ));
                }
            }
        }
    }
}

impl<A: Accept<Connection>> TryTls<A> {
    /// Polls the underlying socket for more data and buffers it.
    ///
    /// The buffer is matched for a Tls client hello message.
    ///
    /// `NotMatched` is returned if the underlying socket has closed.
    fn poll_match_client_hello(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<conditional_accept::Match, Error>> {
        use crate::io::AsyncRead;

        let sz = futures::ready!(Pin::new(&mut self.socket).poll_read_buf(cx, &mut self.peek_buf))?;
        trace!(%sz, "read");
        if sz == 0 {
            // XXX: It is ambiguous whether this is the start of a Tls handshake or not.
            // For now, resolve the ambiguity in favor of plaintext. TODO: revisit this
            // when we add support for Tls policy.
            return Poll::Ready(Ok(conditional_accept::Match::NotMatched.into()));
        }

        let buf = self.peek_buf.as_ref();
        let m = conditional_accept::match_client_hello(buf, &self.server_name);
        trace!(sni = %self.server_name, r#match = ?m, "conditional_accept");
        Poll::Ready(Ok(m))
    }
}

fn client_identity<S>(tls: &tokio_rustls::server::TlsStream<S>) -> Option<identity::Name> {
    use rustls::Session;
    use webpki::GeneralDNSNameRef;

    let (_io, session) = tls.get_ref();
    let certs = session.get_peer_certificates()?;
    let c = certs.first().map(rustls::Certificate::as_ref)?;
    let end_cert = webpki::EndEntityCert::from(c).ok()?;
    let dns_names = end_cert.dns_names().ok()?;

    match dns_names.first()? {
        GeneralDNSNameRef::DNSName(n) => Some(identity::Name::from(dns::Name::from(n.to_owned()))),
        GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
    }
}

impl HasConfig for identity::CrtKey {
    fn tls_server_name(&self) -> identity::Name {
        identity::CrtKey::tls_server_name(self)
    }

    fn tls_server_config(&self) -> Arc<Config> {
        identity::CrtKey::tls_server_config(self)
    }
}
