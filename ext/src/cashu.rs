use std::io::Read;
use std::str::FromStr;
use std::sync::Arc;

use cashu::MintUrl;
use cdk::cdk_database;
use cdk::nuts::Token;
use cdk::wallet::types::WalletKey;
use cdk::wallet::MultiMintWallet;
use cdk::wallet::ReceiveOptions;
use cdk::wallet::WalletBuilder;
use cdk::Wallet;
use cdk_common::database::WalletDatabase;
use cdk_common::util::unix_time;
use cdk_common::CurrencyUnit;
use cdk_sqlite::WalletSqliteDatabase;

use url::Url;

use crate::config::Config;
use crate::metrics::StateTrait;

use keychat_rust_ffi_plugin::api_cashu::MnemonicInfo;

pub async fn crate_cashu_wallet(conf: &Config, add_mints: bool) -> anyhow::Result<MultiMintWallet> {
    if conf.timeout_ms == 0 {
        return Err(format_err!("zero ms timeout"));
    }
    if conf.fee.mints.is_empty() {
        return Err(format_err!("empty mints"));
    }

    std::env::set_var("RUST_BACKTRACE", "1");
    let mi = MnemonicInfo::with_words(&conf.words)?;
    let seed = mi.mnemonic().to_seed("");
    let localstore: Arc<dyn WalletDatabase<Err = cdk_database::Error> + Send + Sync> =
        Arc::new(WalletSqliteDatabase::new(&conf.database).await?);

    let mut wallets: Vec<Wallet> = Vec::new();

    let mints = localstore.get_mints().await?;
    if mints.is_empty() {}

    for (mint_url, mint_info) in mints {
        let mut units = if let Some(mint_info) = mint_info {
            mint_info.supported_units().into_iter().cloned().collect()
        } else {
            vec![CurrencyUnit::Sat]
        };
        if units.is_empty() {
            units.push(CurrencyUnit::Sat);
        }

        for unit in units {
            let mint_url_clone = mint_url.clone();
            let builder = WalletBuilder::new()
                .mint_url(mint_url_clone.clone())
                .unit(unit)
                .localstore(localstore.clone())
                .seed(&seed);

            let wallet = builder.build()?;

            let wallet_clone = wallet.clone();

            tokio::spawn(async move {
                if let Err(err) = wallet_clone.get_mint_info().await {
                    error!(
                        "Could not get mint quote for {}, {}",
                        wallet_clone.mint_url, err
                    );
                }
            });

            wallets.push(wallet);
        }
    }
    let multi_mint_wallet = MultiMintWallet::new(localstore, Arc::new(seed), wallets);

    if add_mints {
        for mint in conf.mints() {
            let res = multi_mint_wallet
                .localstore
                .add_mint(mint.clone(), None)
                .await;
            warn!("cashu load mint (plugin): {:?} {:?}", mint, res);
            res?;
        }
    }

    if let Some(listfile) = &conf.mints_file {
        MintsBlocker::load(listfile).await?;
    }

    Ok(multi_mint_wallet)
}

use std::collections::BTreeMap as Map;
use std::fs::File;
use tokio::sync::Mutex;
#[derive(Debug, Default)]
pub(crate) struct MintsBlocker {
    map: Map<String, MintRecord>,
    file: PathBuf,
    file_modified: u64,
}
use std::sync::OnceLock;
static MINTS_BLOCKEDR: OnceLock<Mutex<MintsBlocker>> = OnceLock::new();
use std::path::PathBuf;
impl MintsBlocker {
    async fn get<State>(url: &Url, state: State) -> anyhow::Result<Option<Blocker>>
    where
        State: StateTrait + Send + 'static,
    {
        let host = url
            .host_str()
            .ok_or_else(|| format_err!("mint url not contains host"))?;
        let lock = MINTS_BLOCKEDR.get().unwrap();

        let blocker;
        let mut lock = lock.lock().await;
        if let Some(m) = lock.map.get(host) {
            if m.amount > state.as_config().fee.untrusted_mint_balance_limit {
                return Err(format_err!(
                    "the host of mintUrl {} already blocked temporaty: {}",
                    host,
                    url.as_str()
                )
                .into());
            }

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
            let record = MintRecord::new(url.clone());
            blocker = record.blocker.clone().unwrap();
            lock.map.insert(host.to_owned(), record);
        }

        let _blocker = blocker.lock_owned().await;
        let w = state.as_wallet();
        let url = MintUrl::from_str(url.as_str())?;
        let res = w.localstore.add_mint(url.clone(), None).await;
        warn!("add_mint_with_units {:?} got: {:?}", url, res);
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
                                .host_str()
                                .ok_or_else(|| format_err!("the host url is none"))?;
                            map.insert(host.to_owned(), mb);
                        }
                        Err(e) => {
                            error!(
                                "MintRecord {} line-{} parse faild: {} {}",
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

    pub(crate) async fn update_balances(
        balances: impl Iterator<Item = (&str, u64)>,
    ) -> anyhow::Result<usize> {
        let lock = MINTS_BLOCKEDR.get().unwrap();
        let size = {
            let mut lock = lock.lock().await;
            for (k, v) in balances {
                let url = k.parse::<Url>()?;
                let host = url
                    .host_str()
                    .ok_or_else(|| format_err!("the host url is none"))?;

                if let Some(b) = lock.map.get_mut(host) {
                    b.amount = v;
                } else {
                    let mut b = MintRecord::new(k.parse()?);
                    b.amount = v;

                    lock.map.insert(host.to_string(), b);
                }
            }
            lock.map.len()
        };

        Ok(size)
    }
    #[allow(dead_code)]
    async fn flush() -> anyhow::Result<()> {
        let lock = MINTS_BLOCKEDR.get().unwrap();
        let lock = lock.lock().await;
        let mut blocked = Vec::with_capacity(lock.map.len());
        for b in lock.map.values() {
            let js = serde_json::to_string(&b).unwrap();
            blocked.push((b.ts, js));
        }
        blocked.sort_by_key(|bs| bs.0);

        let mut str = String::new();
        for (_, b) in blocked {
            str.push_str(&b);
            str.push_str("\n");
        }

        if str.len() > 1 {
            std::fs::write(&lock.file, &str)?;
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
        assert_eq!(url.host_str().unwrap(), "8333.space");

        let url: Url = "https://8333.space:8338".parse().unwrap();
        assert_eq!(url.host_str().unwrap(), "8333.space");

        let url: Url = "https://mint.8333.space:8338".parse().unwrap();
        assert_eq!(url.host_str().unwrap(), "mint.8333.space");
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
    amount: u64,
    blocked: bool,
    #[serde(skip)]
    blocker: Option<Blocker>,
}
impl MintRecord {
    fn new(url: Url) -> Self {
        Self {
            url,
            ts: unix_time(),
            amount: 0,
            blocked: false,
            blocker: Some(new_blocker(false)),
        }
    }
}

pub async fn receive_tokens<State>(
    cashu: Vec<&str>,
    eventid: &str,
    ip: &str,
    price: u64,
    state: State,
) -> anyhow::Result<Option<()>>
where
    State: StateTrait + Send + 'static + Clone,
{
    let mut total_amount = 0u64;
    let mut all_proofs = Vec::new();
    let mut mint_url: Option<MintUrl> = None;
    let mut unit = None;

    for token_str in cashu {
        let tokens: Token = Token::from_str(token_str)
            .map_err(|e| format_err!(format!("cashu tokens decode: {}", e)))?;

        if mint_url.is_none() {
            mint_url = Some(tokens.mint_url()?);
            unit = tokens.unit();
        }

        let this_mint = tokens.mint_url()?;
        let this_unit = tokens.unit().unwrap_or_default();

        let amount: u64 = tokens.value()?.into();
        if amount < price {
            return Err(format_err!("cashu tokens amount not enough: {}/{}", amount, price).into());
        }

        total_amount += amount;

        let wallet = match state
            .as_wallet()
            .get_wallet(&WalletKey::new(this_mint.clone(), this_unit.clone()))
            .await
        {
            Some(wallet) => Ok(wallet.clone()),
            None => {
                debug!("Wallet does not exist creating..");
                state
                    .as_wallet()
                    .create_and_add_wallet(&this_mint.to_string(), this_unit.clone(), None)
                    .await
            }
        }?;

        let keysets_info = match state
            .as_wallet()
            .localstore
            .get_mint_keysets(this_mint.clone())
            .await?
        {
            Some(keysets_info) => keysets_info,
            // Hit the keysets endpoint if we don't have the keysets for this Mint
            None => wallet.get_mint_keysets().await?,
        };
        let proofs = tokens.proofs(&keysets_info)?;
        all_proofs.extend(proofs);

        let conf = state.as_config();

        let has = state
            .as_wallet()
            .has(&WalletKey::new(this_mint.clone(), this_unit.clone()))
            .await;
        if !(conf.mints().contains(&this_mint) && has) {
            let url = Url::parse(&this_mint.to_string())?;
            let blocker = MintsBlocker::get(&url, state.clone()).await?;
            if let Some(blocker) = blocker {
                let b = blocker.lock().await;
                if b.load(Ordering::SeqCst) {
                    return Err(format_err!(
                        "the host of mintUrl {} already blocked: {}",
                        url.host_str().unwrap_or_default(),
                        url.to_string()
                    )
                    .into());
                }
            }
        }
    }

    // if state.as_limits().cashu_failed_check(ip, &conf.limits) {
    //     return Ok(None);
    // }

    let start = std::time::Instant::now();
    let eventid = eventid.to_owned();
    let ip = ip.to_owned();

    let mint_url = mint_url.ok_or_else(|| format_err!("no mint url"))?;
    let unit = unit.unwrap_or_default();
    let wallet = match state
        .as_wallet()
        .get_wallet(&WalletKey::new(mint_url.clone(), unit.clone()))
        .await
    {
        Some(w) => w,
        None => {
            state
                .as_wallet()
                .localstore
                .add_mint(mint_url.clone(), None)
                .await?;
            state
                .as_wallet()
                .create_and_add_wallet(&mint_url.to_string(), unit.clone(), None)
                .await?
        }
    };

    let mint_url_clone = mint_url.clone();
    let unit_clone = unit.clone();
    let proofs_for_send = all_proofs.clone();
    let encoded_token = Token::new(mint_url.clone(), proofs_for_send, None, unit).to_string();
    let proofs_for_retry = all_proofs.clone();
    let wallet_cloned = wallet.clone();

    let fut = async move {
        let res = state
            .as_wallet()
            .receive(&encoded_token, ReceiveOptions::default())
            .await;
        match res {
            Ok(tx) if *tx.amount.as_ref() <= total_amount => {
                let costms = start.elapsed().as_millis();

                info!(
                    "{}'s {:?} tokens receive {}/price {} ms ok: {:?}",
                    eventid, ip, price, costms, tx.amount,
                );
                state
                    .as_metrics()
                    .0
                    .send(crate::Metric {
                        costms: costms as _,
                        amount: tx.amount.into(),
                    })
                    .unwrap();
            }
            Err(e) => {
                let costms = start.elapsed().as_millis();
                let msg = e.to_string();
                error!(
                    "{}'s {:?} tokens receive {}/price {} ms failed: {:?}",
                    eventid, ip, price, costms, msg
                );
                // if some tokens are already spent, we can check which ones are unspent and try to receive them again
                if msg.contains("Token Already Spent") {
                    match wallet_cloned
                        .check_proofs_spent(proofs_for_retry.clone())
                        .await
                    {
                        Ok(statuses) => {
                            let unspent: cashu::Proofs = proofs_for_retry
                                .into_iter()
                                .zip(statuses)
                                .filter_map(|(p, s)| {
                                    (s.state == cashu::State::Unspent).then_some(p)
                                })
                                .collect();
                            if !unspent.is_empty() {
                                let encoded_token =
                                    Token::new(mint_url_clone, unspent, None, unit_clone)
                                        .to_string();
                                match state
                                    .as_wallet()
                                    .receive(&encoded_token, ReceiveOptions::default())
                                    .await
                                {
                                    Ok(tx) if *tx.amount.as_ref() <= total_amount => {
                                        let costms = start.elapsed().as_millis();

                                        info!(
                                            "{}'s {:?} tokens retry receive {}/price {} ms ok: {:?}",
                                            eventid, ip, price, costms, tx.amount,
                                        );
                                        state
                                            .as_metrics()
                                            .0
                                            .send(crate::Metric {
                                                costms: costms as _,
                                                amount: tx.amount.into(),
                                            })
                                            .unwrap();
                                    }
                                    Err(e) => {
                                        let costms = start.elapsed().as_millis();
                                        error!(
                                            "{}'s {:?} tokens retry receive {}/price {} ms failed: {:?}",
                                            eventid, ip, price, costms, e
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(es) => {
                            error!("check_proofs_spent failed: {}", es);
                        }
                    }
                }
            }
            _ => {}
        }
    };

    tokio::spawn(fut);

    Ok(Some(()))
}
