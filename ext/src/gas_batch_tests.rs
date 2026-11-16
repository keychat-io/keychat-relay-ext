use super::*;
use crate::cashu::tests::{fixture, inject_database, reopen};
use std::sync::atomic::Ordering;

fn config(size: usize) -> BatchConfig {
    BatchConfig {
        enabled: true,
        size,
        ..Default::default()
    }
}
fn event(n: u64) -> String {
    format!("{n:064x}")
}
fn parsed(token: &str) -> ParsedToken {
    parse_token(token, &InputLimits::default()).unwrap()
}
async fn warm(w: &CashuWallet, url: &MintUrl, unit: &CurrencyUnit) {
    w.wallet(url, unit)
        .unwrap()
        .keysets(KeysetLoadPolicy::Refresh)
        .await
        .unwrap();
    w.initialize_batch(&config(10)).await.unwrap();
}
async fn wait_for(mut predicate: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn scheduler_wakes_on_tenth_unique_event_one_real_merged_swap() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(10);
    let worker = w.start_batch_worker(cfg.clone());
    for i in 0..9 {
        w.enqueue(event(i), parsed(&m.token(&url, 1)), 1, &cfg)
            .await
            .unwrap();
    }
    for _ in 0..10 {
        w.enqueue(event(0), parsed(&m.token(&url, 1)), 1, &cfg)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    w.enqueue(event(9), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .unwrap();
    wait_for(async || w.load(&event(9)).await.unwrap().unwrap().paid).await;
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    let txs = w.db.list_transactions(None, None, None).await.unwrap();
    assert_eq!(txs.len(), 1);
    assert_eq!(txs[0].ys.len(), 10);
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn tail_flush_29_not_30_yes_replay_keeps_age_across_restart() {
    let (w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(10);
    for i in 0..9 {
        w.enqueue_at(event(i), parsed(&m.token(&url, 1)), 1, &cfg, 100)
            .await
            .unwrap();
    }
    w.enqueue_at(event(0), parsed(&m.token(&url, 1)), 1, &cfg, 120)
        .await
        .unwrap();
    w.batch_tick(&cfg, 129).await.unwrap();
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    drop(w);
    let w = reopen(&dir, &url, CurrencyUnit::Sat).await;
    w.initialize_batch(&cfg).await.unwrap();
    assert_eq!(
        w.load(&event(0)).await.unwrap().unwrap().queued_at,
        Some(100)
    );
    w.batch_tick(&cfg, 130).await.unwrap();
    assert!(w.load(&event(0)).await.unwrap().unwrap().paid);
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn stalled_swap_does_not_delay_new_or_replayed_local_admissions_and_bounds_inflight() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = BatchConfig {
        max_pending: 2,
        ..config(1)
    };
    m.mode.store(8, Ordering::SeqCst);
    w.enqueue(event(1), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .unwrap();
    let worker = w.start_batch_worker(cfg.clone());
    wait_for(async || m.requests.load(Ordering::SeqCst) == 1).await;
    tokio::time::timeout(Duration::from_millis(500), async {
        assert_eq!(w.admission_status(&event(1)).await.unwrap(), Some(true));
        w.enqueue(event(1), parsed(&m.token(&url, 1)), 1, &cfg)
            .await
            .unwrap();
        w.enqueue(event(2), parsed(&m.token(&url, 1)), 1, &cfg)
            .await
            .unwrap();
    })
    .await
    .expect("admission waited for network gate");
    assert!(w
        .enqueue(event(3), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .is_err());
    assert_eq!(m.requests.load(Ordering::SeqCst), 1);
    m.mode.store(0, Ordering::SeqCst);
    m.release_swap.notify_one();
    wait_for(async || w.load(&event(2)).await.unwrap().unwrap().paid).await;
    assert_eq!(m.swaps.load(Ordering::SeqCst), 2);
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn unknown_keyset_is_retryable_and_canonical_claim_race_has_one_winner() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let cfg = config(10);
    assert!(w
        .enqueue(event(1), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .is_err());
    assert!(w.load(&event(1)).await.unwrap().is_none());
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let token = m.token(&url, 1);
    let a = parsed(&token).token.to_v3_string();
    let (a, b) = tokio::join!(
        w.enqueue(event(1), parsed(&a), 1, &cfg),
        w.enqueue(event(2), parsed(&token), 1, &cfg)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(w.registration.lock().await.pending.len(), 1);
    server.abort();
}

#[tokio::test]
async fn partial_admission_manifest_receive_and_finalization_recover_once() {
    use super::super::fault_db::CrashPoint::*;
    for unit in [CurrencyUnit::Sat, CurrencyUnit::custom("stamp")] {
        for point in [
            Intent,
            FirstClaim,
            AfterManifest,
            AfterAssignment,
            AfterSaga,
            AfterPendingTx,
            AfterOutputs,
            AfterCompletedTx,
            BeforePaid,
            BeforeMemberPaid,
        ] {
            let (mut w, m, url, dir, server) = fixture(unit.clone(), 0).await;
            warm(&w, &url, &unit).await;
            let cfg = config(2);
            let admission_fault = matches!(point, Intent | FirstClaim);
            let fault = if admission_fault {
                Some(inject_database(&mut w, &url, unit.clone(), point))
            } else {
                None
            };
            let original = m.token(&url, 1);
            let result = w
                .enqueue_at(event(1), parsed(&original), 1, &cfg, 100)
                .await;
            if admission_fault {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
            if !admission_fault {
                w.enqueue_at(event(2), parsed(&m.token(&url, 1)), 1, &cfg, 100)
                    .await
                    .unwrap();
            }
            let fault = fault.unwrap_or_else(|| inject_database(&mut w, &url, unit.clone(), point));
            if !admission_fault {
                let _ = w.batch_tick(&cfg, 130).await;
            }
            assert!(fault.fired.load(Ordering::SeqCst));
            drop(fault);
            drop(w);
            let w = reopen(&dir, &url, unit.clone()).await;
            w.initialize_batch(&cfg).await.unwrap();
            assert_eq!(w.admission_status(&event(1)).await.unwrap(), Some(true));
            assert!(w
                .enqueue(event(3), parsed(&original), 1, &cfg)
                .await
                .is_err());
            w.batch_tick(&cfg, 130).await.unwrap();
            assert!(w.load(&event(1)).await.unwrap().unwrap().paid);
            assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
            drop(w);
            let w = reopen(&dir, &url, unit.clone()).await;
            w.initialize_batch(&cfg).await.unwrap();
            w.batch_tick(&cfg, 1000).await.unwrap();
            assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }
}

#[tokio::test]
async fn lost_batch_response_recovers_fixed_original_without_second_swap() {
    for mode in [1, 3] {
        let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
        warm(&w, &url, &CurrencyUnit::Sat).await;
        let cfg = config(2);
        m.mode.store(mode, Ordering::SeqCst);
        for i in 0..2 {
            w.enqueue_at(event(i), parsed(&m.token(&url, 1)), 1, &cfg, 100)
                .await
                .unwrap();
        }
        w.batch_tick(&cfg, 100).await.unwrap();
        assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
        w.batch_tick(&cfg, 101).await.unwrap();
        assert_eq!(w.load(&event(0)).await.unwrap().unwrap().paid, mode == 1);
        assert_eq!(w.admission_status(&event(0)).await.unwrap(), Some(true));
        assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn definitively_bad_member_is_isolated_without_replacing_or_losing_good_payment() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(2);
    let mut bad = m.proof(1);
    bad.c = m.proof(1).c;
    let bad = Token::new(url.clone(), vec![bad], None, CurrencyUnit::Sat).to_string();
    w.enqueue(event(1), parsed(&bad), 1, &cfg).await.unwrap();
    w.enqueue(event(2), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .unwrap();
    w.batch_tick(&cfg, now()).await.unwrap();
    let bad = w.load(&event(1)).await.unwrap().unwrap();
    let good = w.load(&event(2)).await.unwrap().unwrap();
    assert!(bad.failure.is_some() && !bad.paid);
    assert!(good.paid);
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(m.requests.load(Ordering::SeqCst), 3);
    assert_eq!(w.admission_status(&event(1)).await.unwrap(), Some(true));
    w.enqueue(event(1), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .unwrap();
    w.batch_tick(&cfg, now() + 100).await.unwrap();
    assert_eq!(m.requests.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn mode_switch_and_legacy_receipts_preserve_original_semantics() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    w.pay(event(0), parsed(&m.token(&url, 1)), 1, 3000)
        .await
        .unwrap();
    assert_eq!(w.admission_status(&event(0)).await.unwrap(), Some(true));
    m.mode.store(4, Ordering::SeqCst);
    assert!(w
        .pay(event(1), parsed(&m.token(&url, 1)), 1, 3000)
        .await
        .is_err());
    assert_eq!(w.admission_status(&event(1)).await.unwrap(), Some(false));
    assert!(w
        .enqueue(event(1), parsed(&m.token(&url, 1)), 1, &config(10))
        .await
        .is_err());
    w.enqueue(event(2), parsed(&m.token(&url, 1)), 1, &config(10))
        .await
        .unwrap();
    assert!(w.initialize_batch(&BatchConfig::default()).await.is_err());
    assert_eq!(w.admission_status(&event(2)).await.unwrap(), Some(true));
    server.abort();
}
#[tokio::test]
async fn singleton_rejection_finishes_and_disabling_after_fallback_is_safe() {
    let (w, m, url, dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(1);
    let mut bad = m.proof(1);
    bad.c = m.proof(1).c;
    let bad = Token::new(url.clone(), vec![bad], None, CurrencyUnit::Sat).to_string();
    w.enqueue(event(1), parsed(&bad), 1, &cfg).await.unwrap();
    w.batch_tick(&cfg, now()).await.unwrap();
    assert!(w.load(&event(1)).await.unwrap().unwrap().failure.is_some());
    assert_eq!(m.requests.load(Ordering::SeqCst), 2);
    drop(w);
    let w = reopen(&dir, &url, CurrencyUnit::Sat).await;
    w.initialize_batch(&BatchConfig::default()).await.unwrap();
    assert_eq!(w.admission_status(&event(1)).await.unwrap(), Some(true));
    w.batch_tick(&cfg, now()).await.unwrap();
    assert_eq!(m.requests.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn definitive_local_preparation_failure_does_not_poison_good_members() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(2);
    let mut bad = m.proof(1);
    bad.dleq = Some(cdk::nuts::ProofDleq::new(
        cdk::nuts::SecretKey::generate(),
        cdk::nuts::SecretKey::generate(),
        cdk::nuts::SecretKey::generate(),
    ));
    let bad = Token::new(url.clone(), vec![bad], None, CurrencyUnit::Sat).to_string();
    w.enqueue(event(1), parsed(&bad), 1, &cfg).await.unwrap();
    w.enqueue(event(2), parsed(&m.token(&url, 1)), 1, &cfg)
        .await
        .unwrap();
    w.batch_tick(&cfg, now()).await.unwrap();
    assert!(w.load(&event(1)).await.unwrap().unwrap().failure.is_some());
    assert!(w.load(&event(2)).await.unwrap().unwrap().paid);
    assert_eq!(m.requests.load(Ordering::SeqCst), 1);
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn unknown_metadata_refresh_runs_in_worker_then_retry_admits() {
    let (w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let cfg = config(10);
    let token = m.token(&url, 1);
    assert!(w.enqueue(event(1), parsed(&token), 1, &cfg).await.is_err());
    let worker = w.start_batch_worker(cfg.clone());
    wait_for(async || w.db.get_mint_keysets(url.clone()).await.unwrap().is_some()).await;
    w.enqueue(event(1), parsed(&token), 1, &cfg).await.unwrap();
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn mint_unit_buckets_do_not_share_threshold_and_net_fee_is_per_input() {
    let (mut w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    let (_other, m2, url2, _dir2, server2) = fixture(CurrencyUnit::Sat, 0).await;
    let (_stamp, m3, url3, _dir3, server3) = fixture(CurrencyUnit::custom("stamp"), 1000).await;
    for (url, unit) in [
        (&url2, CurrencyUnit::Sat),
        (&url3, CurrencyUnit::custom("stamp")),
    ] {
        let wallet = WalletBuilder::new()
            .mint_url(url.clone())
            .unit(unit.clone())
            .seed([42; 64])
            .localstore(w.db.clone())
            .without_rate_limiting()
            .build()
            .unwrap();
        Arc::get_mut(&mut w)
            .unwrap()
            .wallets
            .insert((url.to_string(), unit.to_string()), wallet);
        w.wallet(url, &unit)
            .unwrap()
            .keysets(KeysetLoadPolicy::Refresh)
            .await
            .unwrap();
    }
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(10);
    for i in 0..9 {
        w.enqueue_at(event(i), parsed(&m.token(&url, 1)), 1, &cfg, 100)
            .await
            .unwrap();
    }
    w.enqueue_at(event(20), parsed(&m2.token(&url2, 1)), 1, &cfg, 100)
        .await
        .unwrap();
    w.enqueue_at(event(30), parsed(&m3.token(&url3, 2)), 1, &cfg, 100)
        .await
        .unwrap();
    w.batch_tick(&cfg, 129).await.unwrap();
    assert_eq!(
        m.requests.load(Ordering::SeqCst)
            + m2.requests.load(Ordering::SeqCst)
            + m3.requests.load(Ordering::SeqCst),
        0
    );
    w.enqueue_at(event(9), parsed(&m.token(&url, 1)), 1, &cfg, 100)
        .await
        .unwrap();
    w.batch_tick(&cfg, 129).await.unwrap();
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(
        m2.swaps.load(Ordering::SeqCst) + m3.swaps.load(Ordering::SeqCst),
        0
    );
    w.batch_tick(&cfg, 130).await.unwrap();
    assert_eq!(m2.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(m3.swaps.load(Ordering::SeqCst), 1);
    let transactions =
        w.db.list_transactions(Some(url3), None, None)
            .await
            .unwrap();
    assert_eq!(u64::from(transactions[0].amount), 1);
    server.abort();
    server2.abort();
    server3.abort();
}

#[tokio::test]
async fn aggregate_bounds_flush_smaller_groups_without_hiding_multiple_swaps() {
    let (mut w, m, url, _dir, server) = fixture(CurrencyUnit::Sat, 0).await;
    Arc::get_mut(&mut w).unwrap().input_limits.max_amount = 3;
    warm(&w, &url, &CurrencyUnit::Sat).await;
    let cfg = config(10);
    for i in 0..10 {
        w.enqueue_at(event(i), parsed(&m.token(&url, 1)), 1, &cfg, 100)
            .await
            .unwrap();
    }
    w.batch_tick(&cfg, 100).await.unwrap();
    assert_eq!(m.swaps.load(Ordering::SeqCst), 1);
    // Remaining seven haven't reached the ten-event threshold or age.
    w.batch_tick(&cfg, 130).await.unwrap();
    assert_eq!(m.swaps.load(Ordering::SeqCst), 4);
    for tx in w.db.list_transactions(None, None, None).await.unwrap() {
        assert!(u64::from(tx.amount) <= 3);
    }
    server.abort();
}
