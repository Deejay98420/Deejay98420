//! Dynamic exit signals. Exits react to the executable liquidation value of the position (a
//! size-aware sell quote, never last price) and to on-chain state: flow deterioration, liquidity
//! withdrawal, creator dumping, stagnation and hold time. There are no fixed take-profit targets.

use crate::config::ExitPolicy;
use crate::state::analytics::Features;
use crate::types::sol;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Normal,
    Emergency,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ExitReason {
    TrailingStop { peak_value: u64, value: u64, width: f64 },
    AdverseStop { pnl_fraction: f64, limit: f64 },
    MomentumDeterioration { buy_ratio_30s: f64, net_flow_sol_30s: f64 },
    LiquidityWithdrawal { drop_fraction: f64 },
    RugWarning { detail: String },
    Stagnation { secs_since_peak: f64 },
    MaxHold { held_s: f64 },
    RouteLost { detail: String },
}

impl ExitReason {
    pub fn code(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(String::from))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub struct PositionView {
    pub opened_us: i64,
    pub cost_basis_lamports: u64,
    pub peak_value_lamports: u64,
    pub peak_us: i64,
    pub entry_liquidity_lamports: u64,
    pub entry_creator_sold_fraction: f64,
}

pub fn dynamic_width(vol: f64, mult: f64, lo: f64, hi: f64) -> f64 {
    // vol is per-trade log-return stdev; scale to ~10 trades of noise
    (mult * vol * 10f64.sqrt()).clamp(lo, hi)
}

/// `value`: current executable liquidation value (lamports, after venue fees), None = no route.
pub fn exit_signal(f: &Features, pv: &PositionView, value: Option<u64>, cfg: &ExitPolicy, now_us: i64) -> Option<(ExitReason, Urgency)> {
    // Rug warnings first: creator dumping or liquidity pulled.
    if f.creator_sold_fraction > pv.entry_creator_sold_fraction + 1e-9 && f.creator_sold_fraction >= cfg.creator_sell_fraction {
        return Some((
            ExitReason::RugWarning { detail: format!("creator sold {:.0}% of its buys", f.creator_sold_fraction * 100.0) },
            Urgency::Emergency,
        ));
    }
    if f.phase == crate::state::Phase::AwaitingMigration {
        // Curve completed: liquidity moves to the migration pool. Not a withdrawal; no route yet.
        return None;
    }
    if pv.entry_liquidity_lamports > 0 {
        let drop = 1.0 - f.liquidity_lamports as f64 / pv.entry_liquidity_lamports as f64;
        if drop >= cfg.liquidity_drop_fraction {
            let urg = if f.liquidity_removed_lamports > 0 { Urgency::Emergency } else { Urgency::Normal };
            if f.liquidity_removed_lamports > 0 {
                return Some((
                    ExitReason::RugWarning { detail: format!("LP removed {:.2} SOL", sol(f.liquidity_removed_lamports as i64)) },
                    urg,
                ));
            }
            return Some((ExitReason::LiquidityWithdrawal { drop_fraction: drop }, urg));
        }
    }
    let Some(value) = value else {
        return Some((ExitReason::RouteLost { detail: "no executable exit quote".into() }, Urgency::Normal));
    };
    let pnl = value as f64 / pv.cost_basis_lamports.max(1) as f64 - 1.0;
    let stop = dynamic_width(f.volatility_60s, cfg.stop_vol_mult, cfg.min_stop, cfg.max_stop);
    if pnl <= -stop {
        return Some((ExitReason::AdverseStop { pnl_fraction: pnl, limit: -stop }, Urgency::Normal));
    }
    if pv.peak_value_lamports > pv.cost_basis_lamports {
        let width = dynamic_width(f.volatility_60s, cfg.trail_vol_mult, cfg.min_trail, cfg.max_trail);
        if (value as f64) < pv.peak_value_lamports as f64 * (1.0 - width) {
            return Some((ExitReason::TrailingStop { peak_value: pv.peak_value_lamports, value, width }, Urgency::Normal));
        }
    }
    let br = f.w30.buy_ratio();
    let nf = sol(f.w30.net_flow_lamports());
    if (f.w30.buys + f.w30.sells) >= 4 && br < cfg.deteriorate_buy_ratio_30s && nf < cfg.deteriorate_net_flow_sol_30s {
        return Some((ExitReason::MomentumDeterioration { buy_ratio_30s: br, net_flow_sol_30s: nf }, Urgency::Normal));
    }
    let since_peak = (now_us - pv.peak_us) as f64 / 1e6;
    if since_peak >= cfg.stagnation_s {
        return Some((ExitReason::Stagnation { secs_since_peak: since_peak }, Urgency::Normal));
    }
    let held = (now_us - pv.opened_us) as f64 / 1e6;
    if held >= cfg.max_hold_s {
        return Some((ExitReason::MaxHold { held_s: held }, Urgency::Normal));
    }
    None
}
