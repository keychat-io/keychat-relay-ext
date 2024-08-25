use cashu_wallet::store::ProofExtended;
use cashu_wallet::types::unixtime_ms;
use cashu_wallet::types::TransactionStatus;
use cashu_wallet::wallet::AmountHelper;
use cashu_wallet::wallet::ProofsHelper;
use cashu_wallet::wallet::TokenGeneric;
use cashu_wallet::wallet::Wallet;
use cashu_wallet::Url;
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
        let mut swap = None;

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

                    if let Ok(map) = all {
                        if swap.is_some() {
                            if swap.as_ref().map(|j: &tokio::task::JoinHandle<anyhow::Result<()>> |j.is_finished()).unwrap_or_default() {
                                let j = swap.take().unwrap();
                                let res = j.await;
                                info!("move_token: {:?}", res);
                            }
                        } else {
                            let fut = tokio::spawn(move_token(state.clone(), map));
                            swap = Some(fut);
                        }
                    }
                }
            }
        }
    };
    tokio::spawn(fut);
}

use cashu_wallet::store::MintUrlWithUnit;
use std::collections::BTreeMap;
async fn move_token<State>(
    state: Arc<State>,
    map: BTreeMap<MintUrlWithUnit<'static>, u64>,
) -> anyhow::Result<()>
where
    State: StateTrait + Send + Sync + 'static,
    State::Store: UnitedStore + Clone + Send + Sync + 'static,
    UniError<<State::Store as UnitedStore>::Error>: UniErrorFrom<State::Store>,
{
    let wallet = state.as_wallet();
    let config = state.as_config();
    let trustu = config
        .mints()
        .first()
        .ok_or_else(|| format_err!("get fisrt trust mint"))?;
    let _trustw = wallet.get_wallet(trustu)?;

    let balances_for_untrusted_mint = map
        .iter()
        .filter(|(k, _v)| config.mints().iter().all(|m| m.as_str() != k.mint()))
        .map(|(k, v)| (k.mint(), *v))
        .collect::<Vec<_>>();
    if balances_for_untrusted_mint.len() >= 1 {
        let records =
            crate::cashu::MintsBlocker::update_balances(balances_for_untrusted_mint.into_iter())
                .await;
        info!("MintsBlocker.records: {:?}", records);
    }

    for (k, v) in map {
        let ts = unixtime_ms() - 3600 * 1000;
        let txs = wallet
            .store()
            .delete_transactions(&[TransactionStatus::Success], ts)
            .await;

        info!(
            "move_token: {} {}: {}, remove txs: {:?}",
            k.mint(),
            k.unit(),
            v,
            txs
        );
        let url = k.mint().parse()?;

        if wallet.contains(&url)? {
            // white-list, merge 1sat to 128+
            if config.mints().iter().any(|m| m.as_str() == k.mint()) {
                let mut ps = wallet.store().get_proofs_limit_unit(&url, k.unit()).await?;
                let psc = ps.len();
                ps.retain(|p| p.as_ref().amount.to_u64() < 10);
                let psc_small = ps.len();
                let size = 128;

                info!(
                    "move_token.merge: {} {}: {} {}->{}>={}: {} ",
                    k.mint(),
                    k.unit(),
                    v,
                    psc,
                    psc_small,
                    size,
                    psc_small >= size,
                );
                if ps.len() < size {
                    continue;
                }

                let pss = &ps[..size];
                let merge = merge_token_by_swap(wallet, &url, k.unit(), pss).await;
                info!(
                    "merge_token: {} {} {}: {} {:?}",
                    k.mint(),
                    k.unit(),
                    v,
                    pss.sum().to_u64(),
                    merge
                );
            } else if v >= config.fee.untrusted_mint_should_transfer {
                let mut block = false;
                let swap =
                    move_token_by_swap(wallet, _trustw.as_ref(), &url, k.unit(), &mut block).await;
                info!("move_token: {} {}: {} -> {:?}", k.mint(), k.unit(), v, swap);
                if swap.is_err() && block {
                    // todo
                }
            }
        } else {
            // next time
            let add = wallet
                .add_mint_with_units(url, false, &["sat"], None)
                .await?;
            info!(
                "move_token.add_mint: {} {}: {} -> {:?}",
                k.mint(),
                k.unit(),
                v,
                add
            );
        }
    }

    Ok(())
}

async fn merge_token_by_swap<S>(
    wallet: &UnitedWallet<S>,
    url: &Url,
    unit: &str,
    pss: &[ProofExtended],
) -> anyhow::Result<(usize, usize)>
where
    S: UnitedStore + Clone + Send + Sync + 'static,
    UniError<S::Error>: UniErrorFrom<S>,
{
    let before = pss.len();
    let pss2 = pss.iter().map(|p| p.as_ref().clone()).collect::<Vec<_>>();
    let token = TokenGeneric::new(url.clone(), pss2, None, Some(unit.into()))?;

    let w = wallet.get_wallet(url)?;
    let mut after = 0usize;
    for t in &token.token {
        let ps = w.receive_token(t, Some(unit), wallet.store()).await?;
        after += ps.len();
        let ps = ps.into_extended_with_unit(Some(unit));
        wallet.store().add_proofs(url, &ps).await?;
    }

    wallet.store().delete_proofs(url, pss).await?;

    Ok((after, before))
}

async fn move_token_by_swap<S>(
    wallet: &UnitedWallet<S>,
    trust: &Wallet,
    url: &Url,
    unit: &str,
    block: &mut bool,
) -> anyhow::Result<(u64, u64)>
where
    S: UnitedStore + Clone + Send + Sync + 'static,
    UniError<S::Error>: UniErrorFrom<S>,
{
    let w = wallet.get_wallet(url)?;
    // ensure it alive
    w.client().get_info().await?;

    let ps = wallet.store().get_proofs_limit_unit(url, unit).await?;
    let balance = ps.sum().to_u64();

    let mintquote_pre = trust.request_mint(balance.into(), Some(unit), None).await?;
    let bill = mintquote_pre.request.parse()?;

    *block = true;
    let meltquote_pre = w.request_melt(&bill, Some(unit), None).await?;
    let fee_reserve = meltquote_pre.fee_reserve;
    info!(
        "swap melt form0 {}: {} {}",
        url.as_str(),
        balance,
        fee_reserve
    );
    ensure!(
        balance > fee_reserve,
        "balance<=fee.reserve pre: {}<={}",
        balance,
        fee_reserve
    );

    *block = false;
    let amount = balance - fee_reserve;
    let mintquote = trust.request_mint(amount.into(), Some(unit), None).await?;
    let bill = mintquote.request.parse()?;

    *block = true;
    let meltquote = w.request_melt(&bill, Some(unit), None).await?;
    let mut fee = meltquote.fee_reserve;
    info!("swap melt form1 {}: {} {}", url.as_str(), amount, fee);

    ensure!(
        meltquote.amount == amount,
        "meltquote.amount != amount: {}!={}",
        meltquote.amount,
        amount
    );
    let amount_with_fee = amount + fee;
    if amount_with_fee != balance {
        *block = false; //?
                        // todo: handle < balance
        bail!(
            "swap melt form1 {} amount_with_fee!=balance: {} {}",
            url.as_str(),
            amount_with_fee,
            balance,
        )
    }

    *block = true;
    let pm = w
        .melt(
            &meltquote.quote,
            &ps,
            fee.into(),
            Some(unit),
            None,
            wallet.store(),
        )
        .await?;
    info!(
        "swap melt form1 {}: {} {} got {} {:?} {:?}",
        url.as_str(),
        amount,
        fee,
        pm.paid,
        pm.preimage,
        pm.change.as_ref().map(|ps| ps.len())
    );

    if let Some(remain) = pm.change {
        let remain = remain.into_extended_with_unit(Some(unit));
        wallet.store().add_proofs(url, &remain).await?;
        let ra = remain.sum();
        if fee >= ra.to_u64() {
            fee -= ra.to_u64();
        }
    }

    if pm.paid {
        wallet
            .store()
            .delete_proofs(url, &ps)
            .await
            .map_err(|e| {
                error!(
                    "remove proofs after melt failed {} {}: {}",
                    url.as_str(),
                    amount_with_fee,
                    e
                )
            })
            .ok();
        let tx = wallet
            .mint_tokens(
                trust.client().url(),
                amount,
                mintquote.quote.clone(),
                Some(unit),
            )
            .await?;
        return Ok((tx.amount(), fee));
    }

    bail!("not paid")
}
