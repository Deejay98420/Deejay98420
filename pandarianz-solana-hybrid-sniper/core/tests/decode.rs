//! Venue decoder tests. Fixtures are synthesized with the encoders (documented layouts); the
//! discriminator scheme is checked against a published Pump.fun value in unit tests.

use base64::Engine as _;
use sniper_core::decode::pumpfun::{PumpEvent, PumpTrade};
use sniper_core::decode::pumpswap::PumpSwapEvent;
use sniper_core::decode::raydium_v4::RayLog;
use sniper_core::decode::{build, decode_raw, VenueEvent};
use sniper_core::fixtures::{wallet, Curve, Pool};
use sniper_core::stream::{ws, RawEvent};
use sniper_core::types::{programs, Commitment, DataMode, Pubkey};

fn raw(logs: Vec<String>, inner: Vec<sniper_core::stream::InnerIx>, err: Option<String>) -> RawEvent {
    RawEvent { provider: "t".into(), data_mode: DataMode::Fixture, slot: 1, signature: "s".into(), commitment: Commitment::Confirmed, err, logs, inner_ix: inner, block_time: None, recv_us: 0 }
}

#[test]
fn pump_create_trade_complete_migration_roundtrip() {
    let mut c = Curve::new("T1");
    let create = c.create("Name", "SYM", 1_000_000);
    let buy = c.buy(wallet("u"), 500_000_000, 1_000_000).unwrap();
    let complete = c.complete_ev(2_000_000);
    let logs = build::pump_logs(&[create.clone(), buy.clone(), complete.clone()]);
    let out = decode_raw(&raw(logs, vec![], None));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let evs: Vec<_> = out.events.iter().map(|d| d.event.clone()).collect();
    assert_eq!(evs, vec![VenueEvent::Pump(create), VenueEvent::Pump(buy), VenueEvent::Pump(complete)]);
    // indexes are unique within the tx
    let mut idx: Vec<_> = out.events.iter().map(|d| d.index).collect();
    idx.dedup();
    assert_eq!(idx.len(), 3);
}

#[test]
fn legacy_trade_event_without_trailing_fields_decodes() {
    let t = PumpTrade {
        mint: Pubkey::synthetic("m"),
        sol_amount: 1,
        token_amount: 2,
        is_buy: true,
        user: wallet("u"),
        timestamp: 3,
        virtual_sol_reserves: 4,
        virtual_token_reserves: 5,
        real_sol_reserves: None,
        real_token_reserves: None,
        fee_recipient: None,
        fee_basis_points: None,
        fee: None,
        creator: None,
        creator_fee_basis_points: None,
        creator_fee: None,
    };
    let out = decode_raw(&raw(build::pump_logs(&[PumpEvent::Trade(t.clone())]), vec![], None));
    assert_eq!(out.events[0].event, VenueEvent::Pump(PumpEvent::Trade(t)));
}

#[test]
fn pumpswap_emit_cpi_events_decode_from_inner_instructions() {
    let mint = Pubkey::synthetic("mint");
    let mut p = Pool::new("p", mint, 1_000_000_000_000, 50_000_000_000);
    let create = p.create_ev(wallet("c"), 10_000_000);
    let b = p.buy(wallet("b"), 1_000_000_000, 11_000_000);
    let s = p.sell(wallet("s"), 10_000_000_000, 12_000_000);
    let w = p.withdraw(wallet("c"), 0.5, 13_000_000);
    let (logs, inner) = build::pumpswap_inner(&[create.clone(), b.clone(), s.clone(), w.clone()]);
    let out = decode_raw(&raw(logs, inner, None));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let evs: Vec<_> = out.events.into_iter().map(|d| d.event).collect();
    assert_eq!(evs, vec![VenueEvent::PumpSwap(create), VenueEvent::PumpSwap(b), VenueEvent::PumpSwap(s), VenueEvent::PumpSwap(w)]);
}

#[test]
fn pumpswap_events_invisible_in_logs_only_stream() {
    let mut p = Pool::new("p", Pubkey::synthetic("m"), 1_000_000, 1_000_000);
    let (logs, _inner) = build::pumpswap_inner(&[p.buy(wallet("b"), 1000, 1)]);
    // logsSubscribe delivers logs only → no events (documented limitation)
    assert!(decode_raw(&raw(logs, vec![], None)).events.is_empty());
}

#[test]
fn raydium_ray_log_decodes() {
    let l = RayLog::SwapBaseIn { amount_in: 7, minimum_out: 1, direction: 2, user_source: 7, pool_coin: 100, pool_pc: 200, out_amount: 3 };
    let out = decode_raw(&raw(build::raydium_logs(&l), vec![], None));
    assert_eq!(out.events[0].event, VenueEvent::Raydium(l));
    let init = RayLog::Init { time: 1, pc_decimals: 9, coin_decimals: 6, pc_lot_size: 1, coin_lot_size: 1, pc_amount: 10, coin_amount: 20 };
    let out = decode_raw(&raw(build::raydium_logs(&init), vec![], None));
    assert_eq!(out.events[0].event, VenueEvent::Raydium(init));
}

#[test]
fn failed_transactions_produce_no_events() {
    let mut c = Curve::new("F");
    let logs = build::pump_logs(&[c.buy(wallet("u"), 1_000_000, 0).unwrap()]);
    let out = decode_raw(&raw(logs, vec![], Some("{\"InstructionError\":[0,{\"Custom\":1}]}".into())));
    assert!(out.tx_failed);
    assert!(out.events.is_empty());
}

#[test]
fn program_data_is_attributed_by_invoke_stack() {
    let mut c = Curve::new("N");
    let ev = c.buy(wallet("u"), 1_000_000, 0).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    let payload = b64.encode(sniper_core::decode::pumpfun::encode(&ev));
    let router = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
    let logs = vec![
        format!("Program {router} invoke [1]"),
        // router's own data must not be decoded as a Pump event
        format!("Program data: {payload}"),
        format!("Program {} invoke [2]", programs::PUMP_FUN),
        format!("Program data: {payload}"),
        format!("Program {} success", programs::PUMP_FUN),
        format!("Program {router} success"),
    ];
    let out = decode_raw(&raw(logs, vec![], None));
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].event, VenueEvent::Pump(ev));
}

#[test]
fn garbage_and_truncation_are_warnings_not_panics() {
    let logs = vec![
        format!("Program {} invoke [1]", programs::PUMP_FUN),
        "Program data: !!!not-base64!!!".into(),
        // valid discriminator, truncated body
        format!("Program data: {}", base64::engine::general_purpose::STANDARD.encode(&sniper_core::decode::anchor::event_discriminator("TradeEvent")[..])),
        "Log truncated".into(),
    ];
    let out = decode_raw(&raw(logs, vec![], None));
    assert!(out.events.is_empty());
    assert_eq!(out.warnings.len(), 3, "{:?}", out.warnings);
}

#[test]
fn ws_logs_notification_parses() {
    let mut c = Curve::new("W");
    let logs = build::pump_logs(&[c.buy(wallet("u"), 1_000_000, 0).unwrap()]);
    let msg = serde_json::json!({"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":42},"value":{"signature":"abc","err":null,"logs":logs}},"subscription":1}});
    let ev = ws::parse_notification(&msg.to_string(), "p", Commitment::Confirmed, 7).unwrap();
    assert_eq!(ev.slot, 42);
    assert_eq!(ev.data_mode, DataMode::Live);
    assert_eq!(decode_raw(&ev).events.len(), 1);
    // subscription acks are not events
    assert!(ws::parse_notification(r#"{"jsonrpc":"2.0","result":5,"id":1}"#, "p", Commitment::Confirmed, 0).is_none());
}

#[test]
fn ws_transaction_notification_with_inner_instructions_parses() {
    let mut p = Pool::new("x", Pubkey::synthetic("m"), 1_000_000_000, 1_000_000_000);
    let ev = p.buy(wallet("b"), 1_000_000, 5_000_000);
    let mut data = sniper_core::decode::anchor::EVENT_IX_TAG.to_vec();
    data.extend(sniper_core::decode::pumpswap::encode(&ev));
    let msg = serde_json::json!({"jsonrpc":"2.0","method":"transactionNotification","params":{"result":{
        "signature":"sig1","slot":9,
        "transaction":{"blockTime": 5, "meta":{"err":null,"logMessages":[],"innerInstructions":[{"index":0,"instructions":[
            {"programId": programs::PUMP_SWAP, "data": bs58::encode(&data).into_string()}]}]},
        "transaction":{"signatures":["sig1"]}}}}});
    let raw = ws::parse_notification(&msg.to_string(), "p", Commitment::Confirmed, 1).unwrap();
    let out = decode_raw(&raw);
    assert_eq!(out.events.len(), 1);
    assert!(matches!(out.events[0].event, VenueEvent::PumpSwap(PumpSwapEvent::Swap(_))));
    assert_eq!(raw.block_time, Some(5));
}

#[test]
fn redacts_api_keys_from_urls() {
    assert_eq!(ws::redact_url("wss://mainnet.example-rpc.com/?api-key=SECRET123"), "wss://mainnet.example-rpc.com");
    assert!(!ws::redact_url("https://x.io/v1/SECRET").contains("SECRET"));
}

#[test]
fn fixture_loader_never_produces_live_data() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.jsonl");
    let mut msgs = sniper_core::fixtures::hybrid_session();
    // even if a fixture file claims LIVE, the loader relabels it
    if let sniper_core::stream::StreamMsg::Event(e) = &mut msgs[0] {
        e.data_mode = DataMode::Live;
    }
    std::fs::write(&p, sniper_core::fixtures::to_jsonl(&msgs)).unwrap();
    let loaded = sniper_core::stream::fixture::load_jsonl(&p, DataMode::Fixture).unwrap();
    assert!(loaded.iter().all(|m| match m {
        sniper_core::stream::StreamMsg::Event(e) => e.data_mode == DataMode::Fixture,
        _ => true,
    }));
}
