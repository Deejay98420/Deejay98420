//! Measured latencies, route coverage and trading performance statistics.
//! Latency numbers are measurements of this process (wall clock), never assumed values.

use crate::paper::Position;
use crate::types::sol;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, Default)]
pub struct LatencySeries {
    samples: VecDeque<i64>,
    cap: usize,
    pub count: u64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct LatencySummary {
    pub count: u64,
    pub p50_us: Option<i64>,
    pub p95_us: Option<i64>,
    pub p99_us: Option<i64>,
    pub max_us: Option<i64>,
}

impl LatencySeries {
    pub fn new(cap: usize) -> Self {
        Self { samples: VecDeque::with_capacity(cap), cap, count: 0 }
    }
    pub fn record(&mut self, us: i64) {
        if self.cap == 0 {
            self.cap = 4096;
        }
        if self.samples.len() >= self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(us);
        self.count += 1;
    }
    pub fn summary(&self) -> LatencySummary {
        if self.samples.is_empty() {
            return LatencySummary { count: self.count, ..Default::default() };
        }
        let mut v: Vec<i64> = self.samples.iter().copied().collect();
        v.sort_unstable();
        let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        LatencySummary { count: self.count, p50_us: Some(pct(0.5)), p95_us: Some(pct(0.95)), p99_us: Some(pct(0.99)), max_us: v.last().copied() }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RouteStats {
    pub attempts: u64,
    pub ok: u64,
    pub no_route: u64,
    pub stale: u64,
    pub rate_limited: u64,
    pub errors: u64,
    pub disabled: u64,
    pub last_error: Option<String>,
    pub last_ok_us: Option<i64>,
}

impl RouteStats {
    pub fn availability(&self) -> Option<f64> {
        let denom = self.attempts - self.disabled;
        if denom == 0 { None } else { Some(self.ok as f64 / denom as f64) }
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct PerfStats {
    pub closed_trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub win_rate: Option<f64>,
    pub realized_pnl_sol: f64,
    pub gross_profit_sol: f64,
    pub gross_loss_sol: f64,
    pub expectancy_sol: Option<f64>,
    pub profit_factor: Option<f64>,
    /// max drawdown of cumulative realized P&L (SOL, >= 0)
    pub max_drawdown_sol: f64,
    pub avg_hold_s: Option<f64>,
    pub total_fees_sol: f64,
    pub exit_reasons: BTreeMap<String, u32>,
}

pub fn perf(closed: &[Position]) -> PerfStats {
    let mut v: Vec<&Position> = closed.iter().filter(|p| p.realized_pnl.is_some()).collect();
    v.sort_by_key(|p| p.closed_us.unwrap_or(0));
    let mut s = PerfStats::default();
    let mut cum = 0i64;
    let mut peak = 0i64;
    let mut dd = 0i64;
    let mut hold = 0.0;
    for p in &v {
        let pnl = p.realized_pnl.unwrap();
        s.closed_trades += 1;
        if pnl > 0 {
            s.wins += 1;
            s.gross_profit_sol += sol(pnl);
        } else {
            s.losses += 1;
            s.gross_loss_sol += sol(-pnl);
        }
        cum += pnl;
        peak = peak.max(cum);
        dd = dd.max(peak - cum);
        hold += p.hold_s(p.closed_us.unwrap_or(p.opened_us));
        s.total_fees_sol += sol((p.entry_tx_costs + p.exit_tx_costs + p.failed_exit_costs + p.venue_fees_paid) as i64);
        if let Some(r) = &p.exit_reason {
            *s.exit_reasons.entry(r.code()).or_default() += 1;
        }
    }
    s.realized_pnl_sol = sol(cum);
    s.max_drawdown_sol = sol(dd);
    if s.closed_trades > 0 {
        s.win_rate = Some(s.wins as f64 / s.closed_trades as f64);
        s.expectancy_sol = Some(s.realized_pnl_sol / s.closed_trades as f64);
        s.avg_hold_s = Some(hold / s.closed_trades as f64);
        s.profit_factor = if s.gross_loss_sol > 0.0 { Some(s.gross_profit_sol / s.gross_loss_sol) } else { None };
    }
    s
}
