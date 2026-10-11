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

/// Fixed, data-independent split: even UTC days → "calibration", odd UTC days → "holdout".
/// Thresholds may only be tuned on calibration data; reported performance uses holdout data.
pub fn evidence_split(ts_us: i64) -> &'static str {
    if (ts_us.div_euclid(86_400_000_000)) % 2 == 0 { "calibration" } else { "holdout" }
}

#[derive(Clone, Debug, Serialize)]
pub struct SensitivityRow {
    pub label: String,
    pub fee_mult: f64,
    pub extra_slippage_bps_per_side: f64,
    pub priority_fee_mult: f64,
    pub trades: u32,
    pub net_sol: f64,
    pub expectancy_sol: Option<f64>,
    pub profit_factor: Option<f64>,
    pub win_rate: Option<f64>,
}

/// Re-cost closed trades under harsher fee / slippage / priority-fee assumptions.
pub fn sensitivity(closed: &[Position], priority_fee_per_tx: u64) -> Vec<SensitivityRow> {
    let grid: &[(&str, f64, f64, f64)] = &[
        ("as simulated", 1.0, 0.0, 1.0),
        ("venue fees ×1.5", 1.5, 0.0, 1.0),
        ("venue fees ×2", 2.0, 0.0, 1.0),
        ("+50 bps slippage/side", 1.0, 50.0, 1.0),
        ("+100 bps slippage/side", 1.0, 100.0, 1.0),
        ("+200 bps slippage/side", 1.0, 200.0, 1.0),
        ("priority fee ×2", 1.0, 0.0, 2.0),
        ("priority fee ×5", 1.0, 0.0, 5.0),
        ("stress: fees ×1.5, +100 bps, priority ×2", 1.5, 100.0, 2.0),
    ];
    let trades: Vec<&Position> = closed.iter().filter(|p| p.realized_pnl.is_some()).collect();
    grid.iter()
        .map(|(label, fm, slip, pm)| {
            let pnls: Vec<i64> = trades
                .iter()
                .map(|p| {
                    let entry_in = p.cost_basis.saturating_sub(p.entry_tx_costs);
                    let extra = (fm - 1.0) * p.venue_fees_paid as f64
                        + slip / 10_000.0 * (entry_in + p.proceeds) as f64
                        + (pm - 1.0) * 2.0 * priority_fee_per_tx as f64;
                    p.realized_pnl.unwrap() - extra.round() as i64
                })
                .collect();
            let n = pnls.len() as u32;
            let gp: i64 = pnls.iter().filter(|x| **x > 0).sum();
            let gl: i64 = -pnls.iter().filter(|x| **x <= 0).sum::<i64>();
            let net: i64 = pnls.iter().sum();
            SensitivityRow {
                label: label.to_string(),
                fee_mult: *fm,
                extra_slippage_bps_per_side: *slip,
                priority_fee_mult: *pm,
                trades: n,
                net_sol: sol(net),
                expectancy_sol: (n > 0).then(|| sol(net) / n as f64),
                profit_factor: (gl > 0).then(|| gp as f64 / gl as f64),
                win_rate: (n > 0).then(|| pnls.iter().filter(|x| **x > 0).count() as f64 / n as f64),
            }
        })
        .collect()
}

pub fn size_label(cost_basis: u64) -> String {
    // nearest configured size bucket (0.02 / 0.05 / 0.1 SOL)
    let s = cost_basis as f64 / 1e9;
    let b = [0.02, 0.05, 0.1].iter().copied().min_by(|a, b| (a - s).abs().partial_cmp(&(b - s).abs()).unwrap()).unwrap();
    format!("{b:.2}")
}
