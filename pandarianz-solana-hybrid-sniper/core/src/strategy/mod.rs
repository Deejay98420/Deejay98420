//! Unified decision engine: one evaluation path for launches and for momentum on already-trading
//! tokens; dynamic, state-driven exits (no fixed take-profit targets). Nothing here predicts or
//! promises profit — it only filters on observed flow and estimated executable costs.

pub mod entry;
pub mod exit;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryPath {
    Launch,
    Momentum,
}

impl EntryPath {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryPath::Launch => "launch",
            EntryPath::Momentum => "momentum",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum RejectReason {
    StaleState { age_ms: f64, max_ms: f64 },
    DeliveryLag { lag_ms: f64, max_ms: f64 },
    TooEarly { observed_s: f64, need_s: f64 },
    NoRoute { detail: String },
    CurveNearlyComplete { progress: f64, max: f64 },
    LowLiquidity { sol: f64, need_sol: f64 },
    FewUniqueBuyers { n: u32, need: u32, window_s: u32 },
    WeakBuyRatio { ratio: f64, need: f64 },
    WeakNetFlow { sol: f64, need_sol: f64 },
    WeakPriceMomentum { change: f64, need: f64 },
    Overextended { change: f64, max: f64 },
    DrawdownFromPeak { drawdown: f64, max: f64 },
    BundledLaunch { buyers_in_create_slot: u32, max: u32 },
    Concentrated { top1: f64, top10: f64 },
    CreatorHoldsTooMuch { share: f64, max: f64 },
    CreatorSelling { sold_fraction: f64 },
    WashTrading { round_trip_fraction: f64, max: f64 },
    Unsellable { detail: String },
    LiquidityRemoved { sol: f64 },
    ImpactTooHigh { bps: f64, max_bps: f64, size_lamports: u64 },
    RoundTripCostTooHigh { bps: f64, max_bps: f64, size_lamports: u64 },
    SizeExceedsLiquidityCap { size_lamports: u64, cap_lamports: u64 },
    ScoreTooLow { score: f64, min: f64 },
    AlreadyPositioned,
    Cooldown { remaining_s: f64 },
    RiskLimit { detail: String },
    FeedUnhealthy { state: String, reasons: Vec<String> },
    MintUnchecked { detail: String },
    MintRisk { risks: Vec<String> },
    NonSolQuote,
    InsufficientEdge { expected_move_bps: f64, round_trip_cost_bps: f64, edge_bps: f64, min_bps: f64 },
    LiquidityUnstable { stability: f64, min: f64 },
}

impl RejectReason {
    pub fn code(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(String::from))
            .unwrap_or_default()
    }
}
