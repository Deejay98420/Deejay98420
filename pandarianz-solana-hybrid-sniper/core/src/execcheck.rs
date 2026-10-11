//! Execution-quality evidence: each simulated fill is compared with the most recent REAL market trade
//! in the same direction on the same token AT OR BEFORE the fill time (within a window). Future
//! trades are never used (v1.1 compared with the next trade, which biased results in trending
//! markets and used information not available at fill time).

use crate::paper::Order;
use crate::quote::Side;
use crate::state::TradeObs;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExecutionCheck {
    pub order_id: String,
    pub book_id: String,
    pub scenario: String,
    pub mint: String,
    pub side: Side,
    /// our simulated effective price, lamports per raw token unit (swap fees included, tx fees not)
    pub our_price: f64,
    pub our_quote_lamports: u64,
    pub market_price: Option<f64>,
    pub market_quote_lamports: Option<u64>,
    /// positive = our simulated fill was WORSE than the observed market execution
    pub adverse_bps: Option<f64>,
    pub delay_ms: Option<f64>,
    pub outcome: String,
    pub fill_us: i64,
    /// age of the compared market trade at fill time (ms, >= 0)
    #[serde(default)]
    pub market_trade_age_ms: Option<f64>,
    /// market trade size / our size (quote lamports); far from 1 → price impact differs
    #[serde(default)]
    pub size_ratio: Option<f64>,
}

/// Compare a filled order with the latest same-side trade at or before `fill_us` (no look-ahead).
pub fn check_prior(o: &Order, fill_us: i64, trades: &std::collections::VecDeque<TradeObs>, window_us: i64) -> Option<ExecutionCheck> {
    let f = o.fill.as_ref()?;
    if f.out_amount == 0 || f.in_amount == 0 {
        return None;
    }
    let (price, quote) = match o.side {
        Side::Buy => (f.in_amount as f64 / f.out_amount as f64, f.in_amount),
        Side::Sell => (f.out_amount as f64 / f.in_amount as f64, f.out_amount),
    };
    let want_buy = o.side == Side::Buy;
    let prior = trades.iter().rev().find(|t| t.ts_us <= fill_us && t.ts_us >= fill_us - window_us && t.is_buy == want_buy && t.tokens > 0 && t.user_quote > 0);
    let mut c = ExecutionCheck {
        order_id: o.order_id.clone(),
        book_id: o.book_id.clone(),
        scenario: o.scenario.clone(),
        mint: o.mint.to_string(),
        side: o.side,
        our_price: price,
        our_quote_lamports: quote,
        market_price: None,
        market_quote_lamports: None,
        adverse_bps: None,
        delay_ms: None,
        outcome: "no_prior_trade".into(),
        fill_us,
        market_trade_age_ms: None,
        size_ratio: None,
    };
    if let Some(t) = prior {
        let market = t.user_quote as f64 / t.tokens as f64;
        c.market_price = Some(market);
        c.market_quote_lamports = Some(t.user_quote);
        c.adverse_bps = Some(match o.side {
            Side::Buy => (price / market - 1.0) * 10_000.0,
            Side::Sell => (market / price - 1.0) * 10_000.0,
        });
        c.market_trade_age_ms = Some((fill_us - t.ts_us) as f64 / 1e3);
        c.size_ratio = Some(t.user_quote as f64 / quote as f64);
        c.outcome = "compared_prior".into();
    }
    Some(c)
}

/// Thin wrapper holding the comparison window.
#[derive(Default)]
pub struct ExecChecker {
    pub window_us: i64,
}

impl ExecChecker {
    pub fn new(window_s: f64) -> Self {
        Self { window_us: (window_s * 1e6) as i64 }
    }
    pub fn check(&self, o: &Order, fill_us: i64, trades: &std::collections::VecDeque<TradeObs>) -> Option<ExecutionCheck> {
        check_prior(o, fill_us, trades, self.window_us)
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct ExecSummary {
    pub checks: u64,
    pub compared: u64,
    pub no_comparable_trade: u64,
    pub median_adverse_bps: Option<f64>,
    pub mean_adverse_bps: Option<f64>,
    /// fraction of compared fills within ±150 bps of the observed market execution
    pub within_150bps: Option<f64>,
}

pub fn summarize(v: &VecDeque<ExecutionCheck>) -> ExecSummary {
    let mut xs: Vec<f64> = v.iter().filter_map(|c| c.adverse_bps).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ExecSummary {
        checks: v.len() as u64,
        compared: xs.len() as u64,
        no_comparable_trade: v.iter().filter(|c| c.outcome == "no_prior_trade").count() as u64,
        median_adverse_bps: xs.get(xs.len() / 2).copied(),
        mean_adverse_bps: if xs.is_empty() { None } else { Some(xs.iter().sum::<f64>() / xs.len() as f64) },
        within_150bps: if xs.is_empty() { None } else { Some(xs.iter().filter(|x| x.abs() <= 150.0).count() as f64 / xs.len() as f64) },
    }
}
