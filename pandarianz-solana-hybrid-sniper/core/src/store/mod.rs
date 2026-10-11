//! Persistence. The engine never blocks on the database: it sends `StoreCmd`s through a bounded
//! channel to a single writer task (ordering preserved). Backends: PostgreSQL and in-memory (tests).

pub mod pg;

use crate::paper::{LedgerEntry, Order, OrderStatus, Position};
use crate::quote::Quote;
use crate::stream::RawEvent;
use crate::types::Commitment;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationRecord {
    pub eval_id: String,
    pub run_id: String,
    pub data_mode: String,
    pub ts_us: i64,
    pub mint: String,
    pub symbol: Option<String>,
    pub path: String,
    pub decision: String,
    pub strategy_version: String,
    pub score: f64,
    pub size_lamports: u64,
    pub reasons: serde_json::Value,
    pub features: serde_json::Value,
    pub size_checks: serde_json::Value,
    pub quote: Option<Quote>,
    pub trigger_signature: String,
    pub trigger_slot: u64,
    pub stamps: serde_json::Value,
    #[serde(default)]
    pub expected_move_bps: Option<f64>,
    #[serde(default)]
    pub net_edge_bps: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteCheck {
    pub ts_us: i64,
    pub mint: String,
    pub side: String,
    pub source: String,
    pub amount: u64,
    pub ok: bool,
    pub latency_us: i64,
    pub error: Option<String>,
    pub out_amount: Option<u64>,
    pub route: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookRecord {
    pub book_id: String,
    pub scenario: String,
    pub start_balance: u64,
    pub created_us: i64,
    pub scenario_config: serde_json::Value,
}

#[derive(Debug)]
pub enum StoreCmd {
    Run { run_id: String, data_mode: String, started_us: i64, strategy_version: String, config: serde_json::Value },
    RawEvent { run_id: String, ev: Box<RawEvent> },
    Evaluation(Box<EvaluationRecord>),
    Book(BookRecord),
    OrderSubmitted(Box<Order>),
    OrderCompleted(Box<Order>),
    Ledger(LedgerEntry),
    Position(Box<Position>),
    PositionEvent { position_id: String, ts_us: i64, kind: String, doc: serde_json::Value },
    RouteCheck(RouteCheck),
    ObservationGap { run_id: String, gap: crate::feed::Gap },
    ForwardOutcome(Box<crate::forward::ForwardOutcome>),
    ExecutionCheck(Box<crate::execcheck::ExecutionCheck>),
    /// Applied atomically (one DB transaction): e.g. a fill's ledger entries + position + order.
    Batch(Vec<StoreCmd>),
    Flush(oneshot::Sender<()>),
}

/// State recovered at startup for one profile.
#[derive(Debug, Default, Clone)]
pub struct Recovered {
    pub books: HashMap<String, (u64 /*start*/, i64 /*ledger sum*/, u64 /*last seq*/)>,
    pub positions: Vec<Position>,
    /// submissions that never completed (process stopped while pending)
    pub dangling_orders: Vec<Order>,
    pub idempotency_keys: Vec<String>,
    pub signatures: Vec<(String, Commitment)>,
    pub failed_entry_costs: HashMap<String, u64>,
}

#[derive(Default, Debug)]
pub struct MemData {
    pub runs: Vec<String>,
    pub raw: Vec<RawEvent>,
    pub raw_keys: std::collections::HashSet<String>,
    pub evaluations: Vec<EvaluationRecord>,
    pub books: HashMap<String, BookRecord>,
    pub submissions: Vec<Order>,
    pub orders: Vec<Order>,
    pub ledger: Vec<LedgerEntry>,
    pub positions: HashMap<String, Position>,
    pub position_events: Vec<(String, i64, String)>,
    pub route_checks: Vec<RouteCheck>,
    pub duplicate_idempotency_rejects: u64,
    pub gaps: Vec<crate::feed::Gap>,
    pub forward: Vec<crate::forward::ForwardOutcome>,
    pub exec_checks: Vec<crate::execcheck::ExecutionCheck>,
}

impl MemData {
    fn apply(&mut self, cmd: StoreCmd, profile_key: &str) {
        match cmd {
            StoreCmd::Run { run_id, .. } => self.runs.push(run_id),
            StoreCmd::RawEvent { ev, .. } => {
                if self.raw_keys.insert(format!("{profile_key}:{}", ev.signature)) {
                    self.raw.push(*ev);
                }
            }
            StoreCmd::Evaluation(e) => self.evaluations.push(*e),
            StoreCmd::Book(b) => {
                self.books.entry(b.book_id.clone()).or_insert(b);
            }
            StoreCmd::OrderSubmitted(o) => {
                if self.submissions.iter().any(|s| s.idempotency_key == o.idempotency_key) {
                    self.duplicate_idempotency_rejects += 1;
                } else {
                    self.submissions.push(*o);
                }
            }
            StoreCmd::OrderCompleted(o) => {
                if self.orders.iter().any(|s| s.idempotency_key == o.idempotency_key) {
                    self.duplicate_idempotency_rejects += 1;
                } else {
                    self.orders.push(*o);
                }
            }
            StoreCmd::Ledger(l) => self.ledger.push(l),
            StoreCmd::Position(p) => {
                let newer = self.positions.get(&p.position_id).map(|x| p.version >= x.version).unwrap_or(true);
                if newer {
                    self.positions.insert(p.position_id.clone(), *p);
                }
            }
            StoreCmd::PositionEvent { position_id, ts_us, kind, .. } => self.position_events.push((position_id, ts_us, kind)),
            StoreCmd::RouteCheck(r) => self.route_checks.push(r),
            StoreCmd::ObservationGap { gap, .. } => self.gaps.push(gap),
            StoreCmd::ForwardOutcome(f) => {
                if !self.forward.iter().any(|x| x.eval_id == f.eval_id && x.horizon_s == f.horizon_s) {
                    self.forward.push(*f)
                }
            }
            StoreCmd::ExecutionCheck(c) => self.exec_checks.push(*c),
            StoreCmd::Batch(cmds) => {
                for c in cmds {
                    self.apply(c, profile_key);
                }
            }
            StoreCmd::Flush(tx) => {
                let _ = tx.send(());
            }
        }
    }

    pub fn recover(&self, book_prefix: &str) -> Recovered {
        let mut r = Recovered::default();
        for (id, b) in &self.books {
            if id.starts_with(book_prefix) {
                let sum: i64 = self.ledger.iter().filter(|l| &l.book_id == id).map(|l| l.amount).sum();
                let seq = self.ledger.iter().filter(|l| &l.book_id == id).map(|l| l.seq).max().unwrap_or(0);
                r.books.insert(id.clone(), (b.start_balance, sum, seq));
            }
        }
        r.positions = self.positions.values().filter(|p| p.book_id.starts_with(book_prefix)).cloned().collect();
        let done: std::collections::HashSet<&str> = self.orders.iter().map(|o| o.order_id.as_str()).collect();
        r.dangling_orders = self
            .submissions
            .iter()
            .filter(|s| s.book_id.starts_with(book_prefix) && !done.contains(s.order_id.as_str()))
            .cloned()
            .collect();
        r.idempotency_keys = self.submissions.iter().filter(|s| s.book_id.starts_with(book_prefix)).map(|s| s.idempotency_key.clone()).collect();
        r.signatures = self.raw.iter().map(|e| (e.signature.clone(), e.commitment)).collect();
        for o in self.orders.iter().filter(|o| o.book_id.starts_with(book_prefix)) {
            if o.kind == crate::paper::OrderKind::Entry && o.status == OrderStatus::FailedSlippage {
                *r.failed_entry_costs.entry(o.book_id.clone()).or_default() += o.costs.base_fee + o.costs.priority_fee;
            }
        }
        r
    }
}

#[derive(Default)]
pub struct StoreStats {
    pub written: AtomicU64,
    pub errors: AtomicU64,
    pub queue_waits: AtomicU64,
}

#[derive(Clone)]
pub struct StoreHandle {
    tx: mpsc::Sender<StoreCmd>,
    pub stats: Arc<StoreStats>,
    pub backend_name: &'static str,
}

impl StoreHandle {
    pub async fn send(&self, cmd: StoreCmd) {
        match self.tx.try_send(cmd) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(c)) => {
                self.stats.queue_waits.fetch_add(1, Ordering::Relaxed);
                let _ = self.tx.send(c).await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCmd::Flush(tx)).await;
        let _ = rx.await;
    }
}

/// In-memory backend; the `Arc<Mutex<MemData>>` outlives engine instances so restart can be tested.
pub fn spawn_mem(data: Arc<Mutex<MemData>>, profile_key: String, capacity: usize) -> StoreHandle {
    let (tx, mut rx) = mpsc::channel::<StoreCmd>(capacity);
    let stats = Arc::new(StoreStats::default());
    let st = stats.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            data.lock().unwrap().apply(cmd, &profile_key);
            st.written.fetch_add(1, Ordering::Relaxed);
        }
    });
    StoreHandle { tx, stats, backend_name: "memory" }
}

pub fn spawn_pg(pg: pg::PgStore, capacity: usize) -> StoreHandle {
    let (tx, mut rx) = mpsc::channel::<StoreCmd>(capacity);
    let stats = Arc::new(StoreStats::default());
    let st = stats.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match pg.apply(cmd).await {
                Ok(()) => {
                    st.written.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    st.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(error = %e, "store write failed");
                }
            }
        }
    });
    StoreHandle { tx, stats, backend_name: "postgres" }
}
