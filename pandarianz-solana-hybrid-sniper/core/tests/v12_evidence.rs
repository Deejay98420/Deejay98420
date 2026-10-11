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
use sniper_core::types::{programs, Commitment, DataMode, Pubkey};
use sniper_core::txbuild::{self, Emission};
use sniper_core::verify::{summarize, verify_tx, Outcome};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const S: i64 = 1_000_000;

/// Realistic synthetic curve trade transactions (instruction tree, parsed transfers, log+CPI events).
fn curve_trades(n_buys: usize, n_sells: usize) -> Vec<(Value, String)> {
    let mut c = Curve::new("EVID");
    let acc = txbuild::CurveAccounts::of(&c);
    let mut out = Vec::new();
    let mut held = Vec::new();
    for i in 0..n_buys.max(n_sells) {
        let user = wallet(&format!("evid-user-{i}"));
        let PumpEvent::Trade(t) = c.buy(user, 300_000_000, i as i64 * S).unwrap() else { panic!() };
        held.push((user, t.token_amount));
        if i < n_buys {
            let sig = format!("evid-buy-{i}");
            out.push((txbuild::pump_trade_tx(&acc, &t, 900_000_000_000_000, Emission::Both, json!(0), &sig), sig));
        }
    }
    for (i, (user, tokens)) in held.into_iter().take(n_sells).enumerate() {
        let PumpEvent::Trade(t) = c.sell(user, tokens / 2, 100 * S).unwrap() else { panic!() };
        let sig = format!("evid-sell-{i}");
        out.push((txbuild::pump_trade_tx(&acc, &t, 900_000_000_000_000, Emission::Both, json!(1), &sig), sig));
    }
    out
}

#[test]
fn transaction_evidence_verifies_curve_trades_and_catches_mismatches() {
    let txs = curve_trades(1, 1);
    for (tx, sig) in &txs {
        let v = verify_tx(tx, sig);
        assert!(v.passed, "{:#?}", v.checks);
        for name in [
            "trade.instruction",
            "trade.token_transfer",
            "trade.curve_vault_delta",
            "trade.user_token_delta",
            "trade.sol_amount",
            "trade.protocol_fee_observed",
            "trade.creator_fee_observed",
            "trade.protocol_fee_formula",
            "trade.creator_fee_formula",
            "trade.curve_invariant",
            "token.mint_conservation",
        ] {
            assert!(v.checks.iter().any(|c| c.name == name && c.outcome == Outcome::Pass), "{name}: {:#?}", v.checks);
        }
        assert_eq!(v.event_outcomes(0).1 + v.event_outcomes(0).2, 0, "no failed or inconclusive hard check");
    }
    let v = verify_tx(&txs[0].0, &txs[0].1);
    assert_eq!(v.classification, vec!["trade_buy"]);
    assert_eq!(v.duplicates_merged, 1, "log + self-CPI copy of one trade merged");
    assert_eq!(verify_tx(&txs[1].0, &txs[1].1).version, "1");
    // decoder output that disagrees with chain evidence fails HARD checks
    let mut c = Curve::new("EVID");
    let acc = txbuild::CurveAccounts::of(&c);
    let PumpEvent::Trade(t) = c.buy(wallet("u"), 300_000_000, 0).unwrap() else { panic!() };
    let mut bad = t.clone();
    bad.token_amount += 1;
    let mut tx = txbuild::SynthTx::new(&t.user.to_string(), json!(0));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &t.user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade_as(&mut tx, &acc, &t, &PumpEvent::Trade(bad), Emission::Both);
    let v = verify_tx(&tx.build("tampered-event", 1), "tampered-event");
    assert!(!v.passed);
    for name in ["trade.token_transfer", "trade.user_token_delta", "trade.curve_vault_delta"] {
        assert!(v.checks.iter().any(|c| c.name == name && c.outcome == Outcome::Fail && c.hard), "{name}");
    }
    // balance evidence that disagrees with the parsed transfers fails too
    let (mut tx, sig) = curve_trades(1, 0).remove(0);
    let post = tx["meta"]["postTokenBalances"].as_array_mut().unwrap();
    let e = post.iter_mut().find(|b| b["owner"] != json!(acc.curve)).unwrap();
    let amt: i128 = e["uiTokenAmount"]["amount"].as_str().unwrap().parse().unwrap();
    e["uiTokenAmount"]["amount"] = json!((amt - 5).to_string());
    let v = verify_tx(&tx, &sig);
    assert!(!v.passed);
    assert!(v.checks.iter().any(|c| c.name == "trade.user_token_delta" && c.outcome == Outcome::Fail));
    assert!(v.checks.iter().any(|c| c.name == "token.mint_conservation" && c.outcome == Outcome::Fail));
}

#[test]
fn pumpswap_swap_verified_against_instruction_accounts() {
    let mint = Pubkey::synthetic("mint:VAULT");
    let mut p = Pool::new("VAULT", mint, 500_000_000_000_000, 80_000_000_000);
    let buy = p.buy(wallet("u"), 1_000_000_000, 0);
    let sell = p.sell(wallet("u"), 1_000_000_000, 0);
    for (ev, item) in [(buy, "pumpswap/BuyEvent"), (sell, "pumpswap/SellEvent")] {
        let tx = txbuild::pumpswap_swap_tx(&ev, &mint.to_string(), json!(1), "ps");
        let v = verify_tx(&tx, "ps");
        assert!(v.passed, "{:#?}", v.checks);
        assert_eq!(v.items, vec![item]);
        assert_eq!(v.event_outcomes(0).1 + v.event_outcomes(0).2, 0, "{:#?}", v.checks);
        for name in ["swap.instruction", "swap.pool_base_delta", "swap.pool_quote_delta", "swap.user_base_delta", "swap.protocol_fee_observed", "swap.creator_fee_observed", "swap.lp_fee"] {
            assert!(v.checks.iter().any(|c| c.name == name && c.outcome == Outcome::Pass), "{name}");
        }
    }
}

#[test]
fn summary_marks_venues_mainnet_verified_only_with_enough_fully_verified_events() {
    let mut results: Vec<_> = curve_trades(2, 1).iter().map(|(t, s)| verify_tx(t, s)).collect();
    let s = summarize(&results, 5);
    assert!(!s.items["pump_bonding_curve/TradeEvent"].mainnet_verified, "3 < 5 samples");
    assert_eq!(s.venues["pump_bonding_curve"].status, "insufficient_evidence");
    results = curve_trades(4, 2).iter().map(|(t, s)| verify_tx(t, s)).collect();
    let s = summarize(&results, 5);
    assert!(s.items["pump_bonding_curve/TradeEvent"].mainnet_verified);
    assert_eq!(s.venues["pump_bonding_curve"].status, "mainnet_verified", "{:#?}", s.venues);
    assert!(s.venues_mainnet_verified["pump_bonding_curve"]);
    assert!(!s.venues_mainnet_verified["pumpswap"], "no PumpSwap samples → stays unverified");
    assert_eq!(s.venues["raydium_amm_v4"].status, "disabled");
    assert_eq!(s.venues["pump_bonding_curve"].duplicate_emissions_merged, 6);
    // buys only → not verified even with many events
    let only_buys: Vec<_> = curve_trades(6, 0).iter().map(|(t, s)| verify_tx(t, s)).collect();
    assert_eq!(summarize(&only_buys, 5).venues["pump_bonding_curve"].status, "insufficient_evidence");
    // any hard failure revokes verification
    let (mut tx, sig) = curve_trades(1, 0).remove(0);
    let fee_rec = Pubkey::synthetic("fee-recipient").to_string();
    let keys: Vec<String> = tx["transaction"]["message"]["accountKeys"].as_array().unwrap().iter().map(|k| k["pubkey"].as_str().unwrap().to_string()).collect();
    let i = keys.iter().position(|k| *k == fee_rec).unwrap();
    let post = tx["meta"]["postBalances"][i].as_i64().unwrap();
    tx["meta"]["postBalances"][i] = json!(post - 1);
    results.push(verify_tx(&tx, &sig));
    let s = summarize(&results, 5);
    assert_eq!(s.venues["pump_bonding_curve"].status, "MISMATCH");
    assert!(!s.items["pump_bonding_curve/TradeEvent"].mainnet_verified);
    assert!(s.failures.iter().any(|f| f.contains("trade.protocol_fee_observed")), "{:?}", s.failures);
    assert!(s.venues["pump_bonding_curve"].examples.iter().any(|e| e.contains("protocol_fee_observed")));
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
    for (i, (mut tx, _)) in curve_trades(4, 2).into_iter().enumerate() {
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
                    "getTransaction" => {
                        let tx = txs.get(req["params"][0].as_str().unwrap()).cloned().unwrap_or(Value::Null);
                        // real provider behaviour: a version-1 transaction requested without
                        // maxSupportedTransactionVersion ≥ 1 is an error (-32015)
                        let max = req["params"][1]["maxSupportedTransactionVersion"].as_u64();
                        if tx["version"].as_u64().is_some_and(|v| max.is_none_or(|m| v > m)) {
                            return Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32015,"message":"Transaction version (1) is not supported by the requesting client."}}));
                        }
                        tx
                    }
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
    // an existing (user) fixture must never be modified or replaced
    let original = r#"{"provenance":{"network":"user-original"},"keep":"byte-identical"}"#;
    std::fs::write(dir.path().join("capsig0.json"), original).unwrap();
    let reports = tempfile::tempdir().unwrap();
    let r = reports.path().to_str().unwrap().to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sniper"))
            .args(["capture", "--from-live", "5", "--max-requests", "40", "--max-seconds", "60", "--rate", "50", "--out", &d, "--report-dir", &r])
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
    assert_eq!(f["provenance"]["request"]["maxSupportedTransactionVersion"], 1);
    assert_eq!(std::fs::read_to_string(dir.path().join("capsig0.json")).unwrap(), original, "existing fixture untouched");
    let v1: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("capsig5.json")).unwrap()).unwrap();
    assert_eq!(v1["provenance"]["transaction_version"], "1", "version-1 transaction captured");
    assert!(stdout.contains("saved 5/5 new unique transactions"), "{stdout}");
    assert!(stdout.contains("1 already captured"), "{stdout}");
    assert!(!dir.path().join("verification.json").exists(), "reports are not written into the fixture directory");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(reports.path().join("verification.json")).unwrap()).unwrap();
    assert_eq!(v["files"], 5);
    assert_eq!(v["venues"]["pump_bonding_curve"]["status"], "mainnet_verified", "{stdout}");
    assert_eq!(v["venues"]["pumpswap"]["status"], "insufficient_evidence");
    assert!(stdout.contains("MAINNET_VERIFIED"), "{stdout}");
    assert!(reports.path().join("verification-detail.json").exists());
}
