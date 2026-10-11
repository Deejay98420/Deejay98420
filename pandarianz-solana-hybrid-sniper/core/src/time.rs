//! Time sources. Live mode uses the wall clock; fixture/replay mode uses event time so runs are deterministic.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn wall_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

/// Structured pipeline timestamps (microseconds since unix epoch). `None` = stage not reached / unknown.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Stamps {
    /// On-chain block time (seconds resolution from provider) in µs, if known.
    pub block_time_us: Option<i64>,
    /// Timestamp embedded in the decoded venue event, if any (seconds resolution) in µs.
    pub event_ts_us: Option<i64>,
    /// When our process received the message from the provider.
    pub recv_us: i64,
    /// When decoding finished.
    pub decoded_us: Option<i64>,
}

impl Stamps {
    /// Best estimate of when the event happened on chain.
    pub fn chain_us(&self) -> Option<i64> {
        self.event_ts_us.or(self.block_time_us)
    }
    /// Provider delivery lag (recv - chain). Coarse because chain times are second resolution.
    pub fn delivery_lag_us(&self) -> Option<i64> {
        self.chain_us().map(|c| self.recv_us - c)
    }
}

/// Clock used by the engine. In fixture/replay the engine advances it from event receive stamps.
#[derive(Clone, Debug)]
pub enum Clock {
    Wall,
    Event { now_us: i64 },
}

impl Clock {
    pub fn now(&self) -> i64 {
        match self {
            Clock::Wall => wall_us(),
            Clock::Event { now_us } => *now_us,
        }
    }
    pub fn observe(&mut self, ts_us: i64) {
        if let Clock::Event { now_us } = self {
            if ts_us > *now_us {
                *now_us = ts_us;
            }
        }
    }
    pub fn is_event_time(&self) -> bool {
        matches!(self, Clock::Event { .. })
    }
}
