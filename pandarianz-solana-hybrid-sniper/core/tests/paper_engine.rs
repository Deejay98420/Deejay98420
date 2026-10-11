//! PAPER execution accounting and end-to-end engine lifecycle.

mod common;
use sniper_core::config::{CostConfig, EngineConfig, ExitPolicy, ScenarioConfig};
use sniper_core::decode::VenueEvent;
use sniper_core::fixtures::{hybrid_session, wallet, Curve};
use sniper_core::paper::{fill_order, Book, FillOutcome, LedgerKind, Order, OrderKind, OrderStatus, PositionStatus};
use sniper_core::quote::{venue_quote, Side, VenueFees};
use sniper_core::state::{Input, Universe};
use sniper_core::store::MemData;
use sniper_core::strategy::EntryPath;
use sniper_core::stream::{SlotStatus, StreamMsg};
use sniper_core::types::DataMode;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const S: i64 = 1_000_000;

fn fees() -> VenueFees {
    VenueFees { curve_fee_bps_default: 125, pumpswap_fee_bps: 30 }
}

fn launched(u: &mut Universe, label: &str) -> Curve {
    let mut c = Curve::new(label);
    u.apply(Input { slot: 1, signature: "c".into(), index: 0, ts_us: 0, event: VenueEvent::Pump(c.create(label, label, 0)) });
    for i in 0..20 {
        let e = c.buy(wallet(&format!("{label}{i}")), 500_000_000, S).unwrap();
        u.apply(Input { slot: 2, signature: format!("b{i}"), index: 0, ts_us: S, event: VenueEvent::Pump(e) });
    }
    c
}

fn entry_order(c: &Curve, q: sniper_core::quote::Quote, tol: u32, amount: u64) -> Order {
    Order {
        order_id: "ord-1".into(),
        idempotency_key: "k1".into(),
        book_id: "b".into(),
        scenario: "base".into(),
        data_mode: DataMode::Fixture,
        strategy_version: "v".into(),
        kind: OrderKind::Entry,
        side: Side::Buy,
        mint: c.mint,
        path: EntryPath::Launch,
        position_id: None,
        amount,
        slippage_bps: tol,
        min_out: (q.out_amount as u128 * (10_000 - tol) as u128 / 10_000) as u64,
        decision_quote: Some(q),
        decision_us: S,
        fill_at_us: 2 * S,
        trigger_signature: "t".into(),
        trigger_slot: 2,
        attempt: 1,
        reason: "test".into(),
        status: OrderStatus::Pending,
        fill: None,
        fill_quote: None,
        costs: Default::default(),
        error: None,
        completed_us: None,
        observed_quote: None,
        fill_basis: None,
        aggregator_divergence_bps: None,
    }
}

#[test]
fn entry_fill_charges_exact_costs_and_uses_fill_time_state() {
    let mut u = Universe::new(0);
    let mut c = launched(&mut u, "E");
    let costs = CostConfig::default();
    let scen = ScenarioConfig { drop_probability: 0.0, ..ScenarioConfig::base() };
    let mut book = Book::new("b".into(), scen.clone(), 1_000_000_000);
    let q = venue_quote(&u.tokens[&c.mint], Side::Buy, 100_000_000, 0, &fees(), 1e9, S).unwrap();
    // someone buys 2 SOL between our decision and our landing
    let e = c.buy(wallet("frontrunner"), 2_000_000_000, S + 1).unwrap();
    u.apply(Input { slot: 3, signature: "fr".into(), index: 0, ts_us: S + 1, event: VenueEvent::Pump(e) });
    let mut o = entry_order(&c, q.clone(), 3000, 100_000_000);
    let (out, ledger) = fill_order(&mut o, u.tokens.get(&c.mint), &mut book, &fees(), &costs, &ExitPolicy::default(), 4000.0, 2 * S);
    let FillOutcome::EntryFilled { position } = out else { panic!("{:?}", o.error) };
    let fill = o.fill.clone().unwrap();
    assert!(fill.out_amount < q.out_amount, "fill reflects worse state at fill time, not decision quote");
    assert!(o.costs.adverse_slippage_lamports > 0, "scenario adverse slippage applied");
    assert_eq!(o.costs.base_fee, 5_000);
    assert_eq!(o.costs.priority_fee, scen.priority_fee_lamports);
    assert_eq!(o.costs.tip, scen.tip_lamports);
    assert_eq!(o.costs.ata_rent_deposit, costs.ata_rent_lamports);
    assert!(o.costs.venue_fee > 0);
    let total: i64 = ledger.iter().map(|l| l.amount).sum();
    assert_eq!(total, -((100_000_000 + 5_000 + scen.priority_fee_lamports + scen.tip_lamports + costs.ata_rent_lamports) as i64));
    assert_eq!(position.cost_basis, 100_000_000 + 5_000 + scen.priority_fee_lamports + scen.tip_lamports);
    assert_eq!(position.tokens, fill.out_amount);
    book.verify().unwrap();
}

#[test]
fn slippage_failure_charges_fees_and_drop_charges_nothing() {
    let mut u = Universe::new(0);
    let mut c = launched(&mut u, "F");
    let costs = CostConfig::default();
    let q = venue_quote(&u.tokens[&c.mint], Side::Buy, 100_000_000, 0, &fees(), 1e9, S).unwrap();
    let e = c.buy(wallet("whale"), 20_000_000_000, S + 1).unwrap();
    u.apply(Input { slot: 3, signature: "w".into(), index: 0, ts_us: S + 1, event: VenueEvent::Pump(e) });
    let scen = ScenarioConfig { drop_probability: 0.0, ..ScenarioConfig::base() };
    let mut book = Book::new("b".into(), scen.clone(), 1_000_000_000);
    let mut o = entry_order(&c, q.clone(), 100, 100_000_000);
    let (out, ledger) = fill_order(&mut o, u.tokens.get(&c.mint), &mut book, &fees(), &costs, &ExitPolicy::default(), 4000.0, 2 * S);
    assert!(matches!(out, FillOutcome::Failed { .. }));
    assert_eq!(o.status, OrderStatus::FailedSlippage);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].kind, LedgerKind::FailedTxFee);
    assert_eq!(ledger[0].amount, -((5_000 + scen.priority_fee_lamports) as i64));
    assert_eq!(book.balance, 1_000_000_000 - 5_000 - scen.priority_fee_lamports as i64);
    // dropped: nothing charged
    let scen = ScenarioConfig { drop_probability: 1.0, ..ScenarioConfig::base() };
    let mut book = Book::new("b".into(), scen, 1_000_000_000);
    let mut o = entry_order(&c, q, 3000, 100_000_000);
    let (_, ledger) = fill_order(&mut o, u.tokens.get(&c.mint), &mut book, &fees(), &costs, &ExitPolicy::default(), 4000.0, 2 * S);
    assert_eq!(o.status, OrderStatus::Dropped);
    assert!(ledger.is_empty());
    assert_eq!(book.balance, 1_000_000_000);
}

#[test]
fn no_route_at_fill_time_is_recorded_not_filled() {
    let mut u = Universe::new(0);
    let c = launched(&mut u, "N");
    let q = venue_quote(&u.tokens[&c.mint], Side::Buy, 20_000_000, 0, &fees(), 1e9, S).unwrap();
    u.apply(Input { slot: 4, signature: "done".into(), index: 0, ts_us: S + 1, event: VenueEvent::Pump(c.complete_ev(S + 1)) });
    let mut book = Book::new("b".into(), ScenarioConfig::base(), 1_000_000_000);
    let mut o = entry_order(&c, q, 3000, 20_000_000);
    let (_, ledger) = fill_order(&mut o, u.tokens.get(&c.mint), &mut book, &fees(), &CostConfig::default(), &ExitPolicy::default(), 4000.0, 2 * S);
    assert_eq!(o.status, OrderStatus::NoRoute);
    assert!(o.error.as_ref().unwrap().contains("migration"));
    assert!(ledger.is_empty());
}

async fn run_full(data: Arc<Mutex<MemData>>, profile: &str) -> sniper_core::engine::Engine {
    let mut e = common::engine_with(EngineConfig::default(), data, profile);
    e.boot(serde_json::json!({})).await;
    let mut msgs = hybrid_session();
    let end = msgs.last().unwrap().recv_us();
    msgs.push(StreamMsg::End { provider: "f".into(), recv_us: end });
    common::feed(&mut e, msgs).await;
    e
}

/// Every closed position's realized P&L must equal the sum of its ledger entries.
fn reconcile(data: &MemData) -> usize {
    let mut by_ref: HashMap<&str, i64> = HashMap::new();
    for l in &data.ledger {
        *by_ref.entry(l.ref_id.as_str()).or_default() += l.amount;
    }
    let mut n = 0;
    for p in data.positions.values().filter(|p| p.status == PositionStatus::Closed) {
        let mut refs: Vec<&str> = vec![p.entry_order_id.as_str()];
        for o in data.orders.iter().filter(|o| o.position_id.as_deref() == Some(p.position_id.as_str()) && o.kind == OrderKind::Exit) {
            refs.push(o.order_id.as_str());
        }
        let ledger_sum: i64 = refs.iter().map(|r| by_ref.get(r).copied().unwrap_or(0)).sum();
        assert_eq!(ledger_sum, p.realized_pnl.unwrap(), "position {} ledger vs realized", p.position_id);
        n += 1;
    }
    n
}

#[tokio::test]
async fn end_to_end_paper_lifecycle_with_exact_accounting() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = run_full(data.clone(), "e2e").await;
    e.publish(true);
    let s = e.snapshot();
    assert!(s.paper_only);
    assert_eq!(s.data_mode, Some(DataMode::Fixture));
    assert!(s.data_mode_note.contains("NOT market data"));
    assert!(s.health.decoded_events > 500);
    assert_eq!(s.health.failed_tx_skipped, 1);
    assert_eq!(s.health.engine_duplicates_dropped, 1, "redundant provider copy dropped");
    assert_eq!(s.health.rollbacks, 1);
    assert!(s.health.raydium_decode_only >= 1);
    let d = data.lock().unwrap();
    // entries happened, exits happened, in all three scenario books
    for b in &s.books {
        assert!(b.accounting_ok, "{:?}", b.accounting_error);
        assert!(b.perf.closed_trades >= 3, "{} closed {}", b.scenario, b.perf.closed_trades);
        let start: i64 = 1_000_000_000;
        let ledger: i64 = d.ledger.iter().filter(|l| l.book_id == b.book_id).map(|l| l.amount).sum();
        assert_eq!((b.balance_sol * 1e9).round() as i64, start + ledger);
    }
    let n = reconcile(&d);
    assert!(n >= 9);
    // immutable records carry versions/timestamps/costs
    assert!(d.orders.iter().all(|o| !o.strategy_version.is_empty() && o.completed_us.is_some()));
    assert!(d.orders.iter().filter(|o| o.status == OrderStatus::Filled).all(|o| o.costs.base_fee == 5_000 && o.fill_quote.is_some()));
    assert!(d.evaluations.iter().any(|e| e.decision == "reject" && e.reasons.as_array().is_some_and(|a| !a.is_empty())));
    assert!(d.evaluations.iter().any(|e| e.decision == "enter" && e.quote.is_some()));
    // no duplicate trades: idempotency keys unique, one live position per (book, mint) at a time
    assert_eq!(d.duplicate_idempotency_rejects, 0);
    let keys: HashSet<_> = d.submissions.iter().map(|o| o.idempotency_key.clone()).collect();
    assert_eq!(keys.len(), d.submissions.len());
    let mut entries_per_book_mint: HashMap<(String, String), usize> = HashMap::new();
    for o in d.orders.iter().filter(|o| o.kind == OrderKind::Entry && o.status == OrderStatus::Filled) {
        *entries_per_book_mint.entry((o.book_id.clone(), o.mint.to_string())).or_default() += 1;
    }
    assert!(entries_per_book_mint.values().all(|n| *n == 1), "cooldown prevents re-entry within the session: {entries_per_book_mint:?}");
    // the scenarios are ordered by cost: pessimistic pays more fees per trade than optimistic
    let fee = |name: &str| s.books.iter().find(|b| b.scenario == name).unwrap().perf.total_fees_sol / s.books.iter().find(|b| b.scenario == name).unwrap().perf.closed_trades as f64;
    assert!(fee("pessimistic") > fee("base") && fee("base") > fee("optimistic"));
    // jupiter is explicitly recorded as disabled in fixture mode (never faked)
    let j = &s.routes["jupiter"];
    assert_eq!(j.ok, 0);
    assert_eq!(j.disabled, j.attempts);
}

#[tokio::test]
async fn deterministic_replay() {
    let a = run_full(Arc::new(Mutex::new(MemData::default())), "d1").await.snapshot();
    let b = run_full(Arc::new(Mutex::new(MemData::default())), "d1").await.snapshot();
    for (x, y) in a.books.iter().zip(b.books.iter()) {
        assert_eq!(x.balance_sol, y.balance_sol);
        assert_eq!(x.perf.closed_trades, y.perf.closed_trades);
    }
}

#[tokio::test]
async fn redelivered_stream_creates_no_duplicate_trades() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = run_full(data.clone(), "dup").await;
    let orders_before = data.lock().unwrap().submissions.len();
    let balances = e.ledger_for_test();
    let msgs = hybrid_session();
    let n = msgs.iter().filter(|m| matches!(m, StreamMsg::Event(_))).count() as u64;
    let before = e.health.engine_duplicates_dropped;
    common::feed(&mut e, msgs).await;
    assert_eq!(e.health.engine_duplicates_dropped - before, n);
    e.flush().await;
    assert_eq!(data.lock().unwrap().submissions.len(), orders_before);
    assert_eq!(e.ledger_for_test(), balances);
}

#[tokio::test]
async fn rollback_cancels_pending_entry_from_dead_slot() {
    let mut cfg = EngineConfig::default();
    cfg.scenarios = vec![ScenarioConfig { latency_ms: 4_000, latency_jitter_ms: 0, drop_probability: 0.0, ..ScenarioConfig::base() }];
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = common::engine_with(cfg, data.clone(), "rb");
    e.boot(serde_json::json!({})).await;
    let mut cancelled_slot = None;
    for m in hybrid_session() {
        e.handle_stream(m).await;
        if cancelled_slot.is_none() {
            if let Some(o) = e.books[0].pending.iter().find(|o| o.kind == OrderKind::Entry) {
                let slot = o.trigger_slot;
                e.handle_stream(StreamMsg::Slot { slot, status: SlotStatus::Dead, recv_us: e.now() }).await;
                cancelled_slot = Some(slot);
            }
        }
    }
    e.handle_stream(StreamMsg::End { provider: "f".into(), recv_us: e.now() }).await;
    let slot = cancelled_slot.expect("an entry was decided");
    let d = data.lock().unwrap();
    let o = d.orders.iter().find(|o| o.status == OrderStatus::CancelledRollback).expect("cancelled order recorded");
    assert_eq!(o.trigger_slot, slot);
    assert!(o.fill.is_none());
    assert!(e.books[0].verify().is_ok());
    assert_eq!(e.books[0].reserved, 0, "reservation released");
}

#[tokio::test]
async fn concurrent_signals_never_open_two_positions_for_one_mint() {
    // very long latency: many more events for the same mint arrive while the entry is pending
    let mut cfg = EngineConfig::default();
    cfg.scenarios = vec![ScenarioConfig { latency_ms: 20_000, latency_jitter_ms: 0, drop_probability: 0.0, ..ScenarioConfig::base() }];
    cfg.strategy.entry.min_eval_interval_ms = 0.0;
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = common::engine_with(cfg, data.clone(), "race");
    e.boot(serde_json::json!({})).await;
    for m in hybrid_session() {
        e.handle_stream(m).await;
        let b = &e.books[0];
        let mut mints = HashSet::new();
        for o in b.pending.iter().filter(|o| o.kind == OrderKind::Entry) {
            assert!(mints.insert(o.mint), "two pending entries for one mint");
        }
        for p in b.positions.values().filter(|p| p.is_live()) {
            assert!(mints.insert(p.mint), "pending entry + live position for one mint");
        }
        // at most one pending exit per position
        let mut exits = HashSet::new();
        for o in b.pending.iter().filter(|o| o.kind == OrderKind::Exit) {
            assert!(exits.insert(o.position_id.clone()));
        }
    }
}

#[tokio::test]
async fn restart_recovery_resumes_without_duplicates() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let msgs = hybrid_session();
    let cut = msgs.len() * 3 / 5;
    let (first, _) = msgs.split_at(cut);
    // --- first process: runs part of the session, then stops (no End)
    let mut e1 = common::engine_with(EngineConfig::default(), data.clone(), "restart");
    e1.boot(serde_json::json!({})).await;
    common::feed(&mut e1, first.to_vec()).await;
    let pending_at_stop: usize = e1.books.iter().map(|b| b.pending.len()).sum();
    let balances_at_stop = e1.ledger_for_test();
    let live_at_stop: usize = e1.books.iter().map(|b| b.positions.values().filter(|p| p.is_live()).count()).sum();
    eprintln!("restart cut: pending={pending_at_stop} live={live_at_stop} balances={balances_at_stop:?}");
    assert!(live_at_stop + pending_at_stop > 0, "cut point must have in-flight state to be meaningful");
    e1.flush().await;
    drop(e1);
    // --- second process: recover from the store, warm up state from the journal, receive the full stream again
    let mut e2 = common::engine_with(EngineConfig::default(), data.clone(), "restart");
    let journal = data.lock().unwrap().raw.clone();
    e2.warmup(&journal);
    e2.boot(serde_json::json!({})).await;
    assert_eq!(e2.ledger_for_test(), balances_at_stop, "balances recovered exactly");
    let live_after: usize = e2.books.iter().map(|b| b.positions.values().filter(|p| p.is_live()).count()).sum();
    assert_eq!(live_after, live_at_stop, "open positions restored");
    assert!(e2.health.recovery_notes.iter().any(|n| n.contains("recovered balance")));
    let mut all = msgs.clone();
    all.push(StreamMsg::End { provider: "f".into(), recv_us: msgs.last().unwrap().recv_us() });
    common::feed(&mut e2, all).await;
    e2.flush().await;
    let first_events = first.iter().filter(|m| matches!(m, StreamMsg::Event(_))).count() as u64;
    assert!(e2.health.engine_duplicates_dropped >= first_events, "already-processed events are not re-applied");
    let d = data.lock().unwrap();
    assert_eq!(d.orders.iter().filter(|o| o.status == OrderStatus::AbandonedRestart).count(), pending_at_stop);
    assert_eq!(d.duplicate_idempotency_rejects, 0);
    for b in &e2.books {
        b.verify().unwrap();
        let mut seqs: Vec<u64> = d.ledger.iter().filter(|l| l.book_id == b.id).map(|l| l.seq).collect();
        let n = seqs.len();
        seqs.dedup();
        assert_eq!(seqs.len(), n, "no duplicate ledger sequence numbers");
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "contiguous ledger");
    }
    reconcile(&d);
}

#[tokio::test]
async fn restart_with_in_flight_order_marks_it_abandoned_and_never_fills_it() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let msgs = hybrid_session();
    let mut e1 = common::engine_with(EngineConfig::default(), data.clone(), "inflight");
    e1.boot(serde_json::json!({})).await;
    let mut consumed = 0;
    for m in msgs.iter() {
        e1.handle_stream(m.clone()).await;
        consumed += 1;
        if e1.books.iter().any(|b| !b.pending.is_empty()) {
            break;
        }
    }
    let inflight: Vec<String> = e1.books.iter().flat_map(|b| b.pending.iter().map(|o| o.order_id.clone())).collect();
    assert!(!inflight.is_empty());
    let balances = e1.ledger_for_test();
    e1.flush().await;
    drop(e1); // "crash" with orders in flight
    let mut e2 = common::engine_with(EngineConfig::default(), data.clone(), "inflight");
    e2.boot(serde_json::json!({})).await;
    e2.flush().await;
    assert_eq!(e2.ledger_for_test(), balances, "in-flight orders never debited");
    assert!(e2.books.iter().all(|b| b.pending.is_empty() && b.reserved == 0));
    {
        let d = data.lock().unwrap();
        for id in &inflight {
            let o = d.orders.iter().find(|o| &o.order_id == id).expect("completion recorded");
            assert_eq!(o.status, OrderStatus::AbandonedRestart);
            assert!(o.fill.is_none());
        }
    }
    // continuing the stream: the same trigger is a duplicate (dedup seeded) → no re-entry from it
    let journal = data.lock().unwrap().raw.clone();
    e2.warmup(&journal);
    common::feed(&mut e2, msgs[consumed..].to_vec()).await;
    let d = data.lock().unwrap();
    assert_eq!(d.duplicate_idempotency_rejects, 0);
    for b in &e2.books {
        b.verify().unwrap();
    }
}
