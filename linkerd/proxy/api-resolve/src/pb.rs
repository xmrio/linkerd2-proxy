use crate::api::destination::{
    protocol_hint::Protocol, AuthorityOverride, TlsIdentity, WeightedAddr,
};
use crate::api::net::TcpAddress;
use crate::identity;
use crate::metadata::{Metadata, ProtocolHint};
use http::uri::Authority;
use indexmap::IndexMap;
use std::{collections::HashMap, net::SocketAddr};

/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
pub(in crate) fn to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
) -> Option<(SocketAddr, Metadata)> {
    let authority_override = pb.authority_override.and_then(to_authority);
    let addr = pb.addr.and_then(to_sock_addr)?;

    let meta = {
        let mut t = set_labels
            .iter()
            .chain(pb.metric_labels.iter())
            .collect::<Vec<(&String, &String)>>();
        t.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

        let mut m = IndexMap::with_capacity(t.len());
        for (k, v) in t.into_iter() {
            m.insert(k.clone(), v.clone());
        }

        m
    };

    let mut proto_hint = ProtocolHint::Unknown;
    if let Some(hint) = pb.protocol_hint {
        if let Some(proto) = hint.protocol {
            match proto {
                Protocol::H2(..) => {
                    proto_hint = ProtocolHint::Http2;
                }
            }
        }
    }

    let tls_id = pb.tls_identity.and_then(to_id);
    let meta = Metadata::new(meta, proto_hint, tls_id, pb.weight, authority_override);
    Some((addr, meta))
}

fn to_id(pb: TlsIdentity) -> Option<identity::Name> {
    use crate::api::destination::tls_identity::Strategy;

    let Strategy::DnsLikeIdentity(i) = pb.strategy?;
    match identity::Name::from_hostname(i.name.as_bytes()) {
        Ok(i) => Some(i),
        Err(_) => {
            tracing::warn!("Ignoring invalid identity: {}", i.name);
            None
        }
    }
}

pub(in crate) fn to_authority(o: AuthorityOverride) -> Option<Authority> {
    match o.authority_override.parse() {
        Ok(name) => Some(name),
        Err(_) => {
            tracing::debug!(
                "Ignoring invalid authority override: {}",
                o.authority_override
            );
            None
        }
    }
}

pub(in crate) fn to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use std::convert::TryFrom;
    SocketAddr::try_from(pb).ok()
}
