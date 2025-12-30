use cashu::amount::SplitTarget;
use cashu::util::unix_time;
use cashu::Amount;
use cashu::MintUrl;
use cashu::Proof;
use cdk::wallet::MultiMintWallet;
use cdk::wallet::ReceiveOptions;
use cdk::wallet::SendOptions;
use cdk::Wallet;
use cdk_common::wallet::WalletKey;
use cdk_common::CurrencyUnit;
use cdk_common::ProofsMethods;
use dashmap::DashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::config::Limits;

pub type MetricsMpmc = (flume::Sender<Metric>, flume::Receiver<Metric>);
pub trait StateTrait {
    fn as_wallet(&self) -> &MultiMintWallet;
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
        std::thread::sleep(std::time::Duration::from_millis(2000));
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

impl<T> StateTrait for T
where
    T: AsRef<crate::State>,
{
    fn as_config(&self) -> &Config {
        &self.as_ref().config
    }
    fn as_wallet(&self) -> &MultiMintWallet {
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
    pub amount: u64,
}

pub fn start_show_metrics<State>(state: Arc<State>)
where
    State: StateTrait + Send + Sync + 'static,
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
                    let all = wallet.get_balances(&CurrencyUnit::Sat).await;

                    let avg = statis[2]/(std::cmp::max(tokens_count, 1) as u128);
                    info!("tick {:?}, amount: {}, tokens: {}, oks: {}, errs: {}, [{} {}] {}ms, {:?}", tick.elapsed(), amount, tokens_count, tokens_ok, tokens_err, statis[0], statis[1], avg, all);

                    let size = limits.store_for_cashu_failed.len();
                    limits.cashu_failed_clear(&conf.limits);
                    let size2 = limits.store_for_cashu_failed.len();
                    info!("tick {:?}, limits.cashu_failed: {}->{}", tick.elapsed(), size, size2);

                    if let Ok(map) = all {
                        if swap.as_ref().map(|j: &tokio::task::JoinHandle<anyhow::Result<()>> |j.is_finished()).unwrap_or_default() {
                            let j = swap.take().unwrap();
                            let res = j.await;
                            info!("move_token: {:?}", res);
                        }

                        if swap.is_none() {
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

use std::collections::BTreeMap;
async fn move_token<State>(state: Arc<State>, map: BTreeMap<MintUrl, Amount>) -> anyhow::Result<()>
where
    State: StateTrait + Send + Sync + 'static,
{
    let wallet = state.as_wallet();
    let config = state.as_config();
    let trustu = config
        .mints()
        .first()
        .ok_or_else(|| format_err!("get first trust mint"))?;
    let _trustw = wallet
        .get_wallet(&WalletKey::new(trustu.clone(), CurrencyUnit::Sat))
        .await;

    let balances_for_untrusted_mint = map
        .iter()
        .filter(|(k, _v)| {
            config
                .mints()
                .iter()
                .all(|m| m.to_string() != k.to_string())
        })
        .map(|(k, v)| (k.to_string(), *v.as_ref()))
        .collect::<Vec<_>>();
    if balances_for_untrusted_mint.len() >= 1 {
        let records = crate::cashu::MintsBlocker::update_balances(
            balances_for_untrusted_mint
                .iter()
                .map(|(s, v)| (s.as_str(), *v)),
        )
        .await;
        info!("MintsBlocker.records: {:?}", records);
    }

    for (k, v) in map {
        let ts = unix_time() - 3600 * 24 * 7; // 7 days
        let txs = wallet.localstore.remove_transactions(ts).await;

        info!(
            "move_token: {} {}: {}, remove txs: {:?}",
            k.to_string(),
            CurrencyUnit::Sat,
            v,
            txs
        );

        if wallet
            .has(&WalletKey::new(k.clone(), CurrencyUnit::Sat))
            .await
        {
            // white-list, merge 1sat to 128+
            if config
                .mints()
                .iter()
                .any(|m| m.to_string() == k.to_string())
            {
                let mut ps = wallet
                    .localstore
                    .get_proofs(
                        Some(k.clone()),
                        Some(CurrencyUnit::Sat),
                        Some(vec![cashu::State::Unspent]),
                        None,
                    )
                    .await?
                    .into_iter()
                    .map(|p| p.proof)
                    .collect::<Vec<_>>();
                let psc = ps.len();
                ps.retain(|p| *p.amount.as_ref() < 10);
                let psc_small = ps.len();
                let size = 128;

                info!(
                    "move_token.merge: {} {}: {} {}->{}>={}: {} ",
                    k.clone().to_string(),
                    CurrencyUnit::Sat,
                    v,
                    psc,
                    psc_small,
                    size,
                    psc_small >= size,
                );
                if ps.len() < size {
                    continue;
                }

                let pss = ps[..size].to_vec();
                let merge = merge_token_by_swap(wallet, &k.clone(), pss.clone()).await;
                info!(
                    "merge_token: {} {} {}: {} {:?}",
                    k.clone().to_string(),
                    CurrencyUnit::Sat,
                    v,
                    pss.clone().iter().map(|p| *p.amount.as_ref()).sum::<u64>(),
                    merge
                );
            } else if *v.as_ref() >= config.fee.untrusted_mint_should_transfer {
                let mut block = false;
                let swap = if let Some(trust_wallet) = _trustw.as_ref() {
                    move_token_by_swap(wallet, trust_wallet, &k.clone(), &mut block).await
                } else {
                    return Err(anyhow::anyhow!(
                        "Trust wallet is None, skipping move_token_by_swap"
                    ));
                };
                info!(
                    "move_token: {} {}: {} -> {:?}",
                    k.clone().to_string(),
                    CurrencyUnit::Sat,
                    v,
                    swap
                );
                if swap.is_err() && block {
                    error!("move_token_by_swap blocked for {} {}", k.to_string(), v);
                }
            }
        } else {
            // next time
            let add = wallet.localstore.add_mint(k.clone(), None).await?;
            info!(
                "move_token.add_mint: {} {}: {} -> {:?}",
                k.to_string(),
                CurrencyUnit::Sat,
                v,
                add
            );
        }
    }

    Ok(())
}

async fn merge_token_by_swap(
    w: &MultiMintWallet,
    url: &MintUrl,
    pss: Vec<Proof>,
) -> anyhow::Result<(usize, usize)> {
    let before = pss.len();
    let mut after = 0usize;
    if before == 0 {
        return Ok((after, before));
    }

    if let Some(wallet) = w
        .get_wallet(&WalletKey::new(url.clone(), CurrencyUnit::Sat))
        .await
    {
        // if relay charge fee, then swap will need fee
        let result = wallet
            .swap(None, SplitTarget::default(), pss, None, false)
            .await?;
        if result.is_some() {
            after = result.as_ref().map(|r| r.len()).unwrap_or(0usize);
        }
    }

    Ok((after, before))
}

/// this will move all tokens from untrusted mint to trust mint, but will not execute in new version of CDK
/// because of error 'Token does not match wallet mint'
/// so this wiil deprecated later and do nothing now
async fn move_token_by_swap(
    _w: &MultiMintWallet,
    _trust: &Wallet,
    _url: &MintUrl,
    _block: &mut bool,
) -> anyhow::Result<(u64, u64)> {
    // let mut before = 0u64;
    // let mut after = 0u64;
    // if let Some(wallet) = w
    //     .get_wallet(&WalletKey::new(url.clone(), CurrencyUnit::Sat))
    //     .await
    // {
    //     // ensure it alive
    //     if let Err(err) = wallet.get_mint_keysets().await {
    //         error!(
    //             "wallet.get_mint_keysets({}) fallback failed: {}",
    //             url.to_string(),
    //             err
    //         );
    //         return Err(err.into());
    //     }
    //     // then send all balance to trust mint
    //     *block = false;
    //     let ps = wallet.get_unspent_proofs().await?;
    //     if *ps.total_amount()?.as_ref() == 0 {
    //         let err: anyhow::Error = format_err!("The amount is 0");
    //         return Err(err.into());
    //     }
    //     // let prepared_send = wallet
    //     //     .prepare_send(ps.total_amount()?, SendOptions::default())
    //     //     .await?;
    //     // let tx = wallet.send(prepared_send, None).await?;
    //     // before = *tx.amount.as_ref();

    //     // finally receive in trust mint
    //     *block = true;

    //     // let tx2 = trust.receive(&tx.token, ReceiveOptions::default()).await?;
    //     // after = *tx2.amount.as_ref();

    //     // must receive in own wallet to avoid error 'Token does not match wallet mint'
    //     // let tx2 = wallet.receive(&tx.token, ReceiveOptions::default()).await?;
    //     // after = *tx2.amount.as_ref();
    // }
    return Ok((0, 0));
}
