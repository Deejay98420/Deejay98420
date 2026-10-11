//! Quote abstraction. Venue-local simulation (exact integer math against tracked reserves) and
//! Jupiter aggregator quotes. Every quote records provider, timestamp, latency, state age and errors.
//! Quotes are read-only: nothing here builds, signs or sends a transaction.

pub mod jupiter;
pub mod math;
pub mod ratelimit;

use crate::state::{Phase, TokenState};
use crate::time::wall_us;
use crate::types::{Pubkey, Venue};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSource {
    VenueSim,
    Jupiter,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub source: QuoteSource,
    pub venue: Option<Venue>,
    pub side: Side,
    pub mint: Pubkey,
    /// Buy: lamports in, tokens out. Sell: tokens in, lamports out.
    pub in_amount: u64,
    pub out_amount: u64,
    /// Venue/aggregator trading fee inside the swap (lamports).
    pub venue_fee_lamports: u64,
    /// Execution price vs marginal price, in bps (>= 0 means worse than marginal).
    pub price_impact_bps: f64,
    pub capped_by_reserves: bool,
    pub state_slot: u64,
    pub state_age_ms: f64,
    pub quoted_at_us: i64,
    pub latency_us: i64,
    pub route: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum QuoteError {
    #[error("no route: {0}")]
    NoRoute(String),
    #[error("state stale: {0:.0} ms old")]
    StaleState(f64),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("timeout")]
    Timeout,
    #[error("http {0}")]
    Http(String),
    #[error("disabled: {0}")]
    Disabled(String),
    #[error("parse: {0}")]
    Parse(String),
}

#[derive(Clone, Debug)]
pub struct VenueFees {
    pub curve_fee_bps_default: u64,
    pub pumpswap_fee_bps: u64,
}

/// Venue-local quote from tracked reserves. `now_us` is the engine clock.
///
/// `own_tokens`: tokens we (paper) hold. Paper fills never touch the real reserves, so before quoting
/// a sell the reserves are adjusted as if our earlier buy of `own_tokens` were still in the pool
/// (constant-product consistent). Without this, exit values would be biased.
pub fn venue_quote(
    st: &TokenState,
    side: Side,
    amount: u64,
    own_tokens: u64,
    fees: &VenueFees,
    max_state_age_ms: f64,
    now_us: i64,
) -> Result<Quote, QuoteError> {
    let t0 = wall_us();
    let age_ms = (now_us - st.last_update_us).max(0) as f64 / 1e3;
    if age_ms > max_state_age_ms {
        return Err(QuoteError::StaleState(age_ms));
    }
    let mut marginal = st.marginal_price();
    let (venue, res) = match st.phase {
        Phase::BondingCurve => {
            let mut c = st.curve.ok_or_else(|| QuoteError::NoRoute("curve reserves unknown".into()))?;
            if own_tokens > 0 && own_tokens < c.virtual_token {
                let k = c.virtual_sol as u128 * c.virtual_token as u128;
                let vt = c.virtual_token - own_tokens;
                let vs = k.div_ceil(vt as u128) as u64;
                c.real_sol += vs - c.virtual_sol;
                c.real_token = c.real_token.saturating_sub(own_tokens);
                c.virtual_sol = vs;
                c.virtual_token = vt;
                marginal = Some(c.virtual_sol as f64 / c.virtual_token as f64);
            }
            let fee = st.observed_curve_fee_bps.unwrap_or(fees.curve_fee_bps_default);
            let r = match side {
                Side::Buy => math::curve_buy(c.virtual_sol, c.virtual_token, c.real_token, amount, fee),
                Side::Sell => math::curve_sell(c.virtual_sol, c.virtual_token, c.real_sol, amount, fee),
            };
            (Venue::PumpBondingCurve, r)
        }
        Phase::Amm => {
            let mut p = st.pool.ok_or_else(|| QuoteError::NoRoute("pool reserves unknown".into()))?;
            if own_tokens > 0 && own_tokens < p.base {
                let k = p.base as u128 * p.quote as u128;
                p.base -= own_tokens;
                p.quote = k.div_ceil(p.base as u128) as u64;
                marginal = Some(p.quote as f64 / p.base as f64);
            }
            let r = match side {
                Side::Buy => math::cpmm_buy(p.base, p.quote, amount, fees.pumpswap_fee_bps),
                Side::Sell => math::cpmm_sell(p.base, p.quote, amount, fees.pumpswap_fee_bps),
            };
            (Venue::PumpSwap, r)
        }
        Phase::AwaitingMigration => {
            return Err(QuoteError::NoRoute("bonding curve complete; migration pool not yet observed".into()))
        }
    };
    let r = res.ok_or_else(|| QuoteError::NoRoute(format!("{} returned zero output", venue.as_str())))?;
    let impact = marginal
        .filter(|m| *m > 0.0)
        .map(|m| match side {
            // ideal tokens = (in - fee)/m
            Side::Buy => {
                let ideal = (r.amount_in - r.fee) as f64 / m;
                (1.0 - r.amount_out as f64 / ideal) * 10_000.0
            }
            Side::Sell => {
                let ideal = r.amount_in as f64 * m;
                (1.0 - (r.amount_out + r.fee) as f64 / ideal) * 10_000.0
            }
        })
        .unwrap_or(f64::NAN);
    Ok(Quote {
        source: QuoteSource::VenueSim,
        venue: Some(venue),
        side,
        mint: st.mint,
        in_amount: r.amount_in,
        out_amount: r.amount_out,
        venue_fee_lamports: r.fee,
        price_impact_bps: impact,
        capped_by_reserves: r.capped,
        state_slot: st.last_slot,
        state_age_ms: age_ms,
        quoted_at_us: now_us,
        latency_us: wall_us() - t0,
        route: venue.as_str().into(),
    })
}
