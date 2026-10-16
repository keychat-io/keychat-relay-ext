//! Test-only decorator: fail exactly at a durable-write boundary, then reopen
//! the real SQLite database with the normal upstream adapter. No CDK fork.
use super::Database as DynDatabase;
use cdk_common::{
    bitcoin::bip32::DerivationPath,
    database::{Error, WalletDatabase},
    mint_url::MintUrl,
    nuts::{
        CurrencyUnit, Id, KeySet, KeySetInfo, Keys, MintInfo, PublicKey, SpendingConditions, State,
    },
    wallet::{
        self, MintQuote as WalletMintQuote, ProofInfo, Transaction, TransactionDirection,
        TransactionId, TransactionStatus,
    },
};
use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum CrashPoint {
    Intent = 1,
    FirstClaim,
    BeforeSaga,
    AfterSaga,
    BeforePendingTx,
    AfterPendingTx,
    AfterOutputs,
    AfterCompletedTx,
    BeforePaid,
}
pub struct FaultDatabase {
    inner: DynDatabase,
    point: AtomicU8,
    pub fired: AtomicBool,
}
impl fmt::Debug for FaultDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FaultDatabase")
    }
}
impl FaultDatabase {
    pub fn new(inner: DynDatabase, point: CrashPoint) -> Arc<Self> {
        Arc::new(Self {
            inner,
            point: AtomicU8::new(point as u8),
            fired: AtomicBool::new(false),
        })
    }
    fn hit(&self, p: CrashPoint) -> bool {
        let hit = self
            .point
            .compare_exchange(p as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if hit {
            self.fired.store(true, Ordering::SeqCst);
        }
        hit
    }
    fn crash() -> Error {
        Error::Internal("injected crash boundary".into())
    }
}
#[tonic::async_trait]
impl WalletDatabase<Error> for FaultDatabase {
    async fn get_mint(&self, mint_url: MintUrl) -> Result<Option<MintInfo>, Error> {
        self.inner.get_mint(mint_url).await
    }
    async fn get_mints(&self) -> Result<HashMap<MintUrl, Option<MintInfo>>, Error> {
        self.inner.get_mints().await
    }
    async fn get_mint_keysets(&self, mint_url: MintUrl) -> Result<Option<Vec<KeySetInfo>>, Error> {
        self.inner.get_mint_keysets(mint_url).await
    }
    async fn get_keyset_by_id(&self, keyset_id: &Id) -> Result<Option<KeySetInfo>, Error> {
        self.inner.get_keyset_by_id(keyset_id).await
    }
    async fn get_mint_quote(&self, quote_id: &str) -> Result<Option<WalletMintQuote>, Error> {
        self.inner.get_mint_quote(quote_id).await
    }
    async fn get_mint_quotes(&self) -> Result<Vec<WalletMintQuote>, Error> {
        self.inner.get_mint_quotes().await
    }
    async fn get_unissued_mint_quotes(&self) -> Result<Vec<WalletMintQuote>, Error> {
        self.inner.get_unissued_mint_quotes().await
    }
    async fn get_melt_quote(&self, quote_id: &str) -> Result<Option<wallet::MeltQuote>, Error> {
        self.inner.get_melt_quote(quote_id).await
    }
    async fn get_melt_quotes(&self) -> Result<Vec<wallet::MeltQuote>, Error> {
        self.inner.get_melt_quotes().await
    }
    async fn get_keys(&self, id: &Id) -> Result<Option<Keys>, Error> {
        self.inner.get_keys(id).await
    }
    async fn get_proofs(
        &self,
        mint_url: Option<MintUrl>,
        unit: Option<CurrencyUnit>,
        state: Option<Vec<State>>,
        spending_conditions: Option<Vec<SpendingConditions>>,
    ) -> Result<Vec<ProofInfo>, Error> {
        self.inner
            .get_proofs(mint_url, unit, state, spending_conditions)
            .await
    }
    async fn get_proofs_by_ys(&self, ys: Vec<PublicKey>) -> Result<Vec<ProofInfo>, Error> {
        self.inner.get_proofs_by_ys(ys).await
    }
    async fn get_balance(
        &self,
        mint_url: Option<MintUrl>,
        unit: Option<CurrencyUnit>,
        state: Option<Vec<State>>,
    ) -> Result<u64, Error> {
        self.inner.get_balance(mint_url, unit, state).await
    }
    async fn get_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, Error> {
        self.inner.get_transaction(transaction_id).await
    }
    async fn list_transactions(
        &self,
        mint_url: Option<MintUrl>,
        direction: Option<TransactionDirection>,
        unit: Option<CurrencyUnit>,
    ) -> Result<Vec<Transaction>, Error> {
        self.inner
            .list_transactions(mint_url, direction, unit)
            .await
    }
    async fn update_proofs(
        &self,
        added: Vec<ProofInfo>,
        removed_ys: Vec<PublicKey>,
    ) -> Result<(), Error> {
        let outputs = !removed_ys.is_empty() && added.iter().any(|p| p.state == State::Unspent);
        self.inner.update_proofs(added, removed_ys).await?;
        if outputs && self.hit(CrashPoint::AfterOutputs) {
            return Err(Self::crash());
        }
        Ok(())
    }
    async fn update_proofs_state(&self, ys: Vec<PublicKey>, state: State) -> Result<(), Error> {
        self.inner.update_proofs_state(ys, state).await
    }
    async fn add_transaction(&self, transaction: Transaction) -> Result<(), Error> {
        let pending = transaction.status == TransactionStatus::Pending;
        let completed = transaction.status == TransactionStatus::Completed;
        if pending && self.hit(CrashPoint::BeforePendingTx) {
            return Err(Self::crash());
        }
        self.inner.add_transaction(transaction).await?;
        if (pending && self.hit(CrashPoint::AfterPendingTx))
            || (completed && self.hit(CrashPoint::AfterCompletedTx))
        {
            return Err(Self::crash());
        }
        Ok(())
    }
    async fn update_mint_url(
        &self,
        old_mint_url: MintUrl,
        new_mint_url: MintUrl,
    ) -> Result<(), Error> {
        self.inner.update_mint_url(old_mint_url, new_mint_url).await
    }
    async fn increment_keyset_counter(&self, keyset_id: &Id, count: u32) -> Result<u32, Error> {
        self.inner.increment_keyset_counter(keyset_id, count).await
    }
    async fn increment_derivation_counter(
        &self,
        namespace: &str,
        count: u32,
    ) -> Result<u32, Error> {
        self.inner
            .increment_derivation_counter(namespace, count)
            .await
    }
    async fn add_mint(&self, mint_url: MintUrl, mint_info: Option<MintInfo>) -> Result<(), Error> {
        self.inner.add_mint(mint_url, mint_info).await
    }
    async fn remove_mint(&self, mint_url: MintUrl) -> Result<(), Error> {
        self.inner.remove_mint(mint_url).await
    }
    async fn add_mint_keysets(
        &self,
        mint_url: MintUrl,
        keysets: Vec<KeySetInfo>,
    ) -> Result<(), Error> {
        self.inner.add_mint_keysets(mint_url, keysets).await
    }
    async fn add_mint_quote(&self, quote: WalletMintQuote) -> Result<(), Error> {
        self.inner.add_mint_quote(quote).await
    }
    async fn remove_mint_quote(&self, quote_id: &str) -> Result<(), Error> {
        self.inner.remove_mint_quote(quote_id).await
    }
    async fn add_melt_quote(&self, quote: wallet::MeltQuote) -> Result<(), Error> {
        self.inner.add_melt_quote(quote).await
    }
    async fn remove_melt_quote(&self, quote_id: &str) -> Result<(), Error> {
        self.inner.remove_melt_quote(quote_id).await
    }
    async fn add_keys(&self, keyset: KeySet) -> Result<(), Error> {
        self.inner.add_keys(keyset).await
    }
    async fn remove_keys(&self, id: &Id) -> Result<(), Error> {
        self.inner.remove_keys(id).await
    }
    async fn remove_transaction(&self, transaction_id: TransactionId) -> Result<(), Error> {
        self.inner.remove_transaction(transaction_id).await
    }
    async fn add_saga(&self, saga: wallet::WalletSaga) -> Result<(), Error> {
        if self.hit(CrashPoint::BeforeSaga) {
            return Err(Self::crash());
        }
        self.inner.add_saga(saga).await?;
        if self.hit(CrashPoint::AfterSaga) {
            return Err(Self::crash());
        }
        Ok(())
    }
    async fn get_saga(&self, id: &uuid::Uuid) -> Result<Option<wallet::WalletSaga>, Error> {
        self.inner.get_saga(id).await
    }
    async fn update_saga(&self, saga: wallet::WalletSaga) -> Result<bool, Error> {
        self.inner.update_saga(saga).await
    }
    async fn delete_saga(&self, id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.delete_saga(id).await
    }
    async fn get_incomplete_sagas(&self) -> Result<Vec<wallet::WalletSaga>, Error> {
        self.inner.get_incomplete_sagas().await
    }
    async fn reserve_proofs(
        &self,
        ys: Vec<PublicKey>,
        operation_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        self.inner.reserve_proofs(ys, operation_id).await
    }
    async fn release_proofs(&self, operation_id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.release_proofs(operation_id).await
    }
    async fn get_reserved_proofs(
        &self,
        operation_id: &uuid::Uuid,
    ) -> Result<Vec<ProofInfo>, Error> {
        self.inner.get_reserved_proofs(operation_id).await
    }
    async fn reserve_melt_quote(
        &self,
        quote_id: &str,
        operation_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        self.inner.reserve_melt_quote(quote_id, operation_id).await
    }
    async fn release_melt_quote(&self, operation_id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.release_melt_quote(operation_id).await
    }
    async fn reserve_mint_quote(
        &self,
        quote_id: &str,
        operation_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        self.inner.reserve_mint_quote(quote_id, operation_id).await
    }
    async fn release_mint_quote(&self, operation_id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.release_mint_quote(operation_id).await
    }
    async fn kv_read(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.inner
            .kv_read(primary_namespace, secondary_namespace, key)
            .await
    }
    async fn kv_list(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
    ) -> Result<Vec<String>, Error> {
        self.inner
            .kv_list(primary_namespace, secondary_namespace)
            .await
    }
    async fn kv_write(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
        value: &[u8],
    ) -> Result<(), Error> {
        if primary_namespace == super::NS && secondary_namespace != "proof-claims" {
            let receipt: super::Receipt = serde_json::from_slice(value)?;
            if receipt.paid && self.hit(CrashPoint::BeforePaid) {
                return Err(Self::crash());
            }
        }
        self.inner
            .kv_write(primary_namespace, secondary_namespace, key, value)
            .await?;
        if primary_namespace == super::NS
            && ((secondary_namespace == "proof-claims" && self.hit(CrashPoint::FirstClaim))
                || (secondary_namespace != "proof-claims" && self.hit(CrashPoint::Intent)))
        {
            return Err(Self::crash());
        }
        Ok(())
    }
    async fn kv_remove(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
    ) -> Result<(), Error> {
        self.inner
            .kv_remove(primary_namespace, secondary_namespace, key)
            .await
    }
    async fn add_p2pk_key(
        &self,
        pubkey: &PublicKey,
        derivation_path: DerivationPath,
        derivation_index: u32,
    ) -> Result<(), Error> {
        self.inner
            .add_p2pk_key(pubkey, derivation_path, derivation_index)
            .await
    }
    async fn get_p2pk_key(
        &self,
        pubkey: &PublicKey,
    ) -> Result<Option<wallet::P2PKSigningKey>, Error> {
        self.inner.get_p2pk_key(pubkey).await
    }
    async fn list_p2pk_keys(&self) -> Result<Vec<wallet::P2PKSigningKey>, Error> {
        self.inner.list_p2pk_keys().await
    }
    async fn latest_p2pk(&self) -> Result<Option<wallet::P2PKSigningKey>, Error> {
        self.inner.latest_p2pk().await
    }
}
