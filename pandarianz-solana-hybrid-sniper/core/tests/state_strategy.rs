//! Token state, analytics, rollback, entry filters/sizing and dynamic exits.

use sniper_core::config::{CostConfig, ExitPolicy, ScenarioConfig, StrategyConfig};
use sniper_core::decode::pumpfun::PumpEvent;
use sniper_core::decode::VenueEvent;
use sniper_core::fixtures::{wallet, Curve, Pool};
use sniper_core::quote::{venue_quote, QuoteError, Side, VenueFees};
use sniper_core::state::analytics::compute;
use sniper_core::state::{Input, Phase, Universe};
use sniper_core::strategy::entry::evaluate;
use sniper_core::strategy::exit::{exit_signal, ExitReason, PositionView, Urgency};
use sniper_core::strategy::{EntryPath, RejectReason};
use sniper_core::types::Pubkey;

const S: i64 = 1_000_000;

fn inp(slot: u64, ts: i64, ev: VenueEvent) -> Input {
    Input { slot, signature: format!("s{slot}-{ts}"), index: 0, ts_us: ts, event: ev }
}

fn fees() -> VenueFees {
    VenueFees { curve_fee_bps_default: 125, pumpswap_fee_bps: 30 }
}

/// A healthy launch: create, then `n` distinct buyers over ~n*0.5s, with a few non-creator sells.
fn healthy_launch(u: &mut Universe, label: &str, n: usize, size: u64) -> (Pubkey, i64) {
    let mut c = Curve::new(label);
    let creator = c.creator;
    u.apply(inp(1, 0, VenueEvent::Pump(c.create(label, label, 0))));
    u.apply(inp(1, 0, VenueEvent::Pump(c.buy(creator, 200_000_000, 0).unwrap())));
    let mut t = S;
    for i in 0..n {
        let ev = c.buy(wallet(&format!("{label}-{i}")), size, t).unwrap();
        u.apply(inp(2 + i as u64, t, VenueEvent::Pump(ev)));
        if i % 7 == 6 {
            let ev = c.sell(wallet(&format!("{label}-{}", i - 3)), 1_000_000_000, t + 1);
            if let Some(ev) = ev {
                u.apply(inp(2 + i as u64, t + 1, VenueEvent::Pump(ev)));
            }
        }
        t += S / 2;
    }
    (c.mint, t)
}

#[test]
fn reserves_buyers_and_creator_tracking() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("A");
    let creator = c.creator;
    u.apply(inp(1, 0, VenueEvent::Pump(c.create("A", "A", 0))));
    u.apply(inp(1, 0, VenueEvent::Pump(c.buy(creator, 1_000_000_000, 0).unwrap())));
    u.apply(inp(2, S, VenueEvent::Pump(c.buy(wallet("x"), 500_000_000, S).unwrap())));
    let st = &u.tokens[&c.mint];
    let cr = st.curve.unwrap();
    assert_eq!((cr.virtual_sol, cr.virtual_token, cr.real_sol, cr.real_token), (c.vs, c.vt, c.rs, c.rt));
    assert_eq!(st.buyers.len(), 2);
    assert_eq!(st.observed_curve_fee_bps, Some(100));
    let tokens = st.creator_bought;
    u.apply(inp(3, 2 * S, VenueEvent::Pump(c.sell(creator, tokens / 2, 2 * S).unwrap())));
    let f = compute(&u.tokens[&c.mint], 2 * S);
    assert!((f.creator_sold_fraction - 0.5).abs() < 1e-9);
    assert!(!f.non_creator_sell_seen, "creator sells do not prove sellability");
}

#[test]
fn rollback_of_dead_slot_restores_exact_state() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("R");
    u.apply(inp(10, 0, VenueEvent::Pump(c.create("R", "R", 0))));
    u.apply(inp(11, S, VenueEvent::Pump(c.buy(wallet("a"), 1_000_000_000, S).unwrap())));
    let before = u.tokens[&c.mint].curve.unwrap();
    let buyers_before = u.tokens[&c.mint].buyers.len();
    u.apply(inp(12, 2 * S, VenueEvent::Pump(c.buy(wallet("fork"), 3_000_000_000, 2 * S).unwrap())));
    assert_ne!(u.tokens[&c.mint].curve.unwrap(), before);
    let affected = u.rollback_slot(12);
    assert_eq!(affected, vec![c.mint]);
    let st = &u.tokens[&c.mint];
    assert_eq!(st.curve.unwrap(), before);
    assert_eq!(st.buyers.len(), buyers_before);
    assert_eq!(st.rolled_back_slots, 1);
    // rolling back the creation slot removes the token entirely
    let mut u2 = Universe::new(0);
    let c2 = Curve::new("R2");
    u2.apply(inp(5, 0, VenueEvent::Pump(c2.create("R2", "R2", 0))));
    u2.rollback_slot(5);
    assert!(u2.tokens.is_empty());
}

#[test]
fn pool_mapping_unresolved_and_non_sol_pools() {
    let mut u = Universe::new(0);
    let mint = Pubkey::synthetic("pm");
    let mut p = Pool::new("pm", mint, 1_000_000_000_000, 40_000_000_000);
    // swap before the pool is known → unresolved, counted
    let early = p.buy(wallet("e"), 100_000_000, 0);
    assert!(u.apply(inp(1, 0, VenueEvent::PumpSwap(early))).is_none());
    assert_eq!(u.unresolved_pool_events, 1);
    let mut p = Pool::new("pm", mint, 1_000_000_000_000, 40_000_000_000);
    u.apply(inp(2, S, VenueEvent::PumpSwap(p.create_ev(wallet("c"), S))));
    u.apply(inp(3, 2 * S, VenueEvent::PumpSwap(p.buy(wallet("b"), 1_000_000_000, 2 * S))));
    let st = &u.tokens[&mint];
    assert_eq!(st.phase, Phase::Amm);
    assert_eq!((st.pool.unwrap().base, st.pool.unwrap().quote), (p.base, p.quote));
    u.apply(inp(4, 3 * S, VenueEvent::PumpSwap(p.withdraw(wallet("c"), 0.5, 3 * S))));
    assert!(u.tokens[&mint].liquidity_removed_lamports > 0);
    assert_eq!(u.tokens[&mint].pool.unwrap().quote, p.quote);
    // non-SOL quoted pool is ignored
    let mut ev = Pool::new("usdc", Pubkey::synthetic("m2"), 1, 1).create_ev(wallet("c"), 0);
    if let sniper_core::decode::pumpswap::PumpSwapEvent::CreatePool(cp) = &mut ev {
        cp.quote_mint = Pubkey::synthetic("not-wsol");
    }
    assert!(u.apply(inp(5, 0, VenueEvent::PumpSwap(ev))).is_none());
    assert_eq!(u.non_sol_pools_ignored, 1);
}

#[test]
fn curve_completion_removes_route_until_migration() {
    let mut u = Universe::new(0);
    let c = Curve::new("M");
    u.apply(inp(1, 0, VenueEvent::Pump(c.create("M", "M", 0))));
    u.apply(inp(2, S, VenueEvent::Pump(c.complete_ev(S))));
    let st = &u.tokens[&c.mint];
    assert_eq!(st.phase, Phase::AwaitingMigration);
    let q = venue_quote(st, Side::Sell, 1_000, 0, &fees(), 1e9, S);
    assert!(matches!(q, Err(QuoteError::NoRoute(_))));
}

#[test]
fn stale_state_blocks_quotes_and_entries() {
    let mut u = Universe::new(0);
    let (mint, t) = healthy_launch(&mut u, "ST", 30, 600_000_000);
    let st = &u.tokens[&mint];
    let late = t + 60 * S;
    assert!(matches!(venue_quote(st, Side::Buy, 1, 0, &fees(), 4_000.0, late), Err(QuoteError::StaleState(_))));
    let f = compute(st, late);
    let d = evaluate(st, &f, &StrategyConfig::default(), &CostConfig::default(), &ScenarioConfig::base(), None, late);
    assert!(!d.enter);
    assert!(d.reasons.iter().any(|r| matches!(r, RejectReason::StaleState { .. })));
}

#[test]
fn healthy_launch_enters_with_size_chosen_by_liquidity_and_score() {
    let mut u = Universe::new(0);
    let (mint, t) = healthy_launch(&mut u, "OK", 40, 700_000_000);
    let st = &u.tokens[&mint];
    let f = compute(st, t);
    let mut cfg = StrategyConfig::default();
    let d = evaluate(st, &f, &cfg, &CostConfig::default(), &ScenarioConfig::base(), None, t);
    assert_eq!(d.path, EntryPath::Launch);
    assert!(d.enter, "reasons: {:?}", d.reasons);
    assert!([20_000_000, 50_000_000, 100_000_000].contains(&d.size_lamports));
    let q = d.quote.as_ref().unwrap();
    assert_eq!(q.in_amount, d.size_lamports);
    assert!(q.price_impact_bps > 0.0, "a real size moves the price");
    // tighter liquidity cap forces a smaller size
    cfg.entry.max_size_fraction_of_liquidity = 0.06 * 20_000_000f64 / f.liquidity_lamports as f64 * 10.0;
    let d2 = evaluate(st, &f, &cfg, &CostConfig::default(), &ScenarioConfig::base(), None, t);
    if d2.enter {
        assert!(d2.size_lamports <= d.size_lamports);
    }
    cfg.entry.max_size_fraction_of_liquidity = 1e-6;
    let d3 = evaluate(st, &f, &cfg, &CostConfig::default(), &ScenarioConfig::base(), None, t);
    assert!(!d3.enter);
    assert!(d3.reasons.iter().any(|r| matches!(r, RejectReason::SizeExceedsLiquidityCap { .. })));
}

#[test]
fn rejection_reasons_are_preserved_and_specific() {
    // bundled launch: many buyers in the creation slot
    let mut u = Universe::new(0);
    let mut c = Curve::new("B");
    u.apply(inp(1, 0, VenueEvent::Pump(c.create("B", "B", 0))));
    for i in 0..10 {
        u.apply(inp(1, 0, VenueEvent::Pump(c.buy(wallet(&format!("bundle{i}")), 1_000_000_000, 0).unwrap())));
    }
    let st = &u.tokens[&c.mint];
    let f = compute(st, S);
    let d = evaluate(st, &f, &StrategyConfig::default(), &CostConfig::default(), &ScenarioConfig::base(), None, S);
    let codes: Vec<String> = d.reasons.iter().map(|r| r.code()).collect();
    assert!(codes.contains(&"bundled_launch".to_string()), "{codes:?}");
    assert!(codes.contains(&"too_early".to_string()));
    assert!(codes.contains(&"unsellable".to_string()), "no non-creator sell seen yet");
    // round-trip costs: a tiny curve with a huge minimum makes costs dominate
    let mut cfg = StrategyConfig::default();
    cfg.entry.max_round_trip_cost_bps = 1.0;
    let mut u = Universe::new(0);
    let (mint, t) = healthy_launch(&mut u, "C", 40, 700_000_000);
    let st = &u.tokens[&mint];
    let d = evaluate(st, &compute(st, t), &cfg, &CostConfig::default(), &ScenarioConfig::base(), None, t);
    assert!(d.reasons.iter().any(|r| matches!(r, RejectReason::RoundTripCostTooHigh { .. })));
    assert!(!d.size_checks.is_empty());
}

#[test]
fn discovered_token_uses_momentum_path() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("D");
    for i in 0..5 {
        let _ = c.buy(wallet(&format!("pre{i}")), 1_000_000_000, 0);
    }
    u.apply(inp(1, 0, VenueEvent::Pump(c.buy(wallet("first-seen"), 100_000_000, 0).unwrap())));
    let st = &u.tokens[&c.mint];
    let f = compute(st, S);
    let d = evaluate(st, &f, &StrategyConfig::default(), &CostConfig::default(), &ScenarioConfig::base(), None, S);
    assert_eq!(d.path, EntryPath::Momentum);
    assert!(!f.concentration_complete);
}

fn pv(cost: u64, peak: u64, peak_us: i64, liq: u64) -> PositionView {
    PositionView { opened_us: 0, cost_basis_lamports: cost, peak_value_lamports: peak, peak_us, entry_liquidity_lamports: liq, entry_creator_sold_fraction: 0.0, exit_impact_bps: None }
}

#[test]
fn no_fixed_take_profit_strong_trend_keeps_position() {
    let mut u = Universe::new(0);
    let (mint, t) = healthy_launch(&mut u, "TP", 60, 900_000_000);
    let st = &u.tokens[&mint];
    let f = compute(st, t);
    let cfg = ExitPolicy::default();
    // +400% and still at the peak with strong flow → no exit signal
    let s = exit_signal(&f, &pv(100_000_000, 500_000_000, t, st.liquidity_lamports()), Some(500_000_000), &cfg, t);
    assert!(s.is_none(), "{s:?}");
    // same position after a drawdown from peak beyond the dynamic width → trailing stop
    let s = exit_signal(&f, &pv(100_000_000, 500_000_000, t, st.liquidity_lamports()), Some(300_000_000), &cfg, t);
    assert!(matches!(s, Some((ExitReason::TrailingStop { .. }, Urgency::Normal))), "{s:?}");
}

#[test]
fn rug_warnings_trigger_emergency_exit_and_migration_does_not() {
    let mut u = Universe::new(0);
    let mut c = Curve::new("RG");
    let creator = c.creator;
    u.apply(inp(1, 0, VenueEvent::Pump(c.create("RG", "RG", 0))));
    let PumpEvent::Trade(t0) = c.buy(creator, 1_000_000_000, 0).unwrap() else { panic!() };
    let creator_tokens = t0.token_amount;
    u.apply(inp(1, 0, VenueEvent::Pump(PumpEvent::Trade(t0))));
    for i in 0..10 {
        u.apply(inp(2 + i, S, VenueEvent::Pump(c.buy(wallet(&format!("w{i}")), 500_000_000, S).unwrap())));
    }
    let liq = u.tokens[&c.mint].liquidity_lamports();
    u.apply(inp(20, 2 * S, VenueEvent::Pump(c.sell(creator, creator_tokens, 2 * S).unwrap())));
    let f = compute(&u.tokens[&c.mint], 2 * S);
    let s = exit_signal(&f, &pv(50_000_000, 60_000_000, 2 * S, liq), Some(55_000_000), &ExitPolicy::default(), 2 * S);
    assert!(matches!(s, Some((ExitReason::RugWarning { .. }, Urgency::Emergency))), "{s:?}");
    // curve completion: no exit signal (hold for migration route)
    u.apply(inp(21, 3 * S, VenueEvent::Pump(c.complete_ev(3 * S))));
    let f = compute(&u.tokens[&c.mint], 3 * S);
    let mut view = pv(50_000_000, 60_000_000, 3 * S, liq);
    view.entry_creator_sold_fraction = f.creator_sold_fraction;
    assert!(exit_signal(&f, &view, None, &ExitPolicy::default(), 3 * S).is_none());
}

#[test]
fn stagnation_and_max_hold_are_time_based() {
    let mut u = Universe::new(0);
    let (mint, t) = healthy_launch(&mut u, "TM", 40, 600_000_000);
    let st = &u.tokens[&mint];
    let cfg = ExitPolicy::default();
    let later = t + (cfg.stagnation_s as i64 + 1) * S;
    let f = compute(st, later);
    let s = exit_signal(&f, &pv(100_000_000, 101_000_000, t, st.liquidity_lamports()), Some(100_500_000), &cfg, later);
    assert!(matches!(s, Some((ExitReason::Stagnation { .. }, _))), "{s:?}");
}

#[test]
fn sell_quote_models_own_position_impact() {
    let mut u = Universe::new(0);
    let (mint, t) = healthy_launch(&mut u, "OI", 30, 600_000_000);
    let st = &u.tokens[&mint];
    let buy = venue_quote(st, Side::Buy, 100_000_000, 0, &fees(), 1e9, t).unwrap();
    let naive = venue_quote(st, Side::Sell, buy.out_amount, 0, &fees(), 1e9, t).unwrap();
    let modelled = venue_quote(st, Side::Sell, buy.out_amount, buy.out_amount, &fees(), 1e9, t).unwrap();
    // selling right after our own buy returns ~our SOL minus two fee legs; ignoring our impact undervalues it
    assert!(modelled.out_amount > naive.out_amount);
    assert!(modelled.out_amount < buy.in_amount, "a round trip never makes money by itself");
    let rt = 1.0 - modelled.out_amount as f64 / buy.in_amount as f64;
    assert!(rt > 0.015 && rt < 0.03, "≈2% fee round trip, got {rt}");
}
