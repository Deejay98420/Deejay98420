//! Provider-neutral stream interfaces.
//!
//! Every provider (fixture replay, standard Solana RPC WebSocket, full-transaction streams) produces
//! `StreamMsg`s. The `Ingestor` deduplicates, applies backpressure through a bounded queue and
//! stamps receive times; the engine consumes the queue.

pub mod backoff;
pub mod dedup;
pub mod fixture;
pub mod ws;

use crate::time::wall_us;
use crate::types::{Commitment, DataMode};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InnerIx {
    pub program_id: String,
    pub data_b64: String,
}

/// One transaction as delivered by a provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub provider: String,
    pub data_mode: DataMode,
    pub slot: u64,
    pub signature: String,
    pub commitment: Commitment,
    #[serde(default)]
    pub err: Option<String>,
    #[serde(default)]
    pub logs: Vec<String>,
    #[serde(default)]
    pub inner_ix: Vec<InnerIx>,
    /// Block time, unix seconds (if the provider gives it).
    #[serde(default)]
    pub block_time: Option<i64>,
    /// When this process received the message (µs). For fixtures this is the scripted timeline.
    pub recv_us: i64,
    /// Historical transaction loaded by startup backfill: updates state only, never triggers trades.
    #[serde(default)]
    pub backfill: bool,
    /// µs spent fetching the full transaction after its signature was first seen (logs → getTransaction).
    #[serde(default)]
    pub fetch_us: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    Confirmed,
    Finalized,
    /// Fork abandoned: events from this slot must be rolled back.
    Dead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connected,
    Disconnected,
    Reconnecting,
    Subscribed,
    Exhausted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMsg {
    Event(RawEvent),
    Slot { slot: u64, status: SlotStatus, recv_us: i64 },
    Connection { provider: String, state: ConnState, detail: String, recv_us: i64 },
    /// Chain tip observed by a provider (slotSubscribe): used for slot lag and feed liveness.
    Tip { provider: String, slot: u64, recv_us: i64 },
    /// Result of a subscription request (explicit capability check per provider and method).
    Capability { provider: String, method: String, ok: bool, detail: String, recv_us: i64 },
    /// End of a finite source (fixture file).
    End { provider: String, recv_us: i64 },
}

/// Provider name used to journal slot-status markers alongside transactions.
pub const SLOT_MARKER_PROVIDER: &str = "slot-status";

impl RawEvent {
    /// Journal representation of a dead slot (so replays roll back exactly as the live run did).
    pub fn dead_slot_marker(slot: u64, recv_us: i64, mode: DataMode) -> Self {
        RawEvent {
            provider: SLOT_MARKER_PROVIDER.into(),
            data_mode: mode,
            slot,
            signature: format!("slot-dead:{slot}"),
            commitment: Commitment::Confirmed,
            err: Some("slot_dead".into()),
            logs: vec![],
            inner_ix: vec![],
            block_time: None,
            recv_us,
            backfill: false,
            fetch_us: None,
        }
    }
    pub fn is_slot_marker(&self) -> bool {
        self.provider == SLOT_MARKER_PROVIDER
    }
    /// Convert a journaled event back into the stream message it came from.
    pub fn into_stream_msg(self) -> StreamMsg {
        if self.is_slot_marker() {
            StreamMsg::Slot { slot: self.slot, status: SlotStatus::Dead, recv_us: self.recv_us }
        } else {
            StreamMsg::Event(self)
        }
    }
}

impl StreamMsg {
    pub fn recv_us(&self) -> i64 {
        match self {
            StreamMsg::Event(e) => e.recv_us,
            StreamMsg::Slot { recv_us, .. }
            | StreamMsg::Connection { recv_us, .. }
            | StreamMsg::End { recv_us, .. }
            | StreamMsg::Tip { recv_us, .. }
            | StreamMsg::Capability { recv_us, .. } => *recv_us,
        }
    }
}

#[derive(Debug, Default)]
pub struct IngestStats {
    pub received: AtomicU64,
    pub duplicates: AtomicU64,
    pub commitment_upgrades: AtomicU64,
    pub forwarded: AtomicU64,
    pub backpressure_waits: AtomicU64,
    pub queue_high_water: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestSnapshot {
    pub received: u64,
    pub duplicates: u64,
    pub commitment_upgrades: u64,
    pub forwarded: u64,
    pub backpressure_waits: u64,
    pub queue_high_water: u64,
    pub queue_capacity: usize,
}

/// Dedup + bounded forwarding. Multiple providers may feed the same Ingestor (redundancy);
/// the first copy of a signature wins and later copies are counted as duplicates.
#[derive(Clone)]
pub struct Ingestor {
    tx: mpsc::Sender<StreamMsg>,
    dedup: Arc<std::sync::Mutex<dedup::SignatureDedup>>,
    pub stats: Arc<IngestStats>,
    capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    New,
    Duplicate,
    /// Same signature seen again at a higher commitment. Forwarded so the engine can confirm.
    Upgrade,
}

impl Ingestor {
    pub fn new(capacity: usize, dedup_window: usize) -> (Self, mpsc::Receiver<StreamMsg>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                dedup: Arc::new(std::sync::Mutex::new(dedup::SignatureDedup::new(dedup_window))),
                stats: Arc::new(IngestStats::default()),
                capacity,
            },
            rx,
        )
    }

    /// Pre-seed the dedup window (restart recovery: signatures already journaled).
    pub fn seed(&self, sigs: impl IntoIterator<Item = (String, crate::types::Commitment)>) {
        let mut d = self.dedup.lock().unwrap();
        for (s, c) in sigs {
            d.admit(&s, c);
        }
    }

    /// Push with backpressure: never drops; waits when the queue is full and counts the wait.
    pub async fn push(&self, msg: StreamMsg) -> Result<Admit, mpsc::error::SendError<StreamMsg>> {
        self.stats.received.fetch_add(1, Ordering::Relaxed);
        let admit = match &msg {
            StreamMsg::Event(e) => self.dedup.lock().unwrap().admit(&e.signature, e.commitment),
            _ => Admit::New,
        };
        match admit {
            Admit::Duplicate => {
                self.stats.duplicates.fetch_add(1, Ordering::Relaxed);
                return Ok(Admit::Duplicate);
            }
            Admit::Upgrade => {
                self.stats.commitment_upgrades.fetch_add(1, Ordering::Relaxed);
            }
            Admit::New => {}
        }
        let depth = (self.capacity - self.tx.capacity()) as u64;
        self.stats.queue_high_water.fetch_max(depth, Ordering::Relaxed);
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(m)) => {
                self.stats.backpressure_waits.fetch_add(1, Ordering::Relaxed);
                self.tx.send(m).await?;
            }
            Err(mpsc::error::TrySendError::Closed(m)) => return Err(mpsc::error::SendError(m)),
        }
        self.stats.forwarded.fetch_add(1, Ordering::Relaxed);
        Ok(admit)
    }

    pub fn snapshot(&self) -> IngestSnapshot {
        let s = &self.stats;
        IngestSnapshot {
            received: s.received.load(Ordering::Relaxed),
            duplicates: s.duplicates.load(Ordering::Relaxed),
            commitment_upgrades: s.commitment_upgrades.load(Ordering::Relaxed),
            forwarded: s.forwarded.load(Ordering::Relaxed),
            backpressure_waits: s.backpressure_waits.load(Ordering::Relaxed),
            queue_high_water: s.queue_high_water.load(Ordering::Relaxed),
            queue_capacity: self.capacity,
        }
    }

    pub fn connection(&self, provider: &str, state: ConnState, detail: impl Into<String>) -> StreamMsg {
        StreamMsg::Connection { provider: provider.into(), state, detail: detail.into(), recv_us: wall_us() }
    }
}
