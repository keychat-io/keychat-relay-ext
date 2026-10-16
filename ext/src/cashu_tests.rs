use super::*;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use cdk::{
    nuts::{
        BlindSignature, BlindedMessage, Id, KeySet, KeySetInfo, Keys, Proof, SecretKey, SwapRequest,
    },
    secret::Secret,
    Amount,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};

pub(crate) struct TestMint {
    keyset: KeySet,
    keys: BTreeMap<Amount, SecretKey>,
    spent: Mutex<HashSet<String>>,
    outputs: Mutex<HashMap<String, (BlindedMessage, BlindSignature)>>,
    swaps: AtomicUsize,
    checks: AtomicUsize,
    mode: AtomicU8, // 1: lose response; 2: reject; 3: empty restore after lost response
}
impl TestMint {
    fn new(unit: CurrencyUnit, fee: u64) -> Arc<Self> {
        let keys: BTreeMap<_, _> = (0..10)
            .map(|n| (Amount::from(1u64 << n), SecretKey::generate()))
            .collect();
        let public = Keys::new(keys.iter().map(|(a, s)| (*a, s.public_key())).collect());
        let keyset = KeySet {
            id: Id::v1_from_keys(&public),
            unit,
            active: Some(true),
            keys: public,
            input_fee_ppk: fee,
            final_expiry: None,
        };
        Arc::new(Self {
            keyset,
            keys,
            spent: Mutex::new(HashSet::new()),
            outputs: Mutex::new(HashMap::new()),
            swaps: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
            mode: AtomicU8::new(0),
        })
    }
    fn proof(&self, amount: u64) -> Proof {
        let secret = Secret::new(SecretKey::generate().to_secret_hex());
        let y = cdk::dhke::hash_to_curve(&secret.to_bytes()).unwrap();
        let c = cdk::dhke::sign_message(&self.keys[&Amount::from(amount)], &y).unwrap();
        Proof::new(amount.into(), self.keyset.id, secret, c)
    }
    pub(crate) fn token(&self, url: &MintUrl, amount: u64) -> String {
        Token::new(
            url.clone(),
            vec![self.proof(amount)],
            None,
            self.keyset.unit.clone(),
        )
        .to_string()
    }
}
async fn info() -> Json<Value> {
    Json(serde_json::to_value(cdk::nuts::MintInfo::new()).unwrap())
}
async fn keys(State(m): State<Arc<TestMint>>) -> Json<Value> {
    Json(json!({"keysets":[m.keyset]}))
}
async fn keysets(State(m): State<Arc<TestMint>>) -> Json<Value> {
    Json(
        json!({"keysets":[KeySetInfo { id:m.keyset.id, unit:m.keyset.unit.clone(), active:true, input_fee_ppk:m.keyset.input_fee_ppk, final_expiry:None }]}),
    )
}
async fn swap(
    State(m): State<Arc<TestMint>>,
    Json(req): Json<SwapRequest>,
) -> (StatusCode, Json<Value>) {
    let mode = m.mode.load(Ordering::SeqCst);
    let mut spent = m.spent.lock().await;
    let ys = canonical_ys(req.inputs()).unwrap();
    if mode == 2 || mode == 4 || mode == 7 || ys.iter().any(|y| spent.contains(y)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"code":if mode == 4 || mode == 7 {11002} else {11001},"detail":if mode == 4 { "proofs already pending" } else { "Token already spent" }}),
            ),
        );
    }
    for p in req.inputs() {
        if cdk::dhke::sign_message(&m.keys[&p.amount], &p.y().unwrap()).unwrap() != p.c {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"code":10001,"detail":"Token not verified"})),
            );
        }
    }
    let input: u64 = req.inputs().total_amount().unwrap().into();
    let output: u64 = req.outputs().iter().map(|o| u64::from(o.amount)).sum();
    assert_eq!(
        input - (req.inputs().len() as u64 * m.keyset.input_fee_ppk).div_ceil(1000),
        output
    );
    spent.extend(ys);
    m.swaps.fetch_add(1, Ordering::SeqCst);
    let mut signatures = Vec::new();
    for o in req.outputs() {
        let sig = BlindSignature {
            amount: o.amount,
            keyset_id: o.keyset_id,
            c: cdk::dhke::sign_message(&m.keys[&o.amount], &o.blinded_secret).unwrap(),
            dleq: None,
        };
        m.outputs
            .lock()
            .await
            .insert(o.blinded_secret.to_string(), (o.clone(), sig.clone()));
        signatures.push(sig);
    }
    if mode == 1 || mode == 3 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"code":99999,"detail":"simulated lost response"})),
        );
    }
    if mode == 5 {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    (StatusCode::OK, Json(json!({"signatures":signatures})))
}
async fn restore(State(m): State<Arc<TestMint>>, Json(req): Json<Value>) -> Json<Value> {
    if m.mode.load(Ordering::SeqCst) == 3 {
        return Json(json!({"outputs":[],"signatures":[]}));
    }
    let outputs: Vec<BlindedMessage> = serde_json::from_value(req["outputs"].clone()).unwrap();
    let stored = m.outputs.lock().await;
    let pairs: Vec<_> = outputs
        .iter()
        .filter_map(|o| stored.get(&o.blinded_secret.to_string()))
        .collect();
    Json(
        json!({"outputs":pairs.iter().map(|(o,_)| o).collect::<Vec<_>>(),"signatures":pairs.iter().map(|(_,s)| s).collect::<Vec<_>>()}),
    )
}
async fn checkstate(
    State(m): State<Arc<TestMint>>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mode = m.mode.load(Ordering::SeqCst);
    let call = m.checks.fetch_add(1, Ordering::SeqCst);
    if mode == 6 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"code":99999,"detail":"test checkstate unavailable"})),
        );
    }
    let spent = m.spent.lock().await;
    (
        StatusCode::OK,
        Json(
            json!({"states":req["Ys"].as_array().unwrap().iter().map(|y| json!({"Y":y,"state":if mode==4 || (mode==7 && call>0) {"PENDING"} else if spent.contains(y.as_str().unwrap()) {"SPENT"} else {"UNSPENT"}})).collect::<Vec<_>>()}),
        ),
    )
}
pub(crate) async fn fixture(
    unit: CurrencyUnit,
    fee: u64,
) -> (
    Arc<CashuWallet>,
    Arc<TestMint>,
    MintUrl,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let mint = TestMint::new(unit.clone(), fee);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: MintUrl = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let app = Router::new()
        .route("/v1/info", get(info))
        .route("/v1/keys", get(keys))
        .route("/v1/keys/:id", get(keys))
        .route("/v1/keysets", get(keysets))
        .route("/v1/swap", post(swap))
        .route("/v1/restore", post(restore))
        .route("/v1/checkstate", post(checkstate))
        .with_state(mint.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.cdk018.sqlite");
    let db: Database = Arc::new(
        cdk_sqlite::WalletSqliteDatabase::new(path.clone())
            .await
            .unwrap(),
    );
    let seed = Zeroizing::new([42; 64]); // isolated, never provisioned to a live service
    let w = WalletBuilder::new()
        .mint_url(url.clone())
        .unit(unit.clone())
        .seed(*seed)
        .localstore(db.clone())
        .without_rate_limiting()
        .build()
        .unwrap();
    let mut wallets = BTreeMap::new();
    wallets.insert((url.to_string(), unit.to_string()), w);
    let service = Arc::new(CashuWallet {
        db,
        wallets,
        gate: Mutex::new(()),
        workers: Arc::new(Semaphore::new(MAX_PAYMENT_WORKERS)),
        scope: "test-relay".into(),
        seed,
        _lock: File::create(dir.path().join("lock")).unwrap(),
    });
    (service, mint, url, dir, server)
}
fn parse(s: &str) -> ParsedToken {
    parse_token(s, &InputLimits::default()).unwrap()
}

#[tokio::test]
async fn actual_cdk_receive_sat_stamp_and_net_fees() {
    for (unit, fee, input, net) in [
        (CurrencyUnit::Sat, 0, 1, 1),
        (CurrencyUnit::custom("stamp"), 0, 1, 1),
        (CurrencyUnit::custom("stamp"), 1000, 2, 1),
    ] {
        let (w, m, url, _dir, server) = fixture(unit.clone(), fee).await;
        let token = m.token(&url, input);
        assert_eq!(
            w.pay("event".into(), parse(&token), 1, 3000).await.unwrap(),
            net
        );
        assert!(w.confirmed("event").await.unwrap());
        assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
        let tx = w.db.list_transactions(None, None, None).await.unwrap();
        assert_eq!(tx[0].unit, unit);
        assert_eq!(u64::from(tx[0].amount), net);
        server.abort();
    }
}
#[tokio::test]
async fn concurrency_different_tickets_one_event_and_proof_reuse() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let a = m.token(&url, 1);
    let b = m.token(&url, 1);
    let (ra, rb) = tokio::join!(
        w.pay("event".into(), parse(&a), 1, 3000),
        w.pay("event".into(), parse(&b), 1, 3000)
    );
    assert!(ra.is_ok() && rb.is_ok());
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    assert!(w.pay("other".into(), parse(&a), 1, 3000).await.is_err());
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(w.pay("event".into(), parse(&b), 1, 3000).await.unwrap(), 0);
    server.abort();
}
#[tokio::test]
async fn lost_response_recovers_original_outputs_without_metadata_or_new_ticket() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::custom("stamp"), 0).await;
    m.mode.store(1, Ordering::SeqCst);
    let token = m.token(&url, 1);
    assert!(w.pay("event".into(), parse(&token), 1, 3000).await.is_err());
    assert!(!w.confirmed("event").await.unwrap());
    // Simulate the CDK early crash window: no transaction metadata saved yet.
    for tx in w.db.list_transactions(None, None, None).await.unwrap() {
        w.db.remove_transaction(tx.id()).await.unwrap();
    }
    let replacement = m.token(&url, 1);
    assert_eq!(
        w.pay("event".into(), parse(&replacement), 1, 3000)
            .await
            .unwrap(),
        0
    );
    assert!(w.confirmed("event").await.unwrap());
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn spent_or_empty_restore_never_means_received() {
    for mode in [2, 3, 4] {
        let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
        m.mode.store(mode, Ordering::SeqCst);
        assert!(w
            .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
            .await
            .is_err());
        assert!(w
            .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
            .await
            .is_err());
        assert!(!w.confirmed("event").await.unwrap());
        server.abort();
    }
}
#[tokio::test]
async fn net_underpayment_and_wrong_unit_rejected_before_swap() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 1000).await;
    assert!(w
        .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
        .await
        .is_err());
    let mut wrong = parse(&m.token(&url, 2));
    wrong.unit = CurrencyUnit::custom("stamp");
    assert!(w.pay("wrong".into(), wrong, 1, 3000).await.is_err());
    assert_eq!(m.swaps.load(Ordering::SeqCst), 0);
    server.abort();
}
#[test]
fn synthetic_base64_standard_ab_limits_and_normalized_identifiers() {
    let mint = TestMint::new(CurrencyUnit::Sat, 0);
    let url = "https://example.com".parse().unwrap();
    let token = Token::new(
        url,
        vec![mint.proof(1)],
        Some("\u{ffff}\u{fffe}".into()),
        CurrencyUnit::Sat,
    );
    let a = token.to_v3_string();
    let b = token.to_string();
    let payload = base64_simd::URL_SAFE
        .decode_to_vec(&a[6..])
        .or_else(|_| base64_simd::URL_SAFE_NO_PAD.decode_to_vec(&a[6..]))
        .unwrap();
    let old = format!("cashuA{}", base64_simd::STANDARD.encode_to_string(&payload));
    assert!(old.contains('/') || old.contains('+'));
    let keysets = vec![KeySetInfo {
        id: mint.keyset.id,
        unit: CurrencyUnit::Sat,
        active: true,
        input_fee_ppk: 0,
        final_expiry: None,
    }];
    let expected = canonical_ys(&token.proofs(&keysets).unwrap()).unwrap();
    for t in [&a, &b, &old, &old.trim_end_matches('=').to_string()] {
        assert_eq!(
            canonical_ys(&parse(t).token.proofs(&keysets).unwrap()).unwrap(),
            expected
        );
    }
    let mut v: Value = serde_json::from_slice(&payload).unwrap();
    v.as_object_mut().unwrap().remove("unit");
    let no_unit = format!(
        "cashuA{}",
        base64_simd::STANDARD.encode_to_string(serde_json::to_vec(&v).unwrap())
    );
    assert_eq!(parse(&no_unit).unit, CurrencyUnit::Sat);
    let limits = InputLimits {
        max_token_bytes: 5,
        ..Default::default()
    };
    assert!(parse_token(&a, &limits).is_err());
    let mut mixed = old.clone();
    mixed.push('_');
    assert!(parse_token(&mixed, &InputLimits::default()).is_err());
    assert!(parse_token("cashuB+broken", &InputLimits::default()).is_err());
    let extra = v["token"][0].clone();
    v["token"].as_array_mut().unwrap().push(extra);
    let multi = format!(
        "cashuA{}",
        base64_simd::URL_SAFE_NO_PAD.encode_to_string(serde_json::to_vec(&v).unwrap())
    );
    assert!(parse_token(&multi, &InputLimits::default()).is_err());
}
#[test]
fn ownership_prevents_old_db_second_process_and_changed_scope() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("wallet.cdk018.sqlite");
    let p = path.to_str().unwrap();
    std::fs::write(&path, b"old database marker").unwrap();
    assert!(lock_database(p, "one").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"old database marker");
    std::fs::remove_file(&path).unwrap();
    let lock = lock_database(p, "one").unwrap();
    assert!(lock_database(p, "one").is_err());
    drop(lock);
    assert!(lock_database(p, "two").is_err());
    assert!(lock_database(p, "one").is_ok());
    std::fs::remove_file(&path).unwrap();
    assert!(lock_database(p, "one").is_err());
}

async fn reopen(dir: &tempfile::TempDir, url: &MintUrl, unit: CurrencyUnit) -> Arc<CashuWallet> {
    let db: Database = Arc::new(
        cdk_sqlite::WalletSqliteDatabase::new(dir.path().join("test.cdk018.sqlite"))
            .await
            .unwrap(),
    );
    let seed = Zeroizing::new([42; 64]);
    let wallet = WalletBuilder::new()
        .mint_url(url.clone())
        .unit(unit.clone())
        .seed(*seed)
        .localstore(db.clone())
        .without_rate_limiting()
        .build()
        .unwrap();
    let mut wallets = BTreeMap::new();
    wallets.insert((url.to_string(), unit.to_string()), wallet);
    Arc::new(CashuWallet {
        db,
        wallets,
        gate: Mutex::new(()),
        workers: Arc::new(Semaphore::new(MAX_PAYMENT_WORKERS)),
        scope: "test-relay".into(),
        seed,
        _lock: File::create(dir.path().join("lock")).unwrap(),
    })
}
#[tokio::test]
async fn persisted_receipt_and_early_saga_recover_after_restart() {
    for early in [false, true] {
        let unit = CurrencyUnit::custom("stamp");
        let (w, m, url, dir, server) = fixture(unit.clone(), 0).await;
        if early {
            m.mode.store(1, Ordering::SeqCst);
        }
        let result = w
            .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
            .await;
        if early {
            assert!(result.is_err());
            for tx in w.db.list_transactions(None, None, None).await.unwrap() {
                w.db.remove_transaction(tx.id()).await.unwrap();
            }
        } else {
            result.unwrap();
            let mut r = w.load("event").await.unwrap().unwrap();
            r.paid = false;
            r.transaction_id = None;
            w.save(&r).await.unwrap();
        }
        drop(w);
        let restarted = reopen(&dir, &url, unit).await;
        assert!(restarted.reconcile("event").await.unwrap());
        assert!(restarted.confirmed("event").await.unwrap());
        assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
#[tokio::test]
async fn one_proof_competing_events_and_changed_encoding_only_charge_once() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let b = m.token(&url, 1);
    let a = parse(&b).token.to_v3_string();
    let (first, second) = tokio::join!(
        w.pay("first".into(), parse(&a), 1, 3000),
        w.pay("second".into(), parse(&b), 1, 3000)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn intent_without_cdk_operation_does_not_spend_a_replacement_ticket() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let r = Receipt {
        event: "event".into(),
        fingerprint: "test".into(),
        mint: url.clone(),
        unit: CurrencyUnit::Sat,
        input_ys: vec![],
        amount: 1,
        price: 1,
        paid: false,
        transaction_id: None,
        recovery_outputs: None,
        original: None,
        recovery_attempted: false,
    };
    w.save(&r).await.unwrap();
    assert!(w
        .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
        .await
        .is_err());
    assert_eq!(m.swaps.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn request_timeout_reconciles_original_receive_without_replacement() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    // Warm metadata so the short deadline hits the swap response itself.
    w.wallet(&url, &CurrencyUnit::Sat)
        .unwrap()
        .keysets(KeysetLoadPolicy::Refresh)
        .await
        .unwrap();
    m.mode.store(5, Ordering::SeqCst);
    assert!(w
        .pay("event".into(), parse(&m.token(&url, 1)), 1, 50)
        .await
        .is_err());
    // The RPC timed out, but its worker continues owning the receive gate.
    assert!(
        tokio::time::timeout(Duration::from_millis(10), w.confirmed("event"))
            .await
            .is_err()
    );
    let result = w.status("event".into(), 10000).await;
    assert!(
        matches!(result, Ok(Some(true))),
        "status={:?}; accepted_swaps={}; incomplete_sagas={}",
        result,
        m.swaps.load(Ordering::SeqCst),
        w.db.get_incomplete_sagas().await.unwrap().len()
    );
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    server.abort();
}

fn inject_database(
    w: &mut Arc<CashuWallet>,
    url: &MintUrl,
    unit: CurrencyUnit,
    point: super::fault_db::CrashPoint,
) -> Arc<super::fault_db::FaultDatabase> {
    let fault = super::fault_db::FaultDatabase::new(w.db.clone(), point);
    let service = Arc::get_mut(w).unwrap();
    service.db = fault.clone();
    let wallet = WalletBuilder::new()
        .mint_url(url.clone())
        .unit(unit.clone())
        .seed(*service.seed)
        .localstore(fault.clone())
        .without_rate_limiting()
        .build()
        .unwrap();
    service
        .wallets
        .insert((url.to_string(), unit.to_string()), wallet);
    fault
}
#[tokio::test]
async fn durable_write_crashes_reopen_and_collect_only_original_once() {
    use super::fault_db::CrashPoint::*;
    for unit in [CurrencyUnit::Sat, CurrencyUnit::custom("stamp")] {
        for point in [
            Intent,
            FirstClaim,
            BeforeSaga,
            AfterSaga,
            BeforePendingTx,
            AfterPendingTx,
            AfterOutputs,
            AfterCompletedTx,
            BeforePaid,
        ] {
            let (mut w, m, url, dir, server) = fixture(unit.clone(), 0).await;
            let original = Token::new(
                url.clone(),
                vec![m.proof(1), m.proof(1)],
                None,
                unit.clone(),
            )
            .to_string();
            let replacement = m.token(&url, 2);
            let fault = inject_database(&mut w, &url, unit.clone(), point);
            assert!(w
                .pay("event".into(), parse(&original), 1, 3000)
                .await
                .is_err());
            assert!(
                fault.fired.load(Ordering::SeqCst),
                "fault point {} was not reached",
                point as u8
            );
            let r = w.load("event").await.unwrap().unwrap();
            assert!(r.original.is_some() && !r.paid);
            let original_proofs = w.original_proofs(&r).await.unwrap();
            if matches!(point, Intent | FirstClaim) {
                assert_eq!(
                    w.db.kv_list(NS, "proof-claims").await.unwrap().len(),
                    usize::from(matches!(point, FirstClaim))
                );
            }
            if matches!(point, BeforeSaga) {
                assert!(w.db.get_incomplete_sagas().await.unwrap().is_empty());
                assert_eq!(
                    w.db.get_proofs_by_ys(original_proofs.ys().unwrap())
                        .await
                        .unwrap()
                        .len(),
                    2
                );
            }
            if matches!(point, BeforePendingTx) {
                assert_eq!(w.db.get_incomplete_sagas().await.unwrap().len(), 1);
                assert!(w
                    .db
                    .list_transactions(None, None, None)
                    .await
                    .unwrap()
                    .is_empty());
            }
            drop(r);
            drop(fault);
            drop(w);
            let restarted = reopen(&dir, &url, unit.clone()).await;
            // A new ticket is supplied, but only the durable original is used.
            assert_eq!(
                restarted
                    .pay("event".into(), parse(&replacement), 1, 3000)
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(
                restarted.status("event".into(), 3000).await.unwrap(),
                Some(true)
            );
            assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
            let spent = m.spent.lock().await;
            assert!(canonical_ys(&original_proofs)
                .unwrap()
                .iter()
                .all(|y| spent.contains(y)));
            assert_eq!(spent.len(), 2);
            drop(spent);
            drop(restarted);
            let again = reopen(&dir, &url, unit.clone()).await;
            assert_eq!(
                again.status("event".into(), 3000).await.unwrap(),
                Some(true)
            );
            assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }
}
#[tokio::test]
async fn old_receipt_binding_conflict_or_changed_identity_never_collects_replacement() {
    use super::fault_db::CrashPoint::Intent;
    for mutation in 0..8 {
        let (mut w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
        let token = m.token(&url, 1);
        let fault = inject_database(&mut w, &url, CurrencyUnit::Sat, Intent);
        assert!(w.pay("event".into(), parse(&token), 1, 3000).await.is_err());
        let mut r = w.load("event").await.unwrap().unwrap();
        match mutation {
            0 => r.original = None,
            1 => r.original.as_mut().unwrap().scope = "different-scope".into(),
            2 => r.original.as_mut().unwrap().token = m.token(&url, 1),
            3 => r.event = "different-event".into(),
            4 => r.unit = CurrencyUnit::custom("stamp"),
            5 => r.mint = "https://untrusted.invalid".parse().unwrap(),
            6 => r.input_ys = vec!["mismatched-proof".into()],
            7 => {
                let key = hash(&[r.mint.to_string().as_bytes(), r.input_ys[0].as_bytes()]);
                w.db.kv_write(NS, "proof-claims", &key, b"other-event")
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        // Missing original field also exercises serde compatibility with old rows.
        let mut value = serde_json::to_value(&r).unwrap();
        if mutation == 0 {
            value.as_object_mut().unwrap().remove("original");
            value.as_object_mut().unwrap().remove("recovery_attempted");
        }
        w.db.kv_write(NS, &w.scope, "event", &serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();
        drop(r);
        drop(fault);
        drop(w);
        let restarted = reopen(&dir, &url, CurrencyUnit::Sat).await;
        for _ in 0..2 {
            assert!(!matches!(
                restarted.status("event".into(), 3000).await,
                Ok(Some(true))
            ));
        }
        assert!(restarted
            .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
            .await
            .is_err());
        assert_eq!(m.swaps.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
#[tokio::test]
async fn bad_spent_or_invalid_ticket_does_not_permanently_block_good_event() {
    for invalid_signature in [false, true] {
        let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
        let mut proof = m.proof(1);
        if invalid_signature {
            proof.c = SecretKey::generate().public_key();
        } else {
            m.spent.lock().await.insert(proof.y().unwrap().to_string());
        }
        let bad = Token::new(url.clone(), vec![proof], None, CurrencyUnit::Sat).to_string();
        assert!(w.pay("bad".into(), parse(&bad), 1, 3000).await.is_err());
        assert!(!w.confirmed("bad").await.unwrap());
        let good = m.token(&url, 1);
        assert_eq!(
            w.pay("good".into(), parse(&good), 1, 3000).await.unwrap(),
            1
        );
        assert!(w.confirmed("good").await.unwrap());
        assert!(!matches!(
            w.status("bad".into(), 3000).await,
            Ok(Some(true))
        ));
        assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn missing_recovery_plan_and_completed_without_custody_stay_denied_across_restarts() {
    use super::fault_db::CrashPoint::{AfterPendingTx, BeforePendingTx};
    for completed in [false, true] {
        let (mut w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
        let token = m.token(&url, 1);
        let fault = inject_database(
            &mut w,
            &url,
            CurrencyUnit::Sat,
            if completed {
                AfterPendingTx
            } else {
                BeforePendingTx
            },
        );
        assert!(w.pay("event".into(), parse(&token), 1, 3000).await.is_err());
        let mut r = w.load("event").await.unwrap().unwrap();
        let proofs = w.original_proofs(&r).await.unwrap();
        m.spent.lock().await.extend(canonical_ys(&proofs).unwrap());
        let mut saga = w.db.get_incomplete_sagas().await.unwrap().pop().unwrap();
        if let OperationData::Receive(data) = &mut saga.data {
            data.blinded_messages = None;
            data.counter_start = None;
            data.counter_end = None;
        }
        saga.update_state(saga.state);
        assert!(w.db.update_saga(saga.clone()).await.unwrap());
        if completed {
            // Emulate a previous recovery deleting a saga and writing Completed
            // even though no output proofs were restored.
            let mut tx =
                w.db.list_transactions(None, None, None)
                    .await
                    .unwrap()
                    .pop()
                    .unwrap();
            tx.status = TransactionStatus::Completed;
            w.db.add_transaction(tx).await.unwrap();
            w.db.update_proofs(vec![], proofs.ys().unwrap())
                .await
                .unwrap();
            w.db.delete_saga(&saga.id).await.unwrap();
            r.recovery_attempted = true;
            w.save(&r).await.unwrap();
        }
        drop(r);
        drop(fault);
        drop(w);
        for _ in 0..3 {
            let restarted = reopen(&dir, &url, CurrencyUnit::Sat).await;
            for _ in 0..2 {
                assert_eq!(
                    restarted.status("event".into(), 3000).await.unwrap(),
                    Some(false)
                );
            }
            assert!(!restarted.confirmed("event").await.unwrap());
            assert_eq!(m.swaps.load(Ordering::SeqCst), 0);
            if !completed {
                assert_eq!(restarted.db.get_incomplete_sagas().await.unwrap().len(), 1);
            }
        }
        server.abort();
    }
}
#[tokio::test]
async fn empty_restore_repeated_status_and_restarts_never_permit() {
    let (w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    m.mode.store(3, Ordering::SeqCst);
    assert!(w
        .pay("event".into(), parse(&m.token(&url, 1)), 1, 3000)
        .await
        .is_err());
    assert_eq!(w.status("event".into(), 3000).await.unwrap(), Some(false));
    drop(w);
    for _ in 0..3 {
        let restarted = reopen(&dir, &url, CurrencyUnit::Sat).await;
        for _ in 0..3 {
            assert_eq!(
                restarted.status("event".into(), 3000).await.unwrap(),
                Some(false)
            );
        }
        assert!(!restarted.confirmed("event").await.unwrap());
    }
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn pending_mint_state_keeps_original_saga_and_blocks_new_collection() {
    let (w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    m.mode.store(4, Ordering::SeqCst);
    assert!(w
        .pay("pending".into(), parse(&m.token(&url, 1)), 1, 3000)
        .await
        .is_err());
    let ids: Vec<_> =
        w.db.get_incomplete_sagas()
            .await
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
    assert_eq!(ids.len(), 1);
    assert!(w
        .pay("good".into(), parse(&m.token(&url, 1)), 1, 3000)
        .await
        .is_err());
    assert_eq!(
        w.db.get_incomplete_sagas()
            .await
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        ids
    );
    drop(w);
    let restarted = reopen(&dir, &url, CurrencyUnit::Sat).await;
    assert_eq!(
        restarted.status("pending".into(), 3000).await.unwrap(),
        Some(false)
    );
    assert_eq!(
        restarted
            .db
            .get_incomplete_sagas()
            .await
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(m.swaps.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn early_saga_with_externally_spent_inputs_never_becomes_a_permit() {
    let (mut w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let token = m.token(&url, 1);
    let fault = inject_database(
        &mut w,
        &url,
        CurrencyUnit::Sat,
        super::fault_db::CrashPoint::AfterSaga,
    );
    assert!(w.pay("event".into(), parse(&token), 1, 3000).await.is_err());
    let r = w.load("event").await.unwrap().unwrap();
    m.spent
        .lock()
        .await
        .extend(canonical_ys(&w.original_proofs(&r).await.unwrap()).unwrap());
    drop(r);
    drop(fault);
    drop(w);
    for _ in 0..3 {
        let restarted = reopen(&dir, &url, CurrencyUnit::Sat).await;
        for _ in 0..3 {
            assert!(!matches!(
                restarted.status("event".into(), 3000).await,
                Ok(Some(true))
            ));
        }
        assert!(!restarted.confirmed("event").await.unwrap());
    }
    assert_eq!(m.swaps.load(Ordering::SeqCst), 0);
    server.abort();
}
#[tokio::test]
async fn pending_after_preflight_or_unreachable_mint_preserves_the_saga() {
    for race in [false, true] {
        let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
        m.mode.store(if race { 7 } else { 1 }, Ordering::SeqCst);
        assert!(w
            .pay("original".into(), parse(&m.token(&url, 1)), 1, 3000)
            .await
            .is_err());
        if !race {
            m.mode.store(6, Ordering::SeqCst);
        }
        let ids: Vec<_> =
            w.db.get_incomplete_sagas()
                .await
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
        assert_eq!(ids.len(), 1);
        assert!(!matches!(
            w.status("original".into(), 3000).await,
            Ok(Some(true))
        ));
        assert!(w
            .pay("other".into(), parse(&m.token(&url, 1)), 1, 3000)
            .await
            .is_err());
        assert_eq!(
            w.db.get_incomplete_sagas()
                .await
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            ids
        );
        if race {
            assert!(m.checks.load(Ordering::SeqCst) >= 2);
        } else {
            m.mode.store(0, Ordering::SeqCst);
            assert_eq!(w.status("original".into(), 3000).await.unwrap(), Some(true));
            assert_eq!(
                w.pay("other".into(), parse(&m.token(&url, 1)), 1, 3000)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(m.swaps.load(Ordering::SeqCst), 2);
        }
        server.abort();
    }
}

#[tokio::test]
async fn timed_out_wallet_waiters_are_bounded_until_their_workers_finish() {
    let (w, _mint, _url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let guard = w.gate.lock().await;
    let statuses = (0..MAX_PAYMENT_WORKERS).map(|i| w.status(format!("event_{i}"), 10));
    assert!(futures::future::join_all(statuses)
        .await
        .into_iter()
        .all(|r| r.is_err()));
    assert_eq!(w.workers.available_permits(), 0);
    assert!(w
        .status("over_capacity".into(), 100)
        .await
        .unwrap_err()
        .to_string()
        .contains("queue is full"));
    drop(guard);
    tokio::time::timeout(Duration::from_secs(2), async {
        while w.workers.available_permits() != MAX_PAYMENT_WORKERS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(w.status("new_event".into(), 1000).await.unwrap(), None);
    server.abort();
}
