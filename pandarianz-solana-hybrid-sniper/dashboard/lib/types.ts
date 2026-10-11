// Shapes mirror core/src/engine.rs `Snapshot` (serde JSON).
export type DataMode = "LIVE" | "FIXTURE" | "REPLAY";

export interface LatencySummary { count: number; p50_us: number | null; p95_us: number | null; p99_us: number | null; max_us: number | null }
export interface RouteStats { attempts: number; ok: number; no_route: number; stale: number; rate_limited: number; errors: number; disabled: number; last_error: string | null; last_ok_us: number | null }
export interface PerfStats {
  closed_trades: number; wins: number; losses: number; win_rate: number | null; realized_pnl_sol: number;
  gross_profit_sol: number; gross_loss_sol: number; expectancy_sol: number | null; profit_factor: number | null;
  max_drawdown_sol: number; avg_hold_s: number | null; total_fees_sol: number; exit_reasons: Record<string, number>;
}
export interface Reason { code: string; [k: string]: unknown }
export interface Position {
  position_id: string; book_id: string; scenario: string; data_mode: DataMode; strategy_version: string; mint: string;
  symbol: string | null; path: "launch" | "momentum"; status: "open" | "exiting" | "closed" | "unresolved";
  tokens: number; cost_basis: number; rent_locked: number; opened_us: number; peak_value: number; last_value: number | null;
  exit_reason: Reason | null; exit_urgency: string | null; exit_attempts: number; unresolved_reason: string | null;
  route_lost_since_us: number | null; closed_us: number | null; proceeds: number; exit_tx_costs: number;
  failed_exit_costs: number; entry_tx_costs: number; venue_fees_paid: number; realized_pnl: number | null; trigger_rolled_back: boolean;
}
export interface BookView {
  book_id: string; scenario: string; balance_sol: number; reserved_sol: number; rent_locked_sol: number; open_value_sol: number;
  equity_sol: number; start_sol: number; equity_pnl_sol: number; exposure_sol: number; failed_entry_costs_sol: number;
  open_positions: Position[]; pending_orders: number; perf: PerfStats; perf_by_path: Record<string, PerfStats>; accounting_ok: boolean; accounting_error: string | null;
  perf_by_path_size: Record<string, PerfStats>; perf_validated: PerfStats; perf_unverified: PerfStats; perf_validated_holdout: PerfStats; perf_validated_calibration: PerfStats;
  sensitivity_all: Sensitivity[]; sensitivity_validated: Sensitivity[]; order_outcomes: Record<string, number>; execution_failure_rate: number | null; unresolved_positions: number;
}
export interface FeedItem { ts_us: number; slot: number; signature: string; mint: string; symbol: string | null; kind: string; detail: string; data_mode: DataMode }
export interface Candidate {
  mint: string; symbol: string | null; path: string; phase: string; score: number; decision: string; reasons: Reason[];
  liquidity_sol: number; unique_buyers_30s: number; buy_ratio_30s: number; net_flow_sol_30s: number; age_s: number; evaluated_us: number; size_sol: number;
}
export interface ConnInfo { state: string | null; detail: string; since_us: number; reconnects: number }
export interface Health {
  started_us: number; connections: Record<string, ConnInfo>; events_processed: number; decoded_events: number; decode_warnings: number;
  failed_tx_skipped: number; commitment_upgrades_seen: number; engine_duplicates_dropped: number; rollbacks: number;
  last_event_us: number | null; last_slot: number; tokens_tracked: number; unresolved_pool_events: number; raydium_decode_only: number;
  ended: boolean; recovery_notes: string[]; last_decode_warning: string | null;
}
export interface Ingest { received: number; duplicates: number; forwarded: number; backpressure_waits: number; queue_high_water: number; queue_capacity: number }
export interface Gap { start_us: number; end_us: number | null; state: string; reason: string }
export interface FeedStatus {
  state: "HEALTHY" | "DEGRADED" | "DOWN" | "OFFLINE"; reasons: string[]; providers_up: number; providers_total: number;
  tip_slot: number | null; tip_age_ms: number | null; last_event_age_ms: number | null; slot_lag: number | null;
  coverage: number | null; open_gap: Gap | null; recent_gaps: Gap[];
}
export interface ProviderHealth { capabilities: Record<string, string>; state: string | null; detail: string; since_us: number; reconnects: number; first_deliveries: number; last_event_us: number | null; tip_slot: number | null; last_tip_us: number | null }
export interface EndpointStats { host: string; requests: number; ok: number; errors: number; rate_limited: number; last_error: string | null; last_latency_us: number | null }
export interface ForwardBucket { n: number; unsellable: number; mean_net_return: number | null; median_net_return: number | null; positive_fraction: number | null }
export interface ExecSummary { checks: number; compared: number; no_comparable_trade: number; median_adverse_bps: number | null; mean_adverse_bps: number | null; within_150bps: number | null }
export interface ExecCheck { order_id: string; scenario: string; mint: string; side: string; our_price: number; market_price: number | null; adverse_bps: number | null; delay_ms: number | null; outcome: string; fill_us: number }
export interface Verification { venue: string; item: string; status: string; evidence: string }
export interface Counters { [k: string]: number }
export interface Snapshot {
  feed_status: FeedStatus | null; providers: Record<string, ProviderHealth>; rpc_endpoints: EndpointStats[];
  tx_fetch: { requested: number; fetched: number; not_found: number; errors: number; mean_fetch_ms: number | null } | null;
  backfill: { signatures_listed: number; transactions_loaded: number; failed_or_missing: number; duration_ms: number; errors: string[] } | null;
  forward_summary: Record<string, ForwardBucket>; forward_pending: number; execution_quality: ExecSummary;
  recent_execution_checks: ExecCheck[]; decoder_verification: Verification[]; counters: Counters; status_banner: string;
  expected_move_model_status: string; coverage: Record<string, string>;
  generated_us: number; paper_only: boolean; data_mode: DataMode | null; data_mode_note: string; profile_key: string; run_id: string;
  strategy_version: string; health: Health; ingest: Ingest | null; store_backend: string; store_written: number; store_errors: number;
  feed: FeedItem[]; candidates: Candidate[]; rejection_counts: Record<string, number>; books: BookView[]; trades: Position[];
  latency: Record<string, LatencySummary>; routes: Record<string, RouteStats>; evaluations: number; entries_decided: number;
}

export interface Sensitivity { label: string; trades: number; net_sol: number; expectancy_sol: number | null; profit_factor: number | null; win_rate: number | null }
