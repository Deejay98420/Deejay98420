//! Bounded signature deduplication window (FIFO eviction).

use super::Admit;
use crate::types::Commitment;
use std::collections::{HashMap, VecDeque};

pub struct SignatureDedup {
    cap: usize,
    order: VecDeque<String>,
    seen: HashMap<String, Commitment>,
}

impl SignatureDedup {
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), order: VecDeque::new(), seen: HashMap::new() }
    }
    pub fn len(&self) -> usize {
        self.seen.len()
    }
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
    pub fn admit(&mut self, sig: &str, c: Commitment) -> Admit {
        if let Some(prev) = self.seen.get_mut(sig) {
            if c > *prev {
                *prev = c;
                return Admit::Upgrade;
            }
            return Admit::Duplicate;
        }
        if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.order.push_back(sig.to_string());
        self.seen.insert(sig.to_string(), c);
        Admit::New
    }
}
