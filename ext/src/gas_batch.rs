//! Gas-only durable admission and one bounded settlement scheduler.
use super::*;
use crate::config::BatchConfig;
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
pub(super) struct Registration {
    ready: bool,
    // Only unfinished admissions/manifests are indexed. History stays on disk.
    pending: BTreeMap<String, Receipt>,
    batches: BTreeMap<String, Receipt>,
    legacy: BTreeSet<String>,
    refresh: BTreeSet<(String, String)>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl CashuWallet {
    /// One local reconstruction before serving; also used after a failed KV write.
    async fn rebuild_registration(&self, state: &mut Registration) -> anyhow::Result<()> {
        state.ready = false;
        state.pending.clear();
        state.batches.clear();
        state.legacy.clear();
        let mut receipts = Vec::new();
        for event in self.db.kv_list(NS, &self.scope).await? {
            let r = self
                .load(&event)
                .await?
                .ok_or_else(|| anyhow!("missing receipt"))?;
            if r.members.is_none() {
                // Receipt-first ordering recovers a crash before any/all claims.
                self.claim_original(&r).await?;
            }
            receipts.push(r);
        }
        // The manifest is authoritative if assignment stopped halfway through.
        let unfinished: BTreeSet<_> = receipts
            .iter()
            .filter(|r| r.members.is_none() && !r.paid && r.failure.is_none())
            .map(|r| r.event.clone())
            .collect();
        let mut ownership = BTreeMap::new();
        for r in &receipts {
            if let Some(members) = &r.members {
                self.validate_batch(r).await?;
                for member in members {
                    ensure!(
                        ownership.insert(member.clone(), r.event.clone()).is_none(),
                        "event in multiple batches"
                    );
                }
                if members.iter().any(|id| unfinished.contains(id)) {
                    state.batches.insert(r.event.clone(), r.clone());
                }
            }
        }
        for mut r in receipts {
            if r.members.is_some() {
                continue;
            }
            if let Some(batch) = ownership.get(&r.event) {
                ensure!(
                    r.batch_id.as_ref().is_none_or(|b| b == batch),
                    "conflicting batch assignment"
                );
                if r.batch_id.is_none() {
                    r.batch_id = Some(batch.clone());
                    self.save(&r).await?;
                }
            } else {
                ensure!(r.batch_id.is_none(), "missing batch manifest");
            }
            if r.queued_at.is_some() && !r.paid && r.failure.is_none() {
                state.pending.insert(r.event.clone(), r);
            } else if r.queued_at.is_none() && !r.paid {
                state.legacy.insert(r.event.clone());
            }
        }
        state.ready = true;
        Ok(())
    }

    pub async fn initialize_batch(&self, config: &BatchConfig) -> anyhow::Result<()> {
        let mut state = self.registration.lock().await;
        self.rebuild_registration(&mut state).await?;
        ensure!(
            config.enabled || (state.pending.is_empty() && state.batches.is_empty()),
            "batching cannot be disabled while asynchronous collection is outstanding"
        );
        Ok(())
    }

    /// Pending async receipts replay their original permit; legacy intents do not.
    pub async fn admission_status(&self, event: &str) -> anyhow::Result<Option<bool>> {
        let mut state = self.registration.lock().await;
        if !state.ready {
            self.rebuild_registration(&mut state).await?;
        }
        Ok(self
            .load(event)
            .await?
            .map(|r| r.paid || r.queued_at.is_some()))
    }

    async fn cached_proofs(&self, parsed: &ParsedToken) -> anyhow::Result<Proofs> {
        self.wallet(&parsed.mint, &parsed.unit)?;
        let keysets = self
            .db
            .get_mint_keysets(parsed.mint.clone())
            .await?
            .ok_or_else(|| anyhow!("keyset metadata unavailable; retry after refresh"))?;
        let proofs = parsed
            .token
            .proofs(&keysets)
            .map_err(|_| anyhow!("unknown proof keyset; retry after refresh"))?;
        for proof in &proofs {
            ensure!(
                keysets
                    .iter()
                    .any(|k| k.id == proof.keyset_id && k.unit == parsed.unit),
                "proof keyset unit mismatch"
            );
        }
        Ok(proofs)
    }

    async fn cached_fee(&self, mint: &MintUrl, proofs: &Proofs) -> anyhow::Result<u64> {
        let keysets = self
            .db
            .get_mint_keysets(mint.clone())
            .await?
            .ok_or_else(|| anyhow!("missing keyset metadata"))?;
        let fees: HashMap<_, _> = keysets
            .into_iter()
            .map(|k| (k.id, k.input_fee_ppk))
            .collect();
        Ok(cdk::fees::calculate_fee(&proofs.count_by_keyset(), &fees)?
            .total
            .into())
    }

    pub(super) fn original_receipt(
        &self,
        event: String,
        mint: MintUrl,
        unit: CurrencyUnit,
        proofs: Proofs,
        amount: u64,
        price: u64,
    ) -> anyhow::Result<Receipt> {
        let ys = canonical_ys(&proofs)?;
        let fingerprint = hash(&[mint.to_string().as_bytes(), ys.join(",").as_bytes()]);
        let token = Token::new(mint.clone(), proofs, None, unit.clone()).to_string();
        let mut r = Receipt {
            event,
            fingerprint,
            mint,
            unit,
            input_ys: ys,
            amount,
            price,
            paid: false,
            transaction_id: None,
            recovery_outputs: None,
            original: None,
            recovery_attempted: false,
            queued_at: None,
            batch_id: None,
            members: None,
            failure: None,
        };
        r.original = Some(OriginalReceive {
            scope: self.scope.clone(),
            binding: self.original_binding(&r, &token),
            token,
        });
        Ok(r)
    }

    pub async fn enqueue(
        &self,
        event: String,
        parsed: ParsedToken,
        price: u64,
        config: &BatchConfig,
    ) -> anyhow::Result<()> {
        self.enqueue_at(event, parsed, price, config, now()).await
    }

    async fn enqueue_at(
        &self,
        event: String,
        parsed: ParsedToken,
        price: u64,
        config: &BatchConfig,
        accepted_at: u64,
    ) -> anyhow::Result<()> {
        ensure!(config.enabled, "batching is disabled");
        // Real gas IDs are full 32-byte hex; synthetic receipts cannot collide.
        ensure!(
            event.len() == 64 && event.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid gas event ID"
        );
        let mut state = self.registration.lock().await;
        if !state.ready {
            self.rebuild_registration(&mut state).await?;
        }
        if let Some(r) = self.load(&event).await? {
            ensure!(r.paid || r.queued_at.is_some(), "legacy payment unresolved");
            return Ok(());
        }
        let bucket = (parsed.mint.to_string(), parsed.unit.to_string());
        ensure!(
            state
                .pending
                .values()
                .filter(|r| (r.mint.to_string(), r.unit.to_string()) == bucket)
                .count()
                < config.max_pending,
            "pending payment queue is full"
        );
        let proofs = match self.cached_proofs(&parsed).await {
            Ok(proofs) => proofs,
            Err(error) => {
                state.refresh.insert(bucket);
                self.batch_wake.notify_one();
                return Err(error);
            }
        };
        let fee = self.cached_fee(&parsed.mint, &proofs).await?;
        ensure!(
            parsed
                .amount
                .checked_sub(fee)
                .is_some_and(|net| net >= price),
            "net payment amount is insufficient"
        );
        let mut r = self.original_receipt(
            event,
            parsed.mint,
            parsed.unit,
            proofs,
            parsed.amount,
            price,
        )?;
        ensure!(
            r.original.as_ref().unwrap().token.len() <= self.input_limits.max_token_bytes
                && r.input_ys.len() <= self.input_limits.max_proofs
                && r.amount <= self.input_limits.max_amount,
            "canonical admission exceeds input limits"
        );
        for y in &r.input_ys {
            let key = hash(&[r.mint.to_string().as_bytes(), y.as_bytes()]);
            ensure!(
                self.db.kv_read(NS, "proof-claims", &key).await?.is_none(),
                "proof already claimed"
            );
        }
        r.queued_at = Some(accepted_at);
        // A failed write may have committed: force reconstruction before any
        // later registration, including if this future is cancelled mid-write.
        state.ready = false;
        self.save(&r).await?;
        self.claim_original(&r).await?;
        state.pending.insert(r.event.clone(), r);
        state.ready = true;
        self.batch_wake.notify_one();
        debug!(
            accepted = 1,
            pending = state.pending.len(),
            "gas admission accepted"
        );
        Ok(())
    }

    pub(super) async fn validate_batch(&self, batch: &Receipt) -> anyhow::Result<()> {
        let members = batch
            .members
            .as_ref()
            .ok_or_else(|| anyhow!("missing batch members"))?;
        ensure!(
            batch.event.starts_with("gas_batch_") && !members.is_empty(),
            "invalid batch namespace"
        );
        let mut ids = members.clone();
        ids.sort();
        ids.dedup();
        ensure!(ids.len() == members.len(), "duplicate batch members");
        let expected = format!(
            "gas_batch_{}",
            hash(&[
                self.scope.as_bytes(),
                batch.mint.to_string().as_bytes(),
                batch.unit.to_string().as_bytes(),
                ids.join(",").as_bytes()
            ])
        );
        ensure!(batch.event == expected, "batch member binding mismatch");
        let mut ys = Vec::new();
        let mut member_proofs = Vec::new();
        let mut amount = 0u64;
        let mut price = 0u64;
        for id in members {
            let r = self
                .load(id)
                .await?
                .ok_or_else(|| anyhow!("missing batch member"))?;
            ensure!(
                r.queued_at.is_some()
                    && r.members.is_none()
                    && r.mint == batch.mint
                    && r.unit == batch.unit
                    && r.batch_id.as_ref().is_none_or(|b| b == &batch.event),
                "invalid batch member"
            );
            member_proofs.extend(self.original_proofs(&r).await?);
            for y in &r.input_ys {
                let key = hash(&[r.mint.to_string().as_bytes(), y.as_bytes()]);
                ensure!(
                    self.db.kv_read(NS, "proof-claims", &key).await?.as_deref()
                        == Some(id.as_bytes()),
                    "batch proof owner mismatch"
                );
            }
            ys.extend(r.input_ys);
            amount = amount
                .checked_add(r.amount)
                .ok_or_else(|| anyhow!("batch amount overflow"))?;
            price = price
                .checked_add(r.price)
                .ok_or_else(|| anyhow!("batch price overflow"))?;
        }
        ys.sort();
        ensure!(
            ys == batch.input_ys
                && amount == batch.amount
                && price == batch.price
                && amount <= i64::MAX as u64,
            "batch aggregate binding mismatch"
        );
        // Validate the merged original itself, independently of member metadata.
        let batch_proofs = self.original_proofs(batch).await?;
        ensure!(
            batch_proofs.len() == member_proofs.len()
                && batch_proofs.iter().all(|p| member_proofs.contains(p)),
            "batch proofs differ from member originals"
        );
        Ok(())
    }

    async fn make_batch(
        &self,
        state: &mut Registration,
        members: Vec<String>,
    ) -> anyhow::Result<String> {
        let first = state
            .pending
            .get(&members[0])
            .ok_or_else(|| anyhow!("missing admission"))?;
        let mint = first.mint.clone();
        let unit = first.unit.clone();
        let mut proofs = Vec::new();
        let mut amount = 0u64;
        let mut price = 0u64;
        for id in &members {
            let r = &state.pending[id];
            proofs.extend(self.original_proofs(r).await?);
            amount = amount
                .checked_add(r.amount)
                .ok_or_else(|| anyhow!("batch amount overflow"))?;
            price = price
                .checked_add(r.price)
                .ok_or_else(|| anyhow!("batch price overflow"))?;
        }
        ensure!(
            amount <= i64::MAX as u64,
            "batch amount exceeds SQLite range"
        );
        let fee = self.cached_fee(&mint, &proofs).await?;
        ensure!(
            amount.checked_sub(fee).is_some_and(|net| net >= price),
            "batch net amount insufficient"
        );
        let mut ids = members.clone();
        ids.sort();
        let id = format!(
            "gas_batch_{}",
            hash(&[
                self.scope.as_bytes(),
                mint.to_string().as_bytes(),
                unit.to_string().as_bytes(),
                ids.join(",").as_bytes()
            ])
        );
        let mut batch = self.original_receipt(id.clone(), mint, unit, proofs, amount, price)?;
        batch.members = Some(members.clone());
        state.ready = false;
        // Manifest first; reconstruction finishes assignments before any swap.
        self.save(&batch).await?;
        for member in members {
            let r = state.pending.get_mut(&member).unwrap();
            r.batch_id = Some(id.clone());
            self.save(r).await?;
        }
        self.validate_batch(&batch).await?;
        state.batches.insert(id.clone(), batch);
        state.ready = true;
        Ok(id)
    }

    pub(super) fn resolved_parent_transaction(&self, r: &Receipt, tx: &Transaction) -> bool {
        tx.status == TransactionStatus::Failed
            && r.batch_id.as_ref().is_some_and(|parent| {
                tx.metadata.get("keychat_event") == Some(parent)
                    && tx.metadata.get("keychat_scope") == Some(&self.scope)
            })
    }

    /// Only CDK's durable Failed + compensated operation authorizes fallback.
    /// A network error, spent response, or missing saga by itself never does.
    async fn definitively_failed(&self, r: &Receipt) -> anyhow::Result<bool> {
        if r.paid || r.recovery_outputs.is_some() {
            return Ok(false);
        }
        let txs = self
            .db
            .list_transactions(
                Some(r.mint.clone()),
                Some(TransactionDirection::Incoming),
                Some(r.unit.clone()),
            )
            .await?;
        let matching: Vec<_> = txs
            .iter()
            .filter(|t| {
                let mut ys: Vec<_> = t.ys.iter().map(ToString::to_string).collect();
                ys.sort();
                ys == r.input_ys && !self.resolved_parent_transaction(r, t)
            })
            .collect();
        if matching.len() != 1 || matching[0].status != TransactionStatus::Failed {
            return Ok(false);
        }
        let tx = matching[0];
        if tx.metadata.get("keychat_event") != Some(&r.event)
            || tx.metadata.get("keychat_scope") != Some(&self.scope)
        {
            return Ok(false);
        }
        let Some(op) = tx.saga_id else {
            return Ok(false);
        };
        if self.db.get_saga(&op).await?.is_some()
            || !self.db.get_reserved_proofs(&op).await?.is_empty()
        {
            return Ok(false);
        }
        let ys = r
            .input_ys
            .iter()
            .map(|y| y.parse())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.db.get_proofs_by_ys(ys).await?.is_empty())
    }

    async fn locally_rejected(
        &self,
        r: &Receipt,
        error: Option<&anyhow::Error>,
    ) -> anyhow::Result<bool> {
        let Some(error) = error.and_then(|e| e.downcast_ref::<cdk::Error>()) else {
            return Ok(false);
        };
        if !error.is_definitive_failure()
            || r.paid
            || r.recovery_outputs.is_some()
            || r.transaction_id.is_some()
            || !self.db.get_incomplete_sagas().await?.is_empty()
        {
            return Ok(false);
        }
        let txs = self
            .db
            .list_transactions(Some(r.mint.clone()), None, None)
            .await?;
        if txs.iter().any(|t| {
            t.ys.iter().any(|y| r.input_ys.contains(&y.to_string()))
                && !self.resolved_parent_transaction(r, t)
        }) {
            return Ok(false);
        }
        let ys = r
            .input_ys
            .iter()
            .map(|y| y.parse())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.db.get_proofs_by_ys(ys).await?.is_empty())
    }

    async fn finalize_batch(
        &self,
        state: &mut Registration,
        batch: &Receipt,
    ) -> anyhow::Result<()> {
        ensure!(batch.paid, "batch not collected");
        state.ready = false;
        // Batch.paid is durable BEFORE members. Partial finalization is resumed.
        for id in batch.members.as_ref().unwrap() {
            let mut r = self
                .load(id)
                .await?
                .ok_or_else(|| anyhow!("missing member"))?;
            r.paid = true;
            r.transaction_id = batch.transaction_id.clone();
            self.save(&r).await?;
            state.pending.remove(id);
        }
        state.batches.remove(&batch.event);
        state.ready = true;
        let amount = self
            .matching_transaction(batch)
            .await?
            .map(|tx| u64::from(tx.amount))
            .unwrap_or(0);
        info!(
            amount,
            mint=%batch.mint,
            unit=%batch.unit,
            collected = batch.members.as_ref().unwrap().len(),
            pending = state.pending.len(),
            "gas batch collected"
        );
        Ok(())
    }

    async fn settle_batch(&self, id: &str) -> anyhow::Result<()> {
        let mut batch = self
            .load(id)
            .await?
            .ok_or_else(|| anyhow!("missing batch"))?;
        self.validate_batch(&batch).await?;
        if batch.failure.is_none() && !batch.paid {
            // Reconcile also starts a truly unstarted fixed original operation.
            let result = self.reconcile(id).await;
            batch = self
                .load(id)
                .await?
                .ok_or_else(|| anyhow!("missing batch"))?;
            if !batch.paid
                && (self.definitively_failed(&batch).await?
                    || self.locally_rejected(&batch, result.as_ref().err()).await?)
            {
                batch.failure =
                    Some("definitively rejected; collecting original members separately".into());
                self.save(&batch).await?;
            } else if !batch.paid {
                result?;
                return Ok(());
            }
        }
        if batch.paid {
            let mut state = self.registration.lock().await;
            self.finalize_batch(&mut state, &batch).await?;
            return Ok(());
        }
        // The resolved failed parent remains immutable. Each child still uses
        // its own original, claims and recovery guards; never a replacement.
        for id in batch.members.as_ref().unwrap() {
            let mut r = self
                .load(id)
                .await?
                .ok_or_else(|| anyhow!("missing member"))?;
            if r.paid || r.failure.is_some() {
                self.registration.lock().await.pending.remove(id);
                continue;
            }
            let result = self.reconcile(id).await;
            r = self
                .load(id)
                .await?
                .ok_or_else(|| anyhow!("missing member"))?;
            if !r.paid
                && (self.definitively_failed(&r).await?
                    || self.locally_rejected(&r, result.as_ref().err()).await?)
            {
                r.failure = Some("original payment definitively rejected".into());
                self.save(&r).await?;
                warn!(failed = 1, "gas accepted payment collection failed");
            } else if !r.paid {
                // An uncertain child operation must finish before another starts.
                result?;
                if !self.db.get_incomplete_sagas().await?.is_empty() {
                    return Ok(());
                }
            }
            if r.paid || r.failure.is_some() {
                let pending = {
                    let mut state = self.registration.lock().await;
                    state.pending.remove(id);
                    state.pending.len()
                };
                if r.paid {
                    let amount = self
                        .matching_transaction(&r)
                        .await?
                        .map(|tx| u64::from(tx.amount))
                        .unwrap_or(0);
                    info!(collected = 1, amount, mint=%r.mint, unit=%r.unit, pending, "gas original member collected");
                }
            }
        }
        let mut state = self.registration.lock().await;
        if batch
            .members
            .as_ref()
            .unwrap()
            .iter()
            .all(|id| !state.pending.contains_key(id))
        {
            state.batches.remove(&batch.event);
        }
        Ok(())
    }

    /// One pass is also the deterministic clock injection used in tests.
    async fn batch_tick(&self, config: &BatchConfig, at: u64) -> anyhow::Result<()> {
        let (refresh, legacy, mut batches) = {
            let mut state = self.registration.lock().await;
            if !state.ready {
                self.rebuild_registration(&mut state).await?;
            }
            let mut buckets: BTreeMap<(String, String), Vec<(u64, String)>> = BTreeMap::new();
            for r in state.pending.values().filter(|r| r.batch_id.is_none()) {
                buckets
                    .entry((r.mint.to_string(), r.unit.to_string()))
                    .or_default()
                    .push((r.queued_at.unwrap(), r.event.clone()));
            }
            for members in buckets.values_mut() {
                members.sort();
                while !members.is_empty()
                    && (members.len() >= config.size
                        || at.saturating_sub(members[0].0) >= config.max_wait_seconds)
                {
                    let mut count = 0usize;
                    let mut proof_count = 0usize;
                    let mut amount = 0u64;
                    let mut bytes = 0usize;
                    for (_, id) in members.iter().take(config.size) {
                        let r = &state.pending[id];
                        let next_proofs = proof_count
                            .checked_add(r.input_ys.len())
                            .ok_or_else(|| anyhow!("proof count overflow"))?;
                        let next_amount = amount
                            .checked_add(r.amount)
                            .ok_or_else(|| anyhow!("amount overflow"))?;
                        let next_bytes = bytes
                            .checked_add(
                                r.original
                                    .as_ref()
                                    .ok_or_else(|| anyhow!("missing original"))?
                                    .token
                                    .len(),
                            )
                            .ok_or_else(|| anyhow!("token bytes overflow"))?;
                        if next_proofs > self.input_limits.max_proofs
                            || next_amount > self.input_limits.max_amount
                            || next_bytes > self.input_limits.max_token_bytes
                        {
                            break;
                        }
                        count += 1;
                        proof_count = next_proofs;
                        amount = next_amount;
                        bytes = next_bytes;
                    }
                    ensure!(count > 0, "admission exceeds aggregate input limits");
                    let selected: Vec<_> = members.drain(..count).map(|(_, id)| id).collect();
                    self.make_batch(&mut state, selected).await?;
                }
            }
            (
                std::mem::take(&mut state.refresh),
                state.legacy.clone(),
                state.batches.keys().cloned().collect::<Vec<_>>(),
            )
        };
        // Metadata refresh and all CDK work run outside local admission lock.
        let _gate = self.gate.lock().await;
        for key in refresh {
            if let Some(wallet) = self.wallets.get(&key) {
                if wallet.keysets(KeysetLoadPolicy::Refresh).await.is_err() {
                    self.registration.lock().await.refresh.insert(key);
                }
            }
        }
        for id in legacy {
            if self.reconcile(&id).await.unwrap_or(false) {
                self.registration.lock().await.legacy.remove(&id);
            }
        }
        // Prior uncertain operation first, including a fallback child. An
        // unrelated batch's unstarted guard still refuses any incomplete saga.
        let sagas = self.db.get_incomplete_sagas().await?;
        if !sagas.is_empty() {
            let mut ranked = Vec::new();
            for id in batches {
                let batch = self
                    .load(&id)
                    .await?
                    .ok_or_else(|| anyhow!("missing batch"))?;
                let mut active = false;
                for saga in &sagas {
                    if saga.mint_url != batch.mint || saga.unit != batch.unit {
                        continue;
                    }
                    if let OperationData::Receive(data) = &saga.data {
                        if let Some(token) = &data.token {
                            let keys = self
                                .db
                                .get_mint_keysets(batch.mint.clone())
                                .await?
                                .unwrap_or_default();
                            let ys = canonical_ys(&Token::from_str(token)?.proofs(&keys)?)?;
                            active |= ys.iter().all(|y| batch.input_ys.contains(y));
                        }
                    }
                }
                ranked.push((!active, id));
            }
            ranked.sort();
            batches = ranked.into_iter().map(|(_, id)| id).collect();
        }
        for id in batches {
            if self.settle_batch(&id).await.is_err() {
                warn!("gas batch collection unresolved");
            }
            if !self.db.get_incomplete_sagas().await?.is_empty() {
                break;
            }
        }
        Ok(())
    }

    pub fn start_batch_worker(
        self: &Arc<Self>,
        config: BatchConfig,
    ) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            // Request prewarm outside admission. Unknown keysets are retryable.
            if let Some(this) = weak.upgrade() {
                this.registration
                    .lock()
                    .await
                    .refresh
                    .extend(this.wallets.keys().cloned());
            }
            loop {
                let Some(this) = weak.upgrade() else {
                    break;
                };
                if this.batch_tick(&config, now()).await.is_err() {
                    warn!("gas batch scheduler requires retry");
                }
                tokio::select! {
                    _ = this.batch_wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
                // Only one worker; no detached RPC retry tasks.
                if Arc::strong_count(&this) == 1 {
                    break;
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "gas_batch_tests.rs"]
mod tests;
