#![deny(warnings, rust_2018_idioms)]

use indexmap::IndexMap;
use linkerd2_identity as identity;
use linkerd2_proxy_api::destination as api;
use linkerd2_service_profiles as profiles;
use rand::distributions::WeightedIndex;
use std::{convert::TryFrom, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

mod client;

pub use self::client::Client;

#[derive(Clone, Debug)]
pub struct Strategy {
    pub addr: SocketAddr,
    pub detect: Detect,
    pub logical: Logical,
}

#[derive(Copy, Clone, Debug)]
pub enum Detect {
    Opaque,
    Client {
        buffer_capacity: usize,
        timeout: Duration,
    },
}

#[derive(Clone, Debug)]
pub enum Logical {
    Concrete(Concrete),
    Split(Arc<Split>),
    //Fallback(Arc<Fallback>),
}

#[derive(Clone, Debug)]
pub enum Concrete {
    Forward(SocketAddr, Endpoint),
    Balance(http::uri::Authority, Balancer),
}

#[derive(Clone, Debug, Default)]
pub struct Balancer {
    // Peak-EWMA settings, if applicable.
    pub peak_ewma_decay: Duration,
    pub peak_ewma_default_rtt: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Endpoint {
    pub identity: Option<identity::Name>,

    // If set, indicates that the endpoint is running a proxy that supports HTTP
    // transport upgrading (i.e., using the l5d-orig-proto header).
    pub proxy_http_transport: Option<ProxyHttpTransport>,

    pub authority_override: Option<http::uri::Authority>,

    pub metric_labels: IndexMap<String, String>,

    pub connect_timeout: Duration,
}

#[derive(Clone, Debug)]
pub enum ProxyHttpTransport {
    H2,
    // TODO: H3?
}

//TODO
// #[derive(Clone, Debug)]
// pub struct Fallback {
//     pub primary: Logical,
//     pub fallback: Logical,
// }

#[derive(Clone, Debug)]
pub struct Split {
    pub targets: Vec<Logical>,
    pub weights: WeightedIndex<u32>,
}

// === impl Strategy ===

impl Strategy {
    fn new(
        addr: SocketAddr,
        api::StrategyResponse { detect, logical }: api::StrategyResponse,
    ) -> Self {
        use api::protocol_detection as detect;

        let detect = detect
            .and_then(|d| {
                d.strategy.map(|s| match s {
                    detect::Strategy::Opaque(_) => Detect::Opaque,
                    detect::Strategy::Client(detect::Client {
                        buffer_capacity,
                        timeout,
                    }) => Detect::Client {
                        buffer_capacity: buffer_capacity as usize,
                        timeout: timeout
                            .and_then(|t| Duration::try_from(t).ok())
                            .unwrap_or_default(),
                    },
                })
            })
            .unwrap_or(Detect::Opaque);

        let logical = logical
            .and_then(Logical::from_api)
            .unwrap_or_else(|| Logical::Concrete(Concrete::Forward(addr, Endpoint::default())));

        Self {
            addr,
            detect,
            logical,
        }
    }

    pub fn from_profile(addr: SocketAddr, routes: profiles::Routes) -> Self {
        const DETECT: Detect = Detect::Client { buffer_capacity: 0, timeout: Duration::default(), };

        let logical = if routes.dst_overrides.is_empty() {
            Logical::Concrete(Concrete::Forward(addr, Endpoint::default()))
        } else {
            Logical::Split(Arc::new(Split)
        };

        Self {
            addr,
            detect: DETECT,
            logical,
        }
    }
}

// === impl Logical ===

impl Logical {
    fn from_api(api::Logical { kind }: api::Logical) -> Option<Self> {
        let target = match kind? {
            api::logical::Kind::Concrete(api::Concrete { kind }) => Self::Concrete(match kind? {
                api::concrete::Kind::Forward(api::concrete::Forward { addr }) => {
                    let api::WeightedAddr {
                        addr,
                        tls_identity,
                        authority_override,
                        connect_timeout,
                        protocol_hint,
                        metric_labels,
                        weight: _,
                    } = addr?;

                    let addr = SocketAddr::try_from(addr?).ok()?;

                    let endpoint = Endpoint {
                        identity: tls_identity.and_then(|i| {
                            i.strategy.and_then(
                                |api::tls_identity::Strategy::DnsLikeIdentity(n)| {
                                    identity::Name::from_hostname(n.name.as_ref()).ok()
                                },
                            )
                        }),

                        authority_override: authority_override.and_then(|a| {
                            http::uri::Authority::from_str(a.authority_override.as_ref()).ok()
                        }),

                        connect_timeout: connect_timeout
                            .and_then(|t| Duration::try_from(t).ok())
                            .unwrap_or_default(),

                        proxy_http_transport: protocol_hint.and_then(|t| {
                            t.protocol.map(|p| {
                                assert!(matches!(p, api::protocol_hint::Protocol::H2(_)));
                                ProxyHttpTransport::H2
                            })
                        }),

                        metric_labels: metric_labels.into_iter().collect(),
                    };

                    Concrete::Forward(addr, endpoint)
                }

                api::concrete::Kind::Balance(api::concrete::Balance {
                    authority,
                    peak_ewma_decay,
                    peak_ewma_default_rtt,
                }) => Concrete::Balance(
                    http::uri::Authority::from_str(authority.as_ref()).ok()?,
                    Balancer {
                        peak_ewma_decay: peak_ewma_decay
                            .and_then(|t| Duration::try_from(t).ok())
                            .unwrap_or_default(),
                        peak_ewma_default_rtt: peak_ewma_default_rtt
                            .and_then(|t| Duration::try_from(t).ok())
                            .unwrap_or_default(),
                    },
                ),
            }),

            api::logical::Kind::Split(split) => {
                let mut targets = Vec::with_capacity(split.targets.len());
                let mut weights = Vec::with_capacity(split.targets.len());
                for api::logical::split::Weighted { target, weight } in split.targets.into_iter() {
                    if let Some(target) = target {
                        if weight > 0 {
                            if let Some(target) = Logical::from_api(target) {
                                targets.push(target);
                                weights.push(weight);
                            }
                        }
                    }
                }
                debug_assert_eq!(weights.len(), targets.len());

                // Returns None if weights is empty.
                let weights = WeightedIndex::new(weights).ok()?;
                Self::Split(Arc::new(Split { targets, weights }))
            }
        };

        Some(target)
    }
}
