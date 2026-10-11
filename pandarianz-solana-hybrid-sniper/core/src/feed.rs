//! Live feed health: provider liveness, chain-tip freshness, slot lag, failover status and
//! observation gaps. A disconnected or silent feed is never reported as healthy, and new entries
//! are blocked while the feed is not HEALTHY.

use crate::stream::ConnState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeedState {
    Healthy,
    Degraded,
    Down,
    /// Offline fixture / replay source: no live feed exists.
    Offline,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedConfig {
    /// no events for this long → DEGRADED
    pub stale_event_ms: f64,
    /// no events for this long → DOWN
    pub down_event_ms: f64,
    /// chain tip not advancing for this long (when a tip subscription exists) → DEGRADED
    pub stale_tip_ms: f64,
    /// newest event this many slots behind the tip → DEGRADED
    pub max_slot_lag: u64,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self { stale_event_ms: 20_000.0, down_event_ms: 60_000.0, stale_tip_ms: 5_000.0, max_slot_lag: 150 }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProviderHealth {
    pub state: Option<ConnState>,
    pub detail: String,
    pub since_us: i64,
    pub reconnects: u64,
    /// events this provider delivered first (after cross-provider dedup)
    pub first_deliveries: u64,
    pub last_event_us: Option<i64>,
    pub tip_slot: Option<u64>,
    pub last_tip_us: Option<i64>,
    /// subscription method → "accepted: …" / "REJECTED: …" (explicit capability check)
    pub capabilities: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Gap {
    pub start_us: i64,
    pub end_us: Option<i64>,
    pub state: FeedState,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeedStatus {
    pub state: FeedState,
    pub reasons: Vec<String>,
    pub providers_up: usize,
    pub providers_total: usize,
    pub tip_slot: Option<u64>,
    pub tip_age_ms: Option<f64>,
    pub last_event_age_ms: Option<f64>,
    pub slot_lag: Option<i64>,
    /// fraction of observed time spent HEALTHY since the first connection
    pub coverage: Option<f64>,
    pub open_gap: Option<Gap>,
    pub recent_gaps: Vec<Gap>,
}

pub struct FeedMonitor {
    pub cfg: FeedConfig,
    live: bool,
    pub providers: BTreeMap<String, ProviderHealth>,
    tip_slot: Option<u64>,
    last_tip_us: Option<i64>,
    last_event_us: Option<i64>,
    last_event_slot: Option<u64>,
    state: FeedState,
    reasons: Vec<String>,
    open_gap: Option<Gap>,
    gaps: VecDeque<Gap>,
    first_us: Option<i64>,
    last_eval_us: Option<i64>,
    healthy_us: i64,
    total_us: i64,
}

impl FeedMonitor {
    pub fn new(cfg: FeedConfig, live: bool) -> Self {
        Self {
            cfg,
            live,
            providers: BTreeMap::new(),
            tip_slot: None,
            last_tip_us: None,
            last_event_us: None,
            last_event_slot: None,
            state: if live { FeedState::Down } else { FeedState::Offline },
            reasons: if live { vec!["no provider connected yet".into()] } else { vec![] },
            open_gap: None,
            gaps: VecDeque::new(),
            first_us: None,
            last_eval_us: None,
            healthy_us: 0,
            total_us: 0,
        }
    }

    pub fn state(&self) -> FeedState {
        self.state
    }

    pub fn on_connection(&mut self, provider: &str, state: ConnState, detail: &str, ts: i64) {
        let p = self.providers.entry(provider.to_string()).or_default();
        if state == ConnState::Reconnecting && p.state.is_some() {
            p.reconnects += 1;
        }
        p.state = Some(state);
        p.detail = detail.to_string();
        p.since_us = ts;
        if self.first_us.is_none() && matches!(state, ConnState::Connected | ConnState::Subscribed) {
            self.first_us = Some(ts);
        }
    }

    pub fn on_capability(&mut self, provider: &str, method: &str, ok: bool, detail: &str) {
        let p = self.providers.entry(provider.to_string()).or_default();
        p.capabilities.insert(method.to_string(), format!("{}: {detail}", if ok { "accepted" } else { "REJECTED" }));
    }

    pub fn on_event(&mut self, provider: &str, slot: u64, ts: i64) {
        let p = self.providers.entry(provider.to_string()).or_default();
        p.first_deliveries += 1;
        p.last_event_us = Some(ts);
        self.last_event_us = Some(self.last_event_us.map_or(ts, |l| l.max(ts)));
        self.last_event_slot = Some(self.last_event_slot.map_or(slot, |s| s.max(slot)));
    }

    pub fn on_tip(&mut self, provider: &str, slot: u64, ts: i64) {
        let p = self.providers.entry(provider.to_string()).or_default();
        p.tip_slot = Some(slot);
        p.last_tip_us = Some(ts);
        if self.tip_slot.is_none_or(|t| slot > t) {
            self.tip_slot = Some(slot);
            self.last_tip_us = Some(ts);
        }
    }

    pub fn tip_slot(&self) -> Option<u64> {
        self.tip_slot
    }

    fn up(&self) -> usize {
        self.providers.values().filter(|p| matches!(p.state, Some(ConnState::Connected) | Some(ConnState::Subscribed))).count()
    }

    /// Recompute the state. Returns a gap that just closed (to persist).
    pub fn evaluate(&mut self, now: i64) -> Option<Gap> {
        if !self.live {
            return None;
        }
        if let (Some(last), Some(_)) = (self.last_eval_us, self.first_us) {
            let dt = (now - last).max(0);
            self.total_us += dt;
            if self.state == FeedState::Healthy {
                self.healthy_us += dt;
            }
        }
        self.last_eval_us = Some(now);
        let mut down = Vec::new();
        let mut degraded = Vec::new();
        let up = self.up();
        if self.providers.is_empty() {
            down.push("no provider configured or connected".to_string());
        } else if up == 0 {
            down.push("all providers disconnected".into());
        } else if up < self.providers.len() {
            // failover: data still flows through the remaining providers
            let names: Vec<_> = self.providers.iter().filter(|(_, p)| !matches!(p.state, Some(ConnState::Connected) | Some(ConnState::Subscribed))).map(|(k, _)| k.clone()).collect();
            self.reasons_note(format!("failover active: {} down", names.join(",")));
        }
        match self.last_event_us {
            None => down.push("no events received yet".into()),
            Some(l) => {
                let age = (now - l) as f64 / 1e3;
                if age > self.cfg.down_event_ms {
                    down.push(format!("no events for {:.0}s", age / 1e3));
                } else if age > self.cfg.stale_event_ms {
                    degraded.push(format!("no events for {:.0}s", age / 1e3));
                }
            }
        }
        if let Some(t) = self.last_tip_us {
            let age = (now - t) as f64 / 1e3;
            if age > self.cfg.stale_tip_ms {
                degraded.push(format!("chain tip stale for {:.1}s", age / 1e3));
            }
        }
        if let (Some(tip), Some(ev)) = (self.tip_slot, self.last_event_slot) {
            let lag = tip.saturating_sub(ev);
            if lag > self.cfg.max_slot_lag {
                degraded.push(format!("events {lag} slots behind tip"));
            }
        }
        let (state, reasons) = if !down.is_empty() {
            (FeedState::Down, down.into_iter().chain(degraded).collect())
        } else if !degraded.is_empty() {
            (FeedState::Degraded, degraded)
        } else {
            (FeedState::Healthy, vec![])
        };
        self.state = state;
        self.reasons = reasons;
        let mut closed = None;
        match (state, self.open_gap.as_mut()) {
            (FeedState::Healthy, Some(_)) => {
                let mut g = self.open_gap.take().unwrap();
                g.end_us = Some(now);
                self.gaps.push_back(g.clone());
                while self.gaps.len() > 200 {
                    self.gaps.pop_front();
                }
                closed = Some(g);
            }
            (FeedState::Healthy, None) => {}
            (s, Some(g)) => {
                // escalate DEGRADED → DOWN within the same gap
                if s == FeedState::Down {
                    g.state = FeedState::Down;
                }
            }
            (s, None) => {
                self.open_gap = Some(Gap { start_us: now, end_us: None, state: s, reason: self.reasons.join("; ") });
            }
        }
        closed
    }

    fn reasons_note(&mut self, _n: String) {}

    /// Close any open gap (shutdown) and return it for persistence.
    pub fn close(&mut self, now: i64) -> Option<Gap> {
        let mut g = self.open_gap.take()?;
        g.end_us = Some(now);
        g.reason = format!("{} (closed at shutdown)", g.reason);
        self.gaps.push_back(g.clone());
        Some(g)
    }

    pub fn status(&self, now: i64) -> FeedStatus {
        FeedStatus {
            state: self.state,
            reasons: self.reasons.clone(),
            providers_up: self.up(),
            providers_total: self.providers.len(),
            tip_slot: self.tip_slot,
            tip_age_ms: self.last_tip_us.map(|t| (now - t) as f64 / 1e3),
            last_event_age_ms: self.last_event_us.map(|t| (now - t) as f64 / 1e3),
            slot_lag: match (self.tip_slot, self.last_event_slot) {
                (Some(t), Some(e)) => Some(t as i64 - e as i64),
                _ => None,
            },
            coverage: if self.total_us > 0 { Some(self.healthy_us as f64 / self.total_us as f64) } else { None },
            open_gap: self.open_gap.clone(),
            recent_gaps: self.gaps.iter().rev().take(20).cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: i64 = 1_000_000;

    #[test]
    fn disconnected_feed_is_never_healthy() {
        let mut f = FeedMonitor::new(FeedConfig::default(), true);
        f.evaluate(0);
        assert_eq!(f.state(), FeedState::Down);
        f.on_connection("a", ConnState::Subscribed, "", 0);
        f.evaluate(S);
        assert_eq!(f.state(), FeedState::Down, "connected but no data yet");
        f.on_event("a", 100, 2 * S);
        f.evaluate(2 * S);
        assert_eq!(f.state(), FeedState::Healthy);
        let g = f.status(2 * S).recent_gaps;
        assert_eq!(g.len(), 1, "startup gap closed");
        f.on_connection("a", ConnState::Disconnected, "", 3 * S);
        f.evaluate(3 * S);
        assert_eq!(f.state(), FeedState::Down);
        assert!(f.status(3 * S).open_gap.is_some());
    }

    #[test]
    fn silence_degrades_then_downs_and_failover_keeps_healthy() {
        let mut f = FeedMonitor::new(FeedConfig::default(), true);
        f.on_connection("a", ConnState::Subscribed, "", 0);
        f.on_connection("b", ConnState::Subscribed, "", 0);
        f.on_event("a", 1, 0);
        f.evaluate(0);
        assert_eq!(f.state(), FeedState::Healthy);
        f.on_connection("a", ConnState::Disconnected, "", S);
        f.on_event("b", 2, S);
        f.evaluate(S);
        assert_eq!(f.state(), FeedState::Healthy, "b still delivers");
        f.evaluate(25 * S);
        assert_eq!(f.state(), FeedState::Degraded);
        f.evaluate(70 * S);
        assert_eq!(f.state(), FeedState::Down);
        f.on_event("b", 3, 71 * S);
        let closed = f.evaluate(71 * S).expect("gap closed on recovery");
        assert_eq!(closed.state, FeedState::Down);
        assert_eq!(closed.start_us, 25 * S);
        assert!(f.status(71 * S).coverage.unwrap() < 0.5);
    }

    #[test]
    fn stale_tip_and_slot_lag_degrade() {
        let mut f = FeedMonitor::new(FeedConfig::default(), true);
        f.on_connection("a", ConnState::Subscribed, "", 0);
        f.on_tip("a", 1_000, 0);
        f.on_event("a", 990, 0);
        f.evaluate(0);
        assert_eq!(f.state(), FeedState::Healthy);
        f.on_event("a", 991, 6 * S);
        f.evaluate(6 * S);
        assert_eq!(f.state(), FeedState::Degraded, "tip not advancing");
        f.on_tip("a", 2_000, 6 * S);
        f.evaluate(6 * S);
        assert_eq!(f.state(), FeedState::Degraded, "events far behind tip");
    }

    #[test]
    fn offline_source_reports_offline() {
        let mut f = FeedMonitor::new(FeedConfig::default(), false);
        assert!(f.evaluate(0).is_none());
        assert_eq!(f.state(), FeedState::Offline);
    }
}
