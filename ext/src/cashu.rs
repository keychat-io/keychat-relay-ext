//! Upstream CDK 0.18 receive-only adapter. One process owns one durable wallet.
use crate::config::{Config, InputLimits};
use cdk::{
    mint_url::MintUrl,
    nuts::{CurrencyUnit, PreMintSecrets, Proofs, State, Token},
    wallet::{BaseHttpClient, HttpClient, MintConnector, ReceiveOptions, WalletBuilder},
    Wallet,
};
use cdk_common::{
    database::{self, WalletDatabase},
    wallet::{
        KeysetLoadPolicy, OperationData, ReceiveSagaState, Transaction, TransactionDirection,
        TransactionStatus, WalletSagaState,
    },
    ProofsMethods,
};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore};
use zeroize::{Zeroize, Zeroizing};

pub type Database = Arc<dyn WalletDatabase<database::Error> + Send + Sync>;
const NS: &str = "keychat-receipts-v1";
const MAX_PAYMENT_WORKERS: usize = 64;

pub struct CashuWallet {
    pub db: Database,
    wallets: BTreeMap<(String, String), Wallet>,
    gate: Mutex<()>,
    workers: Arc<Semaphore>,
    scope: String,
    seed: Zeroizing<[u8; 64]>,
    _lock: File,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub event: String,
    pub fingerprint: String,
    pub mint: MintUrl,
    pub unit: CurrencyUnit,
    pub input_ys: Vec<String>,
    pub amount: u64,
    pub price: u64,
    pub paid: bool,
    pub transaction_id: Option<String>,
    // Persisted before invoking CDK recovery. CDK may mark a receive Completed
    // when inputs are spent even if /restore returns no outputs; require custody.
    pub recovery_outputs: Option<Vec<(String, u64)>>,
    // Secret recovery material, not a new ticket supplied on retry. Older rows
    // deserialize without it and remain denied if not already confirmed.
    #[serde(default)]
    original: Option<OriginalReceive>,
    #[serde(default)]
    recovery_attempted: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct OriginalReceive {
    scope: String,
    token: String,
    binding: String,
}
impl Drop for OriginalReceive {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}
pub struct ParsedToken {
    pub token: Token,
    pub mint: MintUrl,
    pub unit: CurrencyUnit,
    pub amount: u64,
}
fn hash(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
    }
    faster_hex::hex_string(&h.finalize())
}

pub fn parse_token(input: &str, limits: &InputLimits) -> anyhow::Result<ParsedToken> {
    ensure!(input.len() <= limits.max_token_bytes, "token too large");
    ensure!(
        input.starts_with("cashuA") || input.starts_with("cashuB"),
        "unsupported token format"
    );
    // Only the old A format may use the ordinary Base64 alphabet. Do not accept
    // mixed alphabets, whitespace, payment URI prefixes or noncanonical tails.
    let payload = &input[6..];
    ensure!(
        !payload.bytes().any(|b| b.is_ascii_whitespace()),
        "invalid token encoding"
    );
    let standard = payload.contains(['+', '/']);
    let urlsafe = payload.contains(['-', '_']);
    ensure!(!(standard && urlsafe), "mixed token alphabet");
    let normalized;
    let text = if standard {
        ensure!(
            input.starts_with("cashuA"),
            "legacy Base64 is only supported for cashuA"
        );
        let bytes = if payload.ends_with('=') {
            base64_simd::STANDARD.decode_to_vec(payload)
        } else {
            base64_simd::STANDARD_NO_PAD.decode_to_vec(payload)
        }
        .map_err(|_| anyhow!("invalid legacy token encoding"))?;
        ensure!(
            bytes.len() <= limits.max_token_bytes,
            "decoded token too large"
        );
        normalized = format!(
            "cashuA{}",
            base64_simd::URL_SAFE_NO_PAD.encode_to_string(&bytes)
        );
        &normalized
    } else {
        input
    };
    let token = Token::from_str(text).map_err(|_| anyhow!("invalid cashu token"))?;
    let count: usize = match &token {
        Token::TokenV3(t) => {
            ensure!(t.token.len() == 1, "multiple mints are not supported");
            t.token.iter().map(|t| t.proofs.len()).sum()
        }
        Token::TokenV4(t) => t.token.iter().map(|t| t.proofs.len()).sum(),
    };
    ensure!(
        count > 0 && count <= limits.max_proofs,
        "proof count out of range"
    );
    let mint = token
        .mint_url()
        .map_err(|_| anyhow!("invalid token mint"))?;
    let unit = token.unit().unwrap_or(CurrencyUnit::Sat);
    let amount: u64 = token
        .value()
        .map_err(|_| anyhow!("invalid token amount or duplicate proofs"))?
        .into();
    ensure!(
        amount > 0 && amount <= limits.max_amount,
        "token amount out of range"
    );
    Ok(ParsedToken {
        token,
        mint,
        unit,
        amount,
    })
}

// The lock file is also a format/seed/scope marker. Refuse unmarked databases
// BEFORE opening CDK, so its migrations cannot touch an old SQLite wallet.
fn lock_database(path: &str, identity: &str) -> anyhow::Result<File> {
    let marker = format!("{path}.owner");
    let new = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&marker)
    {
        Ok(f) => {
            if Path::new(path).exists() {
                std::fs::remove_file(&marker)?;
                bail!("refusing an existing unmarked database");
            }
            Some(f)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
        Err(e) => return Err(e.into()),
    };
    let fresh = new.is_some();
    let mut f = match new {
        Some(f) => f,
        None => OpenOptions::new().read(true).write(true).open(&marker)?,
    };
    f.try_lock_exclusive()
        .map_err(|_| anyhow!("wallet is owned by another active process"))?;
    if fresh {
        f.write_all(identity.as_bytes())?;
        f.sync_all()?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
            .sync_all()?;
    } else {
        let mut stored = String::new();
        f.read_to_string(&mut stored)?;
        ensure!(
            stored == identity,
            "wallet seed, format, mode or relay_scope changed"
        );
        ensure!(
            Path::new(path).is_file(),
            "wallet database missing; restore database and ownership marker together"
        );
    }
    Ok(f)
}

impl CashuWallet {
    pub async fn new(conf: &Config, gas: bool) -> anyhow::Result<Arc<Self>> {
        let words = Zeroizing::new(
            std::env::var(&conf.seed_env)
                .map_err(|_| anyhow!("wallet secret environment variable is missing"))?,
        );
        let mnemonic =
            bip39::Mnemonic::from_str(&words).map_err(|_| anyhow!("invalid wallet mnemonic"))?;
        let seed = Zeroizing::new(mnemonic.to_seed(""));
        let scope = if gas {
            conf.relay_scope
                .clone()
                .ok_or_else(|| anyhow!("missing relay_scope"))?
        } else {
            "sas-storage".into()
        };
        let identity = hash(&[b"cdk018-receive-only-v1", scope.as_bytes(), seed.as_ref()]);
        let lock = lock_database(&conf.database, &identity)?;
        let db: Database = Arc::new(
            cdk_sqlite::WalletSqliteDatabase::new(std::path::PathBuf::from(&conf.database)).await?,
        );
        let mut rules: Vec<_> = conf
            .mints()
            .iter()
            .map(|m| (m.clone(), CurrencyUnit::Sat))
            .collect();
        if gas {
            if let Some(s) = &conf.stamp {
                if s.enabled {
                    rules.push((s.mint.clone(), CurrencyUnit::from_str(&s.unit)?));
                }
            }
        }
        let mut wallets = BTreeMap::new();
        for (mint, unit) in rules {
            let w = WalletBuilder::new()
                .mint_url(mint.clone())
                .unit(unit.clone())
                .seed(*seed)
                .localstore(db.clone())
                .build()?;
            wallets.insert((mint.to_string(), unit.to_string()), w);
        }
        let this = Arc::new(Self {
            db,
            wallets,
            gate: Mutex::new(()),
            workers: Arc::new(Semaphore::new(MAX_PAYMENT_WORKERS)),
            scope: hash(&[scope.as_bytes()]),
            seed,
            _lock: lock,
        });
        // Reconcile only known intents. No background balance merges, outgoing
        // payments, old database upgrades or untrusted mint discovery.
        let keys = this.db.kv_list(NS, &this.scope).await?;
        for event in keys {
            match this.status(event.clone(), conf.timeout_ms).await {
                Ok(Some(true)) => debug!(event_id=%event, "receipt confirmed during startup"),
                _ => warn!(event_id=%event, "receipt unresolved during startup; no permit granted"),
            }
        }
        Ok(this)
    }
    fn wallet(&self, mint: &MintUrl, unit: &CurrencyUnit) -> anyhow::Result<&Wallet> {
        self.wallets
            .get(&(mint.to_string(), unit.to_string()))
            .ok_or_else(|| anyhow!("mint/unit is not allow-listed"))
    }
    async fn load(&self, event: &str) -> anyhow::Result<Option<Receipt>> {
        let Some(bytes) = self.db.kv_read(NS, &self.scope, event).await? else {
            return Ok(None);
        };
        let receipt: Receipt = serde_json::from_slice(&bytes)?;
        ensure!(receipt.event == event, "receipt event binding mismatch");
        Ok(Some(receipt))
    }
    async fn save(&self, r: &Receipt) -> anyhow::Result<()> {
        self.db
            .kv_write(NS, &self.scope, &r.event, &serde_json::to_vec(r)?)
            .await?;
        Ok(())
    }
    pub async fn confirmed(&self, event: &str) -> anyhow::Result<bool> {
        let _guard = self.gate.lock().await;
        Ok(self.load(event).await?.is_some_and(|r| r.paid))
    }
    /// Reconcile an existing event without looking at a replacement ticket.
    pub async fn status(
        self: &Arc<Self>,
        event: String,
        timeout_ms: u64,
    ) -> anyhow::Result<Option<bool>> {
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("payment worker queue is full"))?;
        let this = self.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _guard = this.gate.lock().await;
            if this.load(&event).await?.is_none() {
                return Ok(None);
            }
            Ok(Some(this.reconcile(&event).await?))
        });
        tokio::time::timeout(Duration::from_millis(timeout_ms), task)
            .await
            .map_err(|_| anyhow!("payment status unresolved"))??
    }
    pub async fn pay(
        self: &Arc<Self>,
        event: String,
        parsed: ParsedToken,
        price: u64,
        timeout_ms: u64,
    ) -> anyhow::Result<u64> {
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("payment worker queue is full"))?;
        let this = self.clone();
        // A disconnected RPC must not drop the receive future or its lock.
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _guard = this.gate.lock().await;
            // Deadline applies to the RPC waiter. Never cancel a CDK/database
            // future mid-operation: the worker retains exclusive ownership.
            this.pay_locked(&event, parsed, price).await
        });
        tokio::time::timeout(Duration::from_millis(timeout_ms), task)
            .await
            .map_err(|_| anyhow!("payment unresolved; retry this event for reconciliation"))??
    }
    async fn pay_locked(
        &self,
        event: &str,
        parsed: ParsedToken,
        price: u64,
    ) -> anyhow::Result<u64> {
        if let Some(r) = self.load(event).await? {
            if r.paid {
                return Ok(0);
            }
            // Never swap the supplied token again or replace an uncertain token.
            return if self.reconcile(event).await? {
                Ok(0)
            } else {
                bail!("original payment unresolved")
            };
        }
        // Reconcile known earlier operations before considering a new ticket.
        // Definitively invalid proofs are compensated by CDK. Ambiguous spent
        // inputs may need restore, whose absent outputs never become a permit.
        if !self.db.get_incomplete_sagas().await?.is_empty() {
            for prior in self.db.kv_list(NS, &self.scope).await? {
                self.reconcile(&prior).await?;
            }
        }
        // Keep recovery unambiguous across different events as well.
        ensure!(
            self.db.get_incomplete_sagas().await?.is_empty(),
            "another receive requires reconciliation before a new payment"
        );
        let w = self.wallet(&parsed.mint, &parsed.unit)?;
        // This is the first network access; only configured mint/unit wallets exist.
        w.keysets(KeysetLoadPolicy::Refresh).await?;
        let keysets = self
            .db
            .get_mint_keysets(parsed.mint.clone())
            .await?
            .ok_or_else(|| anyhow!("missing mint keysets"))?;
        let proofs = parsed
            .token
            .proofs(&keysets)
            .map_err(|_| anyhow!("unknown or ambiguous proof keyset"))?;
        for p in &proofs {
            ensure!(
                keysets
                    .iter()
                    .any(|k| k.id == p.keyset_id && k.unit == parsed.unit),
                "proof keyset unit mismatch"
            );
        }
        let fee: u64 = w.get_proofs_fee(&proofs).await?.total.into();
        ensure!(
            parsed
                .amount
                .checked_sub(fee)
                .is_some_and(|net| net >= price),
            "net payment amount is insufficient"
        );
        let ys = canonical_ys(&proofs)?;
        for y in &ys {
            let key = hash(&[parsed.mint.to_string().as_bytes(), y.as_bytes()]);
            ensure!(
                self.db.kv_read(NS, "proof-claims", &key).await?.is_none(),
                "proof is already bound to an event"
            );
        }
        let fingerprint = hash(&[parsed.mint.to_string().as_bytes(), ys.join(",").as_bytes()]);
        let canonical =
            Token::new(parsed.mint.clone(), proofs, None, parsed.unit.clone()).to_string();
        let mut r = Receipt {
            event: event.into(),
            fingerprint,
            mint: parsed.mint.clone(),
            unit: parsed.unit.clone(),
            input_ys: ys,
            amount: parsed.amount,
            price,
            paid: false,
            transaction_id: None,
            recovery_outputs: None,
            original: None,
            recovery_attempted: false,
        };
        let binding = self.original_binding(&r, &canonical);
        r.original = Some(OriginalReceive {
            scope: self.scope.clone(),
            token: canonical,
            binding,
        });
        self.save(&r).await?;
        self.claim_original(&r).await?;
        self.receive_original(&mut r).await
    }
    fn original_binding(&self, r: &Receipt, token: &str) -> String {
        hash(&[
            b"original-receive-v1",
            self.scope.as_bytes(),
            r.event.as_bytes(),
            r.mint.to_string().as_bytes(),
            r.unit.to_string().as_bytes(),
            r.input_ys.join(",").as_bytes(),
            r.fingerprint.as_bytes(),
            &r.amount.to_be_bytes(),
            &r.price.to_be_bytes(),
            token.as_bytes(),
        ])
    }
    async fn original_proofs(&self, r: &Receipt) -> anyhow::Result<Proofs> {
        let original = r
            .original
            .as_ref()
            .ok_or_else(|| anyhow!("legacy receipt has no original recovery material"))?;
        ensure!(
            original.scope == self.scope
                && original.binding == self.original_binding(r, &original.token),
            "original receipt binding mismatch"
        );
        let token = Token::from_str(&original.token)
            .map_err(|_| anyhow!("invalid original recovery token"))?;
        ensure!(
            token.mint_url()? == r.mint
                && token.unit() == Some(r.unit.clone())
                && u64::from(token.value()?) == r.amount,
            "original token identity mismatch"
        );
        let keysets = self
            .db
            .get_mint_keysets(r.mint.clone())
            .await?
            .ok_or_else(|| anyhow!("original keyset metadata missing"))?;
        let proofs = token.proofs(&keysets)?;
        ensure!(
            !proofs.is_empty() && canonical_ys(&proofs)? == r.input_ys,
            "original proof identity mismatch"
        );
        ensure!(
            hash(&[
                r.mint.to_string().as_bytes(),
                r.input_ys.join(",").as_bytes()
            ]) == r.fingerprint,
            "original fingerprint mismatch"
        );
        for p in &proofs {
            ensure!(
                keysets
                    .iter()
                    .any(|k| k.id == p.keyset_id && k.unit == r.unit),
                "original proof keyset unit mismatch"
            );
        }
        Ok(proofs)
    }
    async fn claim_original(&self, r: &Receipt) -> anyhow::Result<()> {
        // Check the whole set first. Partial writes can then be completed after
        // restart, but another event's claim is never overwritten.
        for y in &r.input_ys {
            let key = hash(&[r.mint.to_string().as_bytes(), y.as_bytes()]);
            if let Some(owner) = self.db.kv_read(NS, "proof-claims", &key).await? {
                ensure!(
                    owner == r.event.as_bytes(),
                    "original proof belongs to another event"
                );
            }
        }
        for y in &r.input_ys {
            let key = hash(&[r.mint.to_string().as_bytes(), y.as_bytes()]);
            self.db
                .kv_write(NS, "proof-claims", &key, r.event.as_bytes())
                .await?;
        }
        Ok(())
    }
    async fn receive_original(&self, r: &mut Receipt) -> anyhow::Result<u64> {
        let w = self.wallet(&r.mint, &r.unit)?;
        let mut opts = ReceiveOptions::default();
        opts.metadata
            .insert("keychat_event".into(), r.event.clone());
        opts.metadata
            .insert("keychat_scope".into(), self.scope.clone());
        opts.metadata
            .insert("keychat_proofs".into(), r.fingerprint.clone());
        let original = r
            .original
            .as_ref()
            .ok_or_else(|| anyhow!("original recovery token missing"))?;
        let net: u64 = w
            .receive(&original.token, opts)
            .await
            .map_err(|_| anyhow!("payment unconfirmed; retry same event for reconciliation"))?
            .into();
        ensure!(
            net >= r.price && net <= r.amount,
            "confirmed net payment is insufficient or invalid"
        );
        let tx = self
            .matching_transaction(r)
            .await?
            .ok_or_else(|| anyhow!("missing CDK receive transaction"))?;
        ensure!(
            tx.status == TransactionStatus::Completed && u64::from(tx.amount) == net,
            "CDK receipt is unconfirmed"
        );
        r.paid = true;
        r.transaction_id = Some(tx.id().to_string());
        self.save(r).await?;
        Ok(net)
    }
    async fn continue_unstarted(&self, r: &mut Receipt, proofs: &Proofs) -> anyhow::Result<bool> {
        if r.recovery_outputs.is_some() || r.transaction_id.is_some() {
            return Ok(false);
        }
        if !self.db.get_incomplete_sagas().await?.is_empty() {
            return Ok(false);
        }
        // Evidence of *any* related transaction, including Failed or a partial
        // input set, prohibits creating a new operation. CDK 0.18 durably writes
        // a Pending transaction before post_swap; this receiver never prunes it.
        let txs = self
            .db
            .list_transactions(Some(r.mint.clone()), None, None)
            .await?;
        if txs
            .iter()
            .any(|t| t.ys.iter().any(|y| r.input_ys.contains(&y.to_string())))
        {
            return Ok(false);
        }
        let ys = proofs.ys()?;
        let stored = self.db.get_proofs_by_ys(ys).await?;
        if !stored.is_empty() {
            // CDK may have stored the original Pending inputs immediately before
            // add_saga. All rows must belong to one orphan operation, exactly
            // match our original inputs, and have no saga or transaction history.
            let Some(op) = stored[0].used_by_operation else {
                return Ok(false);
            };
            if stored.len() != proofs.len()
                || stored.iter().any(|p| {
                    p.state != State::Pending
                        || p.mint_url != r.mint
                        || p.unit != r.unit
                        || p.used_by_operation != Some(op)
                        || p.created_by_operation.is_some()
                        || !proofs.contains(&p.proof)
                })
            {
                return Ok(false);
            }
            if self.db.get_saga(&op).await?.is_some() || txs.iter().any(|t| t.saga_id == Some(op)) {
                return Ok(false);
            }
            let reserved = self.db.get_reserved_proofs(&op).await?;
            let reserved_proofs: Proofs = reserved.into_iter().map(|p| p.proof).collect();
            if canonical_ys(&reserved_proofs)? != r.input_ys {
                return Ok(false);
            }
            // No deletion is needed: upstream update_proofs atomically replaces
            // these same inputs with the new operation ID before creating a saga.
        }
        let w = self.wallet(&r.mint, &r.unit)?;
        let fee: u64 = w.get_proofs_fee(proofs).await?.total.into();
        if r.amount.checked_sub(fee).is_none_or(|net| net < r.price) {
            return Ok(false);
        }
        self.claim_original(r).await?;
        // A compensated ProofsPending operation has provably never sent a swap.
        // Reset this guard only after *all* no-request checks above passed.
        r.recovery_attempted = false;
        self.save(r).await?;
        self.receive_original(r).await?;
        Ok(true)
    }
    async fn matching_transaction(&self, r: &Receipt) -> anyhow::Result<Option<Transaction>> {
        let txs = self
            .db
            .list_transactions(
                Some(r.mint.clone()),
                Some(TransactionDirection::Incoming),
                Some(r.unit.clone()),
            )
            .await?;
        let matches: Vec<_> = txs
            .into_iter()
            .filter(|t| {
                let mut ys: Vec<_> = t.ys.iter().map(ToString::to_string).collect();
                ys.sort();
                ys == r.input_ys && t.quote_id.is_none() && t.status != TransactionStatus::Failed
            })
            .collect();
        ensure!(
            matches.len() <= 1,
            "ambiguous receive operations; manual reconciliation required"
        );
        Ok(matches.into_iter().next())
    }
    async fn reconcile(&self, event: &str) -> anyhow::Result<bool> {
        let Some(mut r) = self.load(event).await? else {
            return Ok(false);
        };
        if r.paid {
            return Ok(true);
        }
        if r.original.is_none() {
            return Ok(false);
        }
        let original_proofs = self.original_proofs(&r).await?;
        self.claim_original(&r).await?;
        let w = self.wallet(&r.mint, &r.unit)?;
        let sagas = self.db.get_incomplete_sagas().await?;
        let mut has_saga = false;
        for s in &sagas {
            if s.mint_url != r.mint || s.unit != r.unit {
                continue;
            }
            let OperationData::Receive(data) = &s.data else {
                continue;
            };
            let Some(token) = data.token.as_ref() else {
                continue;
            };
            let token = Token::from_str(token)?;
            let keysets = self
                .db
                .get_mint_keysets(r.mint.clone())
                .await?
                .unwrap_or_default();
            if canonical_ys(&token.proofs(&keysets)?)? != r.input_ys {
                continue;
            }
            let saga_proofs = token.proofs(&keysets)?;
            ensure!(
                saga_proofs.len() == original_proofs.len()
                    && saga_proofs.iter().all(|p| original_proofs.contains(p)),
                "CDK operation differs from original proofs"
            );
            has_saga = true;
            r.recovery_attempted = true;
            self.save(&r).await?;
            let early = matches!(
                s.state,
                WalletSagaState::Receive(ReceiveSagaState::ProofsPending)
            );
            let complete_plan = data
                .blinded_messages
                .as_ref()
                .is_some_and(|o| !o.is_empty())
                && data.counter_start.is_some()
                && data.counter_end.is_some();
            // A SwapRequested saga without its recovery plan is ambiguous.
            // Do not let CDK delete it and later turn an unverifiable Completed
            // transaction into a permit. ProofsPending is compensated locally.
            if !early && !complete_plan {
                return Ok(false);
            }
            if !early {
                // Use the read-only connector, not Wallet::check_proofs_spent
                // (which deletes spent local inputs). Pending, missing, mixed
                // or unavailable remote state must not reach CDK compensation.
                let client = HttpClient::new(r.mint.clone(), None);
                let states = client
                    .post_check_state(cdk::nuts::CheckStateRequest {
                        ys: original_proofs.ys()?,
                    })
                    .await?;
                let mut returned: Vec<_> = states.states.iter().map(|s| s.y.to_string()).collect();
                returned.sort();
                if returned != r.input_ys
                    || !(states.states.iter().all(|s| s.state == State::Spent)
                        || states.states.iter().all(|s| s.state == State::Unspent))
                {
                    return Ok(false);
                }
            }
            if let (Some(outputs), Some(start), Some(end)) =
                (&data.blinded_messages, data.counter_start, data.counter_end)
            {
                ensure!(
                    end >= start && end - start <= 4096,
                    "invalid recovery counter range"
                );
                let mut expected = Vec::new();
                for keyset in outputs.iter().map(|o| o.keyset_id).collect::<HashSet<_>>() {
                    let premints = PreMintSecrets::restore_batch(keyset, &self.seed, start, end)?;
                    for p in premints.secrets {
                        if let Some(o) = outputs
                            .iter()
                            .find(|o| o.blinded_secret == p.blinded_message.blinded_secret)
                        {
                            let y = cdk::dhke::hash_to_curve(&p.secret.to_bytes())?;
                            expected.push((y.to_string(), o.amount.into()));
                        }
                    }
                }
                ensure!(
                    expected.len() == outputs.len(),
                    "cannot identify receive recovery outputs"
                );
                r.recovery_outputs = Some(expected);
                self.save(&r).await?;
            }
        }
        if has_saga {
            // This may replay the ORIGINAL CDK operation, never the new ticket.
            // Snapshot all affected receipts before recovering this wallet.
            // One receive is allowed at a time; do not recover unrelated sagas.
            let others = sagas
                .iter()
                .filter(|s| s.mint_url == r.mint && s.unit == r.unit)
                .count();
            ensure!(
                others == 1,
                "multiple incomplete wallet operations require reconciliation"
            );
            let mut recovery_wallet = w.clone();
            recovery_wallet.set_client(Arc::new(BaseHttpClient::with_transport(
                r.mint.clone(),
                recovery_transport::RecoveryTransport::default(),
                None,
            )));
            recovery_wallet.recover_incomplete_sagas().await?;
        }
        if let Some(tx) = self.matching_transaction(&r).await? {
            if tx.status != TransactionStatus::Completed
                || u64::from(tx.amount) < r.price
                || u64::from(tx.amount) > r.amount
            {
                return Ok(false);
            }
            if let Some(expected) = &r.recovery_outputs {
                // Do not accept CDK Completed alone after a recovery attempt.
                let ys = expected
                    .iter()
                    .map(|(y, _)| y.parse())
                    .collect::<Result<Vec<_>, _>>()?;
                let stored = self.db.get_proofs_by_ys(ys).await?;
                let sum =
                    stored.iter().try_fold(0u64, |sum, p| {
                        ensure!(
                            p.mint_url == r.mint
                                && p.unit == r.unit
                                && expected.iter().any(|(y, a)| *y == p.y.to_string()
                                    && *a == u64::from(p.proof.amount)),
                            "recovered output mismatch"
                        );
                        sum.checked_add(p.proof.amount.into())
                            .ok_or_else(|| anyhow!("recovery amount overflow"))
                    })?;
                if stored.len() != expected.len() || sum < r.price || sum != u64::from(tx.amount) {
                    return Ok(false);
                }
            } else if has_saga || r.recovery_attempted {
                return Ok(false);
            }
            r.paid = true;
            r.transaction_id = Some(tx.id().to_string());
            self.save(&r).await?;
            return Ok(true);
        }
        self.continue_unstarted(&mut r, &original_proofs).await
    }
}
fn canonical_ys(proofs: &Proofs) -> anyhow::Result<Vec<String>> {
    let mut ys: Vec<_> = proofs.ys()?.iter().map(ToString::to_string).collect();
    ys.sort();
    ensure!(
        ys.windows(2).all(|p| p[0] != p[1]),
        "duplicate proof identifiers"
    );
    Ok(ys)
}

#[cfg(test)]
#[path = "cashu_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "cashu_fault_db.rs"]
mod fault_db;

#[path = "cashu_recovery_transport.rs"]
mod recovery_transport;
