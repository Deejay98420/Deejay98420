//! V1.2.1 regressions for the mainnet defects: version-1 transactions, HTTP 429 / Retry-After and
//! the shared request scheduler, bounded queues and budgets, duplicate (log + self-CPI) events,
//! multi-trade transactions, token creation in conservation checks, fee reconciliation, and
//! Pass / Fail / Inconclusive outcomes.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use sniper_core::decode::decode_raw;
use sniper_core::decode::pumpfun::PumpEvent;
use sniper_core::fixtures::{wallet, Curve};
use sniper_core::rpc::{get_transaction_params, parse_retry_after, RpcClient, RpcError, SchedulerConfig};
use sniper_core::stream::ws::parse_tx_object;
use sniper_core::txbuild::{self, CurveAccounts, Emission, SynthTx};
use sniper_core::txmodel::{structure_problems, TxModel, TxVersion};
use sniper_core::types::{Commitment, Pubkey};
use sniper_core::verify::{summarize, verify_tx, Outcome};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const S: i64 = 1_000_000;

fn outcome(v: &sniper_core::verify::TxVerification, name: &str) -> Vec<Outcome> {
    v.checks.iter().filter(|c| c.name == name).map(|c| c.outcome).collect()
}

// ---------------------------------------------------------------------------------------------
// transaction versions

#[test]
fn get_transaction_requests_max_supported_version_1_and_versions_are_recorded() {
    assert_eq!(sniper_core::rpc::DEFAULT_MAX_TX_VERSION, 1);
    let p = get_transaction_params("sig", "confirmed", 1);
    assert_eq!(p[1]["maxSupportedTransactionVersion"], 1);
    assert_eq!(p[1]["encoding"], "jsonParsed");
    for (v, label) in [(json!("legacy"), "legacy"), (json!(0), "0"), (json!(1), "1")] {
        let mut c = Curve::new("V");
        let acc = CurveAccounts::of(&c);
        let PumpEvent::Trade(t) = c.buy(wallet("v"), 100_000_000, 0).unwrap() else { panic!() };
        let tx = txbuild::pump_trade_tx(&acc, &t, 900_000_000_000_000, Emission::Both, v, "vsig");
        assert!(structure_problems(&tx).is_empty(), "{:?}", structure_problems(&tx));
        let m = TxModel::parse(&tx, "vsig").unwrap();
        assert_eq!(m.version.label(), label);
        let r = verify_tx(&tx, "vsig");
        assert_eq!(r.version, label);
        assert!(r.passed, "{label}: {:#?}", r.checks);
    }
    assert_eq!(TxVersion::parse(&json!({})), TxVersion::Absent);
}

#[test]
fn lookup_table_accounts_are_part_of_the_account_list() {
    let mut c = Curve::new("ALT");
    let acc = CurveAccounts::of(&c);
    let PumpEvent::Trade(t) = c.buy(wallet("alt"), 100_000_000, 0).unwrap() else { panic!() };
    let mut tx = SynthTx::new(&t.user.to_string(), json!(0));
    tx.loaded = vec![acc.fee_recipient.clone(), acc.creator_vault.clone()];
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &t.user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade(&mut tx, &acc, &PumpEvent::Trade(t.clone()), Emission::Both);
    let tx = tx.build("alt", 1);
    assert!(tx["transaction"]["message"]["accountKeys"].as_array().unwrap().iter().all(|k| k["pubkey"] != json!(acc.fee_recipient)));
    let m = TxModel::parse(&tx, "alt").unwrap();
    assert_eq!(m.lamport_delta(&acc.fee_recipient), Some(t.fee.unwrap() as i128), "fee recipient resolved via loadedAddresses");
    let v = verify_tx(&tx, "alt");
    assert_eq!(outcome(&v, "trade.protocol_fee_observed"), vec![Outcome::Pass], "{:#?}", v.checks);
}

#[test]
fn structural_problems_are_reported_not_ignored() {
    assert!(!structure_problems(&Value::Null).is_empty());
    let p = structure_problems(&json!({"meta": {"preBalances": [1], "postBalances": [1, 2]}, "transaction": {"message": {"accountKeys": ["a"]}}}));
    assert!(p.iter().any(|x| x.contains("postBalances")), "{p:?}");
    assert!(p.iter().any(|x| x.contains("logMessages")), "{p:?}");
    let v = verify_tx(&json!({"slot": 1}), "x");
    assert!(!v.passed);
    assert_eq!(v.checks[0].name, "parse");
}

// ---------------------------------------------------------------------------------------------
// duplicate events, multiple trades, token creation, fees

#[test]
fn log_and_cpi_copies_of_one_trade_count_once_doubled_amount_regression() {
    let mut c = Curve::new("DUP");
    let acc = CurveAccounts::of(&c);
    let PumpEvent::Trade(t) = c.buy(wallet("dup"), 250_000_000, 0).unwrap() else { panic!() };
    for how in [Emission::LogOnly, Emission::CpiOnly, Emission::Both] {
        let tx = txbuild::pump_trade_tx(&acc, &t, 900_000_000_000_000, how, json!(0), "dup");
        let raw = parse_tx_object(&tx, Some("dup"), 1, "t", Commitment::Confirmed, 0).unwrap();
        let out = decode_raw(&raw);
        assert_eq!(out.events.len(), 1, "{how:?}: exactly one trade");
        assert_eq!(out.duplicates_merged, u32::from(how == Emission::Both));
        let p = &out.events[0].provenance;
        assert_eq!(p.outer_ix, Some(1), "{how:?}: emitted under top-level instruction 1");
        assert_eq!(p.channel, match how { Emission::LogOnly => "log", Emission::CpiOnly => "cpi", Emission::Both => "log+cpi" });
        assert!(out.events[0].event_id("dup").starts_with("dup:1:"));
        let v = verify_tx(&tx, "dup");
        assert!(v.passed, "{how:?}: {:#?}", v.checks);
        // the user's token account received exactly one trade's amount (not 2×)
        assert_eq!(outcome(&v, "trade.user_token_delta"), vec![Outcome::Pass]);
    }
}

#[test]
fn duplicate_merge_requires_same_instruction_and_payload_so_repeated_trades_survive() {
    // two genuine buys by the same user in one transaction, each emitted through both channels
    let mut c = Curve::new("MULTI");
    let acc = CurveAccounts::of(&c);
    let user = wallet("multi");
    let PumpEvent::Trade(a) = c.buy(user, 200_000_000, 0).unwrap() else { panic!() };
    let PumpEvent::Trade(b) = c.buy(user, 200_000_000, 0).unwrap() else { panic!() };
    let mut tx = SynthTx::new(&user.to_string(), json!(1));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade(&mut tx, &acc, &PumpEvent::Trade(a.clone()), Emission::Both);
    txbuild::add_pump_trade(&mut tx, &acc, &PumpEvent::Trade(b.clone()), Emission::Both);
    let tx = tx.build("multi", 1);
    let raw = parse_tx_object(&tx, Some("multi"), 1, "t", Commitment::Confirmed, 0).unwrap();
    let out = decode_raw(&raw);
    assert_eq!(out.events.len(), 2, "both genuine trades kept");
    assert_eq!(out.duplicates_merged, 2);
    let ids: std::collections::HashSet<String> = out.events.iter().map(|d| d.event_id("multi")).collect();
    assert_eq!(ids.len(), 2, "distinct event ids");
    let v = verify_tx(&tx, "multi");
    assert!(v.passed, "{:#?}", v.checks);
    // shared accounts are checked against the SUM of both trades
    let d = v.checks.iter().find(|c| c.name == "trade.user_token_delta").unwrap();
    assert!(d.detail.contains(&format!("{}", a.token_amount + b.token_amount)), "{}", d.detail);
    assert!(d.detail.contains("2 events share"));
}

#[test]
fn token_creation_is_minted_supply_not_a_conservation_violation() {
    let mut c = Curve::new("LAUNCH");
    let acc = CurveAccounts::of(&c);
    let create = c.create("Launch", "LCH", 0);
    let creator = c.creator;
    let PumpEvent::Trade(t) = c.buy(creator, 500_000_000, 0).unwrap() else { panic!() };
    let mut tx = SynthTx::new(&creator.to_string(), json!(0));
    txbuild::add_pump_create(&mut tx, &acc, &create, Emission::Both);
    tx.token(&txbuild::user_ata(&creator.to_string(), &acc.mint), &acc.mint, &creator.to_string(), None, Some(0));
    txbuild::add_pump_trade(&mut tx, &acc, &PumpEvent::Trade(t.clone()), Emission::Both);
    let tx = tx.build("launch", 1);
    let v = verify_tx(&tx, "launch");
    assert!(v.passed, "{:#?}", v.checks);
    assert_eq!(v.classification, vec!["launch", "trade_buy"]);
    // 1e15 raw units minted into the curve vault are explained by the parsed mintTo
    assert!(outcome(&v, "token.mint_conservation").iter().all(|o| *o == Outcome::Pass));
    assert_eq!(outcome(&v, "create.minted_supply"), vec![Outcome::Pass]);
    assert_eq!(outcome(&v, "trade.curve_vault_delta"), vec![Outcome::Pass]);
    // the curve account was created in this tx: its rent deposit is explained by createAccount
    assert_eq!(outcome(&v, "trade.sol_amount"), vec![Outcome::Pass]);
    // without the parsed mintTo, conservation can no longer be explained → FAIL, never a silent pass
    let mut stripped = tx.clone();
    for g in stripped["meta"]["innerInstructions"].as_array_mut().unwrap() {
        g["instructions"].as_array_mut().unwrap().retain(|ix| ix["parsed"]["type"] != "mintTo");
    }
    let v = verify_tx(&stripped, "launch");
    assert!(!v.passed);
    assert!(outcome(&v, "token.mint_conservation").contains(&Outcome::Fail));
}

#[test]
fn fee_reconciliation_uses_observed_transfers_and_sdk_formula() {
    let mut c = Curve::new("FEE");
    let acc = CurveAccounts::of(&c);
    let PumpEvent::Trade(t) = c.buy(wallet("fee"), 400_000_000, 0).unwrap() else { panic!() };
    let PumpEvent::Trade(s) = c.sell(wallet("fee"), t.token_amount / 3, S).unwrap() else { panic!() };
    for tr in [&t, &s] {
        let v = verify_tx(&txbuild::pump_trade_tx(&acc, tr, 900_000_000_000_000, Emission::Both, json!(0), "f"), "f");
        assert_eq!(outcome(&v, "trade.protocol_fee_observed"), vec![Outcome::Pass], "{:#?}", v.checks);
        assert_eq!(outcome(&v, "trade.creator_fee_observed"), vec![Outcome::Pass]);
        assert_eq!(outcome(&v, "trade.protocol_fee_formula"), vec![Outcome::Pass]);
        assert_eq!(v.fee_hypotheses, vec!["protocol_fee_to_fee_recipient"]);
    }
    // event fee that is not ceil(sol × bps) → formula FAIL with the implied rate in the detail
    let mut bad = t.clone();
    bad.fee = Some(bad.fee.unwrap() - 1);
    let v = verify_tx(&txbuild::pump_trade_tx(&acc, &bad, 900_000_000_000_000, Emission::Both, json!(0), "f"), "f");
    let f = v.checks.iter().find(|c| c.name == "trade.protocol_fee_formula").unwrap();
    assert_eq!(f.outcome, Outcome::Fail);
    assert!(f.detail.contains("implied"), "{}", f.detail);
    // event says fee X but the recipient received Y → observed FAIL
    let mut effects = t.clone();
    effects.fee = Some(t.fee.unwrap() + 7);
    let mut tx = SynthTx::new(&t.user.to_string(), json!(0));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &t.user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade_as(&mut tx, &acc, &effects, &PumpEvent::Trade(t.clone()), Emission::Both);
    let v = verify_tx(&tx.build("f2", 1), "f2");
    assert_eq!(outcome(&v, "trade.protocol_fee_observed"), vec![Outcome::Fail], "{:#?}", v.checks);
}

#[test]
fn missing_evidence_is_inconclusive_and_never_verifies_a_venue() {
    let mut c = Curve::new("INC");
    let acc = CurveAccounts::of(&c);
    let mut results = Vec::new();
    for i in 0..6 {
        let PumpEvent::Trade(t) = (if i < 4 { c.buy(wallet(&format!("i{i}")), 100_000_000, 0) } else { c.sell(wallet(&format!("i{}", i - 4)), 1_000_000, 0) }).unwrap() else { panic!() };
        let mut tx = txbuild::pump_trade_tx(&acc, &t, 900_000_000_000_000, Emission::LogOnly, json!(0), "inc");
        tx["meta"]["innerInstructions"] = Value::Null; // node did not record CPIs
        let v = verify_tx(&tx, "inc");
        assert!(outcome(&v, "trade.token_transfer").contains(&Outcome::Inconclusive), "{:#?}", v.checks);
        assert!(!v.checks.iter().any(|c| c.hard && c.outcome == Outcome::Fail), "missing evidence is not a failure: {:#?}", v.checks);
        results.push(v);
    }
    let s = summarize(&results, 5);
    let r = &s.venues["pump_bonding_curve"];
    assert_eq!(r.status, "insufficient_evidence");
    assert_eq!(r.events_fully_verified, 0);
    assert_eq!(r.events_inconclusive, 6);
    assert!(r.inconclusive_by_check.contains_key("trade.token_transfer"));
    assert!(!s.venues_mainnet_verified["pump_bonding_curve"]);
}

#[test]
fn wrong_direction_or_account_mismatch_fails_the_instruction_check() {
    let mut c = Curve::new("DIR");
    let acc = CurveAccounts::of(&c);
    let PumpEvent::Trade(t) = c.buy(wallet("dir"), 100_000_000, 0).unwrap() else { panic!() };
    let mut flipped = t.clone();
    flipped.is_buy = false;
    let mut tx = SynthTx::new(&t.user.to_string(), json!(0));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &t.user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade_as(&mut tx, &acc, &t, &PumpEvent::Trade(flipped), Emission::CpiOnly);
    let v = verify_tx(&tx.build("dir", 1), "dir");
    assert_eq!(outcome(&v, "trade.instruction"), vec![Outcome::Fail]);
    assert!(!v.passed);
}

// ---------------------------------------------------------------------------------------------
// shared request scheduler

#[derive(Clone)]
struct Mock {
    hits: Arc<AtomicU64>,
    fail_first: u64,
    retry_after: Option<&'static str>,
    delay_ms: u64,
}

async fn handler(State(m): State<Mock>, Json(req): Json<Value>) -> impl IntoResponse {
    let n = m.hits.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(m.delay_ms)).await;
    if n < m.fail_first {
        let mut h = HeaderMap::new();
        if let Some(ra) = m.retry_after {
            h.insert("retry-after", ra.parse().unwrap());
        }
        return (StatusCode::TOO_MANY_REQUESTS, h, Json(json!({"error": "rate limited"}))).into_response();
    }
    if req["method"] == "getTransaction" && req["params"][1]["maxSupportedTransactionVersion"] != 1 {
        return Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32015,"message":"Transaction version (1) is not supported"}})).into_response();
    }
    Json(json!({"jsonrpc":"2.0","id":1,"result": req["params"][0].clone()})).into_response()
}

async fn mock(fail_first: u64, retry_after: Option<&'static str>, delay_ms: u64) -> (String, Arc<AtomicU64>) {
    let hits = Arc::new(AtomicU64::new(0));
    let app = Router::new().route("/", post(handler)).with_state(Mock { hits: hits.clone(), fail_first, retry_after, delay_ms });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/?api-key=SCHEDSECRET", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, hits)
}

fn cfg() -> SchedulerConfig {
    SchedulerConfig { rate_per_s: 100.0, burst: 100, base_backoff: Duration::from_millis(50), max_backoff: Duration::from_millis(400), ..SchedulerConfig::default() }
}

#[tokio::test]
async fn http_429_honours_retry_after_then_succeeds() {
    assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
    assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    let (url, hits) = mock(2, Some("1"), 0).await;
    let rpc = RpcClient::with_config(vec![url], cfg()).unwrap();
    let t0 = Instant::now();
    let r = rpc.call("getSlot", json!(["ok"])).await.unwrap();
    assert_eq!(r, json!("ok"));
    assert!(t0.elapsed() >= Duration::from_millis(1_900), "two Retry-After: 1 cooldowns honoured ({:?})", t0.elapsed());
    let st = rpc.scheduler_stats();
    assert_eq!((st.http_429, st.retries, st.cooldowns, st.ok, hits.load(Ordering::SeqCst)), (2, 2, 2, 1, 3));
    let ep = &rpc.stats()[0];
    assert!(!ep.host.contains("SCHEDSECRET"));
}

#[tokio::test]
async fn persistent_429_gives_up_after_bounded_retries_with_backoff() {
    let (url, hits) = mock(u64::MAX, None, 0).await;
    let rpc = RpcClient::with_config(vec![url], SchedulerConfig { max_retries: 3, ..cfg() }).unwrap();
    let e = rpc.call("getSlot", json!([1])).await.unwrap_err();
    assert!(matches!(e, RpcError::RateLimited(3)), "{e}");
    assert_eq!(e.class(), "http_429");
    assert_eq!(hits.load(Ordering::SeqCst), 4, "1 attempt + 3 retries, never unbounded");
    // the cooldown applies to every caller of the shared client, not just the failing one
    assert!(rpc.scheduler_stats().cooldowns >= 3);
}

#[tokio::test]
async fn version_flag_is_sent_and_minus_32015_is_classified() {
    let (url, _) = mock(0, None, 0).await;
    let rpc = RpcClient::with_config(vec![url], cfg()).unwrap();
    assert_eq!(rpc.max_tx_version, 1);
    assert_eq!(rpc.get_transaction_raw("abc").await.unwrap(), json!("abc"));
    let e = rpc.call("getTransaction", get_transaction_params("abc", "confirmed", 0)).await.unwrap_err();
    assert_eq!(e.class(), "rpc_-32015");
    assert!(!e.not_sent());
}

#[tokio::test]
async fn bounded_queue_rejects_instead_of_building_a_backlog() {
    let (url, hits) = mock(0, None, 300).await;
    let rpc = Arc::new(RpcClient::with_config(vec![url], SchedulerConfig { max_in_flight: 1, max_queue: 3, ..cfg() }).unwrap());
    let calls: Vec<_> = (0..10).map(|i| {
        let rpc = rpc.clone();
        tokio::spawn(async move { rpc.call("getSlot", json!([i])).await })
    }).collect();
    let mut ok = 0;
    let mut rejected = 0;
    for c in calls {
        match c.await.unwrap() {
            Ok(_) => ok += 1,
            Err(RpcError::QueueFull) => rejected += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(ok + rejected, 10);
    assert!(rejected >= 6, "queue of 3 → most of 10 simultaneous calls rejected ({rejected})");
    assert_eq!(hits.load(Ordering::SeqCst), ok as u64, "rejected calls were never sent");
    let st = rpc.scheduler_stats();
    assert_eq!(st.queue_rejected, rejected as u64);
    assert!(st.max_queue_depth <= 3);
}

#[tokio::test]
async fn request_budget_and_wait_limit_stop_cleanly() {
    let (url, hits) = mock(0, None, 0).await;
    let rpc = RpcClient::with_config(vec![url.clone()], SchedulerConfig { request_budget: Some(3), ..cfg() }).unwrap();
    for i in 0..3 {
        rpc.call("getSlot", json!([i])).await.unwrap();
    }
    let e = rpc.call("getSlot", json!([9])).await.unwrap_err();
    assert!(matches!(e, RpcError::BudgetExhausted) && e.not_sent());
    assert_eq!((hits.load(Ordering::SeqCst), rpc.budget_remaining()), (3, Some(0)));
    // 1 request/s, burst 1, wait limit 200 ms → the second immediate call expires unsent
    let rpc = RpcClient::with_config(vec![url], SchedulerConfig { rate_per_s: 1.0, burst: 1, max_wait: Duration::from_millis(200), ..cfg() }).unwrap();
    rpc.call("getSlot", json!([1])).await.unwrap();
    let e = rpc.call("getSlot", json!([2])).await.unwrap_err();
    assert!(matches!(e, RpcError::Expired(_)), "{e}");
    let st = rpc.scheduler_stats();
    assert_eq!((st.deferred, st.expired), (1, 1));
}

#[tokio::test]
async fn identical_concurrent_requests_share_one_http_call() {
    let (url, hits) = mock(0, None, 200).await;
    let rpc = Arc::new(RpcClient::with_config(vec![url], cfg()).unwrap());
    let calls: Vec<_> = (0..5).map(|_| {
        let rpc = rpc.clone();
        tokio::spawn(async move { rpc.get_transaction_raw("same-sig").await })
    }).collect();
    for c in calls {
        assert_eq!(c.await.unwrap().unwrap(), json!("same-sig"));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.scheduler_stats().deduplicated, 4);
    // and the entry is released afterwards
    rpc.get_transaction_raw("same-sig").await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[test]
fn endpoint_lists_ignore_empty_and_placeholder_values() {
    use sniper_core::live::endpoint_list;
    assert!(endpoint_list("", &["wss"]).is_empty());
    assert!(endpoint_list(" , ", &["wss"]).is_empty());
    assert!(endpoint_list("<wss://your-provider-1/?api-key=...>", &["wss"]).is_empty());
    assert!(endpoint_list("https://x.example", &["wss"]).is_empty(), "wrong scheme");
    assert_eq!(endpoint_list("wss://a.example/?k=1, <wss://b>", &["wss"]), vec!["wss://a.example/?k=1"]);
    let _ = Pubkey::default();
}

// ---------------------------------------------------------------------------------------------
// capture CLI under provider rate limits

#[derive(Clone)]
struct CapState {
    txs: Arc<std::collections::HashMap<String, Value>>,
    sigs: Arc<Vec<String>>,
    hits: Arc<AtomicU64>,
}

async fn cap_handler(State(st): State<CapState>, Json(req): Json<Value>) -> axum::response::Response {
    let n = st.hits.fetch_add(1, Ordering::SeqCst);
    if n % 3 == 2 {
        // free-plan style throttling: every third request is rejected with a short Retry-After
        let mut h = HeaderMap::new();
        h.insert("retry-after", "0.2".parse().unwrap());
        return (StatusCode::TOO_MANY_REQUESTS, h, "slow down").into_response();
    }
    let r = match req["method"].as_str().unwrap() {
        "getSignaturesForAddress" if req["params"][1].get("before").is_none() => {
            json!(st.sigs.iter().map(|s| json!({"signature": s, "slot": 1, "err": null, "blockTime": 1})).collect::<Vec<_>>())
        }
        "getSignaturesForAddress" => json!([]),
        "getTransaction" => st.txs.get(req["params"][0].as_str().unwrap()).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    Json(json!({"jsonrpc":"2.0","id":1,"result":r})).into_response()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_under_429_pressure_stops_cleanly_within_its_request_budget() {
    let mut c = Curve::new("CAP429");
    let acc = CurveAccounts::of(&c);
    let mut txs = std::collections::HashMap::new();
    let mut sigs = Vec::new();
    for i in 0..30 {
        let PumpEvent::Trade(t) = c.buy(wallet(&format!("c{i}")), 50_000_000, 0).unwrap() else { panic!() };
        let sig = format!("cap429sig{i:02}");
        txs.insert(sig.clone(), txbuild::pump_trade_tx(&acc, &t, 900_000_000_000_000, Emission::Both, json!(1), &sig));
        sigs.push(sig);
    }
    let hits = Arc::new(AtomicU64::new(0));
    let app = Router::new().route("/", post(cap_handler)).with_state(CapState { txs: Arc::new(txs), sigs: Arc::new(sigs), hits: hits.clone() });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}/?api-key=CAP429SECRET", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let (d, r) = (dir.path().to_str().unwrap().to_string(), reports.path().to_str().unwrap().to_string());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sniper"))
            .args(["capture", "--from-live", "25", "--max-requests", "20", "--max-seconds", "60", "--rate", "20", "--out", &d, "--report-dir", &r])
            .env("SOLANA_RPC_URLS", &rpc)
            .env("SOLANA_WS_URLS", "")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("CAP429SECRET") && !stderr.contains("CAP429SECRET"));
    assert!(stdout.contains("stop reason: request budget (20) spent"), "{stdout}");
    assert!(hits.load(Ordering::SeqCst) <= 20, "never exceeds the request budget");
    let saved = std::fs::read_dir(dir.path()).unwrap().count();
    assert!(saved > 0 && saved < 25, "{saved}");
    assert!(stdout.contains(&format!("saved {saved}/25")), "{stdout}");
    assert!(stdout.lines().count() < 40, "aggregated output, not one line per failure:\n{stdout}");
    eprintln!("{stdout}");
}

#[test]
fn instruction_layouts_come_from_the_same_official_idls_as_the_event_layouts() {
    for (ix, ev) in [("pump.ix-accounts.json", "pump.idl-subset.json"), ("pump_amm.ix-accounts.json", "pump_amm.idl-subset.json")] {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/idl");
        let a: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(ix)).unwrap()).unwrap();
        let b: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(ev)).unwrap()).unwrap();
        assert_eq!(a["provenance"]["sha256"], b["provenance"]["sha256"], "{ix}");
        assert_eq!(a["provenance"]["npm_package"], b["provenance"]["npm_package"]);
        assert_eq!(a["address"], b["address"]);
    }
    use sniper_core::ixlayout::lookup;
    use sniper_core::types::programs;
    let buy = lookup(programs::PUMP_FUN, [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea]).unwrap();
    assert_eq!((buy.name.as_str(), buy.position("fee_recipient"), buy.position("associated_user"), buy.position("creator_vault")), ("buy", Some(1), Some(5), Some(9)));
    let sell = lookup(programs::PUMP_FUN, [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]).unwrap();
    assert_eq!((sell.position("creator_vault"), sell.position("token_program")), (Some(8), Some(9)), "sell orders these differently from buy");
    let amm_buy = lookup(programs::PUMP_SWAP, [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea]).unwrap();
    assert_eq!((amm_buy.position("pool_base_token_account"), amm_buy.position("coin_creator_vault_ata")), (Some(7), Some(17)));
    assert!(lookup("SomeOtherProgram1111111111111111111111111111", [0; 8]).is_none());
}
