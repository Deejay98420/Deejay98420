//! V1.2: transaction-evidence verification, subscription capability checks, provider diagnostics,
//! fill-evidence classification, sensitivity analysis and the calibration/holdout split.

mod common;
use axum::{routing::post, Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sniper_core::config::EngineConfig;
use sniper_core::decode::pumpfun::PumpEvent;
use sniper_core::decode::{anchor, build, pumpswap};
use sniper_core::fixtures::{hybrid_session, wallet, Curve, Pool};
use sniper_core::store::MemData;
use sniper_core::stream::{ws, ConnState, Ingestor, StreamMsg};
use sniper_core::types::{programs, Commitment, DataMode, Pubkey, WSOL_MINT};
use sniper_core::verify::{summarize, verify_tx};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const S: i64 = 1_000_000;

fn tb(owner: &str, mint: &str, amount: i128, idx: usize) -> Value {
    json!({"accountIndex": idx, "owner": owner, "mint": mint, "uiTokenAmount": {"amount": amount.to_string(), "decimals": 6}})
}

/// Build a getTransaction-shaped curve trade with consistent balance evidence.
fn curve_trade_tx(tamper_event: bool, tamper_balance: bool) -> (Value, String) {
    let mut c = Curve::new("EVID");
    let user = wallet("evid-user");
    let PumpEvent::Trade(mut t) = c.buy(user, 300_000_000, 0).unwrap() else { panic!() };
    let real_tokens = t.token_amount as i128;
    if tamper_event {
        t.token_amount += 1; // decoder output no longer matches chain evidence
    }
    let user_s = user.to_string();
    let curve_s = c.bonding_curve.to_string();
    let mint_s = c.mint.to_string();
    let got = if tamper_balance { real_tokens - 5 } else { real_tokens };
    let paid = t.sol_amount as i64 + t.fee.unwrap() as i64 + t.creator_fee.unwrap() as i64 + 5_000;
    let sig = "evid-sig-1".to_string();
    let tx = json!({
        "slot": 123, "blockTime": 1_760_000_000,
        "meta": {
            "err": null, "fee": 5000,
            "logMessages": build::pump_logs(&[PumpEvent::Trade(t)]),
            "innerInstructions": [],
            "preBalances": [10_000_000_000i64, 1], "postBalances": [10_000_000_000i64 - paid, 1],
            "preTokenBalances": [tb(&curve_s, &mint_s, 793_100_000_000_000, 1)],
            "postTokenBalances": [tb(&curve_s, &mint_s, 793_100_000_000_000 - got, 1), tb(&user_s, &mint_s, got, 2)],
        },
        "transaction": {"signatures": [sig.clone()], "message": {"accountKeys": [{"pubkey": user_s}, {"pubkey": curve_s}]}}
    });
    (tx, sig)
}

#[test]
fn transaction_evidence_verifies_curve_trades_and_catches_mismatches() {
    let (tx, sig) = curve_trade_tx(false, false);
    let v = verify_tx(&tx, &sig);
    assert!(v.passed, "{:#?}", v.checks);
    assert_eq!(v.classification, vec!["trade_buy"]);
    assert_eq!((v.slot, v.block_time), (123, Some(1_760_000_000)));
    for name in ["trade.user_token_delta", "trade.direction", "trade.token_conservation", "trade.protocol_fee", "trade.creator_fee", "trade.curve_invariant"] {
        assert!(v.checks.iter().any(|c| c.name == name && c.ok), "{name}");
    }
    assert!(v.checks.iter().any(|c| c.name == "trade.user_sol_delta" && c.ok), "user paid sol + fees + tx fee");
    // decoder output that disagrees with balance evidence fails a HARD check
    let (tx, sig) = curve_trade_tx(true, false);
    let v = verify_tx(&tx, &sig);
    assert!(!v.passed);
    assert!(v.checks.iter().any(|c| c.name == "trade.user_token_delta" && !c.ok && c.hard));
    let (tx, sig) = curve_trade_tx(false, true);
    assert!(!verify_tx(&tx, &sig).passed);
}

#[test]
fn pumpswap_swap_verified_against_pool_vault_deltas() {
    let mint = Pubkey::synthetic("mint:VAULT");
    let mut p = Pool::new("VAULT", mint, 500_000_000_000_000, 80_000_000_000);
    let ev = p.buy(wallet("u"), 1_000_000_000, 0);
    let pumpswap::PumpSwapEvent::Swap(s) = &ev else { panic!() };
    let mut data = anchor::EVENT_IX_TAG.to_vec();
    data.extend(pumpswap::encode(&ev));
    let (pool, user, m) = (p.pool.to_string(), wallet("u").to_string(), mint.to_string());
    let tx = json!({
        "slot": 9, "blockTime": 1,
        "meta": {"err": null, "fee": 5000, "logMessages": build::pumpswap_inner(&[ev.clone()]).0,
            "innerInstructions": [{"index": 0, "instructions": [{"programId": programs::PUMP_SWAP, "data": bs58::encode(&data).into_string()}]}],
            "preTokenBalances": [tb(&pool, &m, s.pool_base_token_reserves as i128, 1), tb(&pool, WSOL_MINT, s.pool_quote_token_reserves as i128, 2)],
            "postTokenBalances": [tb(&pool, &m, (s.pool_base_token_reserves - s.base_amount) as i128, 1), tb(&pool, WSOL_MINT, (s.pool_quote_token_reserves + s.quote_amount_pool) as i128, 2), tb(&user, &m, s.base_amount as i128, 3)],
            "preBalances": [], "postBalances": []},
        "transaction": {"signatures": ["ps"], "message": {"accountKeys": []}}
    });
    let v = verify_tx(&tx, "ps");
    assert!(v.passed, "{:#?}", v.checks);
    assert_eq!(v.items, vec!["pumpswap/BuyEvent"]);
}

#[test]
fn summary_marks_layouts_mainnet_verified_only_with_enough_passing_events() {
    let mut results = Vec::new();
    for _ in 0..3 {
        let (tx, sig) = curve_trade_tx(false, false);
        results.push(verify_tx(&tx, &sig));
    }
    let s = summarize(&results, 5);
    assert!(!s.items["pump_bonding_curve/TradeEvent"].mainnet_verified, "3 < 5 samples");
    for _ in 0..2 {
        let (tx, sig) = curve_trade_tx(false, false);
        results.push(verify_tx(&tx, &sig));
    }
    let s = summarize(&results, 5);
    assert!(s.items["pump_bonding_curve/TradeEvent"].mainnet_verified);
    assert_eq!(s.venues_mainnet_verified["pump_bonding_curve"], true);
    assert_eq!(s.venues_mainnet_verified["pumpswap"], false, "no PumpSwap samples → stays unverified");
    let (tx, sig) = curve_trade_tx(true, false);
    results.push(verify_tx(&tx, &sig));
    let s = summarize(&results, 5);
    assert!(!s.items["pump_bonding_curve/TradeEvent"].mainnet_verified, "any hard failure revokes verification");
    assert!(s.failures.iter().any(|f| f.contains("trade.user_token_delta")), "{:?}", s.failures);
}

/// WS server accepting logsSubscribe but rejecting transactionSubscribe (common on plain RPC).
async fn picky_ws(send_pump_notification: bool) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                let mut n = 0u64;
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let id = v["id"].as_u64().unwrap();
                    let reply = match v["method"].as_str().unwrap() {
                        "transactionSubscribe" | "slotsUpdatesSubscribe" => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
                        _ => {
                            n += 1;
                            json!({"jsonrpc":"2.0","id":id,"result":n})
                        }
                    };
                    ws.send(Message::Text(reply.to_string())).await.unwrap();
                    if send_pump_notification && v["method"] == "logsSubscribe" && id == 1 {
                        let mut c = Curve::new("DOC");
                        let logs = build::pump_logs(&[c.buy(wallet("d"), 100_000_000, 0).unwrap()]);
                        let note = json!({"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":5},"value":{"signature":"docsig","err":null,"logs":logs}},"subscription":1}});
                        ws.send(Message::Text(note.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn subscription_capabilities_are_checked_explicitly() {
    let url = picky_ws(false).await;
    // transaction-level subscription rejected → capability failure reported, never "Subscribed"
    let (ing, mut rx) = Ingestor::new(256, 1000);
    let mut cfg = sniper_core::live::LiveSetup::new(vec![url.clone()], vec![], true).unwrap().ws.remove(0);
    cfg.backoff = sniper_core::stream::backoff::Backoff::new(10, 20, Some(1));
    ws::run(cfg, ing).await;
    let mut caps = Vec::new();
    let mut subscribed = false;
    while let Ok(m) = rx.try_recv() {
        match m {
            StreamMsg::Capability { method, ok, detail, .. } => caps.push((method, ok, detail)),
            StreamMsg::Connection { state: ConnState::Subscribed, .. } => subscribed = true,
            _ => {}
        }
    }
    assert!(caps.iter().any(|(m, ok, d)| m == "transactionSubscribe" && !ok && d.contains("Method not found")), "{caps:?}");
    assert!(!subscribed, "a rejected primary subscription must not report Subscribed");
    // logs mode on the same provider: accepted → Subscribed
    let (ing, mut rx) = Ingestor::new(256, 1000);
    let cfg = sniper_core::live::LiveSetup::new(vec![url], vec![], false).unwrap().ws.remove(0);
    let h = tokio::spawn(ws::run(cfg, ing));
    let mut ok_logs = false;
    let mut sub = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !(ok_logs && sub) {
        if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            match m {
                StreamMsg::Capability { method, ok: true, .. } if method == "logsSubscribe" => ok_logs = true,
                StreamMsg::Connection { state: ConnState::Subscribed, .. } => sub = true,
                _ => {}
            }
        }
    }
    h.abort();
    assert!(ok_logs && sub);
}

#[tokio::test]
async fn doctor_probes_report_capabilities_coverage_and_never_leak_keys() {
    let url = picky_ws(true).await;
    let w = sniper_core::doctor::probe_ws(&url, Duration::from_millis(600)).await;
    assert!(w.connected);
    assert_eq!(w.subscriptions["logsSubscribe:pump"].accepted, Some(true));
    assert_eq!(w.subscriptions["transactionSubscribe"].accepted, Some(false));
    assert!(w.subscriptions["logsSubscribe:pump"].notifications >= 1);
    assert!(w.decoded_pump_events >= 1);
    // mock RPC: one PumpSwap transaction available via getTransaction
    let mint = Pubkey::synthetic("mint:DOCAMM");
    let mut p = Pool::new("DOCAMM", mint, 500_000_000_000_000, 80_000_000_000);
    let ev = p.buy(wallet("u"), 1_000_000_000, 0);
    let mut data = anchor::EVENT_IX_TAG.to_vec();
    data.extend(pumpswap::encode(&ev));
    let tx = json!({"slot": 9, "blockTime": 1, "meta": {"err": null, "logMessages": build::pumpswap_inner(&[ev]).0, "innerInstructions": [{"index":0,"instructions":[{"programId": programs::PUMP_SWAP, "data": bs58::encode(&data).into_string()}]}]}, "transaction": {"signatures": ["pss"]}});
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let tx = tx.clone();
            async move {
                let r = match req["method"].as_str().unwrap() {
                    "getSlot" => json!(77),
                    "getSignaturesForAddress" => json!([{"signature": "pss", "slot": 9, "err": null, "blockTime": 1}]),
                    "getTransaction" => tx,
                    _ => Value::Null,
                };
                Json(json!({"jsonrpc":"2.0","id":1,"result":r}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}/?api-key=TOPSECRETKEY42", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let r = sniper_core::doctor::probe_rpc(&rpc_url).await;
    assert!(r.ok, "{:?}", r.error);
    assert!(r.pumpswap_events_decoded >= 1);
    let rep = sniper_core::doctor::assess(vec![w], vec![r], sniper_core::doctor::JupiterProbe::default());
    assert!(rep.ready_for_live, "{:?}", rep.problems);
    assert_eq!(rep.recommended_stream_kind.as_deref(), Some("logs"));
    assert!(rep.pumpswap_coverage.contains("getTransaction"));
    let text = serde_json::to_string(&rep).unwrap();
    assert!(!text.contains("TOPSECRETKEY42"), "keys never appear in reports");
    // a failing endpoint with a key in the URL: error text is scrubbed
    let bad = sniper_core::doctor::probe_ws("ws://127.0.0.1:1/?api-key=ANOTHERSECRET99", Duration::from_millis(100)).await;
    assert!(!bad.connected);
    assert!(!serde_json::to_string(&bad).unwrap().contains("ANOTHERSECRET99"));
}

#[tokio::test]
async fn fixture_fills_are_never_validated_and_never_count_as_validated_pnl() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = common::engine_with(EngineConfig::default(), data.clone(), "evid");
    e.boot(json!({})).await;
    common::feed(&mut e, hybrid_session()).await;
    e.flush().await;
    let s = e.snapshot();
    for b in &s.books {
        assert!(b.perf.closed_trades > 0);
        assert_eq!(b.perf_validated.closed_trades, 0, "synthetic fills never validate");
        assert_eq!(b.perf_unverified.closed_trades, b.perf.closed_trades);
        assert_eq!(b.perf_validated_holdout.closed_trades, 0);
        assert!(!b.perf_by_path_size.is_empty());
        assert!(b.execution_failure_rate.is_some());
    }
    let d = data.lock().unwrap();
    let filled: Vec<_> = d.orders.iter().filter(|o| o.status == sniper_core::paper::OrderStatus::Filled).collect();
    assert!(!filled.is_empty());
    for o in filled {
        let ev = o.evidence.as_ref().expect("every fill is classified");
        assert!(!ev.validated);
        assert!(ev.reasons.iter().any(|r| r.contains("FIXTURE data")));
        assert!(ev.reasons.iter().any(|r| r.contains("not verified against captured mainnet")));
    }
    assert!(s.expected_move_model_status.starts_with("UNCALIBRATED"));
}

#[test]
fn live_fill_validates_only_with_verified_decoder_fresh_state_and_observed_fees() {
    use sniper_core::paper::{assess_fill, Fill, Order, OrderKind, OrderStatus};
    use sniper_core::quote::{venue_quote, Side, VenueFees};
    use sniper_core::state::{Input, Universe};
    let mut u = Universe::new(0);
    let mut c = Curve::new("VAL");
    u.apply(Input { slot: 1, signature: "c".into(), index: 0, ts_us: 0, event: sniper_core::decode::VenueEvent::Pump(c.create("V", "V", 0)) });
    u.apply(Input { slot: 2, signature: "b".into(), index: 0, ts_us: S, event: sniper_core::decode::VenueEvent::Pump(c.buy(wallet("x"), 500_000_000, S).unwrap()) });
    let st = &u.tokens[&c.mint];
    let fees = VenueFees { curve_fee_bps_default: 125, pumpswap_fee_bps: 30 };
    let mk = |now: i64| {
        let q = venue_quote(st, Side::Buy, 20_000_000, 0, &fees, f64::MAX, now).unwrap();
        Order {
            order_id: "o".into(), idempotency_key: "k".into(), book_id: "b".into(), scenario: "base".into(), data_mode: DataMode::Live,
            strategy_version: "v".into(), kind: OrderKind::Entry, side: Side::Buy, mint: c.mint, path: sniper_core::strategy::EntryPath::Launch,
            position_id: None, amount: 20_000_000, slippage_bps: 1000, min_out: 0, decision_quote: None, decision_us: 0, fill_at_us: now,
            trigger_signature: String::new(), trigger_slot: 0, attempt: 1, reason: String::new(), status: OrderStatus::Filled,
            fill: Some(Fill { in_amount: q.in_amount, out_amount: q.out_amount, latency_ms: 0.0, quote_age_ms: q.state_age_ms }),
            fill_quote: Some(q), costs: Default::default(), error: None, completed_us: Some(now), observed_quote: None,
            fill_basis: Some("venue_sim".into()), aggregator_divergence_bps: None, evidence: None,
        }
    };
    let fresh = mk(S + 500_000);
    let e = assess_fill(&fresh, Some(st), DataMode::Live, true, 2_000.0);
    assert!(e.validated, "{:?}", e.reasons);
    assert_eq!(e.fee_source, "observed_event");
    assert!(!assess_fill(&fresh, Some(st), DataMode::Live, false, 2_000.0).validated, "decoder not mainnet-verified");
    assert!(!assess_fill(&fresh, Some(st), DataMode::Fixture, true, 2_000.0).validated, "offline data");
    let stale = mk(S + 5_000_000);
    let e = assess_fill(&stale, Some(st), DataMode::Live, true, 2_000.0);
    assert!(!e.validated);
    assert!(e.reasons[0].contains("ms old at fill"));
}

#[tokio::test]
async fn sensitivity_never_improves_results_and_split_is_fixed_by_time() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = common::engine_with(EngineConfig::default(), data, "sens");
    e.boot(json!({})).await;
    common::feed(&mut e, hybrid_session()).await;
    let s = e.snapshot();
    let b = &s.books[1];
    let base = b.sensitivity_all[0].net_sol;
    assert_eq!(b.sensitivity_all[0].label, "as simulated");
    assert!((base - b.perf.realized_pnl_sol).abs() < 1e-9);
    for r in &b.sensitivity_all[1..] {
        assert!(r.net_sol <= base + 1e-12, "{} should not improve P&L", r.label);
    }
    let day = 86_400 * S;
    assert_eq!(sniper_core::metrics::evidence_split(0), "calibration");
    assert_eq!(sniper_core::metrics::evidence_split(day + 1), "holdout");
    assert_eq!(sniper_core::metrics::evidence_split(2 * day + 5), "calibration");
    let _ = Commitment::Confirmed;
}

/// End-to-end CLI: `sniper capture --from-live` against a mock RPC writes provenance-tagged real-shaped
/// fixtures and a verification summary (here with synthetic content served in mainnet JSON shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_cli_writes_provenance_and_verification_summary() {
    let mut txs = std::collections::HashMap::new();
    let mut sigs = Vec::new();
    for i in 0..6 {
        let (mut tx, _) = curve_trade_tx(false, false);
        let sig = format!("capsig{i}");
        tx["transaction"]["signatures"] = json!([sig.clone()]);
        tx["slot"] = json!(1000 + i);
        txs.insert(sig.clone(), tx);
        sigs.push(sig);
    }
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let (txs, sigs) = (txs.clone(), sigs.clone());
            async move {
                let r = match req["method"].as_str().unwrap() {
                    "getSignaturesForAddress" if req["params"][0] == programs::PUMP_FUN && req["params"][1].get("before").is_none() => {
                        json!(sigs.iter().map(|s| json!({"signature": s, "slot": 1, "err": null, "blockTime": 1})).collect::<Vec<_>>())
                    }
                    "getSignaturesForAddress" => json!([]),
                    "getTransaction" => txs.get(req["params"][0].as_str().unwrap()).cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                Json(json!({"jsonrpc":"2.0","id":1,"result":r}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}/?api-key=CAPTURESECRET77", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap().to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sniper"))
            .args(["capture", "--from-live", "6", "--out", &d])
            .env("SOLANA_WS_URLS", "ws://127.0.0.1:9/?api-key=CAPTURESECRET77")
            .env("SOLANA_RPC_URLS", &rpc)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("CAPTURESECRET77") && !stderr.contains("CAPTURESECRET77"), "no key in output");
    let f: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("capsig3.json")).unwrap()).unwrap();
    assert_eq!(f["provenance"]["network"], "mainnet-beta");
    assert_eq!(f["provenance"]["slot"], 1003);
    assert_eq!(f["provenance"]["classification"][0], "trade_buy");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("verification.json")).unwrap()).unwrap();
    assert_eq!(v["files"], 6);
    assert_eq!(v["items"]["pump_bonding_curve/TradeEvent"]["mainnet_verified"], true, "{stdout}");
    assert!(stdout.contains("MAINNET VERIFIED"));
}
