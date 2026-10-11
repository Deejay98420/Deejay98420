//! V1.1 live-mode engine behaviour, driven directly through `handle_stream` / `handle_control`.

mod common;
use sniper_core::config::{EngineConfig, ScenarioConfig};
use sniper_core::decode::pumpfun::PumpEvent;
use sniper_core::decode::VenueEvent;
use sniper_core::engine::{Control, Engine, EngineOptions};
use sniper_core::execcheck::ExecChecker;
use sniper_core::fixtures::{hybrid_session, wallet, Curve, Pool};
use sniper_core::paper::{fill_order_with, Book, FillBasisMode, OrderKind, OrderStatus};
use sniper_core::quote::{venue_quote, Quote, QuoteSource, Side, VenueFees};
use sniper_core::state::{Input, TradeObs, Universe};
use sniper_core::store::{spawn_mem, MemData};
use sniper_core::stream::{ConnState, RawEvent, StreamMsg};
use sniper_core::types::{DataMode, Pubkey, Venue};
use std::sync::{Arc, Mutex};

const S: i64 = 1_000_000;

fn live_engine(cfg: EngineConfig, data: Arc<Mutex<MemData>>) -> Engine {
    let key = "LIVE:test".to_string();
    let store = spawn_mem(data, key.clone(), 4096);
    Engine::new(cfg, EngineOptions { data_mode: DataMode::Live, profile_key: key, run_id: "t".into(), jupiter: None, ingest_stats: None }, store, None)
}

/// Re-label synthetic fixture events as if delivered by a live provider at `now` (wall clock).
fn as_live(msgs: Vec<StreamMsg>, base_now: i64) -> Vec<StreamMsg> {
    let t0 = msgs.iter().map(|m| m.recv_us()).min().unwrap_or(0);
    msgs.into_iter()
        .filter_map(|m| match m {
            StreamMsg::Event(mut e) => {
                e.data_mode = DataMode::Live;
                e.provider = "ws1".into();
                e.recv_us = base_now + (e.recv_us - t0);
                Some(StreamMsg::Event(e))
            }
            _ => None,
        })
        .collect()
}

fn launch_msgs(label: &str) -> Vec<StreamMsg> {
    let mut tl = sniper_core::fixtures::Timeline::new();
    sniper_core::fixtures::scenario_organic(&mut tl, sniper_core::fixtures::T0_US, label, 9);
    tl.sorted()
}

fn reject_codes(d: &MemData) -> Vec<String> {
    d.evaluations.iter().flat_map(|e| e.reasons.as_array().cloned().unwrap_or_default()).filter_map(|r| r["code"].as_str().map(String::from)).collect()
}

#[tokio::test]
async fn live_entries_blocked_while_feed_unhealthy_and_gap_recorded() {
    let mut cfg = EngineConfig::default();
    cfg.strategy.entry.require_mint_check = false;
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = live_engine(cfg, data.clone());
    e.boot(serde_json::json!({})).await;
    // no provider connected: every evaluation must carry feed_unhealthy, no orders submitted
    let now = sniper_core::time::wall_us();
    for m in as_live(launch_msgs("GATE"), now - 200 * S).into_iter().take(60) {
        e.handle_stream(m).await;
    }
    e.flush().await;
    {
        let d = data.lock().unwrap();
        assert!(reject_codes(&d).iter().any(|c| c == "feed_unhealthy"));
        assert!(d.submissions.is_empty(), "no entries while the feed is not HEALTHY");
    }
    // provider connects and data flows → HEALTHY → the startup gap closes and is persisted
    e.handle_stream(StreamMsg::Connection { provider: "ws1".into(), state: ConnState::Subscribed, detail: String::new(), recv_us: now }).await;
    let mut c = Curve::new("FRESH");
    let ev = c.buy(wallet("x"), 100_000_000, now).unwrap();
    let raw = RawEvent {
        provider: "ws1".into(),
        data_mode: DataMode::Live,
        slot: 1,
        signature: "fresh-1".into(),
        commitment: sniper_core::types::Commitment::Confirmed,
        err: None,
        logs: sniper_core::decode::build::pump_logs(&[ev]),
        inner_ix: vec![],
        block_time: None,
        recv_us: sniper_core::time::wall_us(),
        backfill: false,
        fetch_us: None,
    };
    e.handle_stream(StreamMsg::Event(raw)).await;
    assert_eq!(e.feed_mon.state(), sniper_core::feed::FeedState::Healthy);
    e.flush().await;
    let d = data.lock().unwrap();
    assert_eq!(d.gaps.len(), 1, "the not-healthy startup interval is recorded as an observation gap");
    assert!(d.gaps[0].end_us.is_some());
}

#[tokio::test]
async fn mint_safety_gates_entries_in_live_mode() {
    let cfg = EngineConfig::default(); // require_mint_check = true
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = live_engine(cfg, data.clone());
    e.boot(serde_json::json!({})).await;
    let msgs = as_live(launch_msgs("MINT"), sniper_core::time::wall_us() - 100 * S);
    e.handle_stream(StreamMsg::Connection { provider: "ws1".into(), state: ConnState::Subscribed, detail: String::new(), recv_us: 0 }).await;
    for m in msgs.iter().take(30).cloned() {
        e.handle_stream(m).await;
    }
    e.flush().await;
    assert!(reject_codes(&data.lock().unwrap()).iter().any(|c| c == "mint_unchecked"));
    // a risky mint (freeze authority) → mint_risk
    let mint = Pubkey::synthetic("mint:MINT");
    let risky = sniper_core::safety::check_mint(&sniper_core::safety::build_mint(None, Some(Pubkey::synthetic("f")), 1, 6, &[]), sniper_core::safety::SPL_TOKEN);
    e.handle_control(Control::MintChecked { mint, result: risky }).await;
    // live evaluations are throttled on the wall clock (min_eval_interval_ms)
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    for m in msgs.iter().skip(30).take(20).cloned() {
        e.handle_stream(m).await;
    }
    e.flush().await;
    assert!(reject_codes(&data.lock().unwrap()).iter().any(|c| c == "mint_risk"));
    assert_eq!(e.snapshot().counters.mint_checks_risky, 1);
}

#[tokio::test]
async fn backfill_events_build_state_but_never_trigger_decisions() {
    let mut cfg = EngineConfig::default();
    cfg.strategy.entry.require_mint_check = false;
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = live_engine(cfg, data.clone());
    e.boot(serde_json::json!({})).await;
    let now = sniper_core::time::wall_us();
    let msgs = as_live(launch_msgs("BACK"), now - 300 * S);
    for m in msgs.iter().cloned() {
        if let StreamMsg::Event(mut ev) = m {
            ev.backfill = true;
            e.handle_stream(StreamMsg::Event(ev)).await;
        }
    }
    e.flush().await;
    assert!(e.universe.tokens.contains_key(&Pubkey::synthetic("mint:BACK")), "state rebuilt from backfill");
    assert_eq!(e.evaluations, 0, "no decisions on historical data");
    assert_eq!(e.snapshot().counters.backfill_events as usize, msgs.len());
    assert!(data.lock().unwrap().submissions.is_empty());
}

#[test]
fn out_of_order_events_never_regress_reserves() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("OOO");
    u.apply(Input { slot: 10, signature: "c".into(), index: 0, ts_us: 0, event: VenueEvent::Pump(c.create("O", "O", 0)) });
    let old = c.buy(wallet("a"), 1_000_000_000, S).unwrap();
    let new = c.buy(wallet("b"), 1_000_000_000, 2 * S).unwrap();
    u.apply(Input { slot: 12, signature: "n".into(), index: 0, ts_us: 2 * S, event: VenueEvent::Pump(new) });
    let after_new = u.tokens[&c.mint].curve.unwrap();
    u.apply(Input { slot: 11, signature: "o".into(), index: 0, ts_us: S, event: VenueEvent::Pump(old) });
    let st = &u.tokens[&c.mint];
    assert_eq!(st.curve.unwrap(), after_new, "older event must not overwrite newer reserves");
    assert_eq!(st.out_of_order_inputs, 1);
    assert_eq!(st.buyers.len(), 2, "but its flow is still counted");
    assert!(st.trades.iter().zip(st.trades.iter().skip(1)).all(|(a, b)| a.ts_us <= b.ts_us));
}

#[test]
fn non_sol_quoted_curves_are_never_tradeable() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("USDQ");
    let mut create = c.create("U", "U", 0);
    if let PumpEvent::Create(cr) = &mut create {
        cr.tail.as_mut().unwrap().quote_mint = Pubkey::from_b58("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
    }
    u.apply(Input { slot: 1, signature: "c".into(), index: 0, ts_us: 0, event: VenueEvent::Pump(create) });
    u.apply(Input { slot: 2, signature: "b".into(), index: 0, ts_us: S, event: VenueEvent::Pump(c.buy(wallet("a"), 1_000_000, S).unwrap()) });
    let st = &u.tokens[&c.mint];
    assert!(st.quote_non_sol);
    assert!(st.venue().is_none());
    let fees = VenueFees { curve_fee_bps_default: 125, pumpswap_fee_bps: 30 };
    assert!(venue_quote(st, Side::Buy, 1_000, 0, &fees, 1e9, S).is_err());
    let f = sniper_core::state::analytics::compute(st, S);
    let d = sniper_core::strategy::entry::evaluate(st, &f, &Default::default(), &Default::default(), &ScenarioConfig::base(), None, S);
    assert!(d.reasons.iter().any(|r| r.code() == "non_sol_quote"));
}

#[test]
fn pumpswap_quotes_use_virtual_reserves_and_observed_fees() {
    let mut u = Universe::new(0);
    let mint = Pubkey::synthetic("vq");
    let mut p = Pool::new("vq", mint, 1_000_000_000_000, 40_000_000_000);
    u.apply(Input { slot: 1, signature: "c".into(), index: 0, ts_us: 0, event: VenueEvent::PumpSwap(p.create_ev(wallet("c"), 0)) });
    let mut ev = p.buy(wallet("b"), 1_000_000_000, S);
    if let sniper_core::decode::pumpswap::PumpSwapEvent::Swap(s) = &mut ev {
        s.virtual_quote_reserves = Some(10_000_000_000);
    }
    u.apply(Input { slot: 2, signature: "b".into(), index: 0, ts_us: S, event: VenueEvent::PumpSwap(ev) });
    let st = &u.tokens[&mint];
    let pool = st.pool.unwrap();
    assert_eq!(pool.virtual_quote, 10_000_000_000);
    assert_eq!(pool.quote_eff(), pool.quote + 10_000_000_000);
    assert_eq!(pool.fees.unwrap().total(), 30, "lp 20 + protocol 5 + creator 5 from the event");
    let fees = VenueFees { curve_fee_bps_default: 125, pumpswap_fee_bps: 999 };
    let q = venue_quote(st, Side::Sell, 1_000_000_000, 0, &fees, 1e9, S).unwrap();
    let expected = sniper_core::quote::math::amm_sell(pool.base, pool.quote_eff(), pool.quote, 1_000_000_000, pool.fees.unwrap()).unwrap();
    assert_eq!(q.out_amount, expected.amount_out);
    assert_eq!(q.venue, Some(Venue::PumpSwap));
}

#[test]
fn conservative_fill_uses_the_worse_of_simulation_and_live_aggregator_quote() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("AGG");
    u.apply(Input { slot: 1, signature: "c".into(), index: 0, ts_us: 0, event: VenueEvent::Pump(c.create("A", "A", 0)) });
    for i in 0..10 {
        u.apply(Input { slot: 2, signature: format!("b{i}"), index: 0, ts_us: S, event: VenueEvent::Pump(c.buy(wallet(&format!("w{i}")), 500_000_000, S).unwrap()) });
    }
    let fees = VenueFees { curve_fee_bps_default: 125, pumpswap_fee_bps: 30 };
    let st = &u.tokens[&c.mint];
    let q = venue_quote(st, Side::Buy, 50_000_000, 0, &fees, 1e9, S).unwrap();
    let mut agg: Quote = q.clone();
    agg.source = QuoteSource::Jupiter;
    agg.out_amount = q.out_amount * 97 / 100; // aggregator offers 3% fewer tokens
    agg.quoted_at_us = 2 * S;
    let scen = ScenarioConfig { drop_probability: 0.0, adverse_slippage_bps: 0, ..ScenarioConfig::base() };
    let mk = |obs: Option<Quote>| {
        let mut o = sniper_core::paper::Order {
            order_id: "ord-x".into(),
            idempotency_key: "k".into(),
            book_id: "b".into(),
            scenario: "base".into(),
            data_mode: DataMode::Live,
            strategy_version: "v".into(),
            kind: OrderKind::Entry,
            side: Side::Buy,
            mint: c.mint,
            path: sniper_core::strategy::EntryPath::Launch,
            position_id: None,
            amount: 50_000_000,
            slippage_bps: 3000,
            min_out: 1,
            decision_quote: Some(q.clone()),
            decision_us: S,
            fill_at_us: 2 * S,
            trigger_signature: "t".into(),
            trigger_slot: 2,
            attempt: 1,
            reason: String::new(),
            status: OrderStatus::Pending,
            fill: None,
            fill_quote: None,
            costs: Default::default(),
            error: None,
            completed_us: None,
            observed_quote: None,
            fill_basis: None,
            aggregator_divergence_bps: None,
            evidence: None,
        };
        o.observed_quote = obs;
        o
    };
    let mut book = Book::new("b".into(), scen.clone(), 1_000_000_000);
    let mut o = mk(Some(agg.clone()));
    fill_order_with(&mut o, Some(st), &mut book, &fees, &Default::default(), &Default::default(), 1e9, FillBasisMode::Conservative, 2 * S);
    assert_eq!(o.fill_basis.as_deref(), Some("conservative:aggregator"));
    assert!(o.aggregator_divergence_bps.unwrap() > 250.0);
    let conservative_out = o.fill.unwrap().out_amount;
    let mut book2 = Book::new("b".into(), scen, 1_000_000_000);
    let mut o2 = mk(None);
    fill_order_with(&mut o2, Some(st), &mut book2, &fees, &Default::default(), &Default::default(), 1e9, FillBasisMode::Conservative, 2 * S);
    assert_eq!(o2.fill_basis.as_deref(), Some("venue_sim"));
    assert!(conservative_out < o2.fill.unwrap().out_amount);
    // stale state at fill time → no fill invented
    let mut book3 = Book::new("b".into(), ScenarioConfig::base(), 1_000_000_000);
    let mut o3 = mk(None);
    fill_order_with(&mut o3, Some(st), &mut book3, &fees, &Default::default(), &Default::default(), 100.0, FillBasisMode::Conservative, 60 * S);
    assert_eq!(o3.status, OrderStatus::NoRoute);
    assert!(o3.error.unwrap().contains("stale"));
}

#[test]
fn execution_checks_never_look_ahead() {
    let mut x = ExecChecker::new(5.0);
    let mint = Pubkey::synthetic("m");
    let mut o = sniper_core::paper::Order {
        order_id: "o1".into(),
        idempotency_key: "k".into(),
        book_id: "b".into(),
        scenario: "base".into(),
        data_mode: DataMode::Live,
        strategy_version: "v".into(),
        kind: OrderKind::Entry,
        side: Side::Buy,
        mint,
        path: sniper_core::strategy::EntryPath::Launch,
        position_id: None,
        amount: 100,
        slippage_bps: 0,
        min_out: 0,
        decision_quote: None,
        decision_us: 0,
        fill_at_us: 0,
        trigger_signature: String::new(),
        trigger_slot: 0,
        attempt: 1,
        reason: String::new(),
        status: OrderStatus::Filled,
        fill: Some(sniper_core::paper::Fill { in_amount: 110, out_amount: 100, latency_ms: 0.0, quote_age_ms: 0.0 }),
        fill_quote: None,
        costs: Default::default(),
        error: None,
        completed_us: None,
        observed_quote: None,
        fill_basis: None,
        aggregator_divergence_bps: None,
        evidence: None,
    };
    let t = |ts: i64, is_buy: bool, quote: u64| TradeObs { ts_us: ts, slot: 1, user: wallet("u"), is_buy, sol: quote, tokens: 100, price_after: 1.0, venue: Venue::PumpBondingCurve, user_quote: quote };
    let mut trades = std::collections::VecDeque::new();
    trades.push_back(t(8 * S, true, 100)); // prior buy at 1.00/token
    trades.push_back(t(9 * S, false, 90)); // prior sell (other side)
    trades.push_back(t(11 * S, true, 150)); // FUTURE buy at 1.50 — must never be used
    let c = x.check(&o, 10 * S, &trades).unwrap();
    assert_eq!(c.outcome, "compared_prior");
    assert!((c.market_price.unwrap() - 1.0).abs() < 1e-9, "uses the latest PRIOR same-side trade, never a future one");
    assert!((c.adverse_bps.unwrap() - 1000.0).abs() < 1e-6, "we paid 1.10 vs prior market 1.00 → +1000 bps");
    assert_eq!(c.market_trade_age_ms, Some(2000.0));
    // only a future trade exists → no comparison at all
    let only_future: std::collections::VecDeque<_> = [t(11 * S, true, 150)].into_iter().collect();
    assert_eq!(x.check(&o, 10 * S, &only_future).unwrap().outcome, "no_prior_trade");
    // too old for the window
    let stale: std::collections::VecDeque<_> = [t(2 * S, true, 100)].into_iter().collect();
    assert_eq!(x.check(&o, 10 * S, &stale).unwrap().outcome, "no_prior_trade");
    o.order_id = "o2".into();
}

#[tokio::test]
async fn forward_evaluation_records_outcomes_and_cannot_change_decisions() {
    let run = |enabled: bool| async move {
        let mut cfg = EngineConfig::default();
        cfg.forward.enabled = enabled;
        let data = Arc::new(Mutex::new(MemData::default()));
        let mut e = common::engine_with(cfg, data.clone(), "fwd");
        e.boot(serde_json::json!({})).await;
        common::feed(&mut e, hybrid_session()).await;
        e.flush().await;
        let d = data.lock().unwrap();
        let decisions: Vec<(String, String, i64)> = d.evaluations.iter().map(|x| (x.mint.clone(), x.decision.clone(), x.ts_us)).collect();
        (decisions, d.forward.clone(), d.orders.len())
    };
    let (dec_on, fwd, orders_on) = run(true).await;
    let (dec_off, none, orders_off) = run(false).await;
    assert_eq!(dec_on, dec_off, "forward evaluation must not influence decisions (no future information)");
    assert_eq!(orders_on, orders_off);
    assert!(none.is_empty());
    assert!(fwd.iter().any(|o| o.decision == "reject") && fwd.iter().any(|o| o.decision == "enter"));
    for o in &fwd {
        assert!(o.t_us >= o.t0_us + o.horizon_s as i64 * S, "outcome written only after its horizon");
        if let (Some(out), Some(net)) = (o.exit_out, o.net_lamports) {
            assert!(net < out as i64 - o.entry_in as i64 + 1, "fixed costs are subtracted");
        }
    }
}
