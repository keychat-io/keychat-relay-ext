use dashmap::DashMap;
use std::sync::Arc;

use cashu_wallet::store::UnitedStore;
use cashu_wallet::UniError;
use cashu_wallet::UniErrorFrom;
use cashu_wallet::UnitedWallet;

use crate::config::Config;
use crate::config::Limits;

pub type MetricsMpmc = (flume::Sender<Metric>, flume::Receiver<Metric>);
pub trait StateTrait {
    type Store: UnitedStore + Clone + Send + Sync + 'static;
    fn as_wallet(&self) -> &UnitedWallet<Self::Store>;
    fn as_config(&self) -> &Config;
    fn as_metrics(&self) -> &MetricsMpmc;
    fn as_limits(&self) -> &LimiterState;
}

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::Instant;
pub struct LimiterState {
    pub(crate) instant: Instant,
    pub(crate) store_for_cashu_failed: DashMap<String, Mutex<VecDeque<u32>>>,
}

impl LimiterState {
    pub fn new() -> Self {
        Self {
            instant: Instant::now(),
            store_for_cashu_failed: Default::default(),
        }
    }
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            instant: Instant::now(),
            store_for_cashu_failed: DashMap::with_capacity(cap),
        }
    }
    pub fn cashu_failed_check(&self, key: &str, limits: &Limits) -> bool {
        let c = &limits.cashu_failed;
        let now = self.instant.elapsed().as_secs() as u32;
        let mut disallow = false;
        let dr = &mut disallow;
        self.store_for_cashu_failed.remove_if(key, |_k, v| {
            let lock = v.lock();
            let count = lock.iter().filter(|s| now - **s <= c.secs).count();
            *dr = count >= c.allow;
            // println!("{dr}={count}>={}: {:?}", c.allow, lock);
            count == 0
        });
        disallow
    }
    pub fn cashu_failed_count(&self, key: &str, limits: &Limits) {
        let c = &limits.cashu_failed;
        let now = self.instant.elapsed().as_secs() as u32;

        let entry = self
            .store_for_cashu_failed
            .entry(key.to_owned())
            .or_insert_with(|| Mutex::new(VecDeque::with_capacity(c.allow)));
        let mut lock = entry.lock();
        if lock.len() >= c.allow {
            lock.pop_front();
        }

        lock.push_back(now);
    }
    pub fn cashu_failed_clear(&self, limits: &Limits) {
        let c = &limits.cashu_failed;
        let now = self.instant.elapsed().as_secs() as u32;
        self.store_for_cashu_failed.retain(|_k, v| {
            let lock = v.lock();
            let count = lock.iter().filter(|s| now - **s <= c.secs).count();
            count > 0
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::config::Limit;

    #[test]
    fn cashu_failed() {
        let limits = &Limits {
            cashu_failed: Limit { secs: 1, allow: 3 },
        };
        let s = LimiterState::new();

        let key = "key";
        for i in 0..3 {
            assert_eq!(
                s.cashu_failed_check(key, limits),
                false,
                "{i}: cashu_failed_check"
            );
            s.cashu_failed_count(key, limits);
        }
        assert_eq!(s.store_for_cashu_failed.len(), 1);
        s.cashu_failed_count("key2", limits);
        assert_eq!(s.store_for_cashu_failed.len(), 2);
        assert_eq!(s.cashu_failed_check(key, limits), !false);
        s.cashu_failed_count(key, limits);
        assert_eq!(s.cashu_failed_check(key, limits), !false);
        std::thread::sleep_ms(2000);
        for i in 0..3 {
            assert_eq!(
                s.cashu_failed_check(key, limits),
                false,
                "{i}: cashu_failed_check"
            );
            s.cashu_failed_count(key, limits);
        }

        assert_eq!(s.store_for_cashu_failed.len(), 2);
        s.cashu_failed_clear(limits);
        assert_eq!(s.store_for_cashu_failed.len(), 1);
    }
}

use crate::State;
impl AsRef<State> for State {
    fn as_ref(&self) -> &State {
        self
    }
}

use crate::cashu::Redb;
impl<T> StateTrait for T
where
    T: AsRef<crate::State>,
{
    type Store = Arc<Redb>;
    fn as_config(&self) -> &Config {
        &self.as_ref().config
    }
    fn as_wallet(&self) -> &UnitedWallet<Self::Store> {
        &self.as_ref().wallet
    }
    fn as_metrics(&self) -> &MetricsMpmc {
        &self.as_ref().metrics
    }
    fn as_limits(&self) -> &LimiterState {
        &self.as_ref().limiter
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metric {
    pub costms: u32,
    pub amount: u32,
}

pub fn start_show_metrics<State>(state: Arc<State>)
where
    State: StateTrait + Send + Sync + 'static,
    State::Store: UnitedStore + Clone + Send + Sync + 'static,
    UniError<<State::Store as UnitedStore>::Error>: UniErrorFrom<State::Store>,
{
    let fut = async move {
        let conf = state.as_config();
        let metrics = &state.as_metrics().1;
        let wallet = state.as_wallet();
        let limits = state.as_limits();
        let mut ticks = tokio::time::interval(std::time::Duration::from_secs(60));

        let mut amount = 0u64;
        let mut tokens_count = 0u64;
        let mut tokens_ok = 0u64;
        let mut tokens_err = 0u64;

        // min,max,sum,count
        let mut statis = [0u128; 3];
        statis[0] = 1000;
        loop {
            tokio::select! {
                msg = metrics.recv_async() => {
                    match msg {
                        Ok(m) => {
                            let costms = m.costms as _;
                            statis[2] += costms;
                            if costms < statis[0] {
                                statis[0] = costms;
                            }
                            if costms > statis[1] {
                                statis[1] = costms;
                            }

                            tokens_count += 1;
                            if m.amount > 0 {
                                amount += m.amount as u64;
                                tokens_ok += 1;
                            } else {
                                tokens_err += 1;
                            }
                        }
                        Err(e) => {
                            error!("metrics recv_async failed: {}", e);
                            break;
                        },
                    }
                },
                tick = ticks.tick() => {
                    let all = wallet.get_balances().await;

                    let avg = statis[2]/(std::cmp::max(tokens_count, 1) as u128);
                    info!("tick {:?}, amount: {}, tokens: {}, oks: {}, errs: {}, [{} {}] {}ms, {:?}", tick.elapsed(), amount, tokens_count, tokens_ok, tokens_err, statis[0], statis[1], avg, all);

                    let size = limits.store_for_cashu_failed.len();
                    limits.cashu_failed_clear(&conf.limits);
                    let size2 = limits.store_for_cashu_failed.len();
                    info!("tick {:?}, limits.cashu_failed: {}->{}", tick.elapsed(), size, size2);
                }
            }
        }
    };
    tokio::spawn(fut);
}
