//! Configuration. Defaults live here; `config/strategy.toml` overrides; secrets come only from env.
//! The strategy version is a hash of the effective strategy+execution config, so every record can
//! be traced to the exact parameters that produced it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchPolicy {
    pub max_age_s: f64,
    pub min_observed_s: f64,
    pub min_unique_buyers_30s: u32,
    pub min_buy_ratio_30s: f64,
    pub min_net_flow_sol_30s: f64,
    pub max_buyers_in_create_slot: u32,
    pub max_top1_share: f64,
    pub max_top10_share: f64,
    pub max_creator_share: f64,
    pub max_round_trip_fraction: f64,
    pub min_liquidity_sol: f64,
    pub max_curve_progress: f64,
}
impl Default for LaunchPolicy {
    fn default() -> Self {
        Self {
            max_age_s: 180.0,
            min_observed_s: 4.0,
            min_unique_buyers_30s: 8,
            min_buy_ratio_30s: 0.62,
            min_net_flow_sol_30s: 1.5,
            max_buyers_in_create_slot: 5,
            max_top1_share: 0.06,
            max_top10_share: 0.30,
            max_creator_share: 0.08,
            max_round_trip_fraction: 0.35,
            min_liquidity_sol: 2.0,
            max_curve_progress: 0.85,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MomentumPolicy {
    pub min_observed_s: f64,
    pub min_unique_buyers_60s: u32,
    pub min_buy_ratio_60s: f64,
    pub min_price_change_60s: f64,
    pub max_price_change_60s: f64,
    pub min_net_flow_sol_60s: f64,
    pub min_liquidity_sol: f64,
    pub max_drawdown_from_peak: f64,
    pub max_round_trip_fraction: f64,
}
impl Default for MomentumPolicy {
    fn default() -> Self {
        Self {
            min_observed_s: 30.0,
            min_unique_buyers_60s: 10,
            min_buy_ratio_60s: 0.58,
            min_price_change_60s: 0.06,
            max_price_change_60s: 1.50,
            min_net_flow_sol_60s: 2.0,
            min_liquidity_sol: 8.0,
            max_drawdown_from_peak: 0.20,
            max_round_trip_fraction: 0.35,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EntryCommon {
    /// Allowed position sizes (lamports). Selected per opportunity/liquidity; never above the largest.
    pub sizes_lamports: Vec<u64>,
    /// score thresholds unlocking each size (same order as sizes, ascending sizes)
    pub size_score_thresholds: Vec<f64>,
    pub max_entry_impact_bps: f64,
    /// Max estimated round-trip cost (fees + impact both sides + tx costs) as bps of size.
    pub max_round_trip_cost_bps: f64,
    pub max_size_fraction_of_liquidity: f64,
    pub require_non_creator_sell: bool,
    pub max_state_age_ms: f64,
    pub max_delivery_lag_ms: f64,
    pub cooldown_after_exit_s: f64,
    pub min_eval_interval_ms: f64,
    pub min_score: f64,
    /// live mode: require an on-chain mint check (freeze authority / Token-2022 extensions) before entry
    pub require_mint_check: bool,
    /// no paper fill against token state older than this (no invented fills on stale data)
    pub max_fill_state_age_ms: f64,
    /// heuristic expected move at score 1.0 (bps). UNCALIBRATED prior — calibrate with forward evaluation.
    pub expected_move_scale_bps: f64,
    /// required (expected move − estimated round-trip cost), bps
    pub min_net_edge_bps: f64,
    /// 1 − (worst 60s liquidity drawdown) below this → unstable
    pub min_liquidity_stability: f64,
    pub block_entries_when_feed_unhealthy: bool,
}
impl Default for EntryCommon {
    fn default() -> Self {
        Self {
            sizes_lamports: vec![20_000_000, 50_000_000, 100_000_000],
            size_score_thresholds: vec![0.0, 0.62, 0.80],
            max_entry_impact_bps: 350.0,
            max_round_trip_cost_bps: 900.0,
            max_size_fraction_of_liquidity: 0.03,
            require_non_creator_sell: true,
            max_state_age_ms: 4_000.0,
            max_delivery_lag_ms: 6_000.0,
            cooldown_after_exit_s: 600.0,
            min_eval_interval_ms: 750.0,
            min_score: 0.45,
            require_mint_check: true,
            max_fill_state_age_ms: 15_000.0,
            expected_move_scale_bps: 2_500.0,
            min_net_edge_bps: 150.0,
            min_liquidity_stability: 0.75,
            block_entries_when_feed_unhealthy: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExitPolicy {
    /// trailing width = clamp(vol_mult * vol_60s * sqrt(10), min_trail, max_trail) of peak liquidation value
    pub trail_vol_mult: f64,
    pub min_trail: f64,
    pub max_trail: f64,
    /// adverse stop on liquidation value vs cost basis: clamp(stop_vol_mult * vol, min_stop, max_stop)
    pub stop_vol_mult: f64,
    pub min_stop: f64,
    pub max_stop: f64,
    pub deteriorate_buy_ratio_30s: f64,
    pub deteriorate_net_flow_sol_30s: f64,
    pub stagnation_s: f64,
    pub max_hold_s: f64,
    pub liquidity_drop_fraction: f64,
    pub creator_sell_fraction: f64,
    pub normal_slippage_bps: u32,
    pub emergency_slippage_bps: u32,
    pub slippage_step_bps: u32,
    pub max_slippage_bps: u32,
    pub retry_backoff_ms: Vec<u64>,
    pub max_sell_retries: u32,
    pub unresolved_after_s: f64,
    pub unresolved_retry_interval_s: f64,
    pub model_own_impact: bool,
    /// in profit after costs and 10s buy ratio below this with negative 10s net flow → lock gains
    pub fade_buy_ratio_10s: f64,
    /// exit-quote price impact above this → sellability concern, exit now
    pub max_exit_impact_bps: f64,
}
impl Default for ExitPolicy {
    fn default() -> Self {
        Self {
            trail_vol_mult: 3.0,
            min_trail: 0.10,
            max_trail: 0.35,
            stop_vol_mult: 3.5,
            min_stop: 0.12,
            max_stop: 0.30,
            deteriorate_buy_ratio_30s: 0.38,
            deteriorate_net_flow_sol_30s: -1.5,
            stagnation_s: 75.0,
            max_hold_s: 900.0,
            liquidity_drop_fraction: 0.30,
            creator_sell_fraction: 0.25,
            normal_slippage_bps: 800,
            emergency_slippage_bps: 2500,
            slippage_step_bps: 500,
            max_slippage_bps: 4000,
            retry_backoff_ms: vec![400, 800, 1600, 3200, 6400],
            max_sell_retries: 5,
            unresolved_after_s: 120.0,
            unresolved_retry_interval_s: 30.0,
            model_own_impact: true,
            fade_buy_ratio_10s: 0.40,
            max_exit_impact_bps: 1_500.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenarioConfig {
    pub name: String,
    /// decision → landed latency (ms) plus deterministic jitter in [0, jitter)
    pub latency_ms: u64,
    pub latency_jitter_ms: u64,
    /// extra adverse execution vs. reserves at fill time (unseen competing flow / MEV)
    pub adverse_slippage_bps: u32,
    /// probability a tx is dropped / not landed (no fee charged)
    pub drop_probability: f64,
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
    pub entry_slippage_tolerance_bps: u32,
    /// failed-but-landed txs (slippage exceeded) still pay base + priority fee
    pub charge_failed_tx_fees: bool,
}
impl Default for ScenarioConfig {
    fn default() -> Self {
        Self::base()
    }
}
impl ScenarioConfig {
    pub fn optimistic() -> Self {
        Self {
            name: "optimistic".into(),
            latency_ms: 400,
            latency_jitter_ms: 200,
            adverse_slippage_bps: 0,
            drop_probability: 0.02,
            priority_fee_lamports: 50_000,
            tip_lamports: 0,
            entry_slippage_tolerance_bps: 1500,
            charge_failed_tx_fees: true,
        }
    }
    pub fn base() -> Self {
        Self {
            name: "base".into(),
            latency_ms: 1_200,
            latency_jitter_ms: 600,
            adverse_slippage_bps: 75,
            drop_probability: 0.08,
            priority_fee_lamports: 200_000,
            tip_lamports: 100_000,
            entry_slippage_tolerance_bps: 1200,
            charge_failed_tx_fees: true,
        }
    }
    pub fn pessimistic() -> Self {
        Self {
            name: "pessimistic".into(),
            latency_ms: 3_000,
            latency_jitter_ms: 1_500,
            adverse_slippage_bps: 250,
            drop_probability: 0.20,
            priority_fee_lamports: 1_000_000,
            tip_lamports: 1_000_000,
            entry_slippage_tolerance_bps: 1000,
            charge_failed_tx_fees: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RiskConfig {
    pub starting_balance_lamports: u64,
    pub max_open_positions: usize,
    pub max_exposure_lamports: u64,
    pub max_session_loss_lamports: u64,
    pub min_reserve_lamports: u64,
}
impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            starting_balance_lamports: 1_000_000_000,
            max_open_positions: 3,
            max_exposure_lamports: 250_000_000,
            max_session_loss_lamports: 300_000_000,
            min_reserve_lamports: 50_000_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CostConfig {
    pub base_fee_lamports: u64,
    pub ata_rent_lamports: u64,
    pub close_ata_on_exit: bool,
    pub curve_fee_bps_default: u64,
    pub pumpswap_fee_bps: u64,
}
impl Default for CostConfig {
    fn default() -> Self {
        Self {
            base_fee_lamports: 5_000,
            ata_rent_lamports: 2_039_280,
            close_ata_on_exit: true,
            // Used only when the trade event does not report fee bps. Conservative (fees change over time).
            curve_fee_bps_default: 125,
            pumpswap_fee_bps: 30,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyConfig {
    pub label: String,
    pub launch: LaunchPolicy,
    pub momentum: MomentumPolicy,
    pub entry: EntryCommon,
    pub exit: ExitPolicy,
}
impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            label: "hybrid".into(),
            launch: LaunchPolicy::default(),
            momentum: MomentumPolicy::default(),
            entry: EntryCommon::default(),
            exit: ExitPolicy::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub strategy: StrategyConfig,
    pub scenarios: Vec<ScenarioConfig>,
    pub risk: RiskConfig,
    pub costs: CostConfig,
    pub max_tokens_tracked: usize,
    pub queue_capacity: usize,
    pub dedup_window: usize,
    pub tick_ms: u64,
    pub feed_buffer: usize,
    pub feed_health: crate::feed::FeedConfig,
    pub forward: ForwardConfig,
    /// observed-execution comparison window after each simulated fill
    pub exec_check_window_s: f64,
    /// "conservative" (min of venue simulation and a fresh Jupiter observation) or "venue_sim"
    pub fill_basis: String,
    pub evidence: EvidenceConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EvidenceConfig {
    /// a fill is "validated" only if the venue state it priced against was at most this old
    pub validated_max_state_age_ms: f64,
    /// decoded events needed (all hard checks passing) before a layout counts as mainnet-verified
    pub min_events_for_mainnet_verified: u64,
    /// real forward outcomes (calibration split) required before the expected-move model may be called calibrated
    pub min_outcomes_for_calibration: u64,
}
impl Default for EvidenceConfig {
    fn default() -> Self {
        Self { validated_max_state_age_ms: 2_000.0, min_events_for_mainnet_verified: 5, min_outcomes_for_calibration: 1_000 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ForwardConfig {
    pub enabled: bool,
    pub probe_lamports: u64,
    pub horizons_s: Vec<u32>,
}
impl Default for ForwardConfig {
    fn default() -> Self {
        Self { enabled: true, probe_lamports: 50_000_000, horizons_s: vec![30, 120, 300] }
    }
}
impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            strategy: StrategyConfig::default(),
            scenarios: vec![ScenarioConfig::optimistic(), ScenarioConfig::base(), ScenarioConfig::pessimistic()],
            risk: RiskConfig::default(),
            costs: CostConfig::default(),
            max_tokens_tracked: 20_000,
            queue_capacity: 8_192,
            dedup_window: 200_000,
            tick_ms: 250,
            feed_buffer: 300,
            feed_health: crate::feed::FeedConfig::default(),
            forward: ForwardConfig::default(),
            exec_check_window_s: 5.0,
            fill_basis: "conservative".into(),
            evidence: EvidenceConfig::default(),
        }
    }
}

impl EngineConfig {
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) if p.exists() => Ok(toml::from_str(&std::fs::read_to_string(p)?)?),
            _ => Ok(Self::default()),
        }
    }
    /// e.g. "hybrid-1a2b3c4d" — changes whenever any strategy/cost/scenario parameter changes.
    pub fn strategy_version(&self) -> String {
        let j = serde_json::to_string(&(&self.strategy, &self.scenarios, &self.costs, &self.risk)).unwrap_or_default();
        let h = Sha256::digest(format!("{}:{}", env!("CARGO_PKG_VERSION"), j).as_bytes());
        format!("{}-v{}-{}", self.strategy.label, env!("CARGO_PKG_VERSION"), hex8(&h))
    }
}

fn hex8(h: &[u8]) -> String {
    h[..4].iter().map(|b| format!("{b:02x}")).collect()
}
