//! Full LIVE-mode pipeline against local mock providers (no external network): multi-provider
//! WebSocket failover, chain tip, logs → getTransaction fetch of PumpSwap emit_cpi events, startup
//! backfill, pool discovery and mint checks through a read-only JSON-RPC mock.
//!
//! Transactions are synthetic but encoded with the IDL-verified layouts and served in the exact
//! JSON shapes of the Solana RPC API. This validates the adapters, not mainnet behaviour.

use axum::{routing::post, Json, Router};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sniper_core::config::EngineConfig;
use sniper_core::decode::{anchor, build, pumpswap};
use sniper_core::engine::{Control, Engine, EngineOptions, LiveServices};
use sniper_core::fixtures::{wallet, Curve, Pool};
use sniper_core::live::{start_sources, FetchStats, LiveSetup};
use sniper_core::store::{spawn_mem, MemData};
use sniper_core::stream::Ingestor;
use sniper_core::time::wall_us;
use sniper_core::types::{programs, DataMode, Pubkey, WSOL_MINT};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn tx_json(sig: &str, slot: u64, block_time: i64, logs: Vec<String>, inner: Vec<(String, Vec<u8>)>) -> Value {
    json!({
        "slot": slot,
        "blockTime": block_time,
        "meta": {
            "err": null,
            "logMessages": logs,
            "innerInstructions": [{"index": 0, "instructions": inner.iter().map(|(p, d)| json!({"programId": p, "data": bs58::encode(d).into_string()})).collect::<Vec<_>>()}]
        },
        "transaction": {"signatures": [sig]}
    })
}

fn pool_account(pool: &Pool, quote_mint: Pubkey) -> Vec<u8> {
    let mut d = pumpswap::POOL_DISCRIMINATOR.to_vec();
    d.push(255);
    d.extend(0u16.to_le_bytes());
    d.extend(Pubkey::synthetic("creator").0);
    d.extend(pool.mint.0);
    d.extend(quote_mint.0);
    d.extend([0u8; 32 * 3]); // lp mint, pool token accounts
    d.extend(1_000u64.to_le_bytes());
    d.extend(pool.coin_creator.0);
    d
}

struct World {
    txs: HashMap<String, Value>,
    accounts: HashMap<String, (Vec<u8>, String)>,
    backfill_sigs: Vec<String>,
    calls: Mutex<Vec<String>>,
}

async fn mock_rpc(world: Arc<World>) -> String {
    let w = world.clone();
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let w = w.clone();
            async move {
                let method = req["method"].as_str().unwrap_or("").to_string();
                w.calls.lock().unwrap().push(method.clone());
                let p = &req["params"];
                let result = match method.as_str() {
                    "getSignaturesForAddress" => {
                        if p[0] == programs::PUMP_FUN {
                            json!(w.backfill_sigs.iter().map(|s| json!({"signature": s, "slot": 50, "err": null, "blockTime": wall_us() / 1_000_000 - 60})).collect::<Vec<_>>())
                        } else {
                            json!([])
                        }
                    }
                    "getTransaction" => w.txs.get(p[0].as_str().unwrap()).cloned().unwrap_or(Value::Null),
                    "getAccountInfo" => match w.accounts.get(p[0].as_str().unwrap()) {
                        Some((data, owner)) => json!({"context": {"slot": 1}, "value": {"data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"], "owner": owner, "lamports": 1, "executable": false}}),
                        None => json!({"context": {"slot": 1}, "value": null}),
                    },
                    "getSlot" => json!(100),
                    _ => return Json(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "not found"}})),
                };
                Json(json!({"jsonrpc": "2.0", "id": 1, "result": result}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// WebSocket provider: acks, then streams tips and notifications from `script`.
async fn mock_ws(script: Vec<Value>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else { return };
            let script = script.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                let first = ws.next().await.unwrap().unwrap();
                assert!(first.to_text().unwrap().contains("Subscribe"));
                ws.send(Message::Text(r#"{"jsonrpc":"2.0","result":7,"id":1}"#.into())).await.unwrap();
                let mut slot = 1000u64;
                for msg in script {
                    slot += 1;
                    let tip = json!({"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"slot":slot,"parent":slot-1,"root":slot-32},"subscription":9}});
                    if ws.send(Message::Text(tip.to_string())).await.is_err() {
                        return;
                    }
                    if ws.send(Message::Text(msg.to_string())).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                // keep the session alive with tips
                loop {
                    slot += 1;
                    let tip = json!({"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"slot":slot},"subscription":9}});
                    if ws.send(Message::Text(tip.to_string())).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
    });
    format!("ws://{addr}")
}

fn logs_note(sig: &str, slot: u64, logs: &[String]) -> Value {
    json!({"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":slot},"value":{"signature":sig,"err":null,"logs":logs}},"subscription":7}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_pipeline_failover_fetch_backfill_pool_discovery_and_mint_checks() {
    let now_s = wall_us() / 1_000_000;
    // --- backfill: a token launched BEFORE startup
    let mut old = Curve::new("PRESTART");
    let creator = old.creator;
    let create = old.create("Pre", "PRE", (now_s - 90) * 1_000_000);
    let first = old.buy(creator, 300_000_000, (now_s - 90) * 1_000_000).unwrap();
    let b2 = old.buy(wallet("early"), 500_000_000, (now_s - 80) * 1_000_000).unwrap();
    let mut txs = HashMap::new();
    txs.insert("bf1".to_string(), tx_json("bf1", 50, now_s - 90, build::pump_logs(&[create, first]), vec![]));
    txs.insert("bf2".to_string(), tx_json("bf2", 51, now_s - 80, build::pump_logs(&[b2]), vec![]));
    // --- PumpSwap swaps on a pool whose creation we never saw (fetched via getTransaction)
    let mint = Pubkey::synthetic("mint:AMMTOKEN");
    let mut pool = Pool::new("AMMTOKEN", mint, 500_000_000_000_000, 80_000_000_000);
    let mut swap_tx = |sig: &str, ev: pumpswap::PumpSwapEvent| {
        let (logs, _) = build::pumpswap_inner(&[ev.clone()]);
        let mut data = anchor::EVENT_IX_TAG.to_vec();
        data.extend(pumpswap::encode(&ev));
        txs.insert(sig.to_string(), tx_json(sig, 1100, now_s, logs.clone(), vec![(programs::PUMP_SWAP.to_string(), data)]));
        logs
    };
    let ps1_logs = swap_tx("ps1", pool.buy(wallet("amm1"), 1_000_000_000, now_s * 1_000_000));
    let ps2_logs = swap_tx("ps2", pool.buy(wallet("amm2"), 2_000_000_000, now_s * 1_000_000));
    // --- accounts for pool discovery and mint checks
    let mut accounts = HashMap::new();
    accounts.insert(pool.pool.to_string(), (pool_account(&pool, Pubkey::from_b58(WSOL_MINT).unwrap()), programs::PUMP_SWAP.to_string()));
    let safe_mint = sniper_core::safety::build_mint(None, None, 1_000_000_000_000_000, 6, &[]);
    accounts.insert(old.mint.to_string(), (safe_mint.clone(), sniper_core::safety::SPL_TOKEN.to_string()));
    accounts.insert(mint.to_string(), (safe_mint, sniper_core::safety::SPL_TOKEN.to_string()));
    let world = Arc::new(World { txs, accounts, backfill_sigs: vec!["bf1".into(), "bf2".into()], calls: Mutex::new(vec![]) });
    let rpc_url = mock_rpc(world.clone()).await;
    // --- live stream on provider 2: pump.fun logs (decoded directly) + PumpSwap logs (fetched)
    let live_buy = old.buy(wallet("live1"), 400_000_000, now_s * 1_000_000).unwrap();
    let script = vec![
        logs_note("live1", 1101, &build::pump_logs(&[live_buy])),
        logs_note("ps1", 1102, &ps1_logs),
        logs_note("ps2", 1103, &ps2_logs),
    ];
    let ws_live = mock_ws(script).await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        format!("ws://{a}")
    };
    let mut setup = LiveSetup::new(vec![dead, ws_live], vec![rpc_url], false).unwrap();
    if let Some(b) = setup.backfill.as_mut() {
        b.max_signatures_per_program = 10;
    }
    for w in setup.ws.iter_mut() {
        w.backoff = sniper_core::stream::backoff::Backoff::new(50, 200, None);
    }
    // --- engine (LIVE, memory store)
    let mut cfg = EngineConfig::default();
    cfg.feed_health.stale_tip_ms = 2_000.0;
    let data = Arc::new(Mutex::new(MemData::default()));
    let (ingestor, rx) = Ingestor::new(1024, 10_000);
    let store = spawn_mem(data.clone(), "LIVE:pipe".into(), 4096);
    let mut engine = Engine::new(cfg, EngineOptions { data_mode: DataMode::Live, profile_key: "LIVE:pipe".into(), run_id: "p".into(), jupiter: None, ingest_stats: Some(ingestor.clone()) }, store, None);
    let rpc = setup.rpc().unwrap();
    let fetch_stats = Arc::new(FetchStats::default());
    engine.live = LiveServices { rpc: rpc.clone(), fetch_stats: Some(fetch_stats.clone()), backfill: None };
    engine.boot(json!({})).await;
    let (ctl_tx, ctl_rx) = tokio::sync::mpsc::channel(256);
    engine.set_control_sender(ctl_tx.clone());
    let dash = engine.dash.clone();
    start_sources(setup, rpc, ingestor, ctl_tx.clone(), fetch_stats);
    let handle = tokio::spawn(engine.run(rx, ctl_rx));

    // --- wait for the pipeline to converge
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let ok = loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let s = dash.read().unwrap().clone();
        let done = s.counters.backfill_events >= 2
            && s.counters.pools_resolved >= 1
            && s.tx_fetch.as_ref().is_some_and(|f| f.fetched >= 2)
            && s.counters.mint_checks_ok >= 2
            && s.feed_status.as_ref().is_some_and(|f| f.state == sniper_core::feed::FeedState::Healthy);
        if done {
            break true;
        }
        if tokio::time::Instant::now() > deadline {
            eprintln!("{}", serde_json::to_string_pretty(&json!({"counters": s.counters, "fetch": s.tx_fetch, "feed": s.feed_status, "providers": s.providers})).unwrap());
            break false;
        }
    };
    ctl_tx.send(Control::Shutdown).await.unwrap();
    let engine = handle.await.unwrap();
    assert!(ok, "pipeline did not converge");
    let s = engine.snapshot();
    // failover: provider 1 never connected, provider 2 subscribed and delivered
    let p1 = s.providers.iter().find(|(k, _)| k.starts_with("ws1")).map(|(_, v)| v.clone()).unwrap();
    let p2 = s.providers.iter().find(|(k, _)| k.starts_with("ws2")).map(|(_, v)| v.clone()).unwrap();
    assert_eq!(p1.first_deliveries, 0);
    assert!(p2.first_deliveries >= 1);
    assert!(p2.tip_slot.is_some());
    // backfilled pre-start token has state; no decisions were made on backfill alone
    let pre = engine.universe.tokens.get(&old.mint).expect("pre-start token tracked");
    assert!(pre.buyers.len() >= 2);
    // PumpSwap: first swap unresolved → pool looked up → second swap mapped to the mint
    let amm = engine.universe.tokens.get(&mint).expect("pool discovered and mapped");
    assert_eq!(amm.phase, sniper_core::state::Phase::Amm);
    assert!(amm.pool.unwrap().fees.is_some());
    // latency series were measured from real wall-clock processing
    assert!(s.latency["tx_fetch"].count >= 2);
    assert!(s.latency["slot_lag_slots"].count >= 1);
    assert!(s.status_banner.starts_with("LIVE DATA / PAPER TRADING"));
    let calls = world.calls.lock().unwrap().clone();
    assert!(calls.iter().all(|m| ["getSignaturesForAddress", "getTransaction", "getAccountInfo", "getSlot"].contains(&m.as_str())), "read-only RPC only: {calls:?}");
}
