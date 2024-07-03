#[derive(clap::Parser, Debug, Clone)]
#[clap(version = env!("CARGO_PKG_VERSION"))]
pub struct Opts {
    // #[clap(short, long, help = "The address of serve")]
    // pub listen: SocketAddr,
    #[clap(
        short,
        long,
        parse(from_occurrences),
        help = "Loglevel: -v(Info), -vv(Debug), -vvv+(Trace)"
    )]
    pub verbose: u8,
    #[clap(short, long, help = "The path of Config", default_value = "gas.toml")]
    pub config: String,
}

use tracing::Level;
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
        let fc = std::fs::read_to_string(&self.config)?;
        let mut c = toml::from_str::<Config>(&fc)?;
        c.fee.prices.sort_by_key(|f| f.min);

        if c.fee.prices.is_empty() {
            bail!("prices is empty");
        }
        for (i, p) in c.fee.prices.iter().enumerate() {
            if i == 0 {
                if p.min != 0 {
                    bail!("prices[0].min should is 0");
                }

                continue;
            }

            if p.min > p.max {
                bail!("prices[{}].min > max: {} > {}", i, p.min, p.max);
            }

            let prev = c.fee.prices.get(i - 1).unwrap();
            if p.min != prev.max {
                bail!(
                    "prices[{}].min {} != prices[{}].max: {}",
                    i,
                    p.min,
                    i - 1,
                    prev.max
                );
            }
        }

        Ok(c)
    }
}

use std::net::SocketAddr;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub enabled: bool,
    #[serde(default)]
    pub allow_pending: bool,
    #[serde(default)]
    pub allow_free: bool,
    pub database: String,
    pub timeout_ms: u64,
    #[serde(default)]
    pub remote_ip_header: String,
    #[serde(default)]
    pub kinds: Option<Vec<u64>>,
    pub fee: Fee,
    pub limits: Limits,
}
impl Config {
    pub fn mints(&self) -> &[cashu_wallet::Url] {
        &self.fee.mints
    }
    // Cost author to pay per event
    pub fn cost_per_event(&self) -> u64 {
        self.fee.prices[0].price
    }

    pub fn compute_price(&self, size: u64) -> anyhow::Result<u64> {
        let fee = &self.fee;
        if size == 0 {
            return Ok(fee.prices[0].price);
        }

        if size > fee.maxsize {
            bail!("size exceeds limit: {}", fee.maxsize);
        }

        let mut f = fee
            .prices
            .iter()
            .take_while(|p| size <= p.max)
            .find(|p| size > p.min && size <= p.max)
            .map(|p| p.price);

        if f.is_none() {
            // max
            let p = fee.prices.last().unwrap();
            let x = size as f64 / p.max as f64;
            let p = p.price * (x.ceil() as u64);
            f = Some(p);
        }

        Ok(f.unwrap())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fee {
    pub unit: String,
    pub mints: Vec<cashu_wallet::Url>,
    pub maxsize: u64,
    pub prices: Vec<Price>,
    #[serde(default)]
    pub expired: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Price {
    min: u64,
    max: u64,
    price: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    pub(crate) cashu_failed: Limit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limit {
    pub(crate) secs: u32,
    pub(crate) allow: usize,
}
