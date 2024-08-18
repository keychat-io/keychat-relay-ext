use std::io::Read;
use std::sync::Arc;

use cashu_wallet::cashu::util::unix_time;
use cashu_wallet::store::UnitedStore;
use cashu_wallet::wallet::AmountHelper;
use cashu_wallet::wallet::HttpOptions;
use cashu_wallet::UniError;
use cashu_wallet::UniErrorFrom;
use cashu_wallet::UnitedWallet;
use cashu_wallet::Url;

pub use cashu_wallet::store::impl_redb::Redb;
pub type UniWallet = UnitedWallet<Arc<Redb>>;

use crate::config::Config;
use crate::metrics::StateTrait;

pub async fn crate_cashu_wallet(conf: &Config, _add_mints: bool) -> Result<UniWallet, UniError> {
    if conf.timeout_ms == 0 {
        return Err(format_err!("zero ms timeout").into());
    };

    if conf.fee.mints.is_empty() {
        return Err(format_err!("empty mints").into());
    }

    let store = Redb::open(&conf.database, Default::default())?;
    let http = HttpOptions::new()
        .connection_verbose(true)
        .timeout_connect_ms(2000)
        .timeout_get_ms(conf.timeout_ms)
        .timeout_swap_ms(conf.timeout_ms)
        .connection_verbose(true);

    let wallet = UnitedWallet::new(store, http);
    if _add_mints {
        try_add_mints(&wallet, conf).await?;
    }

    if let Some(listfile) = &conf.mints_file {
        MintsBlocker::load(listfile).await?;
    }

    Ok(wallet)
}

async fn try_add_mints<S>(w: &UnitedWallet<S>, conf: &Config) -> Result<(), UniError<S::Error>>
where
    S: UnitedStore + Send + Sync + Clone + 'static,
    UniError<S::Error>: UniErrorFrom<S>,
{
    let urls = w.mint_urls()?;
    for mint in conf
        .mints()
        .iter()
        .filter(|s| urls.iter().all(|u| u != s.as_str()))
    {
        let res = w.add_mint(mint.clone(), false).await;
        warn!("cashu load mint: {} {:?}", mint.as_str(), res);
        res?;
    }
    Ok(())
}

use std::collections::BTreeMap as Map;
use std::fs::File;
use tokio::sync::Mutex;
#[derive(Debug, Default)]
struct MintsBlocker {
    map: Map<String, MintRecord>,
    file: PathBuf,
    file_modified: u64,
}
use std::sync::OnceLock;
static MINTS_BLOCKEDR: OnceLock<Mutex<MintsBlocker>> = OnceLock::new();
use std::path::PathBuf;
impl MintsBlocker {
    async fn get<State>(
        url: &Url,
        state: State,
    ) -> anyhow::Result<Option<Blocker>, UniError<<State::Store as UnitedStore>::Error>>
    where
        State: StateTrait + Send + 'static,
        UniError<<State::Store as UnitedStore>::Error>: UniErrorFrom<State::Store>,
    {
        let host = url
            .as_ref()
            .host_str()
            .ok_or_else(|| format_err!("mint url not contains host"))?;
        let lock = MINTS_BLOCKEDR.get().unwrap();

        let blocker;
        let mut lock = lock.lock().await;
        if let Some(m) = lock.map.get(host) {
            if m.blocked {
                return Err(format_err!(
                    "the host of mintUrl {} already blocked: {}",
                    host,
                    url.as_str()
                )
                .into());
            } else {
                return Ok(m.blocker.clone());
            }
        } else {
            let record = MintRecord::new(url);
            blocker = record.blocker.clone().unwrap();
            lock.map.insert(host.to_owned(), record);
        }

        let _blocker = blocker.lock_owned().await;
        let w = state.as_wallet();
        let res = w
            .add_mint_with_units(url.clone(), false, &["sat"], None)
            .await;
        warn!("add_mint_with_units {} got: {:?}", url.as_str(), res);
        // if res.is_err() {
        // }
        let _ = lock;

        Ok(None)
    }
    async fn load(file: &PathBuf) -> anyhow::Result<()> {
        let inited = if let Some(lock) = MINTS_BLOCKEDR.get() {
            let l = lock.lock().await;
            Some(l.file_modified)
        } else {
            None
        };

        let mut f = File::options()
            .read(true)
            .write(true)
            .create(true)
            // .append(true)
            .open(file)?;
        let file_modified = f
            .metadata()?
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|t| t.as_millis() as u64)
            .unwrap_or(0);

        let load = inited
            .map(|ts| file_modified == 0 || ts < file_modified)
            .unwrap_or(true);

        let mut map = Map::new();
        if load {
            let mut str = String::new();
            f.read_to_string(&mut str)?;
            for (i, l) in str.lines().enumerate() {
                let js = l.trim();
                if js.starts_with('{') {
                    match serde_json::from_str::<MintRecord>(js) {
                        Ok(mut mb) => {
                            mb.blocker = Some(new_blocker(mb.blocked));
                            let host = mb
                                .url
                                .as_ref()
                                .host_str()
                                .ok_or_else(|| format_err!("the host url is none"))?;
                            map.insert(host.to_owned(), mb);
                        }
                        Err(e) => {
                            error!(
                                "MintRecord {} line-{} load faild: {} {}",
                                file.display(),
                                i,
                                js,
                                e
                            );
                        }
                    }
                }
            }
        }

        let blocker = MintsBlocker {
            file_modified,
            map,
            file: file.to_owned(),
        };

        if inited.is_none() {
            MINTS_BLOCKEDR
                .set(Mutex::new(blocker))
                .expect("MINTS_BLOCKEDR.set");
        } else {
            // todo: use old blocker for per mint
            let mut lock = MINTS_BLOCKEDR.get().unwrap().lock().await;
            *lock = blocker;
        }

        Ok(())
    }
    async fn flush(file: &PathBuf) -> anyhow::Result<()> {
        // Self::load(file)?;
        let lock = MINTS_BLOCKEDR.get().unwrap();
        let blocked = {
            let lock = lock.lock().await;
            let mut blocked = Vec::with_capacity(lock.map.len());
            for b in lock.map.values() {
                let js = serde_json::to_string(&b).unwrap();
                blocked.push((b.ts, js));
            }
            blocked.sort_by_key(|bs| bs.0);
            blocked
        };

        let mut str = String::new();
        for (_, b) in blocked {
            str.push_str(&b);
            str.push_str("\n");
        }

        if str.len() > 1 {
            std::fs::write(file, &str)?;
        }

        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_for_mint_host() {
        let url: Url = "https://8333.space".parse().unwrap();
        assert_eq!(url.as_ref().host_str().unwrap(), "8333.space");

        let url: Url = "https://8333.space:8338".parse().unwrap();
        assert_eq!(url.as_ref().host_str().unwrap(), "8333.space");

        let url: Url = "https://mint.8333.space:8338".parse().unwrap();
        assert_eq!(url.as_ref().host_str().unwrap(), "mint.8333.space");
    }
}

type Blocker = Arc<Mutex<AtomicBool>>;
fn new_blocker(b: bool) -> Blocker {
    Arc::new(Mutex::new(AtomicBool::new(b)))
}

use std::sync::atomic::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MintRecord {
    url: Url,
    ts: u64,
    blocked: bool,
    #[serde(skip)]
    blocker: Option<Blocker>,
}
impl MintRecord {
    fn new(url: &Url) -> Self {
        Self {
            url: url.clone(),
            ts: unix_time(),
            blocked: false,
            blocker: Some(new_blocker(false)),
        }
    }
}

pub async fn receive_tokens<State>(
    cashu: &str,
    eventid: &str,
    ip: &str,
    price: u64,
    state: State,
) -> Result<Option<()>, UniError<<State::Store as UnitedStore>::Error>>
where
    State: StateTrait + Send + 'static + Clone,
    UniError<<State::Store as UnitedStore>::Error>: UniErrorFrom<State::Store>,
{
    let tokens: cashu_wallet::wallet::Token = cashu
        .parse()
        .map_err(|e| format_err!(format!("cashu tokens decode: {}", e)))?;
    let amount = tokens
        .token
        .iter()
        .map(|a| a.proofs.iter().map(|s| s.amount.to_u64()).sum::<u64>())
        .sum::<u64>();

    if amount < price {
        return Err(format_err!("cashu tokens amount not enough: {}/{}", amount, price).into());
    }

    // let conf = state.as_config();
    let mint_url = tokens
        .token
        .iter()
        .map(|t| &t.mint)
        .next()
        .ok_or_else(|| format_err!("cashu tokens not contains mint url"))?;

    // if !conf.mints().contains(&mint_url) {
    //     return Err(format_err!("unsupport mint url").into());
    // }
    if !state.as_wallet().contains(&mint_url)? {
        let blocker = MintsBlocker::get(&mint_url, state.clone()).await?;
        if let Some(blocker) = blocker {
            let b = blocker.lock().await;
            if b.load(Ordering::SeqCst) {
                return Err(format_err!(
                    "the host of mintUrl {} already blocked: {}",
                    mint_url.as_ref().host_str().unwrap_or_default(),
                    mint_url.as_str()
                )
                .into());
            }
        }
    }

    // if state.as_limits().cashu_failed_check(ip, &conf.limits) {
    //     return Ok(None);
    // }

    let start = std::time::Instant::now();
    let eventid = eventid.to_owned();
    let ip = ip.to_owned();
    let fut = async move {
        let mut txs = vec![];
        let res = state
            .as_wallet()
            .receive_tokens_full_limit_unit(&tokens, &mut txs, &[])
            .await;
        let a = txs.iter().map(|tx| tx.amount()).sum::<u64>();
        let costms = start.elapsed().as_millis();
        state
            .as_metrics()
            .0
            .send(crate::Metric {
                costms: costms as _,
                amount: a as u32,
            })
            .unwrap();

        match res {
            Ok(_) if a >= amount => {
                info!(
                    "{}'s {:?} tokens receive {} {}ms ok: {}",
                    eventid, ip, price, costms, a,
                );
            }
            res => {
                // state.as_limits().cashu_failed_count(ip, &conf.limits);
                error!(
                    "{}'s {:?} tokens receive {} {}ms failed: {:?}",
                    eventid, ip, price, costms, res
                );
                // return res.and_then(|a| Err(format_err!("tokens amount {}<{}", a, price).into()));
            }
        }
    };

    tokio::spawn(fut);

    Ok(Some(()))
}
