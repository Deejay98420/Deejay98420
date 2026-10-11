//! Flow / concentration / suspicious-activity features computed from observed trades.
//! Holdings are reconstructed from venue trades only (transfers are invisible), so concentration is
//! flagged `complete=false` for tokens we did not observe from creation.

use super::{Origin, Phase, TokenState};
use crate::types::Pubkey;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Window {
    pub secs: u32,
    pub buys: u32,
    pub sells: u32,
    pub buy_lamports: u64,
    pub sell_lamports: u64,
    pub unique_buyers: u32,
    pub unique_sellers: u32,
    /// price change over the window, fraction (0.10 = +10%); None if no trade before the window
    pub price_change: Option<f64>,
}

impl Window {
    pub fn net_flow_lamports(&self) -> i64 {
        self.buy_lamports as i64 - self.sell_lamports as i64
    }
    pub fn buy_ratio(&self) -> f64 {
        let t = self.buy_lamports + self.sell_lamports;
        if t == 0 { 0.5 } else { self.buy_lamports as f64 / t as f64 }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Features {
    pub mint: Pubkey,
    pub origin: Origin,
    pub phase: Phase,
    pub age_s: f64,
    pub observed_s: f64,
    pub liquidity_lamports: u64,
    pub peak_liquidity_lamports: u64,
    pub marginal_price: Option<f64>,
    pub w10: Window,
    pub w30: Window,
    pub w60: Window,
    pub w300: Window,
    pub unique_buyers_total: u32,
    pub trades_total: u32,
    pub top1_share: f64,
    pub top10_share: f64,
    pub concentration_complete: bool,
    pub creator_share: f64,
    pub creator_sold_fraction: f64,
    pub buyers_in_create_slot: u32,
    /// fraction of recent wallets that both bought and sold within 60s (wash-like round trips)
    pub round_trip_wallet_fraction: f64,
    /// stdev of per-trade log returns over 60s
    pub volatility_60s: f64,
    /// drawdown of marginal price from its 60s peak (fraction, >=0)
    pub drawdown_from_peak_60s: f64,
    pub curve_progress: Option<f64>,
    pub ms_since_last_event: f64,
    pub non_creator_sell_seen: bool,
    pub liquidity_removed_lamports: u64,
}

fn window(st: &TokenState, now_us: i64, secs: u32) -> Window {
    let start = now_us - secs as i64 * 1_000_000;
    let mut w = Window { secs, ..Default::default() };
    let mut b: HashSet<Pubkey> = HashSet::new();
    let mut s: HashSet<Pubkey> = HashSet::new();
    let mut first_price: Option<f64> = None;
    let mut last_price: Option<f64> = None;
    // price immediately before the window starts
    let mut before: Option<f64> = None;
    for t in st.trades.iter() {
        if t.ts_us < start {
            before = Some(t.price_after);
            continue;
        }
        if t.ts_us > now_us {
            break;
        }
        if first_price.is_none() {
            first_price = Some(t.price_after);
        }
        last_price = Some(t.price_after);
        if t.is_buy {
            w.buys += 1;
            w.buy_lamports += t.sol;
            b.insert(t.user);
        } else {
            w.sells += 1;
            w.sell_lamports += t.sol;
            s.insert(t.user);
        }
    }
    w.unique_buyers = b.len() as u32;
    w.unique_sellers = s.len() as u32;
    if let (Some(p0), Some(p1)) = (before, last_price) {
        if p0 > 0.0 {
            w.price_change = Some(p1 / p0 - 1.0);
        }
    }
    w
}

pub fn compute(st: &TokenState, now_us: i64) -> Features {
    let created = st.created_us.unwrap_or(st.first_seen_us);
    let circulating: i128 = st.holdings.values().filter(|v| **v > 0).sum();
    let mut hs: Vec<i128> = st.holdings.values().copied().filter(|v| *v > 0).collect();
    hs.sort_unstable_by(|a, b| b.cmp(a));
    let supply = st.total_supply.max(1) as f64;
    let top1 = hs.first().copied().unwrap_or(0) as f64 / supply;
    let top10 = hs.iter().take(10).sum::<i128>() as f64 / supply;
    let creator_hold = st.creator.and_then(|c| st.holdings.get(&c).copied()).unwrap_or(0).max(0) as f64;
    let _ = circulating;

    // round trips & volatility over 60s
    let start60 = now_us - 60_000_000;
    let mut per_wallet: HashMap<Pubkey, (bool, bool)> = HashMap::new();
    let mut rets: Vec<f64> = Vec::new();
    let mut prev: Option<f64> = None;
    let mut peak: f64 = 0.0;
    let mut last: f64 = 0.0;
    for t in st.trades.iter().filter(|t| t.ts_us >= start60 && t.ts_us <= now_us) {
        let e = per_wallet.entry(t.user).or_default();
        if t.is_buy { e.0 = true } else { e.1 = true }
        if let Some(p) = prev {
            if p > 0.0 && t.price_after > 0.0 {
                rets.push((t.price_after / p).ln());
            }
        }
        prev = Some(t.price_after);
        peak = peak.max(t.price_after);
        last = t.price_after;
    }
    let rt = if per_wallet.is_empty() {
        0.0
    } else {
        per_wallet.values().filter(|(b, s)| *b && *s).count() as f64 / per_wallet.len() as f64
    };
    let vol = if rets.len() >= 2 {
        let m = rets.iter().sum::<f64>() / rets.len() as f64;
        (rets.iter().map(|r| (r - m).powi(2)).sum::<f64>() / (rets.len() - 1) as f64).sqrt()
    } else {
        0.0
    };
    let dd = if peak > 0.0 { (1.0 - last / peak).max(0.0) } else { 0.0 };
    let curve_progress = st.curve.filter(|_| st.phase == Phase::BondingCurve).map(|c| {
        let initial_real_token = 793_100_000_000_000f64;
        (1.0 - c.real_token as f64 / initial_real_token).clamp(0.0, 1.0)
    });

    Features {
        mint: st.mint,
        origin: st.origin,
        phase: st.phase,
        age_s: (now_us - created) as f64 / 1e6,
        observed_s: (now_us - st.first_seen_us) as f64 / 1e6,
        liquidity_lamports: st.liquidity_lamports(),
        peak_liquidity_lamports: st.peak_liquidity_lamports,
        marginal_price: st.marginal_price(),
        w10: window(st, now_us, 10),
        w30: window(st, now_us, 30),
        w60: window(st, now_us, 60),
        w300: window(st, now_us, 300),
        unique_buyers_total: st.buyers.len() as u32,
        trades_total: st.trades.len() as u32,
        top1_share: top1,
        top10_share: top10,
        concentration_complete: st.origin == Origin::Launch,
        creator_share: creator_hold / supply,
        creator_sold_fraction: if st.creator_bought > 0 {
            st.creator_sold as f64 / st.creator_bought as f64
        } else if st.creator_sold > 0 {
            1.0
        } else {
            0.0
        },
        buyers_in_create_slot: st.buyers_in_create_slot,
        round_trip_wallet_fraction: rt,
        volatility_60s: vol,
        drawdown_from_peak_60s: dd,
        curve_progress,
        ms_since_last_event: (now_us - st.last_update_us) as f64 / 1e3,
        non_creator_sell_seen: st.non_creator_sell_seen,
        liquidity_removed_lamports: st.liquidity_removed_lamports,
    }
}
