use crate::cashu::CashuWallet;
use crate::config::{Config, Limits};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Instant,
};

pub type MetricsMpmc = (flume::Sender<Metric>, flume::Receiver<Metric>);
pub trait StateTrait {
    fn as_wallet(&self) -> &CashuWallet;
    fn as_config(&self) -> &Config;
    fn as_metrics(&self) -> &MetricsMpmc;
    fn as_limits(&self) -> &LimiterState;
}
pub struct LimiterState {
    instant: Instant,
    store: DashMap<String, Mutex<VecDeque<u64>>>,
}
impl LimiterState {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            instant: Instant::now(),
            store: DashMap::with_capacity(cap),
        }
    }
    pub fn cashu_failed_check(&self, key: &str, limits: &Limits) -> bool {
        let now = self.instant.elapsed().as_secs();
        self.store.get(key).is_some_and(|v| {
            v.lock()
                .iter()
                .filter(|t| now.saturating_sub(**t) <= u64::from(limits.cashu_failed.secs))
                .count()
                >= limits.cashu_failed.allow
        })
    }
    pub fn cashu_failed_count(&self, key: &str, limits: &Limits) {
        let v = self.store.entry(key.into()).or_default();
        let mut q = v.lock();
        if q.len() >= limits.cashu_failed.allow {
            q.pop_front();
        }
        q.push_back(self.instant.elapsed().as_secs());
    }
    pub fn cashu_failed_clear(&self, limits: &Limits) {
        let now = self.instant.elapsed().as_secs();
        self.store.retain(|_, v| {
            v.lock()
                .iter()
                .any(|t| now.saturating_sub(*t) <= u64::from(limits.cashu_failed.secs))
        });
    }
}
impl AsRef<crate::State> for crate::State {
    fn as_ref(&self) -> &Self {
        self
    }
}
impl<T: AsRef<crate::State>> StateTrait for T {
    fn as_wallet(&self) -> &CashuWallet {
        &self.as_ref().wallet
    }
    fn as_config(&self) -> &Config {
        &self.as_ref().config
    }
    fn as_metrics(&self) -> &MetricsMpmc {
        &self.as_ref().metrics
    }
    fn as_limits(&self) -> &LimiterState {
        &self.as_ref().limiter
    }
}
#[derive(Debug, Clone)]
pub struct Metric {
    pub costms: u64,
    pub amount: u64,
    pub mint: String,
    pub unit: String,
}
pub fn start_show_metrics<S: StateTrait + Send + Sync + 'static>(state: Arc<S>) {
    tokio::spawn(async move {
        let mut totals: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
        let mut ticks = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                msg = state.as_metrics().1.recv_async() => match msg {
                    Ok(m) => { let t = totals.entry((m.mint, m.unit)).or_default(); t.0 = t.0.saturating_add(m.amount); t.1 = t.1.saturating_add(1); debug!(cost_ms=m.costms, "confirmed receive"); },
                    Err(_) => break,
                },
                _ = ticks.tick() => { info!(?totals, "confirmed receipts by mint and unit"); state.as_limits().cashu_failed_clear(&state.as_config().limits); }
            }
        }
    });
}
