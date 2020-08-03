#![deny(warnings, rust_2018_idioms)]

pub use self::{requests::Requests, retries::Retries};
use indexmap::IndexMap;
// use parking_lot::RwLock;
use std::fmt;
use std::hash::Hash;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

pub mod requests;
pub mod retries;

#[derive(Debug)]
struct Registry<T, M>
where
    T: Hash + Eq,
{
    by_target: IndexMap<T, Arc<RwLock<M>>>,
}

/// Reports metrics for prometheus.
#[derive(Debug)]
pub struct Report<T, M>
where
    T: Hash + Eq,
{
    prefix: &'static str,
    registry: Arc<RwLock<Registry<T, M>>>,
    clock: quanta::Clock,
    /// The amount time metrics with no updates should be retained for reports
    retain_idle: Duration,
    /// Whether latencies should be reported.
    include_latencies: bool,
}

impl<T: Hash + Eq, M> Clone for Report<T, M> {
    fn clone(&self) -> Self {
        Self {
            include_latencies: self.include_latencies,
            clock: self.clock.clone(),
            prefix: self.prefix.clone(),
            registry: self.registry.clone(),
            retain_idle: self.retain_idle,
        }
    }
}

struct Prefixed<'p, N: fmt::Display> {
    prefix: &'p str,
    name: N,
}

trait LastUpdate {
    fn last_update(&self) -> quanta::Instant;
}

#[derive(Debug)]
pub(crate) struct LastUpdateTime {
    clock: quanta::Clock,
    last_update: AtomicU64,
}

impl<T, M> Default for Registry<T, M>
where
    T: Hash + Eq,
{
    fn default() -> Self {
        Self {
            by_target: IndexMap::default(),
        }
    }
}

impl<T, M> Registry<T, M>
where
    T: Hash + Eq,
    M: LastUpdate,
{
    /// Retains metrics for all targets that (1) no longer have an active
    /// reference to the `RequestMetrics` structure and (2) have not been updated since `epoch`.
    fn retain_since(&mut self, epoch: quanta::Instant) {
        self.by_target.retain(|_, m| {
            Arc::strong_count(&m) > 1 || m.read().map(|m| m.last_update() >= epoch).unwrap_or(false)
        })
    }
}

impl<T, M> Registry<T, M>
where
    T: Hash + Eq,
    M: Default,
{
    pub(crate) fn get_or_insert_target(registry: &Arc<RwLock<Self>>, target: T) -> Arc<RwLock<M>> {
        if let Some(metrics) = registry.read().unwrap().by_target.get(&target) {
            metrics.clone()
        } else {
            let metrics = Arc::new(RwLock::new(M::default()));
            registry
                .write()
                .unwrap()
                .by_target
                .insert(target, metrics.clone());
            metrics
        }
    }
}

impl<T, M> Report<T, M>
where
    T: Hash + Eq,
{
    fn new(retain_idle: Duration, registry: Arc<RwLock<Registry<T, M>>>) -> Self {
        Self {
            prefix: "",
            clock: quanta::Clock::new(),
            registry,
            retain_idle,
            include_latencies: true,
        }
    }

    pub fn with_prefix(self, prefix: &'static str) -> Self {
        if prefix.is_empty() {
            return self;
        }

        Self { prefix, ..self }
    }

    pub fn without_latencies(self) -> Self {
        Self {
            include_latencies: false,
            ..self
        }
    }

    fn prefix_key<N: fmt::Display>(&self, name: N) -> Prefixed<'_, N> {
        Prefixed {
            prefix: &self.prefix,
            name,
        }
    }
}

impl<'p, N: fmt::Display> fmt::Display for Prefixed<'p, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix.is_empty() {
            return self.name.fmt(f);
        }

        write!(f, "{}_{}", self.prefix, self.name)
    }
}

impl LastUpdateTime {
    pub(crate) fn update(&self) {
        let now = self.clock.raw();
        self.last_update.store(now, Ordering::Release);
    }

    pub(crate) fn last_update(&self) -> quanta::Instant {
        self.clock.scaled(self.last_update.load(Ordering::Acquire))
    }
}

impl Default for LastUpdateTime {
    fn default() -> Self {
        let clock = quanta::Clock::new();
        let last_update = AtomicU64::new(clock.raw());
        Self { clock, last_update }
    }
}
