//! PAPER execution. Simulated fills only: no keys, no signing, no transaction building, no sending.
//!
//! Each scenario (optimistic / base / pessimistic) has an independent book with its own balance,
//! ledger and positions. Orders fill after a scenario latency against the venue state AT FILL TIME
//! (not the decision-time state), with fees, own price impact, extra adverse slippage, drops and
//! slippage-tolerance failures. Last price is never used as a fill price.

use crate::config::{CostConfig, ExitPolicy, ScenarioConfig};
use crate::quote::{venue_quote, Quote, QuoteError, Side, VenueFees};
use crate::state::TokenState;
use crate::strategy::exit::{ExitReason, Urgency};
use crate::strategy::EntryPath;
use crate::types::{DataMode, Pubkey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Deterministic uniform [0,1) from a seed string (reproducible simulations).
pub fn det_unit(seed: &str) -> f64 {
    let h = Sha256::digest(seed.as_bytes());
    let v = u64::from_le_bytes(h[..8].try_into().unwrap());
    (v >> 11) as f64 / (1u64 << 53) as f64
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub venue_fee: u64,
    pub base_fee: u64,
    pub priority_fee: u64,
    pub tip: u64,
    pub ata_rent_deposit: u64,
    pub ata_rent_refund: u64,
    /// Value lost vs marginal price due to size (informational; already inside amounts).
    pub price_impact_lamports: i64,
    /// Extra adverse execution applied by the scenario (informational; already inside amounts).
    pub adverse_slippage_lamports: i64,
}

impl CostBreakdown {
    /// Cash cost charged outside the swap amounts (fees + tip). Rent is tracked separately.
    pub fn tx_costs(&self) -> u64 {
        self.base_fee + self.priority_fee + self.tip
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    Entry,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Filled,
    /// Landed but failed: output below slippage tolerance. Base + priority fee charged.
    FailedSlippage,
    /// Not landed (dropped/expired). No fee.
    Dropped,
    /// No executable route at fill time. Not sent, no fee.
    NoRoute,
    /// Trigger slot rolled back before fill.
    CancelledRollback,
    /// Pending when the process stopped; never filled.
    AbandonedRestart,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub in_amount: u64,
    pub out_amount: u64,
    pub latency_ms: f64,
    pub quote_age_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub idempotency_key: String,
    pub book_id: String,
    pub scenario: String,
    pub data_mode: DataMode,
    pub strategy_version: String,
    pub kind: OrderKind,
    pub side: Side,
    pub mint: Pubkey,
    pub path: EntryPath,
    pub position_id: Option<String>,
    /// lamports for buys, tokens for sells
    pub amount: u64,
    pub slippage_bps: u32,
    pub min_out: u64,
    pub decision_quote: Option<Quote>,
    pub decision_us: i64,
    pub fill_at_us: i64,
    pub trigger_signature: String,
    pub trigger_slot: u64,
    pub attempt: u32,
    pub reason: String,
    pub status: OrderStatus,
    pub fill: Option<Fill>,
    pub fill_quote: Option<Quote>,
    pub costs: CostBreakdown,
    pub error: Option<String>,
    pub completed_us: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Open,
    Exiting,
    Closed,
    /// Could not be exited (no route / repeated failures). Kept explicit, valued conservatively.
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub position_id: String,
    pub book_id: String,
    pub scenario: String,
    pub data_mode: DataMode,
    pub strategy_version: String,
    pub mint: Pubkey,
    pub symbol: Option<String>,
    pub path: EntryPath,
    pub status: PositionStatus,
    pub tokens: u64,
    /// lamports in + entry tx costs (excludes refundable rent)
    pub cost_basis: u64,
    pub rent_locked: u64,
    pub entry_order_id: String,
    pub opened_us: i64,
    pub entry_liquidity: u64,
    /// venue the liquidity reference was taken on (rebased after migration)
    #[serde(default)]
    pub liquidity_ref_venue: Option<crate::types::Venue>,
    pub entry_creator_sold_fraction: f64,
    pub peak_value: u64,
    pub peak_us: i64,
    pub last_value: Option<u64>,
    pub last_value_us: i64,
    pub exit_reason: Option<ExitReason>,
    pub exit_urgency: Option<Urgency>,
    pub exit_attempts: u32,
    pub next_exit_attempt_us: i64,
    pub exit_slippage_bps: u32,
    pub route_lost_since_us: Option<i64>,
    pub unresolved_reason: Option<String>,
    /// fees paid by failed exit attempts
    pub failed_exit_costs: u64,
    pub entry_tx_costs: u64,
    /// venue/aggregator swap fees on entry + exit (inside the swap amounts)
    pub venue_fees_paid: u64,
    pub closed_us: Option<i64>,
    pub proceeds: u64,
    pub exit_tx_costs: u64,
    pub realized_pnl: Option<i64>,
    pub exit_order_id: Option<String>,
    pub trigger_signature: String,
    pub trigger_slot: u64,
    pub trigger_rolled_back: bool,
    pub version: u64,
}

impl Position {
    pub fn is_live(&self) -> bool {
        matches!(self.status, PositionStatus::Open | PositionStatus::Exiting | PositionStatus::Unresolved)
    }
    pub fn hold_s(&self, now_us: i64) -> f64 {
        (self.closed_us.unwrap_or(now_us) - self.opened_us) as f64 / 1e6
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Start,
    EntrySpend,
    ExitProceeds,
    TxFee,
    Tip,
    RentDeposit,
    RentRefund,
    FailedTxFee,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub book_id: String,
    pub seq: u64,
    pub ts_us: i64,
    pub kind: LedgerKind,
    pub amount: i64,
    pub ref_id: String,
    pub balance_after: i64,
}

pub struct Book {
    pub id: String,
    pub scenario: ScenarioConfig,
    pub start_balance: i64,
    pub balance: i64,
    pub ledger_seq: u64,
    pub ledger_sum: i64,
    /// lamports reserved by pending entry orders (prevents overdraft races)
    pub reserved: u64,
    pub positions: HashMap<String, Position>,
    pub closed: Vec<Position>,
    pub pending: Vec<Order>,
    pub last_exit_us: HashMap<Pubkey, i64>,
    pub entered_keys: HashSet<String>,
    pub failed_entry_costs: u64,
    pub order_counter: u64,
}

pub struct Posted {
    pub entries: Vec<LedgerEntry>,
}

impl Book {
    pub fn new(id: String, scenario: ScenarioConfig, start: u64) -> Self {
        Self {
            id,
            scenario,
            start_balance: start as i64,
            balance: start as i64,
            ledger_seq: 0,
            ledger_sum: 0,
            reserved: 0,
            positions: HashMap::new(),
            closed: Vec::new(),
            pending: Vec::new(),
            last_exit_us: HashMap::new(),
            entered_keys: HashSet::new(),
            failed_entry_costs: 0,
            order_counter: 0,
        }
    }

    pub fn post(&mut self, ts_us: i64, kind: LedgerKind, amount: i64, ref_id: &str) -> LedgerEntry {
        self.ledger_seq += 1;
        self.balance += amount;
        self.ledger_sum += amount;
        LedgerEntry {
            book_id: self.id.clone(),
            seq: self.ledger_seq,
            ts_us,
            kind,
            amount,
            ref_id: ref_id.into(),
            balance_after: self.balance,
        }
    }

    /// Accounting invariant: balance == start + Σ ledger and never negative.
    pub fn verify(&self) -> Result<(), String> {
        if self.balance != self.start_balance + self.ledger_sum {
            return Err(format!("balance {} != start {} + ledger {}", self.balance, self.start_balance, self.ledger_sum));
        }
        if self.balance < 0 {
            return Err(format!("negative balance {}", self.balance));
        }
        Ok(())
    }

    pub fn exposure(&self) -> u64 {
        self.positions.values().filter(|p| p.is_live()).map(|p| p.cost_basis).sum::<u64>()
            + self.pending.iter().filter(|o| o.kind == OrderKind::Entry).map(|o| o.amount).sum::<u64>()
    }
    pub fn live_count(&self) -> usize {
        self.positions.values().filter(|p| p.is_live()).count()
            + self.pending.iter().filter(|o| o.kind == OrderKind::Entry).count()
    }
    pub fn has_mint(&self, mint: &Pubkey) -> bool {
        self.positions.values().any(|p| p.is_live() && p.mint == *mint)
            || self.pending.iter().any(|o| o.mint == *mint && o.kind == OrderKind::Entry)
    }
    pub fn rent_locked(&self) -> u64 {
        self.positions.values().filter(|p| p.is_live()).map(|p| p.rent_locked).sum()
    }
    pub fn next_id(&mut self, prefix: &str, now_us: i64) -> String {
        self.order_counter += 1;
        // globally unique: book id carries the profile namespace and scenario
        format!("{prefix}-{}-{}-{}", self.id, now_us, self.order_counter)
    }
    pub fn realized_pnl(&self) -> i64 {
        self.closed.iter().filter_map(|p| p.realized_pnl).sum()
    }
}

/// Lamports reserved for a pending entry (size + all fixed costs + refundable rent).
pub fn entry_reservation(amount: u64, scen: &ScenarioConfig, costs: &CostConfig) -> u64 {
    amount + costs.ata_rent_lamports + scen.priority_fee_lamports + scen.tip_lamports + costs.base_fee_lamports
}

/// Simulation seed independent of the profile namespace: same inputs → same simulated fills.
pub fn sim_seed(book_id: &str, scenario: &str, order_id: &str) -> String {
    order_id.replacen(book_id, scenario, 1)
}

pub fn latency_us(scen: &ScenarioConfig, seed: &str) -> i64 {
    let j = det_unit(&format!("{seed}:lat")) * scen.latency_jitter_ms as f64;
    ((scen.latency_ms as f64 + j) * 1000.0) as i64
}

fn apply_adverse(out: u64, bps: u32) -> (u64, i64) {
    let adj = (out as u128 * (10_000 - bps.min(10_000)) as u128 / 10_000) as u64;
    (adj, out as i64 - adj as i64)
}

fn impact_lamports(q: &Quote) -> i64 {
    if q.price_impact_bps.is_nan() {
        return 0;
    }
    match q.side {
        Side::Buy => ((q.in_amount - q.venue_fee_lamports) as f64 * q.price_impact_bps / 10_000.0) as i64,
        Side::Sell => ((q.out_amount + q.venue_fee_lamports) as f64 * q.price_impact_bps / (10_000.0 - q.price_impact_bps).max(1.0)) as i64,
    }
}

pub enum FillOutcome {
    EntryFilled { position: Position },
    ExitFilled,
    Failed { retryable: bool },
}

/// Attempt to fill a pending order at `now_us` against `st` (state at fill time).
/// Mutates `order` and `book` (ledger). Returns ledger entries produced.
#[allow(clippy::too_many_arguments)]
pub fn fill_order(
    order: &mut Order,
    st: Option<&TokenState>,
    book: &mut Book,
    fees: &VenueFees,
    costs: &CostConfig,
    exit_cfg: &ExitPolicy,
    max_state_age_ms: f64,
    now_us: i64,
) -> (FillOutcome, Vec<LedgerEntry>) {
    let scen = book.scenario.clone();
    let mut ledger = Vec::new();
    order.completed_us = Some(now_us);
    let own = match order.kind {
        OrderKind::Exit if exit_cfg.model_own_impact => order.amount,
        _ => 0,
    };
    let q: Result<Quote, QuoteError> = match st {
        None => Err(QuoteError::NoRoute("token state not available".into())),
        // At fill time we accept older state than at decision time: the fill is what it is.
        Some(s) => venue_quote(s, order.side, order.amount, own, fees, max_state_age_ms.max(60_000.0), now_us),
    };
    if order.kind == OrderKind::Entry {
        book.reserved = book.reserved.saturating_sub(entry_reservation(order.amount, &scen, costs));
    }
    // Aggregator fallback: when the tracked venue has no route but the decision used a fresh Jupiter
    // quote, fill against that quote (plus scenario adverse slippage). No fresher state exists.
    let q = match (q, &order.decision_quote) {
        (Err(QuoteError::NoRoute(_)), Some(dq))
            if dq.source == crate::quote::QuoteSource::Jupiter && now_us - dq.quoted_at_us <= 5_000_000 =>
        {
            Ok(dq.clone())
        }
        (q, _) => q,
    };
    let q = match q {
        Ok(q) => q,
        Err(e) => {
            order.status = OrderStatus::NoRoute;
            order.error = Some(e.to_string());
            return (FillOutcome::Failed { retryable: true }, ledger);
        }
    };
    order.fill_quote = Some(q.clone());
    if det_unit(&format!("{}:drop", sim_seed(&book.id, &scen.name, &order.order_id))) < scen.drop_probability {
        order.status = OrderStatus::Dropped;
        order.error = Some("transaction not landed (simulated drop)".into());
        return (FillOutcome::Failed { retryable: true }, ledger);
    }
    let (out, adverse) = apply_adverse(q.out_amount, scen.adverse_slippage_bps);
    let latency_ms = (now_us - order.decision_us) as f64 / 1e3;
    order.costs.venue_fee = q.venue_fee_lamports;
    order.costs.price_impact_lamports = impact_lamports(&q);
    order.costs.adverse_slippage_lamports = if order.side == Side::Sell { adverse } else { 0 };
    if out < order.min_out {
        order.status = OrderStatus::FailedSlippage;
        order.error = Some(format!("out {out} < min_out {} (tolerance {} bps)", order.min_out, order.slippage_bps));
        order.costs.venue_fee = 0;
        if scen.charge_failed_tx_fees {
            order.costs.base_fee = costs.base_fee_lamports;
            order.costs.priority_fee = scen.priority_fee_lamports;
            let amt = (costs.base_fee_lamports + scen.priority_fee_lamports) as i64;
            ledger.push(book.post(now_us, LedgerKind::FailedTxFee, -amt, &order.order_id));
            if order.kind == OrderKind::Entry {
                book.failed_entry_costs += amt as u64;
            }
        }
        return (FillOutcome::Failed { retryable: true }, ledger);
    }
    order.status = OrderStatus::Filled;
    order.fill = Some(Fill { in_amount: order.amount, out_amount: out, latency_ms, quote_age_ms: q.state_age_ms });
    order.costs.base_fee = costs.base_fee_lamports;
    order.costs.priority_fee = scen.priority_fee_lamports;
    order.costs.tip = scen.tip_lamports;
    match order.kind {
        OrderKind::Entry => {
            if order.side == Side::Buy {
                // buy-side adverse slippage = fewer tokens; value it at the fill price for the record
                order.costs.adverse_slippage_lamports = if q.out_amount > 0 {
                    (adverse as f64 * order.amount as f64 / q.out_amount as f64) as i64
                } else {
                    0
                };
            }
            order.costs.ata_rent_deposit = costs.ata_rent_lamports;
            ledger.push(book.post(now_us, LedgerKind::EntrySpend, -(order.amount as i64), &order.order_id));
            ledger.push(book.post(now_us, LedgerKind::TxFee, -((costs.base_fee_lamports + scen.priority_fee_lamports) as i64), &order.order_id));
            if scen.tip_lamports > 0 {
                ledger.push(book.post(now_us, LedgerKind::Tip, -(scen.tip_lamports as i64), &order.order_id));
            }
            ledger.push(book.post(now_us, LedgerKind::RentDeposit, -(costs.ata_rent_lamports as i64), &order.order_id));
            let st = st.expect("filled implies state");
            let pid = format!("pos-{}", order.order_id.trim_start_matches("ord-"));
            let cost_basis = order.amount + order.costs.tx_costs();
            let position = Position {
                position_id: pid.clone(),
                book_id: book.id.clone(),
                scenario: scen.name.clone(),
                data_mode: order.data_mode,
                strategy_version: order.strategy_version.clone(),
                mint: order.mint,
                symbol: st.symbol.clone(),
                path: order.path,
                status: PositionStatus::Open,
                tokens: out,
                cost_basis,
                rent_locked: costs.ata_rent_lamports,
                entry_order_id: order.order_id.clone(),
                opened_us: now_us,
                entry_liquidity: st.liquidity_lamports(),
                liquidity_ref_venue: st.venue(),
                entry_creator_sold_fraction: crate::state::analytics::compute(st, now_us).creator_sold_fraction,
                peak_value: 0,
                peak_us: now_us,
                last_value: None,
                last_value_us: now_us,
                exit_reason: None,
                exit_urgency: None,
                exit_attempts: 0,
                next_exit_attempt_us: 0,
                exit_slippage_bps: exit_cfg.normal_slippage_bps,
                route_lost_since_us: None,
                unresolved_reason: None,
                failed_exit_costs: 0,
                entry_tx_costs: order.costs.tx_costs(),
                venue_fees_paid: order.costs.venue_fee,
                closed_us: None,
                proceeds: 0,
                exit_tx_costs: 0,
                realized_pnl: None,
                exit_order_id: None,
                trigger_signature: order.trigger_signature.clone(),
                trigger_slot: order.trigger_slot,
                trigger_rolled_back: false,
                version: 1,
            };
            order.position_id = Some(pid);
            (FillOutcome::EntryFilled { position }, ledger)
        }
        OrderKind::Exit => {
            if costs.close_ata_on_exit {
                order.costs.ata_rent_refund = costs.ata_rent_lamports;
            }
            ledger.push(book.post(now_us, LedgerKind::ExitProceeds, out as i64, &order.order_id));
            ledger.push(book.post(now_us, LedgerKind::TxFee, -((costs.base_fee_lamports + scen.priority_fee_lamports) as i64), &order.order_id));
            if scen.tip_lamports > 0 {
                ledger.push(book.post(now_us, LedgerKind::Tip, -(scen.tip_lamports as i64), &order.order_id));
            }
            if costs.close_ata_on_exit {
                ledger.push(book.post(now_us, LedgerKind::RentRefund, costs.ata_rent_lamports as i64, &order.order_id));
            }
            (FillOutcome::ExitFilled, ledger)
        }
    }
}
