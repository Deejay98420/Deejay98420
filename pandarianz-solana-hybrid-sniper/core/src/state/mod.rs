//! Per-token state derived purely from decoded venue events. State is rebuildable from its
//! retained inputs, which is how slot rollbacks are handled (drop the dead slot, recompute).

pub mod analytics;

use crate::decode::pumpfun::PumpEvent;
use crate::decode::pumpswap::PumpSwapEvent;
use crate::decode::VenueEvent;
use crate::types::{Pubkey, Venue, WSOL_MINT};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

/// Pump.fun initial virtual SOL reserves (30 SOL). Used to derive real SOL when the event lacks it.
pub const PUMP_INITIAL_VIRTUAL_SOL: u64 = 30_000_000_000;
pub const MAX_INPUTS_PER_TOKEN: usize = 20_000;
pub const MAX_TRADES_PER_TOKEN: usize = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    BondingCurve,
    /// Curve complete; no SOL-quoted pool known yet → no executable route on a tracked venue.
    AwaitingMigration,
    Amm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// We observed the creation event (launch path).
    Launch,
    /// First observation was a trade / pool on an already-trading token (momentum path).
    Discovered,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct CurveReserves {
    pub virtual_sol: u64,
    pub virtual_token: u64,
    pub real_sol: u64,
    pub real_token: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PoolReserves {
    pub pool: Pubkey,
    pub base: u64,
    /// quote vault balance (real quote available for sells/withdrawals)
    pub quote: u64,
    /// PumpSwap `virtual_quote_reserves` (signed); pricing uses quote + virtual (SDK)
    pub virtual_quote: i128,
    /// fee rates observed on the latest swap event
    #[serde(skip)]
    pub fees: Option<crate::quote::math::AmmFees>,
}

impl PoolReserves {
    pub fn new(pool: Pubkey, base: u64, quote: u64) -> Self {
        Self { pool, base, quote, virtual_quote: 0, fees: None }
    }
    /// Effective quote reserve used for pricing.
    pub fn quote_eff(&self) -> u64 {
        (self.quote as i128 + self.virtual_quote).clamp(0, u64::MAX as i128) as u64
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TradeObs {
    pub ts_us: i64,
    pub slot: u64,
    pub user: Pubkey,
    pub is_buy: bool,
    pub sol: u64,
    pub tokens: u64,
    /// lamports per raw token unit after the trade (marginal)
    pub price_after: f64,
    pub venue: Venue,
    /// Quote the trader actually paid (buys, incl. fees) or received (sells, after fees), lamports.
    pub user_quote: u64,
}

/// One state input, retained for rollback recomputation.
#[derive(Clone, Debug)]
pub struct Input {
    pub slot: u64,
    pub signature: String,
    pub index: u16,
    pub ts_us: i64,
    pub event: VenueEvent,
}

#[derive(Clone, Debug, Serialize)]
pub struct TokenState {
    pub mint: Pubkey,
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub creator: Option<Pubkey>,
    pub origin: Origin,
    pub first_seen_us: i64,
    pub created_us: Option<i64>,
    pub create_slot: Option<u64>,
    pub phase: Phase,
    pub curve: Option<CurveReserves>,
    pub pool: Option<PoolReserves>,
    pub peak_liquidity_lamports: u64,
    pub last_update_us: i64,
    pub last_slot: u64,
    #[serde(skip)]
    pub trades: VecDeque<TradeObs>,
    /// Net token flow per wallet observed on tracked venues (approximate holdings).
    #[serde(skip)]
    pub holdings: HashMap<Pubkey, i128>,
    #[serde(skip)]
    pub buyers: HashSet<Pubkey>,
    #[serde(skip)]
    pub sellers: HashSet<Pubkey>,
    pub creator_bought: u64,
    pub creator_sold: u64,
    pub creator_initial_buy: u64,
    pub buyers_in_create_slot: u32,
    pub total_supply: u64,
    pub liquidity_removed_lamports: u64,
    /// Observed a successful sell by someone other than the creator.
    pub non_creator_sell_seen: bool,
    pub rolled_back_slots: u32,
    /// Total curve fee bps (protocol + creator) observed on the most recent trade event, if reported.
    pub observed_curve_fee_bps: Option<u64>,
    /// Fee components observed on the most recent trade event.
    #[serde(skip)]
    pub curve_fees: Option<crate::quote::math::CurveFees>,
    /// Curve or pool quoted in a non-SOL mint: never traded (amounts are not lamports).
    pub quote_non_sol: bool,
    /// Result of the on-chain mint check (live mode), if performed.
    pub mint_safety: Option<Result<crate::safety::MintSafety, String>>,
    /// Inputs that arrived older than the newest applied slot (backfill / reordering): analytics only.
    pub out_of_order_inputs: u32,
    /// (ts, liquidity lamports) after each applied input, bounded
    #[serde(skip)]
    pub liq_samples: VecDeque<(i64, u64)>,
    #[serde(skip)]
    pub inputs: VecDeque<Input>,
}

impl TokenState {
    pub fn new(mint: Pubkey, origin: Origin, now_us: i64) -> Self {
        Self {
            mint,
            name: None,
            symbol: None,
            creator: None,
            origin,
            first_seen_us: now_us,
            created_us: None,
            create_slot: None,
            phase: Phase::BondingCurve,
            curve: None,
            pool: None,
            peak_liquidity_lamports: 0,
            last_update_us: now_us,
            last_slot: 0,
            trades: VecDeque::new(),
            holdings: HashMap::new(),
            buyers: HashSet::new(),
            sellers: HashSet::new(),
            creator_bought: 0,
            creator_sold: 0,
            creator_initial_buy: 0,
            buyers_in_create_slot: 0,
            total_supply: 1_000_000_000_000_000,
            liquidity_removed_lamports: 0,
            non_creator_sell_seen: false,
            rolled_back_slots: 0,
            observed_curve_fee_bps: None,
            curve_fees: None,
            quote_non_sol: false,
            mint_safety: None,
            out_of_order_inputs: 0,
            liq_samples: VecDeque::new(),
            inputs: VecDeque::new(),
        }
    }

    pub fn venue(&self) -> Option<Venue> {
        if self.quote_non_sol {
            return None;
        }
        match self.phase {
            Phase::BondingCurve if self.curve.is_some() => Some(Venue::PumpBondingCurve),
            Phase::Amm if self.pool.is_some() => Some(Venue::PumpSwap),
            _ => None,
        }
    }

    /// SOL actually backing the price on the current venue (lamports).
    pub fn liquidity_lamports(&self) -> u64 {
        if self.quote_non_sol {
            return 0;
        }
        match self.phase {
            Phase::BondingCurve => self.curve.map(|c| c.real_sol).unwrap_or(0),
            Phase::AwaitingMigration => 0,
            Phase::Amm => self.pool.map(|p| p.quote).unwrap_or(0),
        }
    }

    /// Marginal price, lamports per raw token unit. Display/analytics only — never used as a fill price.
    pub fn marginal_price(&self) -> Option<f64> {
        match self.phase {
            Phase::BondingCurve => self.curve.filter(|c| c.virtual_token > 0).map(|c| c.virtual_sol as f64 / c.virtual_token as f64),
            Phase::Amm => self.pool.filter(|p| p.base > 0).map(|p| p.quote_eff() as f64 / p.base as f64),
            Phase::AwaitingMigration => None,
        }
    }

    fn record_trade(&mut self, obs: TradeObs) {
        let delta = obs.tokens as i128 * if obs.is_buy { 1 } else { -1 };
        *self.holdings.entry(obs.user).or_insert(0) += delta;
        if obs.is_buy {
            self.buyers.insert(obs.user);
            if Some(obs.slot) == self.create_slot && Some(obs.user) != self.creator {
                self.buyers_in_create_slot += 1;
            }
        } else {
            self.sellers.insert(obs.user);
            if Some(obs.user) != self.creator {
                self.non_creator_sell_seen = true;
            }
        }
        if Some(obs.user) == self.creator {
            if obs.is_buy {
                self.creator_bought += obs.tokens;
                if Some(obs.slot) == self.create_slot {
                    self.creator_initial_buy += obs.tokens;
                }
            } else {
                self.creator_sold += obs.tokens;
            }
        }
        self.trades.push_back(obs);
        while self.trades.len() > MAX_TRADES_PER_TOKEN {
            self.trades.pop_front();
        }
    }

    fn touch(&mut self, input: &Input) {
        self.last_update_us = self.last_update_us.max(input.ts_us);
        self.last_slot = self.last_slot.max(input.slot);
        let liq = self.liquidity_lamports();
        self.peak_liquidity_lamports = self.peak_liquidity_lamports.max(liq);
        self.liq_samples.push_back((input.ts_us, liq));
        while self.liq_samples.len() > 600 {
            self.liq_samples.pop_front();
        }
    }

    /// Apply one input. Returns false if the event does not concern this token.
    pub fn apply(&mut self, input: &Input) -> bool {
        // Absolute-reserve events older than what we already applied would move state backwards.
        if self.last_slot > 0 && input.slot < self.last_slot {
            self.out_of_order_inputs += 1;
            return self.apply_analytics_only(input);
        }
        match &input.event {
            VenueEvent::Pump(PumpEvent::Create(c)) => {
                self.name = Some(c.name.clone());
                self.symbol = Some(c.symbol.clone());
                self.creator = Some(c.creator.unwrap_or(c.user));
                self.created_us = Some(c.timestamp.map(|t| t * 1_000_000).unwrap_or(input.ts_us));
                self.create_slot = Some(input.slot);
                self.origin = Origin::Launch;
                if !crate::decode::pumpfun::is_sol_quote(c.quote_mint().as_ref()) {
                    self.quote_non_sol = true;
                }
                if let Some(s) = c.token_total_supply {
                    self.total_supply = s;
                }
                self.curve = Some(CurveReserves {
                    virtual_sol: c.virtual_sol_reserves.unwrap_or(PUMP_INITIAL_VIRTUAL_SOL),
                    virtual_token: c.virtual_token_reserves.unwrap_or(1_073_000_000_000_000),
                    real_sol: 0,
                    real_token: c.real_token_reserves.unwrap_or(793_100_000_000_000),
                });
                self.phase = Phase::BondingCurve;
            }
            VenueEvent::Pump(PumpEvent::Trade(t)) => {
                let real_sol = t
                    .real_sol_reserves
                    .unwrap_or_else(|| t.virtual_sol_reserves.saturating_sub(PUMP_INITIAL_VIRTUAL_SOL));
                let real_token = t.real_token_reserves.unwrap_or_else(|| self.curve.map(|c| c.real_token).unwrap_or(0));
                self.curve = Some(CurveReserves {
                    virtual_sol: t.virtual_sol_reserves,
                    virtual_token: t.virtual_token_reserves,
                    real_sol,
                    real_token,
                });
                if self.phase != Phase::Amm {
                    self.phase = Phase::BondingCurve;
                }
                let price = if t.virtual_token_reserves > 0 {
                    t.virtual_sol_reserves as f64 / t.virtual_token_reserves as f64
                } else {
                    0.0
                };
                if !t.is_sol_quoted() {
                    self.quote_non_sol = true;
                }
                if let (Some(total), Some(p)) = (t.total_fee_bps(), t.fee_basis_points) {
                    self.observed_curve_fee_bps = Some(total);
                    self.curve_fees = Some(crate::quote::math::CurveFees { protocol_bps: p, creator_bps: total - p });
                }
                if self.creator.is_none() {
                    if let Some(c) = t.creator {
                        self.creator = Some(c);
                    }
                }
                self.record_trade(TradeObs {
                    ts_us: input.ts_us,
                    slot: input.slot,
                    user: t.user,
                    is_buy: t.is_buy,
                    sol: t.sol_amount,
                    tokens: t.token_amount,
                    price_after: price,
                    venue: Venue::PumpBondingCurve,
                    // curve events report net SOL; buys pay fees on top, sells receive net of fees
                    user_quote: {
                        let fees = t.fee.unwrap_or(0) + t.creator_fee.unwrap_or(0);
                        if t.is_buy { t.sol_amount + fees } else { t.sol_amount.saturating_sub(fees) }
                    },
                });
            }
            VenueEvent::Pump(PumpEvent::Complete(_)) => {
                if self.pool.is_none() {
                    self.phase = Phase::AwaitingMigration;
                }
            }
            VenueEvent::Pump(PumpEvent::Migration(m)) => {
                if self.pool.is_none() {
                    self.pool = Some(PoolReserves::new(m.pool, m.mint_amount, m.sol_amount));
                }
                self.phase = Phase::Amm;
            }
            VenueEvent::PumpSwap(PumpSwapEvent::CreatePool(c)) => {
                self.pool = Some(PoolReserves::new(c.pool, c.pool_base_amount, c.pool_quote_amount));
                self.phase = Phase::Amm;
                if self.creator.is_none() {
                    self.creator = Some(c.creator);
                }
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) => {
                let (base, quote) = if s.is_buy {
                    (
                        s.pool_base_token_reserves.saturating_sub(s.base_amount),
                        s.pool_quote_token_reserves + s.quote_amount_pool,
                    )
                } else {
                    (
                        s.pool_base_token_reserves + s.base_amount,
                        s.pool_quote_token_reserves.saturating_sub(s.quote_amount_pool),
                    )
                };
                let fees = crate::quote::math::AmmFees {
                    lp_bps: s.lp_fee_bps,
                    protocol_bps: s.protocol_fee_bps,
                    creator_bps: s.total_fee_bps() - s.lp_fee_bps - s.protocol_fee_bps,
                };
                let prev_virtual = self.pool.map(|p| p.virtual_quote).unwrap_or(0);
                self.pool = Some(PoolReserves {
                    pool: s.pool,
                    base,
                    quote,
                    virtual_quote: s.virtual_quote_reserves.unwrap_or(prev_virtual),
                    fees: if s.lp_fee_bps + s.protocol_fee_bps > 0 { Some(fees) } else { self.pool.and_then(|p| p.fees) },
                });
                self.phase = Phase::Amm;
                self.record_trade(TradeObs {
                    ts_us: input.ts_us,
                    slot: input.slot,
                    user: s.user,
                    is_buy: s.is_buy,
                    sol: s.quote_amount_user,
                    tokens: s.base_amount,
                    price_after: self.pool.filter(|p| p.base > 0).map(|p| p.quote_eff() as f64 / p.base as f64).unwrap_or(0.0),
                    venue: Venue::PumpSwap,
                    user_quote: s.quote_amount_user,
                });
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Liquidity(l)) => {
                let (base, quote) = if l.is_deposit {
                    (l.pool_base_token_reserves + l.base_amount, l.pool_quote_token_reserves + l.quote_amount)
                } else {
                    self.liquidity_removed_lamports += l.quote_amount;
                    (
                        l.pool_base_token_reserves.saturating_sub(l.base_amount),
                        l.pool_quote_token_reserves.saturating_sub(l.quote_amount),
                    )
                };
                let prev = self.pool;
                self.pool = Some(PoolReserves {
                    pool: l.pool,
                    base,
                    quote,
                    virtual_quote: prev.map(|p| p.virtual_quote).unwrap_or(0),
                    fees: prev.and_then(|p| p.fees),
                });
                self.phase = Phase::Amm;
            }
            VenueEvent::Raydium(_) => return false,
        }
        self.touch(input);
        true
    }

    /// Record flow analytics for an out-of-order trade without touching reserves.
    fn apply_analytics_only(&mut self, input: &Input) -> bool {
        let obs = match &input.event {
            VenueEvent::Pump(PumpEvent::Trade(t)) => TradeObs {
                ts_us: input.ts_us,
                slot: input.slot,
                user: t.user,
                is_buy: t.is_buy,
                sol: t.sol_amount,
                tokens: t.token_amount,
                price_after: if t.virtual_token_reserves > 0 { t.virtual_sol_reserves as f64 / t.virtual_token_reserves as f64 } else { 0.0 },
                venue: Venue::PumpBondingCurve,
                user_quote: t.sol_amount,
            },
            VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) => TradeObs {
                ts_us: input.ts_us,
                slot: input.slot,
                user: s.user,
                is_buy: s.is_buy,
                sol: s.quote_amount_user,
                tokens: s.base_amount,
                price_after: 0.0,
                venue: Venue::PumpSwap,
                user_quote: s.quote_amount_user,
            },
            _ => return true,
        };
        self.record_trade(obs);
        // keep the deque time-ordered for window analytics
        self.trades.make_contiguous().sort_by_key(|t| t.ts_us);
        true
    }

    /// Rebuild from retained inputs, excluding a dead slot.
    pub fn rebuild_without_slot(&self, dead_slot: u64) -> TokenState {
        let mut fresh = TokenState::new(self.mint, Origin::Discovered, self.first_seen_us);
        fresh.rolled_back_slots = self.rolled_back_slots + 1;
        for inp in self.inputs.iter().filter(|i| i.slot != dead_slot) {
            fresh.apply(inp);
            fresh.inputs.push_back(inp.clone());
        }
        fresh
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Launch,
    Trade { is_buy: bool },
    CurveComplete,
    Migrated,
    PoolCreated,
    LiquidityRemoved,
    LiquidityAdded,
}

#[derive(Debug, Clone)]
pub struct Applied {
    pub mint: Pubkey,
    pub change: Change,
    pub user: Option<Pubkey>,
}

/// All tracked tokens plus pool→mint resolution and slot index for rollbacks.
#[derive(Default)]
pub struct Universe {
    pub tokens: HashMap<Pubkey, TokenState>,
    pub pool_to_mint: HashMap<Pubkey, Pubkey>,
    pub slot_mints: HashMap<u64, HashSet<Pubkey>>,
    pub unresolved_pool_events: u64,
    /// pools seen in swaps but not yet mapped to a mint (drained by the engine for RPC lookup)
    pub pending_pools: Vec<Pubkey>,
    pub pools_resolved: u64,
    pub non_sol_curves_seen: u64,
    pub non_sol_pools_ignored: u64,
    pub raydium_decode_only: u64,
    pub max_tokens: usize,
}

impl Universe {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens, ..Default::default() }
    }

    fn resolve_mint(&mut self, ev: &VenueEvent) -> Option<(Pubkey, Change, Option<Pubkey>)> {
        match ev {
            VenueEvent::Pump(PumpEvent::Create(c)) => Some((c.mint, Change::Launch, Some(c.creator.unwrap_or(c.user)))),
            VenueEvent::Pump(PumpEvent::Trade(t)) => Some((t.mint, Change::Trade { is_buy: t.is_buy }, Some(t.user))),
            VenueEvent::Pump(PumpEvent::Complete(c)) => Some((c.mint, Change::CurveComplete, None)),
            VenueEvent::Pump(PumpEvent::Migration(m)) => {
                self.pool_to_mint.insert(m.pool, m.mint);
                Some((m.mint, Change::Migrated, None))
            }
            VenueEvent::PumpSwap(PumpSwapEvent::CreatePool(c)) => {
                let wsol = Pubkey::from_b58(WSOL_MINT).ok()?;
                if c.quote_mint != wsol {
                    self.non_sol_pools_ignored += 1;
                    return None;
                }
                self.pool_to_mint.insert(c.pool, c.base_mint);
                Some((c.base_mint, Change::PoolCreated, Some(c.creator)))
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) => match self.pool_to_mint.get(&s.pool) {
                Some(m) => Some((*m, Change::Trade { is_buy: s.is_buy }, Some(s.user))),
                None => {
                    self.unresolved_pool_events += 1;
                    if !self.pending_pools.contains(&s.pool) && self.pending_pools.len() < 10_000 {
                        self.pending_pools.push(s.pool);
                    }
                    None
                }
            },
            VenueEvent::PumpSwap(PumpSwapEvent::Liquidity(l)) => match self.pool_to_mint.get(&l.pool) {
                Some(m) => Some((
                    *m,
                    if l.is_deposit { Change::LiquidityAdded } else { Change::LiquidityRemoved },
                    Some(l.user),
                )),
                None => {
                    self.unresolved_pool_events += 1;
                    None
                }
            },
            VenueEvent::Raydium(_) => {
                self.raydium_decode_only += 1;
                None
            }
        }
    }

    /// Register an externally resolved pool (e.g. RPC pool-account lookup in live mode).
    pub fn register_pool(&mut self, pool: Pubkey, mint: Pubkey) {
        if self.pool_to_mint.insert(pool, mint).is_none() {
            self.pools_resolved += 1;
        }
    }

    pub fn apply(&mut self, input: Input) -> Option<Applied> {
        let (mint, change, user) = self.resolve_mint(&input.event)?;
        if !self.tokens.contains_key(&mint) && self.max_tokens > 0 && self.tokens.len() >= self.max_tokens {
            self.evict_oldest();
        }
        let origin = if change == Change::Launch { Origin::Launch } else { Origin::Discovered };
        let st = self.tokens.entry(mint).or_insert_with(|| TokenState::new(mint, origin, input.ts_us));
        if st.apply(&input) {
            self.slot_mints.entry(input.slot).or_default().insert(mint);
            st.inputs.push_back(input);
            while st.inputs.len() > MAX_INPUTS_PER_TOKEN {
                st.inputs.pop_front();
            }
            Some(Applied { mint, change, user })
        } else {
            None
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self.tokens.values().min_by_key(|t| t.last_update_us).map(|t| t.mint) {
            self.tokens.remove(&oldest);
        }
    }

    /// Roll back every token touched in `slot`. Returns affected mints.
    pub fn rollback_slot(&mut self, slot: u64) -> Vec<Pubkey> {
        let Some(mints) = self.slot_mints.remove(&slot) else { return vec![] };
        let mut affected = Vec::new();
        for m in mints {
            if let Some(st) = self.tokens.get(&m) {
                let rebuilt = st.rebuild_without_slot(slot);
                if rebuilt.inputs.is_empty() {
                    self.tokens.remove(&m);
                } else {
                    self.tokens.insert(m, rebuilt);
                }
                affected.push(m);
            }
        }
        affected
    }

    /// Forget slot index entries older than `min_slot` (finalized; can no longer roll back).
    pub fn prune_slots(&mut self, min_slot: u64) {
        self.slot_mints.retain(|s, _| *s >= min_slot);
    }
}
