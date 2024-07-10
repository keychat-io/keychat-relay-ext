use std::sync::Arc;

use cashu_wallet::store::UnitedStore;
use cashu_wallet::wallet::AmountHelper;
use cashu_wallet::wallet::HttpOptions;
use cashu_wallet::UniError;
use cashu_wallet::UniErrorFrom;
use cashu_wallet::UnitedWallet;

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

pub async fn receive_tokens<State>(
    cashu: &str,
    eventid: &str,
    ip: &str,
    price: u64,
    state: State,
) -> Result<Option<()>, UniError<<State::Store as UnitedStore>::Error>>
where
    State: StateTrait,
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

    if state.as_limits().cashu_failed_check(ip, &conf.limits) {
        return Ok(None);
    }

    let start = std::time::Instant::now();
    let res = state.as_wallet().receive_tokens(cashu).await;
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
            state.as_limits().cashu_failed_count(ip, &conf.limits);
            error!(
                "{}'s {:?} tokens receive {} {}ms failed: {:?}",
                eventid, ip, price, costms, res
            );
            return res.and_then(|a| Err(format_err!("tokens amount {}<{}", a, price).into()));
        }
    }

    Ok(Some(()))
}
