//! Execution-quality evidence: after each simulated fill, the next REAL market trade in the same
//! direction on the same token (within a window) is compared with our simulated effective price.
//! This separates three things on every order: the decision quote, the simulated fill, and an
//! observed market execution.

use crate::paper::Order;
use crate::quote::Side;
use crate::state::TradeObs;
use crate::types::Pubkey;
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
}

#[derive(Clone, Debug)]
struct Pending {
    check: ExecutionCheck,
    mint: Pubkey,
    deadline_us: i64,
}

#[derive(Default)]
pub struct ExecChecker {
    pending: VecDeque<Pending>,
    pub window_us: i64,
}

impl ExecChecker {
    pub fn new(window_s: f64) -> Self {
        Self { pending: VecDeque::new(), window_us: (window_s * 1e6) as i64 }
    }

    /// Register a filled order.
    pub fn add(&mut self, o: &Order, fill_us: i64) {
        let Some(f) = &o.fill else { return };
        if f.out_amount == 0 || f.in_amount == 0 {
            return;
        }
        let (price, quote) = match o.side {
            Side::Buy => (f.in_amount as f64 / f.out_amount as f64, f.in_amount),
            Side::Sell => (f.out_amount as f64 / f.in_amount as f64, f.out_amount),
        };
        self.pending.push_back(Pending {
            mint: o.mint,
            deadline_us: fill_us + self.window_us,
            check: ExecutionCheck {
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
                outcome: "pending".into(),
                fill_us,
            },
        });
    }

    /// A real trade was observed; resolve matching checks.
    pub fn on_trade(&mut self, mint: &Pubkey, t: &TradeObs) -> Vec<ExecutionCheck> {
        if t.tokens == 0 || t.user_quote == 0 {
            return vec![];
        }
        let side = if t.is_buy { Side::Buy } else { Side::Sell };
        let market = t.user_quote as f64 / t.tokens as f64;
        let mut done = Vec::new();
        self.pending.retain(|p| {
            if p.mint != *mint || p.check.side != side || t.ts_us < p.check.fill_us || t.ts_us > p.deadline_us {
                return true;
            }
            let mut c = p.check.clone();
            c.market_price = Some(market);
            c.market_quote_lamports = Some(t.user_quote);
            c.adverse_bps = Some(match side {
                Side::Buy => (c.our_price / market - 1.0) * 10_000.0,
                Side::Sell => (market / c.our_price - 1.0) * 10_000.0,
            });
            c.delay_ms = Some((t.ts_us - c.fill_us) as f64 / 1e3);
            c.outcome = "compared".into();
            done.push(c);
            false
        });
        done
    }

    pub fn expire(&mut self, now: i64) -> Vec<ExecutionCheck> {
        let mut done = Vec::new();
        self.pending.retain(|p| {
            if now > p.deadline_us {
                let mut c = p.check.clone();
                c.outcome = "no_comparable_trade".into();
                done.push(c);
                false
            } else {
                true
            }
        });
        done
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
        no_comparable_trade: v.iter().filter(|c| c.outcome == "no_comparable_trade").count() as u64,
        median_adverse_bps: xs.get(xs.len() / 2).copied(),
        mean_adverse_bps: if xs.is_empty() { None } else { Some(xs.iter().sum::<f64>() / xs.len() as f64) },
        within_150bps: if xs.is_empty() { None } else { Some(xs.iter().filter(|x| x.abs() <= 150.0).count() as f64 / xs.len() as f64) },
    }
}
