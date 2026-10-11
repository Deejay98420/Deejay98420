//! Selective capture helpers (read-only). Candidate selection for venue-specific evidence capture:
//! classify logs-only notifications by the venue's own instruction log lines, deduplicate
//! signatures, keep bounded per-side queues and always fetch the side with less evidence next, so a
//! small request budget yields useful, balanced buy/sell samples instead of raw volume.

use crate::types::programs;
use std::collections::{HashSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

/// Side of a PumpSwap swap from a transaction's logs: the `Instruction: Buy|BuyExactQuoteIn|Sell…`
/// line logged while PumpSwap itself is executing (not a router's own instruction log). Failed
/// transactions and transactions without such a line return None. A transaction with both sides
/// returns the first.
pub fn pumpswap_side_from_logs(logs: &[String], failed: bool) -> Option<Side> {
    if failed {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for l in logs {
        let Some(rest) = l.strip_prefix("Program ") else { continue };
        if let Some(msg) = rest.strip_prefix("log: Instruction: ") {
            if stack.last() == Some(&programs::PUMP_SWAP) {
                let m = msg.trim();
                if m.starts_with("Buy") {
                    return Some(Side::Buy);
                }
                if m.starts_with("Sell") {
                    return Some(Side::Sell);
                }
            }
            continue;
        }
        let mut parts = rest.splitn(2, ' ');
        let id = parts.next().unwrap_or("");
        let tail = parts.next().unwrap_or("");
        if tail.starts_with("invoke [") {
            stack.push(id);
        } else if tail == "success" || tail.starts_with("failed") {
            stack.pop();
        }
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    Queued,
    Duplicate,
    /// that side's queue is full: the candidate is dropped (bounded memory, no backlog)
    Dropped,
}

/// Bounded, deduplicated candidate queues per side.
#[derive(Debug)]
pub struct Selector {
    cap_per_side: usize,
    seen: HashSet<String>,
    buys: VecDeque<String>,
    sells: VecDeque<String>,
    pub duplicates: u64,
    pub dropped: u64,
}

impl Selector {
    pub fn new(cap_per_side: usize, already_captured: impl IntoIterator<Item = String>) -> Self {
        Self { cap_per_side: cap_per_side.max(1), seen: already_captured.into_iter().collect(), buys: VecDeque::new(), sells: VecDeque::new(), duplicates: 0, dropped: 0 }
    }
    pub fn offer(&mut self, signature: &str, side: Side) -> Offer {
        if self.seen.contains(signature) {
            self.duplicates += 1;
            return Offer::Duplicate;
        }
        let q = match side {
            Side::Buy => &mut self.buys,
            Side::Sell => &mut self.sells,
        };
        if q.len() >= self.cap_per_side {
            // keep the freshest candidates: drop the oldest queued one
            q.pop_front();
            self.dropped += 1;
        }
        self.seen.insert(signature.to_string());
        q.push_back(signature.to_string());
        Offer::Queued
    }
    /// Next candidate: newest first from the side with fewer saved samples (falls back to the other).
    pub fn next(&mut self, saved_buys: usize, saved_sells: usize) -> Option<(String, Side)> {
        let prefer_sell = saved_sells < saved_buys;
        let order = if prefer_sell { [Side::Sell, Side::Buy] } else { [Side::Buy, Side::Sell] };
        for s in order {
            let q = match s {
                Side::Buy => &mut self.buys,
                Side::Sell => &mut self.sells,
            };
            if let Some(sig) = q.pop_back() {
                return Some((sig, s));
            }
        }
        None
    }
    pub fn queued(&self) -> (usize, usize) {
        (self.buys.len(), self.sells.len())
    }
}
