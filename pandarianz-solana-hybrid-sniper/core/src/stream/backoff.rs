//! Exponential reconnect backoff with deterministic jitter and a cap.

use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Backoff {
    pub base_ms: u64,
    pub max_ms: u64,
    pub max_attempts: Option<u32>,
    attempt: u32,
    seed: u64,
}

impl Backoff {
    pub fn new(base_ms: u64, max_ms: u64, max_attempts: Option<u32>) -> Self {
        Self { base_ms, max_ms, max_attempts, attempt: 0, seed: 0x9E37_79B9_7F4A_7C15 }
    }
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
    /// Next delay or None when attempts are exhausted.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if let Some(m) = self.max_attempts {
            if self.attempt >= m {
                return None;
            }
        }
        let exp = self.base_ms.saturating_mul(1u64 << self.attempt.min(20));
        let capped = exp.min(self.max_ms);
        // xorshift jitter in [0, capped/4]
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let jitter = if capped >= 4 { self.seed % (capped / 4) } else { 0 };
        self.attempt += 1;
        Some(Duration::from_millis((capped + jitter).min(self.max_ms)))
    }
}
