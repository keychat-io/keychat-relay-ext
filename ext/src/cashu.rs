use std::io::Read;
use std::sync::Arc;

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
    map: Map<String, MintBlocked>,
    file_modified: u64,
}
use std::sync::OnceLock;
static MINTS_BLOCKEDR: OnceLock<parking_lot::Mutex<MintsBlocker>> = OnceLock::new();
use std::path::PathBuf;
impl MintsBlocker {
    fn load(file: &PathBuf) -> anyhow::Result<()> {
        let inited = MINTS_BLOCKEDR.get().map(|s| s.lock().file_modified);

        let mut f = File::options()
            .read(true)
            .append(true)
            .create_new(true)
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
                    match serde_json::from_str::<MintBlocked>(js) {
                        Ok(mut mb) => {
                            mb.blocker = Some(Arc::new(Mutex::new(AtomicBool::new(mb.added))));
                            let host = mb
                                .url
                                .as_ref()
                                .host_str()
                                .ok_or_else(|| format_err!("the host url is none"))?;
                            map.insert(host.to_owned(), mb);
                        }
                        Err(e) => {
                            error!(
                                "MintBlocked {} line-{} load faild: {} {}",
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

        let blocker = MintsBlocker { file_modified, map };

        if inited.is_none() {
            MINTS_BLOCKEDR
                .set(parking_lot::Mutex::new(blocker))
                .expect("MINTS_BLOCKEDR.set");
        } else {
            // todo: use old blocker for per mint
            let mut lock = MINTS_BLOCKEDR.get().unwrap().lock();
            *lock = blocker;
        }

        Ok(())
    }
    fn flush(file: &PathBuf) -> anyhow::Result<()> {
        // Self::load(file)?;
        let lock = MINTS_BLOCKEDR.get().unwrap();
        let blocked = {
            let lock = lock.lock();
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

use std::sync::atomic::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MintBlocked {
    url: Url,
    ts: u64,
    added: bool,
    #[serde(skip)]
    blocker: Option<Arc<Mutex<AtomicBool>>>,
}

pub async fn receive_tokens<State>(
    cashu: &str,
    eventid: &str,
    ip: &str,
    price: u64,
    state: State,
) -> Result<Option<()>, UniError<<State::Store as UnitedStore>::Error>>
where
    State: StateTrait + Send + 'static,
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

    let conf = state.as_config();
    let mint_url = tokens
        .token
        .iter()
        .map(|t| &t.mint)
        .next()
        .ok_or_else(|| format_err!("cashu tokens not contains mint url"))?;

    if !conf.mints().contains(&mint_url) {
        return Err(format_err!("unsupport mint url").into());
    }

    // if state.as_limits().cashu_failed_check(ip, &conf.limits) {
    //     return Ok(None);
    // }

    let start = std::time::Instant::now();
    let eventid = eventid.to_owned();
    let ip = ip.to_owned();
    let cashu = cashu.to_owned();
    let fut = async move {
        let res = state.as_wallet().receive_tokens(&cashu).await;
        let costms = start.elapsed().as_millis();
        state
            .as_metrics()
            .0
            .send(crate::Metric {
                costms: costms as _,
                amount: res.as_ref().map(|a| *a as u32).unwrap_or(0),
            })
            .unwrap();

        match res {
            Ok(a) if a >= amount => {
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
