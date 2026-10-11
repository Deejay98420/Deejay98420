//! Entry evaluation and size selection.

use super::{EntryPath, RejectReason};
use crate::config::{CostConfig, EntryCommon, ScenarioConfig, StrategyConfig};
use crate::quote::{venue_quote, Quote, Side, VenueFees};
use crate::state::analytics::Features;
use crate::state::{Origin, Phase, TokenState};
use crate::types::{sol, LAMPORTS_PER_SOL};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SizeCheck {
    pub size_lamports: u64,
    pub entry_impact_bps: f64,
    pub round_trip_cost_bps: f64,
    pub tokens_out: u64,
    pub ok: bool,
    pub fail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntryDecision {
    pub path: EntryPath,
    pub score: f64,
    pub enter: bool,
    pub size_lamports: u64,
    pub quote: Option<Quote>,
    pub reasons: Vec<RejectReason>,
    pub size_checks: Vec<SizeCheck>,
    /// heuristic, UNCALIBRATED expected favourable move (bps) — see `expected_move_bps`
    pub expected_move_bps: f64,
    /// expected move − estimated round-trip cost of the selected (or smallest checked) size
    pub net_edge_bps: Option<f64>,
}

/// Heuristic expected move: scale × blend(score, persistence, acceleration). It is a prior, not a
/// prediction; forward evaluation records whether it is calibrated.
pub fn expected_move_bps(score: f64, f: &Features, cfg: &StrategyConfig) -> f64 {
    let accel = clamp01((f.buy_acceleration - 0.5) / 1.5);
    cfg.entry.expected_move_scale_bps * (0.5 * clamp01(score) + 0.3 * clamp01(f.momentum_persistence) + 0.2 * accel)
}

fn clamp01(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

pub fn classify(st: &TokenState, f: &Features, cfg: &StrategyConfig) -> EntryPath {
    if st.origin == Origin::Launch && f.age_s <= cfg.launch.max_age_s && st.phase == Phase::BondingCurve {
        EntryPath::Launch
    } else {
        EntryPath::Momentum
    }
}

pub fn score(path: EntryPath, f: &Features, cfg: &StrategyConfig) -> f64 {
    match path {
        EntryPath::Launch => {
            let p = &cfg.launch;
            let s1 = clamp01(f.w30.unique_buyers as f64 / (2.0 * p.min_unique_buyers_30s.max(1) as f64));
            let s2 = clamp01((f.w30.buy_ratio() - 0.5) / 0.4);
            let s3 = clamp01(sol(f.w30.net_flow_lamports()) / (3.0 * p.min_net_flow_sol_30s.max(0.01)));
            let s4 = 1.0 - 0.5 * clamp01(f.top10_share / p.max_top10_share.max(0.01));
            0.30 * s1 + 0.25 * s2 + 0.30 * s3 + 0.15 * s4
        }
        EntryPath::Momentum => {
            let p = &cfg.momentum;
            let s1 = clamp01(f.w60.unique_buyers as f64 / (2.0 * p.min_unique_buyers_60s.max(1) as f64));
            let s2 = clamp01((f.w60.buy_ratio() - 0.5) / 0.4);
            let chg = f.w60.price_change.unwrap_or(0.0);
            let s3 = clamp01(chg / (3.0 * p.min_price_change_60s.max(0.001)));
            let s4 = clamp01(sol(f.w60.net_flow_lamports()) / (3.0 * p.min_net_flow_sol_60s.max(0.01)));
            0.25 * s1 + 0.25 * s2 + 0.25 * s3 + 0.25 * s4
        }
    }
}

fn market_filters(path: EntryPath, st: &TokenState, f: &Features, cfg: &StrategyConfig, lag_ms: Option<f64>) -> Vec<RejectReason> {
    let e = &cfg.entry;
    let mut r = Vec::new();
    if f.quote_non_sol {
        r.push(RejectReason::NonSolQuote);
    }
    if f.trades_total >= 5 && f.liquidity_stability < e.min_liquidity_stability {
        r.push(RejectReason::LiquidityUnstable { stability: f.liquidity_stability, min: e.min_liquidity_stability });
    }
    if f.ms_since_last_event > e.max_state_age_ms {
        r.push(RejectReason::StaleState { age_ms: f.ms_since_last_event, max_ms: e.max_state_age_ms });
    }
    if let Some(l) = lag_ms {
        if l > e.max_delivery_lag_ms {
            r.push(RejectReason::DeliveryLag { lag_ms: l, max_ms: e.max_delivery_lag_ms });
        }
    }
    if st.venue().is_none() {
        r.push(RejectReason::NoRoute { detail: format!("phase {:?} has no tracked venue route", st.phase) });
    }
    if f.creator_sold_fraction > 0.0 && f.age_s < 600.0 {
        r.push(RejectReason::CreatorSelling { sold_fraction: f.creator_sold_fraction });
    }
    if f.liquidity_removed_lamports > 0 {
        r.push(RejectReason::LiquidityRemoved { sol: sol(f.liquidity_removed_lamports as i64) });
    }
    if e.require_non_creator_sell && !f.non_creator_sell_seen {
        r.push(RejectReason::Unsellable { detail: "no successful sell by a non-creator wallet observed yet".into() });
    }
    match path {
        EntryPath::Launch => {
            let p = &cfg.launch;
            if f.observed_s < p.min_observed_s {
                r.push(RejectReason::TooEarly { observed_s: f.observed_s, need_s: p.min_observed_s });
            }
            if let Some(cp) = f.curve_progress {
                if cp > p.max_curve_progress {
                    r.push(RejectReason::CurveNearlyComplete { progress: cp, max: p.max_curve_progress });
                }
            }
            let liq = sol(f.liquidity_lamports as i64);
            if liq < p.min_liquidity_sol {
                r.push(RejectReason::LowLiquidity { sol: liq, need_sol: p.min_liquidity_sol });
            }
            if f.w30.unique_buyers < p.min_unique_buyers_30s {
                r.push(RejectReason::FewUniqueBuyers { n: f.w30.unique_buyers, need: p.min_unique_buyers_30s, window_s: 30 });
            }
            if f.w30.buy_ratio() < p.min_buy_ratio_30s {
                r.push(RejectReason::WeakBuyRatio { ratio: f.w30.buy_ratio(), need: p.min_buy_ratio_30s });
            }
            let nf = sol(f.w30.net_flow_lamports());
            if nf < p.min_net_flow_sol_30s {
                r.push(RejectReason::WeakNetFlow { sol: nf, need_sol: p.min_net_flow_sol_30s });
            }
            if f.buyers_in_create_slot > p.max_buyers_in_create_slot {
                r.push(RejectReason::BundledLaunch { buyers_in_create_slot: f.buyers_in_create_slot, max: p.max_buyers_in_create_slot });
            }
            if f.top1_share > p.max_top1_share || f.top10_share > p.max_top10_share {
                r.push(RejectReason::Concentrated { top1: f.top1_share, top10: f.top10_share });
            }
            if f.creator_share > p.max_creator_share {
                r.push(RejectReason::CreatorHoldsTooMuch { share: f.creator_share, max: p.max_creator_share });
            }
            if f.round_trip_wallet_fraction > p.max_round_trip_fraction {
                r.push(RejectReason::WashTrading { round_trip_fraction: f.round_trip_wallet_fraction, max: p.max_round_trip_fraction });
            }
        }
        EntryPath::Momentum => {
            let p = &cfg.momentum;
            if f.observed_s < p.min_observed_s {
                r.push(RejectReason::TooEarly { observed_s: f.observed_s, need_s: p.min_observed_s });
            }
            let liq = sol(f.liquidity_lamports as i64);
            if liq < p.min_liquidity_sol {
                r.push(RejectReason::LowLiquidity { sol: liq, need_sol: p.min_liquidity_sol });
            }
            if f.w60.unique_buyers < p.min_unique_buyers_60s {
                r.push(RejectReason::FewUniqueBuyers { n: f.w60.unique_buyers, need: p.min_unique_buyers_60s, window_s: 60 });
            }
            if f.w60.buy_ratio() < p.min_buy_ratio_60s {
                r.push(RejectReason::WeakBuyRatio { ratio: f.w60.buy_ratio(), need: p.min_buy_ratio_60s });
            }
            let nf = sol(f.w60.net_flow_lamports());
            if nf < p.min_net_flow_sol_60s {
                r.push(RejectReason::WeakNetFlow { sol: nf, need_sol: p.min_net_flow_sol_60s });
            }
            let chg = f.w60.price_change.unwrap_or(0.0);
            if chg < p.min_price_change_60s {
                r.push(RejectReason::WeakPriceMomentum { change: chg, need: p.min_price_change_60s });
            }
            if chg > p.max_price_change_60s {
                r.push(RejectReason::Overextended { change: chg, max: p.max_price_change_60s });
            }
            if f.drawdown_from_peak_60s > p.max_drawdown_from_peak {
                r.push(RejectReason::DrawdownFromPeak { drawdown: f.drawdown_from_peak_60s, max: p.max_drawdown_from_peak });
            }
            if f.round_trip_wallet_fraction > p.max_round_trip_fraction {
                r.push(RejectReason::WashTrading { round_trip_fraction: f.round_trip_wallet_fraction, max: p.max_round_trip_fraction });
            }
            if f.concentration_complete && (f.top1_share > cfg.launch.max_top1_share * 1.5 || f.top10_share > cfg.launch.max_top10_share * 1.3) {
                r.push(RejectReason::Concentrated { top1: f.top1_share, top10: f.top10_share });
            }
        }
    }
    r
}

/// Estimated round-trip cost in bps of size: buy now, immediately sell the tokens back
/// (with our own impact modelled), plus two transactions' fixed costs under `scen`.
pub fn round_trip_cost_bps(
    st: &TokenState,
    size: u64,
    fees: &VenueFees,
    costs: &CostConfig,
    scen: &ScenarioConfig,
    max_age_ms: f64,
    now_us: i64,
) -> Result<(Quote, f64), String> {
    let buy = venue_quote(st, Side::Buy, size, 0, fees, max_age_ms, now_us).map_err(|e| e.to_string())?;
    let sell = venue_quote(st, Side::Sell, buy.out_amount, buy.out_amount, fees, max_age_ms, now_us).map_err(|e| e.to_string())?;
    let tx = 2 * (costs.base_fee_lamports + scen.priority_fee_lamports + scen.tip_lamports)
        + if costs.close_ata_on_exit { 0 } else { costs.ata_rent_lamports };
    let back = sell.out_amount as f64 - tx as f64;
    Ok((buy, (size as f64 - back) / size as f64 * 10_000.0))
}

fn allowed_sizes(e: &EntryCommon, score: f64) -> Vec<u64> {
    let mut v: Vec<(u64, f64)> = e
        .sizes_lamports
        .iter()
        .copied()
        .zip(e.size_score_thresholds.iter().copied().chain(std::iter::repeat(1.01)))
        .collect();
    v.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    v.into_iter().filter(|(_, t)| score >= *t).map(|(s, _)| s).collect()
}

/// Evaluate a candidate. `cost_scenario` is the scenario whose fixed costs are used for the
/// round-trip estimate (the base scenario by convention).
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    st: &TokenState,
    f: &Features,
    cfg: &StrategyConfig,
    costs: &CostConfig,
    cost_scenario: &ScenarioConfig,
    lag_ms: Option<f64>,
    now_us: i64,
) -> EntryDecision {
    let path = classify(st, f, cfg);
    let score = score(path, f, cfg);
    let mut reasons = market_filters(path, st, f, cfg, lag_ms);
    let e = &cfg.entry;
    if score < e.min_score {
        reasons.push(RejectReason::ScoreTooLow { score, min: e.min_score });
    }
    let fees = VenueFees { curve_fee_bps_default: costs.curve_fee_bps_default, pumpswap_fee_bps: costs.pumpswap_fee_bps };
    let mut checks = Vec::new();
    let mut chosen: Option<(u64, Quote)> = None;
    // Size checks are only meaningful when a route exists; run them even when other filters fail so
    // rejection records show the cost picture too.
    if st.venue().is_some() {
        let liq_cap = (f.liquidity_lamports as f64 * e.max_size_fraction_of_liquidity) as u64;
        let sizes = allowed_sizes(e, score);
        let sizes = if sizes.is_empty() { vec![*e.sizes_lamports.iter().min().unwrap_or(&20_000_000)] } else { sizes };
        let mut last_fail: Option<RejectReason> = None;
        for size in sizes {
            let mut c = SizeCheck { size_lamports: size, entry_impact_bps: f64::NAN, round_trip_cost_bps: f64::NAN, tokens_out: 0, ok: false, fail: None };
            if size > liq_cap {
                c.fail = Some("exceeds liquidity cap".into());
                last_fail = Some(RejectReason::SizeExceedsLiquidityCap { size_lamports: size, cap_lamports: liq_cap });
                checks.push(c);
                continue;
            }
            match round_trip_cost_bps(st, size, &fees, costs, cost_scenario, e.max_state_age_ms, now_us) {
                Err(err) => {
                    c.fail = Some(err.clone());
                    last_fail = Some(RejectReason::NoRoute { detail: err });
                }
                Ok((q, rt)) => {
                    c.entry_impact_bps = q.price_impact_bps;
                    c.round_trip_cost_bps = rt;
                    c.tokens_out = q.out_amount;
                    if q.price_impact_bps > e.max_entry_impact_bps {
                        c.fail = Some("entry impact".into());
                        last_fail = Some(RejectReason::ImpactTooHigh { bps: q.price_impact_bps, max_bps: e.max_entry_impact_bps, size_lamports: size });
                    } else if rt > e.max_round_trip_cost_bps {
                        c.fail = Some("round-trip cost".into());
                        last_fail = Some(RejectReason::RoundTripCostTooHigh { bps: rt, max_bps: e.max_round_trip_cost_bps, size_lamports: size });
                    } else {
                        c.ok = true;
                        if chosen.is_none() {
                            chosen = Some((size, q));
                        }
                    }
                }
            }
            let ok = c.ok;
            checks.push(c);
            if ok {
                break;
            }
        }
        if chosen.is_none() {
            if let Some(f) = last_fail {
                reasons.push(f);
            }
        }
    }
    let expected = expected_move_bps(score, f, cfg);
    let cost = checks
        .iter()
        .find(|c| c.ok)
        .or_else(|| checks.iter().filter(|c| c.round_trip_cost_bps.is_finite()).last())
        .map(|c| c.round_trip_cost_bps)
        .filter(|c| c.is_finite());
    let net_edge = cost.map(|c| expected - c);
    if let (Some(edge), Some(c)) = (net_edge, cost) {
        if chosen.is_some() && edge < e.min_net_edge_bps {
            reasons.push(RejectReason::InsufficientEdge { expected_move_bps: expected, round_trip_cost_bps: c, edge_bps: edge, min_bps: e.min_net_edge_bps });
        }
    }
    let enter = reasons.is_empty() && chosen.is_some();
    let (size, quote) = chosen.map(|(s, q)| (s, Some(q))).unwrap_or((0, None));
    EntryDecision {
        path,
        score,
        enter,
        size_lamports: if enter { size } else { 0 },
        quote: if enter { quote } else { None },
        reasons,
        size_checks: checks,
        expected_move_bps: expected,
        net_edge_bps: net_edge,
    }
}

pub fn lamports_to_sol_str(l: u64) -> String {
    format!("{:.4}", l as f64 / LAMPORTS_PER_SOL as f64)
}
