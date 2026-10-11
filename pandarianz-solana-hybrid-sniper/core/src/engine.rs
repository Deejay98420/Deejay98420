//! Event-driven engine: a single owner of all mutable state (no locks on the hot path).
//! Inputs: stream messages (bounded queue), ticks, aggregator results. Outputs: store commands
//! and a periodically published dashboard snapshot.

use crate::config::EngineConfig;
use crate::decode::{decode_raw, Decoded};
use crate::metrics::{perf, LatencySeries, LatencySummary, PerfStats, RouteStats};
use crate::execcheck::{ExecChecker, ExecSummary, ExecutionCheck};
use crate::feed::{FeedMonitor, FeedState, FeedStatus};
use crate::forward::{ForwardBucket, ForwardOutcome, ForwardTracker, Probe};
use crate::live::{FetchSnapshot, FetchStats};
use crate::paper::{
    entry_reservation, fill_order_with, latency_us, Book, FillBasisMode, FillOutcome, Order, OrderKind, OrderStatus,
    Position, PositionStatus,
};
use crate::rpc::{EndpointStats, RpcClient};
use crate::quote::jupiter::JupiterClient;
use crate::quote::{venue_quote, Quote, QuoteError, QuoteSource, Side, VenueFees};
use crate::state::analytics::{compute, Features};
use crate::state::{Applied, Change, Input, Universe};
use crate::store::{BookRecord, EvaluationRecord, Recovered, RouteCheck, StoreCmd, StoreHandle};
use crate::strategy::entry::{evaluate, EntryDecision};
use crate::strategy::exit::{exit_signal, PositionView, Urgency};
use crate::strategy::{EntryPath, RejectReason};
use crate::stream::dedup::SignatureDedup;
use crate::stream::{Admit, ConnState, IngestSnapshot, RawEvent, SlotStatus, StreamMsg};
use crate::time::{wall_us, Clock, Stamps};
use crate::types::{sol, DataMode, Pubkey};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum Control {
    Jupiter(JupResult),
    /// PumpSwap pool account lookup result: (base_mint, quote_mint)
    PoolResolved { pool: Pubkey, result: Result<Option<(Pubkey, Pubkey)>, String> },
    MintChecked { mint: Pubkey, result: Result<crate::safety::MintSafety, String> },
    BackfillDone(crate::live::BackfillReport),
    Shutdown,
}

#[derive(Debug)]
pub struct JupResult {
    pub req_id: u64,
    pub result: Result<Quote, QuoteError>,
}

#[derive(Debug, Clone)]
enum JupPurpose {
    /// exit route fallback for a position with no venue route
    Exit { book_idx: usize, position_id: String, attempt: u32 },
    /// live executable quote observation attached to a pending order
    ObserveOrder { book_idx: usize, order_id: String },
}

#[derive(Debug, Clone)]
struct JupReq {
    mint: Pubkey,
    side: Side,
    amount: u64,
    purpose: JupPurpose,
    sent_us: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeedItem {
    pub ts_us: i64,
    pub slot: u64,
    pub signature: String,
    pub mint: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub detail: String,
    pub data_mode: DataMode,
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateView {
    pub mint: String,
    pub symbol: Option<String>,
    pub path: EntryPath,
    pub phase: String,
    pub score: f64,
    pub decision: String,
    pub reasons: Vec<RejectReason>,
    pub liquidity_sol: f64,
    pub unique_buyers_30s: u32,
    pub buy_ratio_30s: f64,
    pub net_flow_sol_30s: f64,
    pub age_s: f64,
    pub evaluated_us: i64,
    pub size_sol: f64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct ConnInfo {
    pub state: Option<ConnState>,
    pub detail: String,
    pub since_us: i64,
    pub reconnects: u64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Health {
    pub started_us: i64,
    pub connections: BTreeMap<String, ConnInfo>,
    pub events_processed: u64,
    pub decoded_events: u64,
    pub decode_warnings: u64,
    pub failed_tx_skipped: u64,
    pub commitment_upgrades_seen: u64,
    pub engine_duplicates_dropped: u64,
    pub rollbacks: u64,
    pub rolled_back_mints: u64,
    pub last_event_us: Option<i64>,
    pub last_slot: u64,
    pub tokens_tracked: usize,
    pub unresolved_pool_events: u64,
    pub raydium_decode_only: u64,
    pub ended: bool,
    pub recovery_notes: Vec<String>,
    pub last_decode_warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BookView {
    pub book_id: String,
    pub scenario: String,
    pub balance_sol: f64,
    pub reserved_sol: f64,
    pub rent_locked_sol: f64,
    pub open_value_sol: f64,
    pub equity_sol: f64,
    pub start_sol: f64,
    pub equity_pnl_sol: f64,
    pub exposure_sol: f64,
    pub failed_entry_costs_sol: f64,
    pub open_positions: Vec<Position>,
    pub pending_orders: usize,
    pub perf: PerfStats,
    /// strategy-specific performance (launch sniper vs momentum scanner)
    pub perf_by_path: BTreeMap<String, PerfStats>,
    /// "launch@0.05" etc.
    pub perf_by_path_size: BTreeMap<String, PerfStats>,
    /// only positions whose entry AND exit fills carried executable-price evidence
    pub perf_validated: PerfStats,
    pub perf_unverified: PerfStats,
    /// validated trades in the holdout split — the only figures to read as performance evidence
    pub perf_validated_holdout: PerfStats,
    pub perf_validated_calibration: PerfStats,
    pub sensitivity_all: Vec<crate::metrics::SensitivityRow>,
    pub sensitivity_validated: Vec<crate::metrics::SensitivityRow>,
    pub order_outcomes: BTreeMap<String, u64>,
    pub execution_failure_rate: Option<f64>,
    pub unresolved_positions: usize,
    pub accounting_ok: bool,
    pub accounting_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Snapshot {
    pub feed_status: Option<FeedStatus>,
    pub providers: BTreeMap<String, crate::feed::ProviderHealth>,
    pub rpc_endpoints: Vec<EndpointStats>,
    pub tx_fetch: Option<FetchSnapshot>,
    pub backfill: Option<crate::live::BackfillReport>,
    pub forward_summary: BTreeMap<String, ForwardBucket>,
    pub forward_pending: usize,
    pub execution_quality: ExecSummary,
    pub recent_execution_checks: Vec<ExecutionCheck>,
    pub decoder_verification: Vec<crate::decode::VerificationRow>,
    pub expected_move_model_status: String,
    /// per venue: how this run actually sees it (e.g. PumpSwap via getTransaction fetch)
    pub coverage: BTreeMap<String, String>,
    pub counters: EngineCounters,
    pub status_banner: String,
    pub generated_us: i64,
    pub paper_only: bool,
    pub data_mode: Option<DataMode>,
    pub data_mode_note: String,
    pub profile_key: String,
    pub run_id: String,
    pub strategy_version: String,
    pub health: Health,
    pub ingest: Option<IngestSnapshot>,
    pub store_backend: String,
    pub store_written: u64,
    pub store_errors: u64,
    pub feed: Vec<FeedItem>,
    pub candidates: Vec<CandidateView>,
    pub rejection_counts: BTreeMap<String, u64>,
    pub books: Vec<BookView>,
    pub trades: Vec<Position>,
    pub latency: BTreeMap<String, LatencySummary>,
    pub routes: BTreeMap<String, RouteStats>,
    pub evaluations: u64,
    pub entries_decided: u64,
}

pub struct EngineOptions {
    pub data_mode: DataMode,
    pub profile_key: String,
    pub run_id: String,
    pub jupiter: Option<Arc<JupiterClient>>,
    pub ingest_stats: Option<crate::stream::Ingestor>,
}

/// Optional live-mode services (read-only RPC, fetcher statistics).
#[derive(Default, Clone)]
pub struct LiveServices {
    pub rpc: Option<Arc<RpcClient>>,
    pub fetch_stats: Option<Arc<FetchStats>>,
    pub backfill: Option<crate::live::BackfillReport>,
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub version: String,
    pub mode: DataMode,
    pub profile_key: String,
    pub run_id: String,
    pub clock: Clock,
    pub universe: Universe,
    pub books: Vec<Book>,
    store: StoreHandle,
    fees: VenueFees,
    applied: SignatureDedup,
    last_eval_us: HashMap<Pubkey, i64>,
    last_reason_sig: HashMap<Pubkey, String>,
    candidates: HashMap<Pubkey, CandidateView>,
    rejection_counts: BTreeMap<String, u64>,
    feed: VecDeque<FeedItem>,
    pub health: Health,
    lat: BTreeMap<&'static str, LatencySeries>,
    pub routes: BTreeMap<String, RouteStats>,
    jup: Option<Arc<JupiterClient>>,
    ctl_tx: Option<mpsc::Sender<Control>>,
    jup_reqs: HashMap<u64, JupReq>,
    jup_counter: u64,
    pub dash: Arc<RwLock<Snapshot>>,
    ingestor: Option<crate::stream::Ingestor>,
    eval_counter: u64,
    pub evaluations: u64,
    pub entries_decided: u64,
    last_publish_wall: i64,
    boot_cmds: Vec<StoreCmd>,
    pub feed_mon: FeedMonitor,
    fwd: ForwardTracker,
    fwd_recent: VecDeque<ForwardOutcome>,
    exec: ExecChecker,
    exec_recent: VecDeque<ExecutionCheck>,
    pub live: LiveServices,
    pool_reqs: HashSet<Pubkey>,
    mint_reqs: HashSet<Pubkey>,
    pub stats: EngineCounters,
    /// real-transaction decoder verification (reports/verification/verification.json)
    pub decoder_verification: Option<crate::verify::VerificationSummary>,
    /// stop the run loop when a finite source ends (fixture/replay without --hold)
    pub exit_on_end: bool,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct EngineCounters {
    pub backfill_events: u64,
    pub pools_resolution_requested: u64,
    pub pools_resolved: u64,
    pub pools_non_sol: u64,
    pub mint_checks_requested: u64,
    pub mint_checks_ok: u64,
    pub mint_checks_risky: u64,
    pub mint_checks_failed: u64,
    pub entries_blocked_feed: u64,
    pub fills_stale_state: u64,
    pub fills_no_route: u64,
    pub fills_dropped: u64,
    pub fills_failed_slippage: u64,
    pub fills_ok: u64,
    pub order_observations_attached: u64,
    pub stale_quote_rejections: u64,
}

fn lat_series() -> LatencySeries {
    LatencySeries::new(4096)
}

impl Engine {
    pub fn new(cfg: EngineConfig, opts: EngineOptions, store: StoreHandle, rec: Option<Recovered>) -> Self {
        let version = cfg.strategy_version();
        let clock = if opts.data_mode == DataMode::Live { Clock::Wall } else { Clock::Event { now_us: 0 } };
        let fees = VenueFees { curve_fee_bps_default: cfg.costs.curve_fee_bps_default, pumpswap_fee_bps: cfg.costs.pumpswap_fee_bps };
        let mut lat = BTreeMap::new();
        for k in [
            "provider_delivery_lag",
            "queue_wait",
            "decode",
            "evaluate",
            "event_to_decision",
            "venue_quote",
            "jupiter_quote",
            "fill_latency_sim",
            "tx_fetch",
            "slot_lag_slots",
            "end_to_end_recv_to_order",
            "quote_age_at_decision",
            "quote_age_at_fill",
        ] {
            lat.insert(k, lat_series());
        }
        let mut e = Self {
            universe: Universe::new(cfg.max_tokens_tracked),
            applied: SignatureDedup::new(cfg.dedup_window),
            books: Vec::new(),
            version,
            mode: opts.data_mode,
            profile_key: opts.profile_key,
            run_id: opts.run_id,
            clock,
            store,
            fees,
            last_eval_us: HashMap::new(),
            last_reason_sig: HashMap::new(),
            candidates: HashMap::new(),
            rejection_counts: BTreeMap::new(),
            feed: VecDeque::new(),
            health: Health { started_us: wall_us(), ..Default::default() },
            lat,
            routes: BTreeMap::new(),
            jup: opts.jupiter,
            ctl_tx: None,
            jup_reqs: HashMap::new(),
            jup_counter: 0,
            dash: Arc::new(RwLock::new(Snapshot::default())),
            ingestor: opts.ingest_stats,
            eval_counter: 0,
            evaluations: 0,
            entries_decided: 0,
            last_publish_wall: 0,
            boot_cmds: Vec::new(),
            feed_mon: FeedMonitor::new(cfg.feed_health.clone(), opts.data_mode == DataMode::Live),
            fwd: ForwardTracker::default(),
            fwd_recent: VecDeque::new(),
            exec: ExecChecker::new(cfg.exec_check_window_s),
            exec_recent: VecDeque::new(),
            live: LiveServices::default(),
            pool_reqs: HashSet::new(),
            mint_reqs: HashSet::new(),
            stats: EngineCounters::default(),
            decoder_verification: None,
            exit_on_end: false,
            cfg,
        };
        e.init_books(rec.unwrap_or_default());
        e
    }

    pub fn set_control_sender(&mut self, tx: mpsc::Sender<Control>) {
        self.ctl_tx = Some(tx);
    }

    fn book_id(&self, scenario: &str) -> String {
        format!("{}:{}", self.profile_key, scenario)
    }

    fn init_books(&mut self, rec: Recovered) {
        let mut pending_cmds = Vec::new();
        for sc in self.cfg.scenarios.clone() {
            let id = self.book_id(&sc.name);
            let mut book = match rec.books.get(&id) {
                Some((start, sum, seq)) => {
                    let mut b = Book::new(id.clone(), sc.clone(), *start);
                    b.balance = *start as i64 + sum;
                    b.ledger_sum = *sum;
                    b.ledger_seq = *seq;
                    self.health.recovery_notes.push(format!("{id}: recovered balance {:.6} SOL from {} ledger entries", sol(b.balance), seq));
                    b
                }
                None => {
                    pending_cmds.push(StoreCmd::Book(BookRecord {
                        book_id: id.clone(),
                        scenario: sc.name.clone(),
                        start_balance: self.cfg.risk.starting_balance_lamports,
                        created_us: wall_us(),
                        scenario_config: serde_json::to_value(&sc).unwrap_or_default(),
                    }));
                    Book::new(id.clone(), sc.clone(), self.cfg.risk.starting_balance_lamports)
                }
            };
            for p in rec.positions.iter().filter(|p| p.book_id == id) {
                if p.is_live() {
                    let mut p = p.clone();
                    if p.status == PositionStatus::Exiting {
                        p.next_exit_attempt_us = 0; // pending exit order (if any) was lost; retry promptly
                    }
                    book.positions.insert(p.position_id.clone(), p);
                } else {
                    book.closed.push(p.clone());
                }
            }
            for k in rec.idempotency_keys.iter().filter(|k| k.starts_with(&id)) {
                book.entered_keys.insert(k.clone());
            }
            for p in &book.closed {
                if let Some(c) = p.closed_us {
                    let e = book.last_exit_us.entry(p.mint).or_insert(c);
                    *e = (*e).max(c);
                }
            }
            book.failed_entry_costs = rec.failed_entry_costs.get(&id).copied().unwrap_or(0);
            let live = book.positions.len();
            if live > 0 {
                self.health.recovery_notes.push(format!("{id}: restored {live} open/unresolved position(s)"));
            }
            self.books.push(book);
        }
        for o in rec.dangling_orders {
            let mut o = o;
            o.status = OrderStatus::AbandonedRestart;
            o.error = Some("process stopped before simulated fill; no fill recorded".into());
            o.completed_us = Some(wall_us());
            self.health.recovery_notes.push(format!("order {} abandoned on restart (never filled)", o.order_id));
            pending_cmds.push(StoreCmd::OrderCompleted(Box::new(o)));
        }
        for (s, c) in rec.signatures {
            self.applied.admit(&s, c);
        }
        // Recorded so the writer persists them before anything else.
        self.boot_cmds = pending_cmds;
    }

    pub async fn boot(&mut self, config_json: serde_json::Value) {
        self.store
            .send(StoreCmd::Run {
                run_id: self.run_id.clone(),
                data_mode: self.mode.as_str().into(),
                started_us: wall_us(),
                strategy_version: self.version.clone(),
                config: config_json,
            })
            .await;
        for c in std::mem::take(&mut self.boot_cmds) {
            self.store.send(c).await;
        }
    }

    /// Rebuild token state from journaled events without making decisions (restart warm-up).
    pub fn warmup(&mut self, events: &[RawEvent]) {
        let mut n = 0;
        for raw in events {
            self.clock.observe(raw.recv_us);
            if raw.is_slot_marker() {
                self.universe.rollback_slot(raw.slot);
                continue;
            }
            let out = decode_raw(raw);
            if out.tx_failed {
                continue;
            }
            for d in out.events {
                self.universe.apply(Input { slot: raw.slot, signature: raw.signature.clone(), index: d.index, ts_us: raw.recv_us, event: d.event });
                n += 1;
            }
            self.applied.admit(&raw.signature, raw.commitment);
        }
        if n > 0 {
            self.health.recovery_notes.push(format!("warm-up: rebuilt token state from {n} journaled events"));
        }
    }

    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    fn lat(&mut self, k: &'static str, us: i64) {
        if let Some(s) = self.lat.get_mut(k) {
            s.record(us);
        }
    }

    fn route(&mut self, key: &str) -> &mut RouteStats {
        self.routes.entry(key.to_string()).or_default()
    }

    fn note_route(&mut self, source: &str, r: &Result<Quote, QuoteError>, now: i64) {
        let rs = self.route(source);
        rs.attempts += 1;
        match r {
            Ok(_) => {
                rs.ok += 1;
                rs.last_ok_us = Some(now);
            }
            Err(QuoteError::NoRoute(e)) => {
                rs.no_route += 1;
                rs.last_error = Some(e.clone());
            }
            Err(QuoteError::StaleState(_)) => rs.stale += 1,
            Err(QuoteError::RateLimited(e)) => {
                rs.rate_limited += 1;
                rs.last_error = Some(e.clone());
            }
            Err(QuoteError::Disabled(e)) => {
                rs.disabled += 1;
                rs.last_error = Some(e.clone());
            }
            Err(e) => {
                rs.errors += 1;
                rs.last_error = Some(e.to_string());
            }
        }
    }

    // ------------------------------------------------------------------ stream handling

    pub async fn handle_stream(&mut self, msg: StreamMsg) {
        let dequeued = wall_us();
        match msg {
            StreamMsg::Event(raw) => self.on_event(raw, dequeued).await,
            StreamMsg::Slot { slot, status, recv_us } => {
                self.clock.observe(recv_us);
                match status {
                    SlotStatus::Dead => {
                        let marker = RawEvent::dead_slot_marker(slot, recv_us, self.mode);
                        self.store.send(StoreCmd::RawEvent { run_id: self.run_id.clone(), ev: Box::new(marker) }).await;
                        self.rollback(slot).await
                    }
                    SlotStatus::Finalized => self.universe.prune_slots(slot.saturating_sub(64)),
                    SlotStatus::Confirmed => {}
                }
            }
            StreamMsg::Capability { provider, method, ok, detail, .. } => {
                self.feed_mon.on_capability(&provider, &method, ok, &detail);
            }
            StreamMsg::Tip { provider, slot, recv_us } => {
                self.feed_mon.on_tip(&provider, slot, recv_us);
                self.feed_tick(self.now()).await;
            }
            StreamMsg::Connection { provider, state, detail, recv_us } => {
                self.feed_mon.on_connection(&provider, state, &detail, recv_us);
                let c = self.health.connections.entry(provider).or_default();
                if state == ConnState::Reconnecting {
                    c.reconnects += 1;
                }
                c.state = Some(state);
                c.detail = detail;
                c.since_us = recv_us;
                self.feed_tick(self.now()).await;
            }
            StreamMsg::End { recv_us, .. } => {
                self.clock.observe(recv_us);
                // settle orders already in flight at end of a finite source
                let horizon = self.now() + 30_000_000;
                self.process_due(horizon, true).await;
                self.health.ended = true;
                self.store.flush().await;
                self.publish(true);
            }
        }
    }

    async fn on_event(&mut self, raw: RawEvent, dequeued_us: i64) {
        if self.mode == DataMode::Live && raw.data_mode != DataMode::Live {
            tracing::error!("non-live event in live engine rejected");
            return;
        }
        if self.mode != DataMode::Live && raw.data_mode == DataMode::Live {
            // never let anything be relabelled; a fixture engine must not ingest live-labelled data
            tracing::error!("live-labelled event in non-live engine rejected");
            return;
        }
        if let (Clock::Wall, false) = (&self.clock, raw.backfill) {
            self.lat("queue_wait", dequeued_us - raw.recv_us);
        }
        self.clock.observe(raw.recv_us);
        let now = self.now();
        match self.applied.admit(&raw.signature, raw.commitment) {
            Admit::Duplicate => {
                self.health.engine_duplicates_dropped += 1;
                return;
            }
            Admit::Upgrade => {
                self.health.commitment_upgrades_seen += 1;
                return;
            }
            Admit::New => {}
        }
        if raw.backfill {
            // Historical transaction: rebuild state only. Never triggers evaluation or trades.
            self.stats.backfill_events += 1;
            self.store.send(StoreCmd::RawEvent { run_id: self.run_id.clone(), ev: Box::new(raw.clone()) }).await;
            let out = decode_raw(&raw);
            if !out.tx_failed {
                for Decoded { index, event, .. } in out.events {
                    self.health.decoded_events += 1;
                    self.universe.apply(Input { slot: raw.slot, signature: raw.signature.clone(), index, ts_us: raw.recv_us, event });
                }
            }
            self.request_lookups().await;
            return;
        }
        if self.mode == DataMode::Live {
            self.feed_mon.on_event(&raw.provider, raw.slot, raw.recv_us);
            if let Some(tip) = self.feed_mon.tip_slot() {
                self.lat("slot_lag_slots", tip as i64 - raw.slot as i64);
            }
            if let Some(f) = raw.fetch_us {
                self.lat("tx_fetch", f);
            }
        }
        // Settle due orders against the state BEFORE this event (state as of their fill time).
        self.process_due(now, false).await;
        self.health.events_processed += 1;
        self.health.last_event_us = Some(raw.recv_us);
        self.health.last_slot = self.health.last_slot.max(raw.slot);
        self.store.send(StoreCmd::RawEvent { run_id: self.run_id.clone(), ev: Box::new(raw.clone()) }).await;

        let t0 = wall_us();
        let out = decode_raw(&raw);
        let decode_us = wall_us() - t0;
        self.lat("decode", decode_us);
        if out.tx_failed {
            self.health.failed_tx_skipped += 1;
            return;
        }
        if !out.warnings.is_empty() {
            self.health.decode_warnings += out.warnings.len() as u64;
            self.health.last_decode_warning = out.warnings.last().cloned();
        }
        let mut stamps = Stamps { block_time_us: raw.block_time.map(|b| b * 1_000_000), event_ts_us: None, recv_us: raw.recv_us, decoded_us: Some(raw.recv_us + decode_us) };
        let mut touched: Vec<(Pubkey, Applied)> = Vec::new();
        for Decoded { index, event, .. } in out.events {
            self.health.decoded_events += 1;
            if stamps.event_ts_us.is_none() {
                stamps.event_ts_us = event.timestamp().map(|t| t * 1_000_000);
            }
            if let Some(a) = self.universe.apply(Input { slot: raw.slot, signature: raw.signature.clone(), index, ts_us: raw.recv_us, event }) {
                touched.push((a.mint, a));
            }
        }
        if let (Clock::Wall, Some(lag)) = (&self.clock, stamps.delivery_lag_us()) {
            self.lat("provider_delivery_lag", lag);
        }
        self.health.tokens_tracked = self.universe.tokens.len();
        self.health.unresolved_pool_events = self.universe.unresolved_pool_events;
        self.health.raydium_decode_only = self.universe.raydium_decode_only;

        self.request_lookups().await;
        self.feed_tick(now).await;
        self.background_tick(now).await;

        let mut seen = HashSet::new();
        for (mint, a) in touched {
            self.push_feed(&raw, &a);
            if seen.insert(mint) {
                self.on_token_update(mint, &raw, &stamps, now).await;
            }
        }
        // Time-based exits for every live position (cheap: few positions).
        self.check_all_positions(now).await;
        self.publish(false);
    }

    fn push_feed(&mut self, raw: &RawEvent, a: &Applied) {
        let st = self.universe.tokens.get(&a.mint);
        let detail = match &a.change {
            Change::Trade { is_buy } => {
                let last = st.and_then(|s| s.trades.back());
                format!(
                    "{} {:.3} SOL by {}",
                    if *is_buy { "buy" } else { "sell" },
                    last.map(|t| sol(t.sol as i64)).unwrap_or(0.0),
                    a.user.map(|u| u.short()).unwrap_or_default()
                )
            }
            Change::Launch => format!("new launch by {}", a.user.map(|u| u.short()).unwrap_or_default()),
            Change::CurveComplete => "bonding curve complete".into(),
            Change::Migrated => "migrated to PumpSwap".into(),
            Change::PoolCreated => "PumpSwap pool created".into(),
            Change::LiquidityRemoved => "liquidity removed".into(),
            Change::LiquidityAdded => "liquidity added".into(),
        };
        let kind = match &a.change {
            Change::Trade { .. } => "trade",
            Change::Launch => "launch",
            Change::CurveComplete => "curve_complete",
            Change::Migrated => "migration",
            Change::PoolCreated => "pool_created",
            Change::LiquidityRemoved => "liquidity_removed",
            Change::LiquidityAdded => "liquidity_added",
        };
        self.feed.push_back(FeedItem {
            ts_us: raw.recv_us,
            slot: raw.slot,
            signature: raw.signature.clone(),
            mint: a.mint.to_string(),
            symbol: st.and_then(|s| s.symbol.clone()),
            kind: kind.into(),
            detail,
            data_mode: raw.data_mode,
        });
        while self.feed.len() > self.cfg.feed_buffer {
            self.feed.pop_front();
        }
    }

    async fn on_token_update(&mut self, mint: Pubkey, raw: &RawEvent, stamps: &Stamps, now: i64) {
        // positions first (exits have priority over entries)
        self.check_positions_for(mint, now).await;
        // throttled entry evaluation
        let min_gap = (self.cfg.strategy.entry.min_eval_interval_ms * 1000.0) as i64;
        if let Some(last) = self.last_eval_us.get(&mint) {
            if now - last < min_gap {
                return;
            }
        }
        if self.books.iter().all(|b| b.has_mint(&mint)) {
            return;
        }
        self.last_eval_us.insert(mint, now);
        let t0 = wall_us();
        let lag_ms = if self.mode == DataMode::Live { stamps.delivery_lag_us().map(|l| l as f64 / 1e3) } else { None };
        let base = self.cfg.scenarios.iter().find(|s| s.name == "base").cloned().unwrap_or_default();
        let (f, mut d, symbol, mint_safety) = {
            let Some(st) = self.universe.tokens.get(&mint) else { return };
            let f = compute(st, now);
            let d = evaluate(st, &f, &self.cfg.strategy, &self.cfg.costs, &base, lag_ms, now);
            (f, d, st.symbol.clone(), st.mint_safety.clone())
        };
        if self.mode == DataMode::Live {
            let e = &self.cfg.strategy.entry;
            if e.block_entries_when_feed_unhealthy && self.feed_mon.state() != FeedState::Healthy {
                let st = self.feed_mon.status(now);
                d.reasons.push(RejectReason::FeedUnhealthy { state: format!("{:?}", st.state), reasons: st.reasons });
                self.stats.entries_blocked_feed += 1;
            }
            if e.require_mint_check {
                match &mint_safety {
                    None => d.reasons.push(RejectReason::MintUnchecked {
                        detail: if self.live.rpc.is_some() { "mint check pending".into() } else { "no RPC configured for mint checks".into() },
                    }),
                    Some(Err(err)) => d.reasons.push(RejectReason::MintUnchecked { detail: err.clone() }),
                    Some(Ok(ms)) if !ms.is_safe() => d.reasons.push(RejectReason::MintRisk { risks: ms.risks.clone() }),
                    _ => {}
                }
            }
            if !d.reasons.is_empty() {
                d.enter = false;
                d.quote = None;
                d.size_lamports = 0;
            }
        }
        let eval_us = wall_us() - t0;
        self.lat("evaluate", eval_us);
        if self.mode == DataMode::Live {
            self.lat("event_to_decision", wall_us() - raw.recv_us);
        }
        self.evaluations += 1;
        self.record_evaluation(mint, symbol, &f, &d, raw, stamps, now).await;
        if d.enter {
            self.entries_decided += 1;
            if let Some(q) = &d.quote {
                self.lat("quote_age_at_decision", (q.state_age_ms * 1000.0) as i64);
            }
            if let Some(q) = &d.quote {
                self.note_route("venue_sim", &Ok(q.clone()), now);
            }
            self.submit_entries(mint, &d, &f, raw, now).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_evaluation(&mut self, mint: Pubkey, symbol: Option<String>, f: &Features, d: &EntryDecision, raw: &RawEvent, stamps: &Stamps, now: i64) {
        let mut codes: Vec<String> = d.reasons.iter().map(|r| r.code()).collect();
        codes.sort();
        let sig = if d.enter { "enter".to_string() } else { codes.join(",") };
        for c in &codes {
            *self.rejection_counts.entry(c.clone()).or_default() += 1;
        }
        self.candidates.insert(
            mint,
            CandidateView {
                mint: mint.to_string(),
                symbol: symbol.clone(),
                path: d.path,
                phase: format!("{:?}", f.phase),
                score: d.score,
                decision: if d.enter { "enter".into() } else { "reject".into() },
                reasons: d.reasons.clone(),
                liquidity_sol: sol(f.liquidity_lamports as i64),
                unique_buyers_30s: f.w30.unique_buyers,
                buy_ratio_30s: f.w30.buy_ratio(),
                net_flow_sol_30s: sol(f.w30.net_flow_lamports()),
                age_s: f.age_s,
                evaluated_us: now,
                size_sol: sol(d.size_lamports as i64),
            },
        );
        if self.candidates.len() > 2_000 {
            if let Some(old) = self.candidates.values().min_by_key(|c| c.evaluated_us).map(|c| c.mint.clone()) {
                if let Ok(pk) = Pubkey::from_b58(&old) {
                    self.candidates.remove(&pk);
                }
            }
        }
        // Persist every entry decision and every change in the rejection-reason set (not every repeat).
        let changed = self.last_reason_sig.get(&mint) != Some(&sig);
        if !(d.enter || changed) {
            return;
        }
        self.last_reason_sig.insert(mint, sig);
        self.eval_counter += 1;
        let rec = EvaluationRecord {
            eval_id: format!("ev-{}-{}-{}", self.run_id, now, self.eval_counter),
            run_id: self.run_id.clone(),
            data_mode: self.mode.as_str().into(),
            ts_us: now,
            mint: mint.to_string(),
            symbol,
            path: d.path.as_str().into(),
            decision: if d.enter { "enter".into() } else { "reject".into() },
            strategy_version: self.version.clone(),
            score: d.score,
            size_lamports: d.size_lamports,
            reasons: serde_json::to_value(&d.reasons).unwrap_or_default(),
            features: serde_json::to_value(f).unwrap_or_default(),
            size_checks: serde_json::to_value(&d.size_checks).unwrap_or_default(),
            quote: d.quote.clone(),
            trigger_signature: raw.signature.clone(),
            trigger_slot: raw.slot,
            stamps: serde_json::to_value(stamps).unwrap_or_default(),
            expected_move_bps: Some(d.expected_move_bps),
            net_edge_bps: d.net_edge_bps,
        };
        // forward evaluation probe (accepted and rejected alike); outcomes are written later
        if self.cfg.forward.enabled {
            if let Some(st) = self.universe.tokens.get(&mint) {
                if let Ok(q) = venue_quote(st, Side::Buy, self.cfg.forward.probe_lamports, 0, &self.fees, f64::MAX, now) {
                    self.fwd.add(Probe {
                        eval_id: rec.eval_id.clone(),
                        mint,
                        path: rec.path.clone(),
                        decision: rec.decision.clone(),
                        reason_codes: codes.clone(),
                        t0_us: now,
                        probe_lamports: self.cfg.forward.probe_lamports,
                        entry_in: q.in_amount,
                        tokens: q.out_amount,
                        horizons_s: self.cfg.forward.horizons_s.clone(),
                        expected_move_bps: rec.expected_move_bps,
                        data_mode: self.mode.as_str().into(),
                    });
                }
            }
        }
        self.store.send(StoreCmd::Evaluation(Box::new(rec))).await;
    }

    async fn submit_entries(&mut self, mint: Pubkey, d: &EntryDecision, f: &Features, raw: &RawEvent, now: i64) {
        let Some(q) = d.quote.clone() else { return };
        let costs = self.cfg.costs.clone();
        let risk = self.cfg.risk.clone();
        let cooldown = (self.cfg.strategy.entry.cooldown_after_exit_s * 1e6) as i64;
        let mut cmds = Vec::new();
        let mut observe: Vec<(usize, String)> = Vec::new();
        for (bi, book) in self.books.iter_mut().enumerate() {
            let key = format!("{}:{}:entry:{}", book.id, mint, raw.signature);
            let block = if book.entered_keys.contains(&key) {
                Some("duplicate trigger (idempotency key already used)".to_string())
            } else if book.has_mint(&mint) {
                Some("already positioned".into())
            } else if let Some(t) = book.last_exit_us.get(&mint).filter(|t| now - **t < cooldown) {
                Some(format!("cooldown {:.0}s remaining", (cooldown - (now - t)) as f64 / 1e6))
            } else if book.live_count() >= risk.max_open_positions {
                Some(format!("max open positions {}", risk.max_open_positions))
            } else if book.exposure() + d.size_lamports > risk.max_exposure_lamports {
                Some("max exposure".into())
            } else if (book.balance - book.reserved as i64 - entry_reservation(d.size_lamports, &book.scenario, &costs) as i64) < risk.min_reserve_lamports as i64 {
                Some("insufficient balance after reserve".into())
            } else if book.start_balance - book.balance - book.rent_locked() as i64 > risk.max_session_loss_lamports as i64 + book.exposure() as i64 {
                Some("session loss limit reached — entries halted".into())
            } else {
                None
            };
            if let Some(why) = block {
                cmds.push(StoreCmd::PositionEvent {
                    position_id: format!("none:{}", book.id),
                    ts_us: now,
                    kind: "entry_blocked".into(),
                    doc: serde_json::json!({"mint": mint.to_string(), "reason": why, "trigger": raw.signature}),
                });
                continue;
            }
            let order_id = book.next_id("ord", now);
            let lat = latency_us(&book.scenario, &crate::paper::sim_seed(&book.id, &book.scenario.name, &order_id));
            let tol = book.scenario.entry_slippage_tolerance_bps;
            let min_out = (q.out_amount as u128 * (10_000 - tol.min(10_000)) as u128 / 10_000) as u64;
            let order = Order {
                order_id: order_id.clone(),
                idempotency_key: key.clone(),
                book_id: book.id.clone(),
                scenario: book.scenario.name.clone(),
                data_mode: self.mode,
                strategy_version: self.version.clone(),
                kind: OrderKind::Entry,
                side: Side::Buy,
                mint,
                path: d.path,
                position_id: None,
                amount: d.size_lamports,
                slippage_bps: tol,
                min_out,
                decision_quote: Some(q.clone()),
                decision_us: now,
                fill_at_us: now + lat,
                trigger_signature: raw.signature.clone(),
                trigger_slot: raw.slot,
                attempt: 1,
                reason: format!("{} score {:.2}", d.path.as_str(), d.score),
                status: OrderStatus::Pending,
                fill: None,
                fill_quote: None,
                costs: Default::default(),
                error: None,
                completed_us: None,
                observed_quote: None,
                fill_basis: None,
                aggregator_divergence_bps: None,
                evidence: None,
            };
            book.entered_keys.insert(key);
            observe.push((bi, order.order_id.clone()));
            book.reserved += entry_reservation(d.size_lamports, &book.scenario, &costs);
            cmds.push(StoreCmd::OrderSubmitted(Box::new(order.clone())));
            book.pending.push(order);
        }
        let _ = f;
        for c in cmds {
            self.store.send(c).await;
        }
        if self.mode == DataMode::Live && !observe.is_empty() {
            self.lat("end_to_end_recv_to_order", wall_us() - raw.recv_us);
        }
        for (bi, oid) in observe {
            self.spawn_jupiter(JupReq { mint, side: Side::Buy, amount: d.size_lamports, purpose: JupPurpose::ObserveOrder { book_idx: bi, order_id: oid }, sent_us: now }).await;
        }
    }

    // ------------------------------------------------------------------ fills

    /// Fill every pending order whose fill time has come. `settle` = end-of-source: fill at fill time.
    pub async fn process_due(&mut self, now: i64, settle: bool) {
        let exit_cfg = self.cfg.strategy.exit.clone();
        let costs = self.cfg.costs.clone();
        let max_fill_age = self.cfg.strategy.entry.max_fill_state_age_ms;
        let basis = FillBasisMode::parse(&self.cfg.fill_basis);
        let mut cmds: Vec<StoreCmd> = Vec::new();
        let mut exec_new: Vec<ExecutionCheck> = Vec::new();
        let mut fill_ages: Vec<i64> = Vec::new();
        let mut fill_lat = Vec::new();
        for bi in 0..self.books.len() {
            let mut due: Vec<Order> = Vec::new();
            {
                let book = &mut self.books[bi];
                let mut keep = Vec::new();
                for o in book.pending.drain(..) {
                    if o.fill_at_us <= now { due.push(o) } else { keep.push(o) }
                }
                book.pending = keep;
            }
            due.sort_by_key(|o| o.fill_at_us);
            for mut o in due {
                let outer_len = cmds.len();
                let fill_ts = if self.clock.is_event_time() || settle { o.fill_at_us } else { now };
                let st = self.universe.tokens.get(&o.mint);
                let book = &mut self.books[bi];
                let (outcome, ledger) = fill_order_with(&mut o, st, book, &self.fees, &costs, &exit_cfg, max_fill_age, basis, fill_ts);
                *book.order_outcomes.entry(format!("{}:{}", serde_json::to_value(o.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(), serde_json::to_value(o.status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default())).or_default() += 1;
                if let Some(q) = &o.fill_quote {
                    fill_ages.push((q.state_age_ms * 1000.0) as i64);
                }
                match o.status {
                    OrderStatus::Filled => {
                        self.stats.fills_ok += 1;
                        let venue_ok = o.fill_quote.as_ref().and_then(|q| q.venue).is_some_and(|v| {
                            self.decoder_verification.as_ref().and_then(|d| d.venues_mainnet_verified.get(v.as_str()).copied()).unwrap_or(false)
                        });
                        o.evidence = Some(crate::paper::assess_fill(&o, st, self.mode, venue_ok, self.cfg.evidence.validated_max_state_age_ms));
                        if let Some(c) = st.and_then(|s| self.exec.check(&o, fill_ts, &s.trades)) {
                            exec_new.push(c);
                        }
                    }
                    OrderStatus::NoRoute => {
                        if o.error.as_deref().is_some_and(|e| e.contains("stale")) {
                            self.stats.fills_stale_state += 1;
                        } else {
                            self.stats.fills_no_route += 1;
                        }
                    }
                    OrderStatus::Dropped => self.stats.fills_dropped += 1,
                    OrderStatus::FailedSlippage => self.stats.fills_failed_slippage += 1,
                    _ => {}
                }
                for l in ledger {
                    cmds.push(StoreCmd::Ledger(l));
                }
                fill_lat.push((fill_ts - o.decision_us, o.fill_quote.clone(), o.status));
                match outcome {
                    FillOutcome::EntryFilled { mut position } => {
                        position.entry_evidence = o.evidence.clone();
                        cmds.push(StoreCmd::Position(Box::new(position.clone())));
                        cmds.push(StoreCmd::PositionEvent {
                            position_id: position.position_id.clone(),
                            ts_us: fill_ts,
                            kind: "opened".into(),
                            doc: serde_json::json!({"order_id": o.order_id, "tokens": position.tokens, "cost_basis": position.cost_basis}),
                        });
                        book.positions.insert(position.position_id.clone(), position);
                    }
                    FillOutcome::ExitFilled => {
                        if let Some(pid) = o.position_id.clone() {
                            if let Some(mut p) = book.positions.remove(&pid) {
                                let fill = o.fill.clone().unwrap();
                                p.status = PositionStatus::Closed;
                                p.exit_evidence = o.evidence.clone();
                                p.closed_us = Some(fill_ts);
                                p.proceeds = fill.out_amount;
                                p.exit_tx_costs = o.costs.tx_costs();
                                p.venue_fees_paid += o.costs.venue_fee;
                                p.exit_order_id = Some(o.order_id.clone());
                                p.realized_pnl = Some(p.proceeds as i64 - p.exit_tx_costs as i64 - p.failed_exit_costs as i64 - p.cost_basis as i64);
                                p.rent_locked = 0;
                                p.version += 1;
                                p.last_value_us = fill_ts;
                                book.last_exit_us.insert(p.mint, fill_ts);
                                cmds.push(StoreCmd::Position(Box::new(p.clone())));
                                cmds.push(StoreCmd::PositionEvent {
                                    position_id: pid,
                                    ts_us: fill_ts,
                                    kind: "closed".into(),
                                    doc: serde_json::json!({"order_id": o.order_id, "proceeds": p.proceeds, "realized_pnl": p.realized_pnl, "reason": p.exit_reason}),
                                });
                                book.closed.push(p);
                            }
                        }
                    }
                    FillOutcome::Failed { .. } => {
                        if o.kind == OrderKind::Exit {
                            if let Some(pid) = o.position_id.clone() {
                                if let Some(p) = book.positions.get_mut(&pid) {
                                    if o.status == OrderStatus::FailedSlippage {
                                        p.failed_exit_costs += o.costs.base_fee + o.costs.priority_fee;
                                    }
                                    p.exit_attempts += 1;
                                    let n = p.exit_attempts as usize;
                                    if p.exit_attempts >= exit_cfg.max_sell_retries {
                                        p.status = PositionStatus::Unresolved;
                                        p.unresolved_reason = Some(format!("{} exit attempts failed; last: {}", p.exit_attempts, o.error.clone().unwrap_or_default()));
                                        p.next_exit_attempt_us = fill_ts + (exit_cfg.unresolved_retry_interval_s * 1e6) as i64;
                                    } else {
                                        let back = exit_cfg.retry_backoff_ms.get(n - 1).or(exit_cfg.retry_backoff_ms.last()).copied().unwrap_or(1000);
                                        p.next_exit_attempt_us = fill_ts + back as i64 * 1000;
                                        p.exit_slippage_bps = (p.exit_slippage_bps + exit_cfg.slippage_step_bps).min(exit_cfg.max_slippage_bps);
                                    }
                                    p.version += 1;
                                    cmds.push(StoreCmd::Position(Box::new(p.clone())));
                                    cmds.push(StoreCmd::PositionEvent {
                                        position_id: pid,
                                        ts_us: fill_ts,
                                        kind: "exit_failed".into(),
                                        doc: serde_json::json!({"order_id": o.order_id, "status": o.status, "error": o.error, "attempt": p.exit_attempts}),
                                    });
                                }
                            }
                        }
                    }
                }
                cmds.push(StoreCmd::OrderCompleted(Box::new(o)));
                let group: Vec<StoreCmd> = cmds.drain(outer_len..).collect();
                cmds.push(StoreCmd::Batch(group));
            }
        }
        for c in exec_new {
            self.record_exec_check(c).await;
        }
        for a in fill_ages {
            self.lat("quote_age_at_fill", a);
        }
        for (l, q, status) in fill_lat {
            self.lat("fill_latency_sim", l);
            if let Some(q) = q {
                self.note_route(if q.source == QuoteSource::Jupiter { "jupiter" } else { "venue_sim" }, &Ok(q), now);
            } else if status == OrderStatus::NoRoute {
                self.note_route("venue_sim", &Err(QuoteError::NoRoute("at fill time".into())), now);
            }
        }
        for c in cmds {
            self.store.send(c).await;
        }
    }

    // ------------------------------------------------------------------ exits

    async fn check_positions_for(&mut self, mint: Pubkey, now: i64) {
        let ids: Vec<(usize, String)> = self
            .books
            .iter()
            .enumerate()
            .flat_map(|(i, b)| b.positions.values().filter(|p| p.mint == mint && p.is_live()).map(move |p| (i, p.position_id.clone())))
            .collect();
        for (bi, pid) in ids {
            self.check_position(bi, &pid, now).await;
        }
    }

    pub async fn check_all_positions(&mut self, now: i64) {
        let ids: Vec<(usize, String)> = self
            .books
            .iter()
            .enumerate()
            .flat_map(|(i, b)| b.positions.values().filter(|p| p.is_live()).map(move |p| (i, p.position_id.clone())))
            .collect();
        for (bi, pid) in ids {
            self.check_position(bi, &pid, now).await;
        }
    }

    async fn check_position(&mut self, bi: usize, pid: &str, now: i64) {
        let exit_cfg = self.cfg.strategy.exit.clone();
        let costs = self.cfg.costs.clone();
        let has_pending = self.books[bi].pending.iter().any(|o| o.position_id.as_deref() == Some(pid));
        let Some(pos) = self.books[bi].positions.get(pid).cloned() else { return };
        if has_pending {
            return;
        }
        let st = self.universe.tokens.get(&pos.mint);
        let scen = self.books[bi].scenario.clone();
        let exit_fixed = costs.base_fee_lamports + scen.priority_fee_lamports + scen.tip_lamports;
        let own = if exit_cfg.model_own_impact { pos.tokens } else { 0 };
        let quote = match st {
            Some(st) => venue_quote(st, Side::Sell, pos.tokens, own, &self.fees, 120_000.0, now),
            None => Err(QuoteError::NoRoute("no token state (not observed since restart)".into())),
        };
        let quote_lat = quote.as_ref().map(|q| q.latency_us).unwrap_or(0);
        let value = quote.as_ref().ok().map(|q| q.out_amount.saturating_sub(exit_fixed));
        let mut p = pos.clone();
        let mut changed = false;
        if let Some(st) = st {
            // After migration the liquidity reference is rebased to the new venue's reserves.
            if st.venue().is_some() && st.venue() != p.liquidity_ref_venue {
                p.entry_liquidity = st.liquidity_lamports();
                p.liquidity_ref_venue = st.venue();
                changed = true;
            }
        }
        let awaiting_migration = st.is_some_and(|s| s.phase == crate::state::Phase::AwaitingMigration);
        if let Some(v) = value {
            p.last_value = Some(v);
            p.last_value_us = now;
            p.route_lost_since_us = None;
            if v > p.peak_value || p.peak_value == 0 {
                p.peak_value = v;
                p.peak_us = now;
            }
        } else if p.route_lost_since_us.is_none() {
            p.route_lost_since_us = Some(now);
            changed = true;
        }
        let mut submit: Option<(Quote, u32)> = None;
        let mut want_jupiter = false;
        match p.status {
            PositionStatus::Open if awaiting_migration => {
                // Hold through migration; becomes Unresolved if the pool never shows up.
                if let Some(since) = p.route_lost_since_us {
                    if (now - since) as f64 / 1e6 >= exit_cfg.unresolved_after_s {
                        p.status = PositionStatus::Unresolved;
                        p.exit_reason = Some(crate::strategy::exit::ExitReason::RouteLost { detail: "curve complete; migration pool not observed".into() });
                        p.unresolved_reason = Some(format!("no migration route for {:.0}s", (now - since) as f64 / 1e6));
                        p.next_exit_attempt_us = now + (exit_cfg.unresolved_retry_interval_s * 1e6) as i64;
                        changed = true;
                        want_jupiter = true;
                    }
                }
            }
            PositionStatus::Open => {
                if let Some(st) = st {
                    let f = compute(st, now);
                    let pv = PositionView {
                        opened_us: p.opened_us,
                        cost_basis_lamports: p.cost_basis,
                        peak_value_lamports: p.peak_value,
                        peak_us: p.peak_us,
                        entry_liquidity_lamports: p.entry_liquidity,
                        entry_creator_sold_fraction: p.entry_creator_sold_fraction,
                        exit_impact_bps: quote.as_ref().ok().map(|q| q.price_impact_bps),
                    };
                    if let Some((reason, urg)) = exit_signal(&f, &pv, value, &exit_cfg, now) {
                        p.exit_reason = Some(reason);
                        p.exit_urgency = Some(urg);
                        p.exit_slippage_bps = if urg == Urgency::Emergency { exit_cfg.emergency_slippage_bps } else { exit_cfg.normal_slippage_bps };
                        p.status = PositionStatus::Exiting;
                        changed = true;
                        match &quote {
                            Ok(q) => submit = Some((q.clone(), p.exit_slippage_bps)),
                            Err(_) => want_jupiter = true,
                        }
                    }
                } else if value.is_none() {
                    // no state at all: treat as route lost
                    p.status = PositionStatus::Exiting;
                    p.exit_reason = Some(crate::strategy::exit::ExitReason::RouteLost { detail: "token state unavailable".into() });
                    changed = true;
                    want_jupiter = true;
                }
            }
            PositionStatus::Exiting | PositionStatus::Unresolved => {
                if now >= p.next_exit_attempt_us {
                    match &quote {
                        Ok(q) => submit = Some((q.clone(), p.exit_slippage_bps)),
                        Err(_) => want_jupiter = true,
                    }
                }
                if p.status == PositionStatus::Exiting {
                    if let Some(since) = p.route_lost_since_us {
                        if (now - since) as f64 / 1e6 >= exit_cfg.unresolved_after_s {
                            p.status = PositionStatus::Unresolved;
                            p.unresolved_reason = Some(format!(
                                "no exit route for {:.0}s: {}",
                                (now - since) as f64 / 1e6,
                                quote.as_ref().err().map(|e| e.to_string()).unwrap_or_default()
                            ));
                            p.next_exit_attempt_us = now + (exit_cfg.unresolved_retry_interval_s * 1e6) as i64;
                            changed = true;
                        }
                    }
                }
            }
            PositionStatus::Closed => {}
        }
        self.lat("venue_quote", quote_lat);
        if want_jupiter && now >= p.next_exit_attempt_us {
            p.next_exit_attempt_us = now + exit_cfg.retry_backoff_ms.first().copied().unwrap_or(1000) as i64 * 1000;
            changed = true;
            self.request_exit_jupiter(bi, &p, now).await;
        }
        if let Some((q, slip)) = submit {
            p.next_exit_attempt_us = i64::MAX; // until the order completes
            p.version += 1;
            self.books[bi].positions.insert(p.position_id.clone(), p.clone());
            self.submit_exit(bi, &p, q, slip, now).await;
            return;
        }
        if changed {
            p.version += 1;
            self.store.send(StoreCmd::Position(Box::new(p.clone()))).await;
        }
        self.books[bi].positions.insert(p.position_id.clone(), p);
    }

    async fn submit_exit(&mut self, bi: usize, p: &Position, q: Quote, slippage_bps: u32, now: i64) {
        let book = &mut self.books[bi];
        let order_id = book.next_id("ord", now);
        let lat = latency_us(&book.scenario, &crate::paper::sim_seed(&book.id, &book.scenario.name, &order_id));
        let min_out = (q.out_amount as u128 * (10_000 - slippage_bps.min(10_000)) as u128 / 10_000) as u64;
        let attempt = p.exit_attempts + 1;
        let order = Order {
            order_id: order_id.clone(),
            idempotency_key: format!("{}:{}:exit:{}", book.id, p.position_id, attempt),
            book_id: book.id.clone(),
            scenario: book.scenario.name.clone(),
            data_mode: self.mode,
            strategy_version: self.version.clone(),
            kind: OrderKind::Exit,
            side: Side::Sell,
            mint: p.mint,
            path: p.path,
            position_id: Some(p.position_id.clone()),
            amount: p.tokens,
            slippage_bps,
            min_out,
            decision_quote: Some(q),
            decision_us: now,
            fill_at_us: now + lat,
            trigger_signature: p.trigger_signature.clone(),
            trigger_slot: p.trigger_slot,
            attempt,
            reason: p.exit_reason.as_ref().map(|r| r.code()).unwrap_or_default(),
            status: OrderStatus::Pending,
            fill: None,
            fill_quote: None,
            costs: Default::default(),
            error: None,
            completed_us: None,
            observed_quote: None,
            fill_basis: None,
            aggregator_divergence_bps: None,
            evidence: None,
        };
        book.pending.push(order.clone());
        let (mint, tokens) = (p.mint, p.tokens);
        self.spawn_jupiter(JupReq { mint, side: Side::Sell, amount: tokens, purpose: JupPurpose::ObserveOrder { book_idx: bi, order_id: order_id.clone() }, sent_us: now }).await;
        self.store.send(StoreCmd::Position(Box::new(p.clone()))).await;
        self.store
            .send(StoreCmd::PositionEvent {
                position_id: p.position_id.clone(),
                ts_us: now,
                kind: "exit_submitted".into(),
                doc: serde_json::json!({"order_id": order_id, "reason": p.exit_reason, "urgency": p.exit_urgency, "slippage_bps": slippage_bps, "attempt": attempt}),
            })
            .await;
        self.store.send(StoreCmd::OrderSubmitted(Box::new(order))).await;
    }

    // ------------------------------------------------------------------ aggregator

    async fn request_exit_jupiter(&mut self, bi: usize, p: &Position, now: i64) {
        let req = JupReq {
            mint: p.mint,
            side: Side::Sell,
            amount: p.tokens,
            purpose: JupPurpose::Exit { book_idx: bi, position_id: p.position_id.clone(), attempt: p.exit_attempts },
            sent_us: now,
        };
        self.spawn_jupiter(req).await;
    }

    async fn spawn_jupiter(&mut self, req: JupReq) {
        let (Some(j), Some(tx)) = (self.jup.clone(), self.ctl_tx.clone()) else {
            let why = if self.mode == DataMode::Live { "jupiter not configured" } else { "offline fixture/replay mode — no network quotes" };
            let r = Err(QuoteError::Disabled(why.into()));
            self.note_route("jupiter", &r, req.sent_us);
            self.store
                .send(StoreCmd::RouteCheck(RouteCheck {
                    ts_us: req.sent_us,
                    mint: req.mint.to_string(),
                    side: format!("{:?}", req.side).to_lowercase(),
                    source: "jupiter".into(),
                    amount: req.amount,
                    ok: false,
                    latency_us: 0,
                    error: Some(why.into()),
                    out_amount: None,
                    route: None,
                }))
                .await;
            return;
        };
        self.jup_counter += 1;
        let id = self.jup_counter;
        let (mint, side, amount) = (req.mint, req.side, req.amount);
        self.jup_reqs.insert(id, req);
        tokio::spawn(async move {
            let result = j.quote(mint, side, amount).await;
            let _ = tx.send(Control::Jupiter(JupResult { req_id: id, result })).await;
        });
    }

    pub async fn handle_control(&mut self, c: Control) -> bool {
        match c {
            Control::Shutdown => return false,
            Control::BackfillDone(rep) => {
                self.health.recovery_notes.push(format!("backfill: {} txs loaded in {:.0} ms", rep.transactions_loaded, rep.duration_ms));
                self.live.backfill = Some(rep);
            }
            Control::PoolResolved { pool, result } => match result {
                Ok(Some((base, quote))) => {
                    if crate::decode::pumpfun::is_sol_quote(Some(&quote)) {
                        self.universe.register_pool(pool, base);
                        self.stats.pools_resolved += 1;
                    } else {
                        self.stats.pools_non_sol += 1;
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    // allow a later retry
                    self.pool_reqs.remove(&pool);
                }
            },
            Control::MintChecked { mint, result } => {
                match &result {
                    Ok(m) if m.is_safe() => self.stats.mint_checks_ok += 1,
                    Ok(_) => self.stats.mint_checks_risky += 1,
                    Err(_) => self.stats.mint_checks_failed += 1,
                }
                if let Some(st) = self.universe.tokens.get_mut(&mint) {
                    st.mint_safety = Some(result);
                }
            }
            Control::Jupiter(r) => {
                let Some(req) = self.jup_reqs.remove(&r.req_id) else { return true };
                let now = self.now();
                self.note_route("jupiter", &r.result, now);
                if let Ok(q) = &r.result {
                    self.lat("jupiter_quote", q.latency_us);
                }
                self.store
                    .send(StoreCmd::RouteCheck(RouteCheck {
                        ts_us: now,
                        mint: req.mint.to_string(),
                        side: format!("{:?}", req.side).to_lowercase(),
                        source: "jupiter".into(),
                        amount: req.amount,
                        ok: r.result.is_ok(),
                        latency_us: r.result.as_ref().map(|q| q.latency_us).unwrap_or(now - req.sent_us),
                        error: r.result.as_ref().err().map(|e| e.to_string()),
                        out_amount: r.result.as_ref().ok().map(|q| q.out_amount),
                        route: r.result.as_ref().ok().map(|q| q.route.clone()),
                    }))
                    .await;
                if let (JupPurpose::ObserveOrder { book_idx, order_id }, Ok(q)) = (&req.purpose, &r.result) {
                    // attach only while the order is still pending (race guard)
                    if let Some(o) = self.books.get_mut(*book_idx).and_then(|b| b.pending.iter_mut().find(|o| &o.order_id == order_id)) {
                        o.observed_quote = Some(q.clone());
                        self.stats.order_observations_attached += 1;
                    }
                }
                if let (JupPurpose::Exit { book_idx, position_id, attempt }, Ok(q)) = (req.purpose, r.result) {
                    // Race guard: position must still be live, unchanged attempt count, no pending order.
                    let ok = self.books.get(book_idx).and_then(|b| b.positions.get(&position_id)).is_some_and(|p| {
                        p.is_live() && p.status != PositionStatus::Open && p.exit_attempts == attempt
                    }) && !self.books[book_idx].pending.iter().any(|o| o.position_id.as_deref() == Some(position_id.as_str()));
                    if ok {
                        let p = self.books[book_idx].positions.get(&position_id).cloned().unwrap();
                        let slip = p.exit_slippage_bps;
                        self.submit_exit(book_idx, &p, q, slip, now).await;
                    }
                }
            }
        }
        true
    }

    // ------------------------------------------------------------------ rollback

    async fn rollback(&mut self, slot: u64) {
        let affected = self.universe.rollback_slot(slot);
        self.health.rollbacks += 1;
        self.health.rolled_back_mints += affected.len() as u64;
        let now = self.now();
        let mut cmds = Vec::new();
        let costs = self.cfg.costs.clone();
        for book in self.books.iter_mut() {
            let mut keep = Vec::new();
            for mut o in book.pending.drain(..) {
                if o.kind == OrderKind::Entry && o.trigger_slot == slot {
                    o.status = OrderStatus::CancelledRollback;
                    o.error = Some(format!("trigger slot {slot} rolled back before fill"));
                    o.completed_us = Some(now);
                    book.reserved = book.reserved.saturating_sub(entry_reservation(o.amount, &book.scenario, &costs));
                    cmds.push(StoreCmd::OrderCompleted(Box::new(o)));
                } else {
                    keep.push(o);
                }
            }
            book.pending = keep;
            for p in book.positions.values_mut() {
                if p.trigger_slot == slot && !p.trigger_rolled_back {
                    p.trigger_rolled_back = true;
                    p.version += 1;
                    cmds.push(StoreCmd::Position(Box::new(p.clone())));
                    cmds.push(StoreCmd::PositionEvent {
                        position_id: p.position_id.clone(),
                        ts_us: now,
                        kind: "trigger_rolled_back".into(),
                        doc: serde_json::json!({"slot": slot}),
                    });
                }
            }
        }
        for m in &affected {
            self.last_eval_us.remove(m);
        }
        for c in cmds {
            self.store.send(c).await;
        }
        for m in affected {
            self.check_positions_for(m, now).await;
        }
    }

    // ------------------------------------------------------------------ live helpers

    async fn feed_tick(&mut self, now: i64) {
        if let Some(gap) = self.feed_mon.evaluate(now) {
            self.store.send(StoreCmd::ObservationGap { run_id: self.run_id.clone(), gap }).await;
        }
    }

    async fn record_exec_check(&mut self, c: ExecutionCheck) {
        self.store.send(StoreCmd::ExecutionCheck(Box::new(c.clone()))).await;
        self.exec_recent.push_back(c);
        while self.exec_recent.len() > 5_000 {
            self.exec_recent.pop_front();
        }
    }

    /// Forward-evaluation horizons and execution-check expiry (time driven).
    async fn background_tick(&mut self, now: i64) {
        if self.cfg.forward.enabled && self.fwd.pending() > 0 {
            let base = self.cfg.scenarios.iter().find(|s| s.name == "base").cloned().unwrap_or_default();
            let fixed = 2 * (self.cfg.costs.base_fee_lamports + base.priority_fee_lamports + base.tip_lamports);
            for o in self.fwd.due(now, &self.universe, &self.fees, fixed) {
                self.store.send(StoreCmd::ForwardOutcome(Box::new(o.clone()))).await;
                self.fwd_recent.push_back(o);
                while self.fwd_recent.len() > 20_000 {
                    self.fwd_recent.pop_front();
                }
            }
        }

    }

    /// Live: resolve unknown PumpSwap pools and check new mints via read-only RPC.
    async fn request_lookups(&mut self) {
        let pending: Vec<Pubkey> = self.universe.pending_pools.drain(..).collect();
        let (Some(rpc), Some(tx)) = (self.live.rpc.clone(), self.ctl_tx.clone()) else { return };
        for pool in pending {
            if !self.pool_reqs.insert(pool) {
                continue;
            }
            self.stats.pools_resolution_requested += 1;
            let (rpc, tx) = (rpc.clone(), tx.clone());
            tokio::spawn(async move {
                let result = match rpc.get_account(&pool.to_string()).await {
                    Ok(Some((data, _owner))) => crate::decode::pumpswap::decode_pool_account(&data).map(|p| Some((p.base_mint, p.quote_mint))).map_err(|e| e.to_string()),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Control::PoolResolved { pool, result }).await;
            });
        }
        if self.cfg.strategy.entry.require_mint_check {
            let new_mints: Vec<Pubkey> = self.universe.tokens.values().filter(|t| t.mint_safety.is_none() && !self.mint_reqs.contains(&t.mint)).map(|t| t.mint).take(50).collect();
            for mint in new_mints {
                self.mint_reqs.insert(mint);
                self.stats.mint_checks_requested += 1;
                let (rpc, tx) = (rpc.clone(), tx.clone());
                tokio::spawn(async move {
                    let result = match rpc.get_account(&mint.to_string()).await {
                        Ok(Some((data, owner))) => crate::safety::check_mint(&data, &owner),
                        Ok(None) => Err("mint account not found".into()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(Control::MintChecked { mint, result }).await;
                });
            }
        }
    }

    fn coverage(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        if self.mode != DataMode::Live {
            m.insert("all".into(), format!("offline {} source", self.mode.as_str()));
            return m;
        }
        let caps: Vec<&BTreeMap<String, String>> = self.feed_mon.providers.values().map(|p| &p.capabilities).collect();
        let accepted = |method: &str| caps.iter().any(|c| c.get(method).is_some_and(|v| v.starts_with("accepted")));
        let fetch = self.live.fetch_stats.as_ref().map(|f| f.snapshot());
        m.insert("pump_bonding_curve".into(), if accepted("logsSubscribe") || accepted("transactionSubscribe") { "covered (subscription accepted)".into() } else { "NOT COVERED: no accepted subscription yet".into() });
        m.insert(
            "pumpswap".into(),
            if accepted("transactionSubscribe") {
                "covered natively (transactionSubscribe)".into()
            } else if self.live.rpc.is_some() {
                format!("covered via getTransaction fetch ({} fetched)", fetch.map(|f| f.fetched).unwrap_or(0))
            } else {
                "NOT COVERED: logs-only provider and no RPC for fetch".into()
            },
        );
        m.insert("raydium_amm_v4".into(), "decode-only (never traded)".into());
        m
    }

    // ------------------------------------------------------------------ snapshot

    pub fn snapshot(&self) -> Snapshot {
        let now = self.now();
        let books: Vec<BookView> = self
            .books
            .iter()
            .map(|b| {
                let open: Vec<Position> = b.positions.values().filter(|p| p.is_live()).cloned().collect();
                // unresolved / no-route positions are valued at zero (conservative)
                let open_value: u64 = open.iter().map(|p| if p.route_lost_since_us.is_some() { 0 } else { p.last_value.unwrap_or(0) }).sum();
                let equity = b.balance + b.rent_locked() as i64 + open_value as i64;
                let acct = b.verify();
                BookView {
                    book_id: b.id.clone(),
                    scenario: b.scenario.name.clone(),
                    balance_sol: sol(b.balance),
                    reserved_sol: sol(b.reserved as i64),
                    rent_locked_sol: sol(b.rent_locked() as i64),
                    open_value_sol: sol(open_value as i64),
                    equity_sol: sol(equity),
                    start_sol: sol(b.start_balance),
                    equity_pnl_sol: sol(equity - b.start_balance),
                    exposure_sol: sol(b.exposure() as i64),
                    failed_entry_costs_sol: sol(b.failed_entry_costs as i64),
                    open_positions: open,
                    pending_orders: b.pending.len(),
                    perf: perf(&b.closed),
                    perf_by_path_size: {
                        let mut m: BTreeMap<String, Vec<Position>> = BTreeMap::new();
                        for p in &b.closed {
                            m.entry(format!("{}@{}", p.path.as_str(), crate::metrics::size_label(p.cost_basis))).or_default().push(p.clone());
                        }
                        m.into_iter().map(|(k, v)| (k, perf(&v))).collect()
                    },
                    perf_validated: perf(&b.closed.iter().filter(|p| p.validated()).cloned().collect::<Vec<_>>()),
                    perf_unverified: perf(&b.closed.iter().filter(|p| !p.validated()).cloned().collect::<Vec<_>>()),
                    perf_validated_holdout: perf(&b.closed.iter().filter(|p| p.validated() && crate::metrics::evidence_split(p.opened_us) == "holdout").cloned().collect::<Vec<_>>()),
                    perf_validated_calibration: perf(&b.closed.iter().filter(|p| p.validated() && crate::metrics::evidence_split(p.opened_us) == "calibration").cloned().collect::<Vec<_>>()),
                    sensitivity_all: crate::metrics::sensitivity(&b.closed, b.scenario.priority_fee_lamports),
                    sensitivity_validated: crate::metrics::sensitivity(&b.closed.iter().filter(|p| p.validated()).cloned().collect::<Vec<_>>(), b.scenario.priority_fee_lamports),
                    order_outcomes: b.order_outcomes.clone(),
                    execution_failure_rate: {
                        let total: u64 = b.order_outcomes.values().sum();
                        let failed: u64 = b.order_outcomes.iter().filter(|(k, _)| !k.ends_with(":filled")).map(|(_, v)| v).sum();
                        (total > 0).then(|| failed as f64 / total as f64)
                    },
                    unresolved_positions: b.positions.values().filter(|p| p.status == PositionStatus::Unresolved).count(),
                    perf_by_path: ["launch", "momentum"]
                        .iter()
                        .map(|k| (k.to_string(), perf(&b.closed.iter().filter(|p| p.path.as_str() == *k).cloned().collect::<Vec<_>>())))
                        .collect(),
                    accounting_ok: acct.is_ok(),
                    accounting_error: acct.err(),
                }
            })
            .collect();
        let mut trades: Vec<Position> = self.books.iter().flat_map(|b| b.closed.iter().cloned()).collect();
        trades.sort_by_key(|p| std::cmp::Reverse(p.closed_us.unwrap_or(0)));
        trades.truncate(300);
        let mut cands: Vec<CandidateView> = self.candidates.values().cloned().collect();
        cands.sort_by_key(|c| std::cmp::Reverse(c.evaluated_us));
        cands.truncate(150);
        let store = &self.store.stats;
        Snapshot {
            feed_status: if self.mode == DataMode::Live { Some(self.feed_mon.status(now)) } else { None },
            providers: self.feed_mon.providers.clone(),
            rpc_endpoints: self.live.rpc.as_ref().map(|r| r.stats()).unwrap_or_default(),
            tx_fetch: self.live.fetch_stats.as_ref().map(|f| f.snapshot()),
            backfill: self.live.backfill.clone(),
            forward_summary: crate::forward::summarize(&self.fwd_recent),
            forward_pending: self.fwd.pending(),
            execution_quality: crate::execcheck::summarize(&self.exec_recent),
            recent_execution_checks: self.exec_recent.iter().rev().take(50).cloned().collect(),
            decoder_verification: crate::decode::verification_with(self.decoder_verification.as_ref()),
            expected_move_model_status: crate::forward::expected_move_status(&self.fwd_recent, self.mode == DataMode::Live, self.cfg.evidence.min_outcomes_for_calibration),
            coverage: self.coverage(),
            counters: EngineCounters {
                pools_resolved: self.universe.pools_resolved,
                ..self.stats.clone()
            },
            status_banner: match self.mode {
                DataMode::Live => format!("LIVE DATA / PAPER TRADING — feed {:?}", self.feed_mon.state()),
                DataMode::Fixture => "OFFLINE FIXTURE / PAPER TRADING".into(),
                DataMode::Replay => "REPLAY / PAPER TRADING".into(),
            },
            generated_us: now,
            paper_only: true,
            data_mode: Some(self.mode),
            data_mode_note: match self.mode {
                DataMode::Live => "LIVE market data from a real provider stream. Trades are PAPER simulations.".into(),
                DataMode::Fixture => "FIXTURE data: synthetic events encoded per venue layouts. NOT market data; P&L is not evidence of real profitability.".into(),
                DataMode::Replay => "REPLAY of a recorded journal. Trades are PAPER simulations.".into(),
            },
            profile_key: self.profile_key.clone(),
            run_id: self.run_id.clone(),
            strategy_version: self.version.clone(),
            health: Health { tokens_tracked: self.universe.tokens.len(), ..self.health.clone() },
            ingest: self.ingestor.as_ref().map(|i| i.snapshot()),
            store_backend: self.store.backend_name.into(),
            store_written: store.written.load(std::sync::atomic::Ordering::Relaxed),
            store_errors: store.errors.load(std::sync::atomic::Ordering::Relaxed),
            feed: self.feed.iter().rev().cloned().collect(),
            candidates: cands,
            rejection_counts: self.rejection_counts.clone(),
            books,
            trades,
            latency: self.lat.iter().map(|(k, v)| (k.to_string(), v.summary())).collect(),
            routes: self.routes.clone(),
            evaluations: self.evaluations,
            entries_decided: self.entries_decided,
        }
    }

    pub fn publish(&mut self, force: bool) {
        let w = wall_us();
        if !force && w - self.last_publish_wall < 250_000 {
            return;
        }
        self.last_publish_wall = w;
        let s = self.snapshot();
        if let Ok(mut d) = self.dash.write() {
            *d = s;
        }
    }

    pub async fn tick(&mut self) {
        let now = self.now();
        if !self.clock.is_event_time() {
            self.process_due(now, false).await;
            self.check_all_positions(now).await;
            self.feed_tick(now).await;
            self.background_tick(now).await;
        }
        self.publish(false);
    }

    /// Main loop for live/streaming use. Tests drive `handle_stream` directly instead.
    pub async fn run(mut self, mut rx: mpsc::Receiver<StreamMsg>, mut ctl: mpsc::Receiver<Control>) -> Self {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(self.cfg.tick_ms));
        loop {
            tokio::select! {
                biased;
                c = ctl.recv() => match c {
                    Some(c) => if !self.handle_control(c).await { break },
                    None => break,
                },
                m = rx.recv() => match m {
                    Some(m) => {
                        self.handle_stream(m).await;
                        if self.exit_on_end && self.health.ended { break }
                    }
                    None => break,
                },
                _ = tick.tick() => self.tick().await,
            }
        }
        if let Some(gap) = self.feed_mon.close(self.now()) {
            self.store.send(StoreCmd::ObservationGap { run_id: self.run_id.clone(), gap }).await;
        }
        self.store.flush().await;
        self.publish(true);
        self
    }

    /// Wait until every store command sent so far is durably applied.
    pub async fn flush(&self) {
        self.store.flush().await;
    }

    pub fn ledger_for_test(&self) -> Vec<(String, i64)> {
        self.books.iter().map(|b| (b.id.clone(), b.balance)).collect()
    }
}
