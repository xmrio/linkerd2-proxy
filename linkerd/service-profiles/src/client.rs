use crate::http as profiles;
use api::destination_client::DestinationClient;
use async_stream::try_stream;
use futures::prelude::*;
use http;
use http_body::Body as HttpBody;
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns as dns;
use linkerd2_error::{Error, Recover};
use linkerd2_proxy_api::destination as api;
use regex::Regex;
use std::convert::TryInto;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time;
use tonic::{
    body::{Body, BoxBody},
    client::GrpcService,
};
use tower::retry::budget::Budget;
use tracing::{debug, error, info, info_span, trace, warn};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    initial_timeout: Duration,
    suffixes: Vec<dns::Suffix>,
    context_token: String,
    service: DestinationClient<S>,
    recover: R,
}

pub type Receiver = watch::Receiver<profiles::Routes>;

type Sender = watch::Sender<profiles::Routes>;

#[derive(Clone, Debug)]
pub struct InvalidProfileAddr(Addr);

// === impl Client ===

impl<S, R> Client<S, R>
where
    // These bounds aren't *required* here, they just help detect the problem
    // earlier (as Client::new), instead of when trying to passing a `Client`
    // to something that wants `impl profiles::GetRoutes`.
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    S::Future: Send,
    R: Recover,
    R::Backoff: Unpin,
{
    pub fn new(
        service: S,
        recover: R,
        initial_timeout: Duration,
        context_token: String,
        suffixes: impl IntoIterator<Item = dns::Suffix>,
    ) -> Self {
        Self {
            initial_timeout,
            suffixes: suffixes.into_iter().collect(),
            recover,
            service: DestinationClient::new(service),
            context_token,
        }
    }
}

impl<S, R> Client<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    S::Future: Send,
    R: Recover + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    pub async fn routes(&mut self, dst: Addr) -> Result<Receiver, Error> {
        let dst = match dst {
            Addr::Name(n) => n,
            Addr::Socket(_) => {
                return Err(InvalidProfileAddr(dst.clone()).into());
            }
        };

        if !self.suffixes.iter().any(|s| s.contains(dst.name())) {
            debug!("name not in profile suffixes");
            self.service = self.service.clone();
            return Err(InvalidProfileAddr(dst.clone().into()).into());
        }

        let profiles = get_profiles(
            self.service.clone(),
            self.recover.clone(),
            api::GetDestination {
                path: dst.to_string(),
                context_token: self.context_token.clone(),
                ..Default::default()
            },
        );

        let (tx, rx) = {
            let profile = tokio::select! {
                res = (&mut profiles).next() => { res? }
                () = time::delay_for(self.initial_timeout) => {
                    info!("Using default service profile after timeout");
                    profiles::Routes::default()
                }
            };

            watch::channel(profile)
        };

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tx.closed() => { return; }
                    p = profiles.next() => {
                        match p {
                            None => { return; }
                            Some(p) => {
                                if tx.broadcast(p).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                };
            }
        });

        Ok(rx)
    }
}

fn get_profiles<S, R>(
    service: DestinationClient<S>,
    recover: R,
    request: api::GetDestination,
) -> impl Stream<Item = Result<profiles::Routes, Error>>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send + 'static,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    S::Future: Send,
    R: Recover,
    R::Backoff: Unpin,
{
    try_stream! {
        let mut backoff: Option<R::Backoff> = None;

        loop {
            if let Some(b) = backoff.as_mut() {
                b.next().await;
            }

            match service
                .get_profile(self.request.clone())
                .await
            {
                Ok(rsp) => {
                    let updates = rsp.into_inner();
                    while let Some(up) = updates.next().await {
                        yield up?;
                    }
                }
                Err(e) => {
                    let b = recover.recover(e)?;
                    backoff = backoff.unwrap_or(b);
                },
            }
        }
    }
}

fn convert_route(
    orig: api::Route,
    retry_budget: Option<&Arc<Budget>>,
) -> Option<(profiles::RequestMatch, profiles::Route)> {
    let req_match = orig.condition.and_then(convert_req_match)?;
    let rsp_classes = orig
        .response_classes
        .into_iter()
        .filter_map(convert_rsp_class)
        .collect();
    let mut route = profiles::Route::new(orig.metrics_labels.into_iter(), rsp_classes);
    if orig.is_retryable {
        set_route_retry(&mut route, retry_budget);
    }
    if let Some(timeout) = orig.timeout {
        set_route_timeout(&mut route, timeout.try_into());
    }
    Some((req_match, route))
}

fn convert_dst_override(orig: api::WeightedDst) -> Option<profiles::WeightedAddr> {
    if orig.weight == 0 {
        return None;
    }
    NameAddr::from_str(orig.authority.as_str())
        .ok()
        .map(|addr| profiles::WeightedAddr {
            addr,
            weight: orig.weight,
        })
}

fn set_route_retry(route: &mut profiles::Route, retry_budget: Option<&Arc<Budget>>) {
    let budget = match retry_budget {
        Some(budget) => budget.clone(),
        None => {
            warn!("retry_budget is missing: {:?}", route);
            return;
        }
    };

    route.set_retries(budget);
}

fn set_route_timeout(route: &mut profiles::Route, timeout: Result<Duration, Duration>) {
    match timeout {
        Ok(dur) => {
            route.set_timeout(dur);
        }
        Err(_) => {
            warn!("route timeout is negative: {:?}", route);
        }
    }
}

fn convert_req_match(orig: api::RequestMatch) -> Option<profiles::RequestMatch> {
    let m = match orig.r#match? {
        api::request_match::Match::All(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            profiles::RequestMatch::All(ms.collect())
        }
        api::request_match::Match::Any(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            profiles::RequestMatch::Any(ms.collect())
        }
        api::request_match::Match::Not(m) => {
            let m = convert_req_match(*m)?;
            profiles::RequestMatch::Not(Box::new(m))
        }
        api::request_match::Match::Path(api::PathMatch { regex }) => {
            let regex = regex.trim();
            let re = match (regex.starts_with('^'), regex.ends_with('$')) {
                (true, true) => Regex::new(regex).ok()?,
                (hd_anchor, tl_anchor) => {
                    let hd = if hd_anchor { "" } else { "^" };
                    let tl = if tl_anchor { "" } else { "$" };
                    let re = format!("{}{}{}", hd, regex, tl);
                    Regex::new(&re).ok()?
                }
            };
            profiles::RequestMatch::Path(re)
        }
        api::request_match::Match::Method(mm) => {
            let m = mm.r#type.and_then(|m| (&m).try_into().ok())?;
            profiles::RequestMatch::Method(m)
        }
    };

    Some(m)
}

fn convert_rsp_class(orig: api::ResponseClass) -> Option<profiles::ResponseClass> {
    let c = orig.condition.and_then(convert_rsp_match)?;
    Some(profiles::ResponseClass::new(orig.is_failure, c))
}

fn convert_rsp_match(orig: api::ResponseMatch) -> Option<profiles::ResponseMatch> {
    let m = match orig.r#match? {
        api::response_match::Match::All(ms) => {
            let ms = ms
                .matches
                .into_iter()
                .filter_map(convert_rsp_match)
                .collect::<Vec<_>>();
            if ms.is_empty() {
                return None;
            }
            profiles::ResponseMatch::All(ms)
        }
        api::response_match::Match::Any(ms) => {
            let ms = ms
                .matches
                .into_iter()
                .filter_map(convert_rsp_match)
                .collect::<Vec<_>>();
            if ms.is_empty() {
                return None;
            }
            profiles::ResponseMatch::Any(ms)
        }
        api::response_match::Match::Not(m) => {
            let m = convert_rsp_match(*m)?;
            profiles::ResponseMatch::Not(Box::new(m))
        }
        api::response_match::Match::Status(range) => {
            let min = http::StatusCode::from_u16(range.min as u16).ok()?;
            let max = http::StatusCode::from_u16(range.max as u16).ok()?;
            profiles::ResponseMatch::Status { min, max }
        }
    };

    Some(m)
}

fn convert_retry_budget(orig: api::RetryBudget) -> Option<Arc<Budget>> {
    let min_retries = if orig.min_retries_per_second <= ::std::i32::MAX as u32 {
        orig.min_retries_per_second
    } else {
        warn!(
            "retry_budget min_retries_per_second overflow: {:?}",
            orig.min_retries_per_second
        );
        return None;
    };
    let retry_ratio = orig.retry_ratio;
    if retry_ratio > 1000.0 || retry_ratio < 0.0 {
        warn!("retry_budget retry_ratio invalid: {:?}", retry_ratio);
        return None;
    }
    let ttl = match orig.ttl {
        Some(pb_dur) => match pb_dur.try_into() {
            Ok(dur) => {
                if dur > Duration::from_secs(60) || dur < Duration::from_secs(1) {
                    warn!("retry_budget ttl invalid: {:?}", dur);
                    return None;
                }
                dur
            }
            Err(negative) => {
                warn!("retry_budget ttl negative: {:?}", negative);
                return None;
            }
        },
        None => {
            warn!("retry_budget ttl missing");
            return None;
        }
    };

    Some(Arc::new(Budget::new(ttl, min_retries, retry_ratio)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::*;

    quickcheck! {
        fn retry_budget_from_proto(
            min_retries_per_second: u32,
            retry_ratio: f32,
            seconds: i64,
            nanos: i32
        ) -> bool {
            let proto = api::RetryBudget {
                min_retries_per_second,
                retry_ratio,
                ttl: Some(prost_types::Duration {
                    seconds,
                    nanos,
                }),
            };
            convert_retry_budget(proto);
            // simply not panicking is good enough
            true
        }
    }
}

impl InvalidProfileAddr {
    pub fn addr(&self) -> &Addr {
        &self.0
    }
}

impl std::fmt::Display for InvalidProfileAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid profile addr: {}", self.0)
    }
}

impl std::error::Error for InvalidProfileAddr {}
