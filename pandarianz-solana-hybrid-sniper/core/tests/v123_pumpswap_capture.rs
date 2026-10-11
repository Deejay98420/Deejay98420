//! V1.2.3: selective PumpSwap capture — log classification, bounded deduplicated per-side queues,
//! and the CLI against a mock provider (logsSubscribe WebSocket + getTransaction with 429s).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::post, Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sniper_core::capture::{pumpswap_side_from_logs, Offer, Selector, Side};
use sniper_core::fixtures::{wallet, Pool};
use sniper_core::types::{programs, Pubkey};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

fn logs(outer: &str, ix: &str, inner_amm: bool) -> Vec<String> {
    let mut l = vec![format!("Program {outer} invoke [1]")];
    if inner_amm {
        l.push(format!("Program {} invoke [2]", programs::PUMP_SWAP));
        l.push(format!("Program log: Instruction: {ix}"));
        l.push(format!("Program {} success", programs::PUMP_SWAP));
    } else {
        l.push(format!("Program log: Instruction: {ix}"));
    }
    l.push(format!("Program {outer} success"));
    l
}

#[test]
fn pumpswap_side_comes_from_pumpswap_own_instruction_log() {
    assert_eq!(pumpswap_side_from_logs(&logs(programs::PUMP_SWAP, "Buy", false), false), Some(Side::Buy));
    assert_eq!(pumpswap_side_from_logs(&logs(programs::PUMP_SWAP, "BuyExactQuoteIn", false), false), Some(Side::Buy));
    assert_eq!(pumpswap_side_from_logs(&logs(programs::PUMP_SWAP, "Sell", false), false), Some(Side::Sell));
    assert_eq!(pumpswap_side_from_logs(&logs("Router111", "Sell", true), false), Some(Side::Sell), "nested CPI");
    assert_eq!(pumpswap_side_from_logs(&logs("Router111", "Buy", false), false), None, "a router's own log is not a PumpSwap swap");
    assert_eq!(pumpswap_side_from_logs(&logs(programs::PUMP_SWAP, "Deposit", false), false), None);
    assert_eq!(pumpswap_side_from_logs(&logs(programs::PUMP_SWAP, "Buy", false), true), None, "failed tx");
}

#[test]
fn selector_deduplicates_bounds_and_balances_sides() {
    let mut s = Selector::new(2, vec!["old".to_string()]);
    assert_eq!(s.offer("old", Side::Buy), Offer::Duplicate, "already captured");
    assert_eq!(s.offer("b1", Side::Buy), Offer::Queued);
    assert_eq!(s.offer("b1", Side::Buy), Offer::Duplicate);
    assert_eq!(s.offer("b2", Side::Buy), Offer::Queued);
    assert_eq!(s.offer("b3", Side::Buy), Offer::Queued, "bounded: the oldest buy is dropped");
    assert_eq!((s.dropped, s.duplicates, s.queued()), (1, 2, (2, 0)));
    assert_eq!(s.offer("s1", Side::Sell), Offer::Queued);
    // more buys saved than sells → a sell is fetched next; newest first within a side
    assert_eq!(s.next(3, 1), Some(("s1".into(), Side::Sell)));
    assert_eq!(s.next(3, 1), Some(("b3".into(), Side::Buy)), "no sells left → fall back to buys");
    assert_eq!(s.next(0, 0), Some(("b2".into(), Side::Buy)));
    assert_eq!(s.next(0, 0), None);
    assert_eq!(s.offer("b2", Side::Buy), Offer::Duplicate, "never re-queued once seen");
}

/// Mock WebSocket provider: accepts logsSubscribe and streams the given notifications.
async fn mock_ws(notes: Vec<(String, Vec<String>, bool)>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((sock, _)) = l.accept().await {
            let notes = notes.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                let Some(Ok(Message::Text(req))) = ws.next().await else { return };
                let req: Value = serde_json::from_str(&req).unwrap();
                assert_eq!(req["method"], "logsSubscribe");
                assert_eq!(req["params"][0]["mentions"][0], programs::PUMP_SWAP);
                ws.send(Message::Text(json!({"jsonrpc":"2.0","result":7,"id":1}).to_string())).await.unwrap();
                for (sig, logs, failed) in notes {
                    let n = json!({"jsonrpc":"2.0","method":"logsNotification","params":{"subscription":7,"result":{"context":{"slot":1},"value":{"signature":sig,"err": if failed { json!({"InstructionError":[0,"x"]}) } else { Value::Null },"logs":logs}}}});
                    if ws.send(Message::Text(n.to_string())).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            });
        }
    });
    format!("ws://{addr}/?api-key=PSWSSECRET")
}

#[derive(Clone)]
struct Rpc {
    txs: Arc<HashMap<String, Value>>,
    hits: Arc<AtomicU64>,
    fetched: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn rpc_handler(State(st): State<Rpc>, Json(req): Json<Value>) -> axum::response::Response {
    let n = st.hits.fetch_add(1, Ordering::SeqCst);
    if n % 4 == 3 {
        let mut h = HeaderMap::new();
        h.insert("retry-after", "0.1".parse().unwrap());
        return (StatusCode::TOO_MANY_REQUESTS, h, "slow down").into_response();
    }
    assert_eq!(req["method"], "getTransaction", "only getTransaction is used");
    assert_eq!(req["params"][1]["maxSupportedTransactionVersion"], 1);
    let sig = req["params"][0].as_str().unwrap().to_string();
    st.fetched.lock().unwrap().push(sig.clone());
    Json(json!({"jsonrpc":"2.0","id":1,"result": st.txs.get(&sig).cloned().unwrap_or(Value::Null)})).into_response()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pumpswap_capture_saves_balanced_unique_swaps_within_budget() {
    let mint = Pubkey::synthetic("mint:PSCAP");
    let mut p = Pool::new("PSCAP", mint, 500_000_000_000_000, 80_000_000_000);
    let mut txs = HashMap::new();
    let mut notes = Vec::new();
    for i in 0..6 {
        let (ev, side) = if i % 3 == 2 { (p.sell(wallet("ps"), 1_000_000_000, 0), "Sell") } else { (p.buy(wallet("ps"), 500_000_000, 0), "Buy") };
        let sig = format!("pscap{i}");
        txs.insert(sig.clone(), sniper_core::txbuild::pumpswap_swap_tx(&ev, &mint.to_string(), json!(1), &sig));
        notes.push((sig.clone(), logs(programs::PUMP_SWAP, side, false), false));
        notes.push((sig, logs(programs::PUMP_SWAP, side, false), false)); // duplicate notification
    }
    notes.push(("deposit0".into(), logs(programs::PUMP_SWAP, "Deposit", false), false));
    notes.push(("failed0".into(), logs(programs::PUMP_SWAP, "Buy", false), true));
    notes.push(("router0".into(), logs("Router111", "Buy", false), false));
    let ws = mock_ws(notes).await;
    let st = Rpc { txs: Arc::new(txs), hits: Arc::new(AtomicU64::new(0)), fetched: Default::default() };
    let app = Router::new().route("/", post(rpc_handler)).with_state(st.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}/?api-key=PSRPCSECRET", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let original = r#"{"provenance":{"network":"user-original"}}"#;
    std::fs::write(dir.path().join("pscap0.json"), original).unwrap(); // already captured: never refetched or replaced
    let reports = tempfile::tempdir().unwrap();
    let (d, r) = (dir.path().to_str().unwrap().to_string(), reports.path().to_str().unwrap().to_string());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sniper"))
            .args(["capture", "--venue", "pumpswap", "--from-live", "4", "--max-requests", "12", "--max-seconds", "30", "--rate", "50", "--out", &d, "--report-dir", &r])
            .env("SOLANA_WS_URLS", &ws)
            .env("SOLANA_RPC_URLS", &rpc)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("SECRET") && !stderr.contains("SECRET"), "no keys in output");
    assert!(stdout.contains("saved 4 new swaps (3 buys, 1 sells)") || stdout.contains("saved 4 new swaps (2 buys, 2 sells)"), "{stdout}");
    assert!(stdout.contains("stop reason: target reached"), "{stdout}");
    let fetched = st.fetched.lock().unwrap().clone();
    assert!(!fetched.iter().any(|s| s == "deposit0" || s == "failed0" || s == "router0" || s == "pscap0"), "only new Buy/Sell candidates fetched: {fetched:?}");
    let mut uniq = fetched.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), fetched.len(), "no signature fetched twice");
    assert!(st.hits.load(Ordering::SeqCst) <= 12);
    assert_eq!(std::fs::read_to_string(dir.path().join("pscap0.json")).unwrap(), original);
    assert!(std::fs::read_dir(reports.path()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("capture-pumpswap-")));
    assert!(stdout.contains("pumpswap") && stdout.contains("VENUES MAINNET-VERIFIED"), "{stdout}");
    // intake accounting: irrelevant traffic and duplicates are counted separately from relevant swaps
    assert!(stdout.contains("15 notifications — 2 irrelevant (not a PumpSwap Buy/Sell), 1 failed txs, 6 duplicate notifications; 6 relevant swaps (4 buys, 2 sells)"), "{stdout}");
    assert!(stdout.contains("latency (measured): notification → fetch start p50"), "{stdout}");
    let manifest = std::fs::read_dir(reports.path()).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("capture-pumpswap-")).unwrap();
    let m: Value = serde_json::from_str(&std::fs::read_to_string(manifest.path()).unwrap()).unwrap();
    assert_eq!(m["intake"]["irrelevant"], 2);
    assert!(m["saved"][0]["notification_received_us"].as_i64().is_some() && m["saved"][0]["fetch_finished_us"].as_i64().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pumpswap_capture_stops_cleanly_when_the_budget_is_spent() {
    let notes: Vec<_> = (0..20).map(|i| (format!("nf{i}"), logs(programs::PUMP_SWAP, if i % 2 == 0 { "Buy" } else { "Sell" }, false), false)).collect();
    let ws = mock_ws(notes).await;
    let st = Rpc { txs: Arc::new(HashMap::new()), hits: Arc::new(AtomicU64::new(0)), fetched: Default::default() };
    let app = Router::new().route("/", post(rpc_handler)).with_state(st.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let (d, r) = (dir.path().to_str().unwrap().to_string(), reports.path().to_str().unwrap().to_string());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sniper"))
            .args(["capture", "--venue", "pumpswap", "--from-live", "10", "--max-requests", "5", "--max-seconds", "30", "--rate", "50", "--out", &d, "--report-dir", &r])
            .env("SOLANA_WS_URLS", &ws)
            .env("SOLANA_RPC_URLS", &rpc)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("stop reason: request budget (5) spent"), "{stdout}");
    assert!(st.hits.load(Ordering::SeqCst) <= 5);
    assert!(stdout.contains("saved 0 new swaps"), "{stdout}");
}
