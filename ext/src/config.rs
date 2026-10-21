use cdk::mint_url::MintUrl;
use std::{net::SocketAddr, path::PathBuf};
use tracing::Level;

#[derive(clap::Parser, Debug, Clone)]
#[clap(version = env!("CARGO_PKG_VERSION"))]
pub struct Opts {
    #[clap(short, long, parse(from_occurrences))]
    pub verbose: u8,
    #[clap(short, long, default_value = "gas.toml")]
    pub config: String,
}
impl Opts {
    pub fn log(&self) -> Level {
        match self.verbose {
            0 => Level::WARN,
            1 => Level::INFO,
            2 => Level::DEBUG,
            _ => Level::TRACE,
        }
    }
    pub fn parse_config(&self) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(&self.config)?;
        // TOML errors can echo secret source lines from old configuration files.
        let mut config: Config = toml::from_str(&text)
            .map_err(|_| anyhow!("invalid configuration; check the secret-free template"))?;
        config.validate()?;
        Ok(config)
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub enabled: bool,
    #[serde(default)]
    pub allow_pending: bool, // accepted for compatibility; never permits pending payments
    #[serde(default)]
    pub allow_free: bool, // SAS only; gas rejects this setting
    pub database: String,
    pub timeout_ms: u64,
    #[serde(default)]
    pub remote_ip_header: String,
    #[serde(default)]
    pub kinds: Option<Vec<u64>>,
    pub fee: Fee,
    #[serde(default)]
    pub words: Option<String>, // rejected; never logged
    #[serde(default = "default_seed_env")]
    pub seed_env: String,
    #[serde(default)]
    pub relay_scope: Option<String>,
    #[serde(default)]
    pub stamp: Option<Stamp>,
    #[serde(default)]
    pub input: InputLimits,
    pub limits: Limits,
    #[serde(default)]
    pub mints_file: Option<PathBuf>, // legacy config, no longer an allow-list fallback
}
fn default_seed_env() -> String {
    "KEYCHAT_CASHU_MNEMONIC".into()
}
impl Config {
    pub fn validate(&mut self) -> anyhow::Result<()> {
        ensure!(
            self.words.is_none(),
            "inline words are no longer supported; use seed_env"
        );
        ensure!(self.timeout_ms > 0, "timeout_ms must be positive");
        ensure!(
            !self.seed_env.is_empty(),
            "seed_env must name a secret environment variable"
        );
        ensure!(
            self.database.ends_with(".cdk018.sqlite"),
            "use a new .cdk018.sqlite database, never an old wallet database"
        );
        ensure!(self.fee.unit == "sat", "legacy fee unit must be sat");
        ensure!(!self.fee.mints.is_empty(), "sat mint allow-list is empty");
        for mint in &self.fee.mints {
            validate_mint(mint)?;
        }
        self.fee.prices.sort_by_key(|p| p.min);
        ensure!(!self.fee.prices.is_empty(), "prices is empty");
        for (i, p) in self.fee.prices.iter().enumerate() {
            ensure!(
                p.max > p.min && (p.price > 0 || self.allow_free),
                "invalid price range or zero price without allow_free"
            );
            ensure!(
                if i == 0 {
                    p.min == 0
                } else {
                    p.min == self.fee.prices[i - 1].max
                },
                "price ranges must be contiguous from zero"
            );
        }
        ensure!(
            self.input.max_token_bytes > 0
                && self.input.max_proofs > 0
                && self.input.max_amount > 0,
            "input limits must be positive"
        );
        ensure!(
            self.input.max_amount <= i64::MAX as u64,
            "max_amount exceeds the SQLite integer range"
        );
        if let Some(s) = &self.stamp {
            ensure!(s.unit == "stamp", "stamp.unit must be stamp");
            validate_mint(&s.mint)?;
            ensure!(
                s.cost_per_event > 0 && !s.kinds.is_empty(),
                "stamp price and kinds must be nonempty"
            );
        }
        Ok(())
    }
    pub fn validate_gas(&self) -> anyhow::Result<()> {
        ensure!(!self.allow_free, "gas cannot allow unpaid charged events");
        let scope = url::Url::parse(self.relay_scope.as_deref().unwrap_or(""))
            .map_err(|_| anyhow!("gas requires fixed relay_scope"))?;
        ensure!(
            matches!(scope.scheme(), "ws" | "wss")
                && scope.host_str().is_some()
                && scope.username().is_empty()
                && scope.password().is_none()
                && scope.query().is_none()
                && scope.fragment().is_none(),
            "invalid relay_scope"
        );
        Ok(())
    }
    pub fn mints(&self) -> &[MintUrl] {
        &self.fee.mints
    }
    pub fn cost_per_event(&self) -> u64 {
        self.fee.prices[0].price
    }
    pub fn sat_charged(&self, kind: u64) -> bool {
        self.kinds
            .as_ref()
            .map(|k| k.contains(&kind))
            .unwrap_or(true)
    }
    pub fn stamp_charged(&self, kind: u64) -> bool {
        self.stamp
            .as_ref()
            .is_some_and(|s| s.enabled && s.kinds.contains(&kind))
    }
    pub fn charged(&self, kind: u64) -> bool {
        self.sat_charged(kind) || self.stamp_charged(kind)
    }
    pub fn price_for(&self, kind: u64, mint: &MintUrl, unit: &str) -> anyhow::Result<u64> {
        if unit == "sat" && self.sat_charged(kind) && self.mints().contains(mint) {
            return Ok(self.cost_per_event());
        }
        if let Some(s) = &self.stamp {
            if s.enabled && s.kinds.contains(&kind) && &s.mint == mint && unit == s.unit {
                return Ok(s.cost_per_event);
            }
        }
        bail!("mint, unit or kind does not match a payment rule")
    }
    pub fn compute_price(&self, size: u64) -> anyhow::Result<u64> {
        ensure!(size <= self.fee.maxsize, "size exceeds limit");
        if size == 0 {
            return Ok(self.cost_per_event());
        }
        if let Some(p) = self
            .fee
            .prices
            .iter()
            .find(|p| size > p.min && size <= p.max)
        {
            return Ok(p.price);
        }
        let p = self.fee.prices.last().unwrap();
        p.price
            .checked_mul(size.div_ceil(p.max))
            .ok_or_else(|| anyhow!("price overflow"))
    }
}
fn validate_mint(mint: &MintUrl) -> anyhow::Result<()> {
    let u = url::Url::parse(&mint.to_string())?;
    ensure!(
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "mint must be an HTTPS URL without credentials, query or fragment"
    );
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stamp {
    pub enabled: bool,
    pub mint: MintUrl,
    pub unit: String,
    pub cost_per_event: u64,
    pub kinds: Vec<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct InputLimits {
    pub max_token_bytes: usize,
    pub max_proofs: usize,
    pub max_amount: u64,
}
impl Default for InputLimits {
    fn default() -> Self {
        Self {
            max_token_bytes: 65536,
            max_proofs: 128,
            max_amount: 1_000_000,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fee {
    pub unit: String,
    pub mints: Vec<MintUrl>,
    pub maxsize: u64,
    pub prices: Vec<Price>,
    #[serde(default)]
    pub untrusted_mint_balance_limit: u64,
    #[serde(default)]
    pub untrusted_mint_should_transfer: u64,
    #[serde(default)]
    pub expired: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Price {
    pub min: u64,
    pub max: u64,
    pub price: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    pub cashu_failed: Limit,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limit {
    pub secs: u32,
    pub allow: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn gas_config() -> Config {
        toml::from_str(include_str!("../gas.toml")).unwrap()
    }
    #[test]
    fn rules_are_alternatives_and_exchange_rate_is_not_a_gas_input() {
        let mut c = gas_config();
        c.validate().unwrap();
        c.validate_gas().unwrap();
        let sat = c.fee.mints[0].clone();
        let stamp = c.stamp.as_ref().unwrap().mint.clone();
        assert_eq!(c.price_for(4, &sat, "sat").unwrap(), 1);
        assert_eq!(c.price_for(1059, &stamp, "stamp").unwrap(), 1);
        assert!(c.price_for(4, &stamp, "sat").is_err());
        assert!(c.price_for(4, &sat, "stamp").is_err());
        assert!(!c.charged(1));
        assert!(c.price_for(1, &stamp, "stamp").is_err());
        // A recharge ratio belongs to bridge/client. Even k=10 cannot change
        // gas's one-stamp message contract.
        let text = format!("stamps_per_sat = 10\n{}", include_str!("../gas.toml"));
        let c: Config = toml::from_str(&text).unwrap();
        assert_eq!(c.price_for(4, &stamp, "stamp").unwrap(), 1);
    }
    #[test]
    fn invalid_or_unsafe_configuration_is_rejected() {
        let mut c = gas_config();
        c.input.max_amount = i64::MAX as u64 + 1;
        assert!(c.validate().is_err());
        let mut c = gas_config();
        c.words = Some("never log this".into());
        assert!(c.validate().is_err());
        let mut c = gas_config();
        c.database = "old.sqlite".into();
        assert!(c.validate().is_err());
        let mut c = gas_config();
        c.allow_free = true;
        assert!(c.validate_gas().is_err());
        let mut c = gas_config();
        c.relay_scope = None;
        assert!(c.validate_gas().is_err());
        let mut c = gas_config();
        c.fee.mints = vec!["https://other-mint.invalid".parse().unwrap()];
        assert!(c
            .price_for(4, &"https://mint.keychat.io".parse().unwrap(), "sat")
            .is_err());
        c.stamp.as_mut().unwrap().unit = "sat".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn free_sas_prices_require_opt_in_and_cannot_enable_free_gas() {
        let mut c: Config = toml::from_str(include_str!("../sas.toml")).unwrap();
        c.fee.prices.truncate(1);
        c.fee.prices[0].price = 0;
        assert!(c.validate().is_err());
        c.allow_free = true;
        c.validate().unwrap();
        for size in [1, 10485760, 10485761, 104857600] {
            assert_eq!(c.compute_price(size).unwrap(), 0);
        }
        assert!(c.compute_price(104857601).is_err());
        let mut gas = gas_config();
        gas.fee.prices[0].price = 0;
        assert!(gas.validate().is_err());
        gas.allow_free = true;
        gas.validate().unwrap();
        assert!(gas.validate_gas().is_err());
    }
    #[test]
    fn sas_size_prices_and_sat_are_preserved() {
        let mut c: Config = toml::from_str(include_str!("../sas.toml")).unwrap();
        c.validate().unwrap();
        assert_eq!(c.fee.unit, "sat");
        assert!(c.stamp.is_none());
        assert_eq!(c.compute_price(1).unwrap(), 1);
        assert_eq!(c.compute_price(10485760).unwrap(), 1);
        assert_eq!(c.compute_price(10485761).unwrap(), 2);
        assert_eq!(c.compute_price(104857600).unwrap(), 2);
        assert!(c.compute_price(104857601).is_err());
    }
}
