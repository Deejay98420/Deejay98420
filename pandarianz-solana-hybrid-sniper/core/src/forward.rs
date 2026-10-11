//! Forward evaluation: for accepted AND rejected candidates, a fixed-size probe is quoted at decision
//! time and re-quoted (executable sell, own impact) at later horizons. Outcomes are written as
//! immutable records strictly after the horizon passes; the decision path never reads them, so no
//! future information can leak into trading decisions.

use crate::quote::{venue_quote, Side, VenueFees};
use crate::state::Universe;
use crate::types::Pubkey;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub eval_id: String,
    pub mint: Pubkey,
    pub path: String,
    pub decision: String,
    pub reason_codes: Vec<String>,
    pub t0_us: i64,
    pub probe_lamports: u64,
    /// quote actually charged and tokens received at decision time (venue simulation)
    pub entry_in: u64,
    pub tokens: u64,
    pub horizons_s: Vec<u32>,
    #[serde(default)]
    pub expected_move_bps: Option<f64>,
    #[serde(default)]
    pub data_mode: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ForwardOutcome {
    pub eval_id: String,
    pub mint: String,
    pub path: String,
    pub decision: String,
    pub reason_codes: Vec<String>,
    pub horizon_s: u32,
    pub t0_us: i64,
    pub t_us: i64,
    pub probe_lamports: u64,
    pub entry_in: u64,
    pub exit_out: Option<u64>,
    /// exit_out - entry_in - fixed tx costs of two transactions (base scenario); None when unsellable
    pub net_lamports: Option<i64>,
    pub net_return: Option<f64>,
    pub note: String,
    #[serde(default)]
    pub expected_move_bps: Option<f64>,
    #[serde(default)]
    pub data_mode: String,
    /// "calibration" | "holdout" (fixed by decision time; see metrics::evidence_split)
    #[serde(default)]
    pub split: String,
}

pub struct ForwardTracker {
    pending: VecDeque<Probe>,
    pub cap: usize,
    pub dropped: u64,
}

impl Default for ForwardTracker {
    fn default() -> Self {
        Self { pending: VecDeque::new(), cap: 50_000, dropped: 0 }
    }
}

impl ForwardTracker {
    pub fn add(&mut self, p: Probe) {
        if self.pending.len() >= self.cap {
            self.pending.pop_front();
            self.dropped += 1;
        }
        self.pending.push_back(p);
    }
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Emit outcomes for every probe horizon that has elapsed at `now`.
    pub fn due(&mut self, now: i64, u: &Universe, fees: &VenueFees, fixed_round_trip_cost: u64) -> Vec<ForwardOutcome> {
        let mut out = Vec::new();
        for p in self.pending.iter_mut() {
            let mut keep = Vec::new();
            for h in p.horizons_s.drain(..) {
                let t = p.t0_us + h as i64 * 1_000_000;
                if t > now {
                    keep.push(h);
                    continue;
                }
                let q = u.tokens.get(&p.mint).map(|st| venue_quote(st, Side::Sell, p.tokens, p.tokens, fees, f64::MAX, now));
                let (exit_out, note) = match q {
                    Some(Ok(q)) => (Some(q.out_amount), String::new()),
                    Some(Err(e)) => (None, e.to_string()),
                    None => (None, "token no longer tracked".into()),
                };
                let net = exit_out.map(|o| o as i64 - p.entry_in as i64 - fixed_round_trip_cost as i64);
                out.push(ForwardOutcome {
                    eval_id: p.eval_id.clone(),
                    mint: p.mint.to_string(),
                    path: p.path.clone(),
                    decision: p.decision.clone(),
                    reason_codes: p.reason_codes.clone(),
                    horizon_s: h,
                    t0_us: p.t0_us,
                    t_us: now,
                    probe_lamports: p.probe_lamports,
                    entry_in: p.entry_in,
                    exit_out,
                    net_lamports: net,
                    net_return: net.map(|n| n as f64 / p.entry_in.max(1) as f64),
                    note,
                    expected_move_bps: p.expected_move_bps,
                    data_mode: p.data_mode.clone(),
                    split: crate::metrics::evidence_split(p.t0_us).to_string(),
                });
            }
            p.horizons_s = keep;
        }
        self.pending.retain(|p| !p.horizons_s.is_empty());
        out
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct ForwardBucket {
    pub n: u64,
    pub unsellable: u64,
    pub mean_net_return: Option<f64>,
    pub median_net_return: Option<f64>,
    pub positive_fraction: Option<f64>,
}

/// Summaries keyed by "decision@horizon" and "reject:<code>@horizon".
pub fn summarize(outcomes: &VecDeque<ForwardOutcome>) -> BTreeMap<String, ForwardBucket> {
    let mut groups: BTreeMap<String, (u64, Vec<f64>)> = BTreeMap::new();
    for o in outcomes {
        let mut keys = vec![format!("{}:{}@{}s", o.path, o.decision, o.horizon_s), format!("{}|{}:{}@{}s", o.split, o.path, o.decision, o.horizon_s)];
        if o.decision == "reject" {
            for c in &o.reason_codes {
                keys.push(format!("reject:{c}@{}s", o.horizon_s));
            }
        }
        for k in keys {
            let g = groups.entry(k).or_default();
            match o.net_return {
                Some(r) => g.1.push(r),
                None => g.0 += 1,
            }
        }
    }
    groups
        .into_iter()
        .map(|(k, (unsellable, mut v))| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let n = v.len() as u64 + unsellable;
            let b = ForwardBucket {
                n,
                unsellable,
                mean_net_return: if v.is_empty() { None } else { Some(v.iter().sum::<f64>() / v.len() as f64) },
                median_net_return: v.get(v.len() / 2).copied(),
                positive_fraction: if v.is_empty() { None } else { Some(v.iter().filter(|r| **r > 0.0).count() as f64 / v.len() as f64) },
            };
            (k, b)
        })
        .collect()
}

/// Calibration status of the heuristic expected-move model. Only LIVE outcomes in the CALIBRATION
/// split count; synthetic data never does.
pub fn expected_move_status(outcomes: &VecDeque<ForwardOutcome>, live: bool, min_n: u64) -> String {
    if !live {
        return "UNCALIBRATED — offline data never counts toward calibration".into();
    }
    let n = outcomes.iter().filter(|o| o.split == "calibration" && o.data_mode == "LIVE" && o.net_return.is_some()).count() as u64;
    if n < min_n {
        format!("UNCALIBRATED — {n} live calibration-split outcomes (need ≥ {min_n}); run `sniper calibrate` when enough exist")
    } else {
        format!("ENOUGH DATA TO CALIBRATE — {n} live calibration-split outcomes; review `sniper calibrate` (changes are never applied automatically)")
    }
}
