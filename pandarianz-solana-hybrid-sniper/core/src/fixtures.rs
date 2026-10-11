//! Deterministic SYNTHETIC fixture generator. Events are encoded with the same byte layouts the
//! decoders read, using a reserve-consistent curve/pool simulation. They are labelled FIXTURE and are
//! not market data: they exist to exercise the pipeline end-to-end, not to measure profitability.

use crate::decode::build;
use crate::decode::pumpfun::{PumpComplete, PumpCreate, PumpEvent, PumpMigration, PumpTrade};
use crate::decode::pumpswap::{CreatePool, Liquidity, PumpSwapEvent, Swap};
use crate::decode::raydium_v4::RayLog;
use crate::quote::math;
use crate::stream::{RawEvent, SlotStatus, StreamMsg};
use crate::types::{Commitment, DataMode, Pubkey, WSOL_MINT};
use sha2::{Digest, Sha256};

pub const T0_US: i64 = 1_760_000_000_000_000;
const SLOT0: u64 = 370_000_000;

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

pub fn wallet(label: &str) -> Pubkey {
    Pubkey::synthetic(&format!("wallet:{label}"))
}

pub fn sig(label: &str) -> String {
    let a = Sha256::digest(format!("sig1:{label}").as_bytes());
    let b = Sha256::digest(format!("sig2:{label}").as_bytes());
    let mut v = a.to_vec();
    v.extend_from_slice(&b);
    bs58::encode(v).into_string()
}

pub fn slot_at(t_us: i64) -> u64 {
    SLOT0 + ((t_us - T0_US).max(0) / 400_000) as u64
}

pub struct Timeline {
    pub msgs: Vec<StreamMsg>,
    counter: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    pub fn new() -> Self {
        Self { msgs: Vec::new(), counter: 0 }
    }
    pub fn push_tx(&mut self, t_us: i64, logs: Vec<String>, inner: Vec<crate::stream::InnerIx>, err: Option<String>) -> String {
        self.counter += 1;
        let s = sig(&format!("tx-{}-{t_us}", self.counter));
        self.msgs.push(StreamMsg::Event(RawEvent {
            provider: "fixture".into(),
            data_mode: DataMode::Fixture,
            slot: slot_at(t_us),
            signature: s.clone(),
            commitment: Commitment::Confirmed,
            err,
            logs,
            inner_ix: inner,
            block_time: Some(t_us / 1_000_000),
            recv_us: t_us,
        }));
        s
    }
    pub fn pump(&mut self, t_us: i64, evs: Vec<PumpEvent>) -> String {
        self.push_tx(t_us, build::pump_logs(&evs), vec![], None)
    }
    pub fn pumpswap(&mut self, t_us: i64, evs: Vec<PumpSwapEvent>) -> String {
        let (logs, inner) = build::pumpswap_inner(&evs);
        self.push_tx(t_us, logs, inner, None)
    }
    pub fn sorted(mut self) -> Vec<StreamMsg> {
        self.msgs.sort_by_key(|m| m.recv_us());
        self.msgs
    }
}

/// Reserve-consistent Pump.fun curve simulation.
pub struct Curve {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub creator: Pubkey,
    pub vs: u64,
    pub vt: u64,
    pub rs: u64,
    pub rt: u64,
    pub fee_bps: u64,
    pub complete: bool,
}

impl Curve {
    pub fn new(label: &str) -> Self {
        Self {
            mint: Pubkey::synthetic(&format!("mint:{label}")),
            bonding_curve: Pubkey::synthetic(&format!("curve:{label}")),
            creator: wallet(&format!("creator-{label}")),
            vs: 30_000_000_000,
            vt: 1_073_000_000_000_000,
            rs: 0,
            rt: 793_100_000_000_000,
            fee_bps: 100,
            complete: false,
        }
    }
    pub fn create(&self, name: &str, symbol: &str, t_us: i64) -> PumpEvent {
        PumpEvent::Create(PumpCreate {
            name: name.into(),
            symbol: symbol.into(),
            uri: format!("https://example.invalid/{symbol}.json"),
            mint: self.mint,
            bonding_curve: self.bonding_curve,
            user: self.creator,
            creator: Some(self.creator),
            timestamp: Some(t_us / 1_000_000),
            virtual_token_reserves: Some(self.vt),
            virtual_sol_reserves: Some(self.vs),
            real_token_reserves: Some(self.rt),
            token_total_supply: Some(1_000_000_000_000_000),
        })
    }
    fn trade_ev(&self, user: Pubkey, sol: u64, tokens: u64, is_buy: bool, fee: u64, t_us: i64) -> PumpEvent {
        PumpEvent::Trade(PumpTrade {
            mint: self.mint,
            sol_amount: sol,
            token_amount: tokens,
            is_buy,
            user,
            timestamp: t_us / 1_000_000,
            virtual_sol_reserves: self.vs,
            virtual_token_reserves: self.vt,
            real_sol_reserves: Some(self.rs),
            real_token_reserves: Some(self.rt),
            fee_recipient: Some(Pubkey::synthetic("fee-recipient")),
            fee_basis_points: Some(95),
            fee: Some(fee),
            creator: Some(self.creator),
            creator_fee_basis_points: Some(5),
            creator_fee: Some(0),
        })
    }
    /// Buy with total spend (lamports). Returns None if the curve is complete.
    pub fn buy(&mut self, user: Pubkey, spend: u64, t_us: i64) -> Option<PumpEvent> {
        if self.complete {
            return None;
        }
        let r = math::curve_buy(self.vs, self.vt, self.rt, spend, self.fee_bps)?;
        let net = r.amount_in - r.fee;
        self.vs += net;
        self.vt -= r.amount_out;
        self.rs += net;
        self.rt -= r.amount_out;
        if self.rt == 0 {
            self.complete = true;
        }
        Some(self.trade_ev(user, net, r.amount_out, true, r.fee, t_us))
    }
    pub fn sell(&mut self, user: Pubkey, tokens: u64, t_us: i64) -> Option<PumpEvent> {
        if self.complete {
            return None;
        }
        let r = math::curve_sell(self.vs, self.vt, self.rs, tokens, self.fee_bps)?;
        let gross = r.amount_out + r.fee;
        self.vs -= gross;
        self.vt += tokens;
        self.rs -= gross;
        self.rt += tokens;
        Some(self.trade_ev(user, r.amount_out, tokens, false, r.fee, t_us))
    }
    pub fn complete_ev(&self, t_us: i64) -> PumpEvent {
        PumpEvent::Complete(PumpComplete { user: wallet("completer"), mint: self.mint, bonding_curve: self.bonding_curve, timestamp: t_us / 1_000_000 })
    }
}


/// Reserve-consistent PumpSwap pool simulation (quote = WSOL).
pub struct Pool {
    pub pool: Pubkey,
    pub mint: Pubkey,
    pub base: u64,
    pub quote: u64,
    pub lp_bps: u64,
    pub protocol_bps: u64,
}

impl Pool {
    pub fn new(label: &str, mint: Pubkey, base: u64, quote: u64) -> Self {
        Self { pool: Pubkey::synthetic(&format!("pool:{label}")), mint, base, quote, lp_bps: 20, protocol_bps: 5 }
    }
    pub fn create_ev(&self, creator: Pubkey, t_us: i64) -> PumpSwapEvent {
        PumpSwapEvent::CreatePool(CreatePool {
            timestamp: t_us / 1_000_000,
            index: 0,
            creator,
            base_mint: self.mint,
            quote_mint: Pubkey::from_b58(WSOL_MINT).unwrap(),
            base_mint_decimals: 6,
            quote_mint_decimals: 9,
            base_amount_in: self.base,
            quote_amount_in: self.quote,
            pool_base_amount: self.base,
            pool_quote_amount: self.quote,
            minimum_liquidity: 100,
            initial_liquidity: 1_000_000,
            lp_token_amount_out: 1_000_000,
            pool_bump: 255,
            pool: self.pool,
            lp_mint: Pubkey::synthetic("lp-mint"),
        })
    }
    pub fn buy(&mut self, user: Pubkey, spend: u64, t_us: i64) -> PumpSwapEvent {
        let total = self.lp_bps + self.protocol_bps;
        let net = (spend as u128 * 10_000 / (10_000 + total as u128)) as u64;
        let lp_fee = net * self.lp_bps / 10_000;
        let protocol_fee = spend - net - lp_fee;
        let out = (self.base as u128 * net as u128 / (self.quote as u128 + net as u128)) as u64;
        let ev = Swap {
            is_buy: true,
            timestamp: t_us / 1_000_000,
            base_amount: out,
            quote_amount_user: spend,
            pool_base_token_reserves: self.base,
            pool_quote_token_reserves: self.quote,
            quote_amount_pool: net + lp_fee,
            lp_fee,
            protocol_fee,
            pool: self.pool,
            user,
        };
        self.base -= out;
        self.quote += net + lp_fee;
        PumpSwapEvent::Swap(ev)
    }
    pub fn sell(&mut self, user: Pubkey, tokens: u64, t_us: i64) -> PumpSwapEvent {
        let gross = (self.quote as u128 * tokens as u128 / (self.base as u128 + tokens as u128)) as u64;
        let lp_fee = gross * self.lp_bps / 10_000;
        let protocol_fee = gross * self.protocol_bps / 10_000;
        let ev = Swap {
            is_buy: false,
            timestamp: t_us / 1_000_000,
            base_amount: tokens,
            quote_amount_user: gross - lp_fee - protocol_fee,
            pool_base_token_reserves: self.base,
            pool_quote_token_reserves: self.quote,
            quote_amount_pool: gross - lp_fee,
            lp_fee,
            protocol_fee,
            pool: self.pool,
            user,
        };
        self.base += tokens;
        self.quote -= gross - lp_fee;
        PumpSwapEvent::Swap(ev)
    }
    pub fn withdraw(&mut self, user: Pubkey, fraction: f64, t_us: i64) -> PumpSwapEvent {
        let b = (self.base as f64 * fraction) as u64;
        let q = (self.quote as f64 * fraction) as u64;
        let ev = Liquidity {
            is_deposit: false,
            timestamp: t_us / 1_000_000,
            lp_token_amount: 1,
            pool_base_token_reserves: self.base,
            pool_quote_token_reserves: self.quote,
            base_amount: b,
            quote_amount: q,
            lp_mint_supply: 1_000_000,
            pool: self.pool,
            user,
        };
        self.base -= b;
        self.quote -= q;
        PumpSwapEvent::Liquidity(ev)
    }
}

const S: i64 = 1_000_000;
const SOL: f64 = 1e9;

fn lam(x: f64) -> u64 {
    (x * SOL) as u64
}

/// Organic launch: broad early buying, non-creator sells, then flow turns and sells dominate.
pub fn scenario_organic(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut c = Curve::new(label);
    let creator = c.creator;
    let create = c.create(&format!("{label} Coin"), label, start);
    let first = c.buy(creator, lam(0.3), start).unwrap();
    tl.pump(start, vec![create, first]);
    let mut holders: Vec<(Pubkey, u64)> = Vec::new();
    // phase 1: 0-75s strong distributed buying with some profit-taking
    let mut t = start + 2 * S;
    let mut n = 0;
    while t < start + 75 * S {
        n += 1;
        if rng.unit() < 0.80 || holders.is_empty() {
            let u = wallet(&format!("{label}-buyer-{n}"));
            if let Some(PumpEvent::Trade(tr)) = c.buy(u, lam(rng.range(0.15, 0.9)), t) {
                holders.push((u, tr.token_amount));
                tl.pump(t, vec![PumpEvent::Trade(tr)]);
            }
        } else {
            let i = (rng.next() as usize) % holders.len();
            let (u, amt) = holders.swap_remove(i);
            if let Some(e) = c.sell(u, amt / 2, t) {
                tl.pump(t, vec![e]);
                holders.push((u, amt - amt / 2));
            }
        }
        t += (rng.range(0.6, 2.2) * S as f64) as i64;
    }
    // phase 2: 75-200s distribution — holders sell, few buys
    while t < start + 200 * S && !holders.is_empty() {
        n += 1;
        if rng.unit() < 0.22 {
            let u = wallet(&format!("{label}-late-{n}"));
            if let Some(e) = c.buy(u, lam(rng.range(0.05, 0.3)), t) {
                tl.pump(t, vec![e]);
            }
        } else {
            let i = (rng.next() as usize) % holders.len();
            let (u, amt) = holders.swap_remove(i);
            if let Some(e) = c.sell(u, amt, t) {
                tl.pump(t, vec![e]);
            }
        }
        t += (rng.range(0.8, 2.5) * S as f64) as i64;
    }
}

/// Launch with good early flow, then the creator dumps its whole bag.
pub fn scenario_creator_rug(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut c = Curve::new(label);
    let creator = c.creator;
    let create = c.create(&format!("{label} Token"), label, start);
    let PumpEvent::Trade(first) = c.buy(creator, lam(0.4), start).unwrap() else { unreachable!() };
    let creator_tokens = first.token_amount;
    tl.pump(start, vec![create, PumpEvent::Trade(first)]);
    let mut t = start + 2 * S;
    let mut n = 0;
    let mut holders = Vec::new();
    while t < start + 45 * S {
        n += 1;
        let u = wallet(&format!("{label}-b-{n}"));
        if rng.unit() < 0.85 || holders.is_empty() {
            if let Some(PumpEvent::Trade(tr)) = c.buy(u, lam(rng.range(0.2, 1.0)), t) {
                holders.push((u, tr.token_amount));
                tl.pump(t, vec![PumpEvent::Trade(tr)]);
            }
        } else {
            let (hu, amt) = holders.pop().unwrap();
            if let Some(e) = c.sell(hu, amt / 3, t) {
                tl.pump(t, vec![e]);
            }
        }
        t += (rng.range(0.5, 1.6) * S as f64) as i64;
    }
    // rug: creator sells everything in one go
    if let Some(e) = c.sell(creator, creator_tokens, t) {
        tl.pump(t, vec![e]);
    }
    t += S;
    for (u, amt) in holders.into_iter().take(15) {
        if let Some(e) = c.sell(u, amt, t) {
            tl.pump(t, vec![e]);
        }
        t += 700_000;
    }
}

/// Bundled launch: many wallets buy in the creation slot.
pub fn scenario_bundled(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut c = Curve::new(label);
    let creator = c.creator;
    let create = c.create(&format!("{label} Bundle"), label, start);
    let mut evs = vec![create, c.buy(creator, lam(0.5), start).unwrap()];
    for i in 0..12 {
        evs.push(c.buy(wallet(&format!("{label}-bundle-{i}")), lam(rng.range(0.8, 1.5)), start).unwrap());
    }
    tl.pump(start, evs);
    let mut t = start + 2 * S;
    for n in 0..40 {
        let u = wallet(&format!("{label}-r-{n}"));
        if let Some(e) = c.buy(u, lam(rng.range(0.1, 0.6)), t) {
            tl.pump(t, vec![e]);
        }
        if n % 5 == 4 {
            if let Some(e) = c.sell(wallet(&format!("{label}-r-{}", n - 2)), 1_000_000_000, t + 100_000) {
                tl.pump(t + 100_000, vec![e]);
            }
        }
        t += (rng.range(0.5, 1.5) * S as f64) as i64;
    }
}

/// Launch that completes its curve, migrates to PumpSwap, and keeps trading there.
pub fn scenario_migration(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut c = Curve::new(label);
    let creator = c.creator;
    let create = c.create(&format!("{label} Moon"), label, start);
    let first = c.buy(creator, lam(0.2), start).unwrap();
    tl.pump(start, vec![create, first]);
    let mut t = start + 2 * S;
    let mut n = 0;
    while !c.complete && t < start + 400 * S {
        n += 1;
        let u = wallet(&format!("{label}-m-{n}"));
        let r = rng.unit();
        if r < 0.85 {
            let size = if t < start + 60 * S { rng.range(0.2, 1.2) } else { rng.range(1.0, 3.5) };
            if let Some(e) = c.buy(u, lam(size), t) {
                tl.pump(t, vec![e]);
            }
        } else if let Some(e) = c.sell(wallet(&format!("{label}-m-{}", n.max(2) - 1)), 2_000_000_000_000, t) {
            tl.pump(t, vec![e]);
        }
        t += (rng.range(0.4, 1.4) * S as f64) as i64;
    }
    tl.pump(t, vec![c.complete_ev(t)]);
    // ~25s with no tracked route while migration happens
    t += 25 * S;
    let mut pool = Pool::new(label, c.mint, 206_900_000_000_000, 79_000_000_000);
    let migr = PumpEvent::Migration(PumpMigration {
        user: wallet("migrator"),
        mint: c.mint,
        mint_amount: pool.base,
        sol_amount: pool.quote,
        pool_migration_fee: 15_000_000,
        bonding_curve: c.bonding_curve,
        timestamp: t / 1_000_000,
        pool: pool.pool,
    });
    tl.pump(t, vec![migr]);
    t += 400_000;
    let create_pool = pool.create_ev(creator, t);
    tl.pumpswap(t, vec![create_pool]);
    t += S;
    for k in 0..80 {
        let u = wallet(&format!("{label}-amm-{k}"));
        let e = if rng.unit() < 0.55 { pool.buy(u, lam(rng.range(0.3, 2.0)), t) } else { pool.sell(u, (rng.range(1.0, 8.0) * 1e12) as u64, t) };
        tl.pumpswap(t, vec![e]);
        t += (rng.range(0.8, 2.0) * S as f64) as i64;
    }
}

/// Already-trading PumpSwap token (pool seen earlier), steady then accelerating flow, then LP pulled.
pub fn scenario_momentum_pool(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mint = Pubkey::synthetic(&format!("mint:{label}"));
    let mut pool = Pool::new(label, mint, 300_000_000_000_000, 60_000_000_000);
    let creator = wallet(&format!("creator-{label}"));
    tl.pumpswap(start, vec![pool.create_ev(creator, start)]);
    let mut t = start + 3 * S;
    let mut k = 0;
    // 0-90s: balanced chop
    while t < start + 90 * S {
        k += 1;
        let u = wallet(&format!("{label}-c-{k}"));
        let e = if rng.unit() < 0.5 { pool.buy(u, lam(rng.range(0.2, 1.0)), t) } else { pool.sell(u, (rng.range(1.0, 4.0) * 1e12) as u64, t) };
        tl.pumpswap(t, vec![e]);
        t += (rng.range(1.0, 3.0) * S as f64) as i64;
    }
    // 90-180s: accelerating broad buying
    while t < start + 180 * S {
        k += 1;
        let u = wallet(&format!("{label}-a-{k}"));
        let e = if rng.unit() < 0.82 { pool.buy(u, lam(rng.range(0.4, 2.0)), t) } else { pool.sell(u, (rng.range(1.0, 3.0) * 1e12) as u64, t) };
        tl.pumpswap(t, vec![e]);
        t += (rng.range(0.5, 1.5) * S as f64) as i64;
    }
    // LP removes 70% of liquidity
    tl.pumpswap(t, vec![pool.withdraw(creator, 0.7, t)]);
    t += S;
    for j in 0..20 {
        let u = wallet(&format!("{label}-p-{j}"));
        let e = pool.sell(u, (rng.range(1.0, 5.0) * 1e12) as u64, t);
        tl.pumpswap(t, vec![e]);
        t += S;
    }
}

/// Curve token we never saw created (discovered mid-life): momentum scanner path.
pub fn scenario_discovered_curve(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut c = Curve::new(label);
    // pre-history not in the stream: advance the curve silently
    for i in 0..30 {
        let _ = c.buy(wallet(&format!("{label}-pre-{i}")), lam(0.5), start);
    }
    let mut t = start;
    let mut k = 0;
    while t < start + 70 * S {
        k += 1;
        let u = wallet(&format!("{label}-d-{k}"));
        let e = if rng.unit() < 0.5 { c.buy(u, lam(rng.range(0.1, 0.6)), t) } else { c.sell(wallet(&format!("{label}-pre-{}", k % 30)), 3_000_000_000_000, t) };
        if let Some(e) = e {
            tl.pump(t, vec![e]);
        }
        t += (rng.range(1.0, 2.5) * S as f64) as i64;
    }
    while t < start + 160 * S && !c.complete {
        k += 1;
        let u = wallet(&format!("{label}-e-{k}"));
        let e = if rng.unit() < 0.8 { c.buy(u, lam(rng.range(0.3, 1.5)), t) } else { c.sell(wallet(&format!("{label}-pre-{}", k % 30)), 2_000_000_000_000, t) };
        if let Some(e) = e {
            tl.pump(t, vec![e]);
        }
        t += (rng.range(0.5, 1.4) * S as f64) as i64;
    }
}

/// Fade: launch with convincing early flow that reverses right after a typical entry time.
pub fn scenario_fade(tl: &mut Timeline, start: i64, label: &str, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut c = Curve::new(label);
    let creator = c.creator;
    let create = c.create(&format!("{label} Fade"), label, start);
    let first = c.buy(creator, lam(0.2), start).unwrap();
    tl.pump(start, vec![create, first]);
    let mut holders = Vec::new();
    let mut t = start + S;
    let mut n = 0;
    while t < start + 14 * S {
        n += 1;
        let u = wallet(&format!("{label}-f-{n}"));
        if n % 6 == 0 && !holders.is_empty() {
            let (hu, amt): (Pubkey, u64) = holders[0];
            if let Some(e) = c.sell(hu, amt / 4, t) {
                tl.pump(t, vec![e]);
            }
        } else if let Some(PumpEvent::Trade(tr)) = c.buy(u, lam(rng.range(0.4, 1.2)), t) {
            holders.push((u, tr.token_amount));
            tl.pump(t, vec![PumpEvent::Trade(tr)]);
        }
        t += (rng.range(0.3, 0.8) * S as f64) as i64;
    }
    // reversal: early buyers dump into late entries
    for (u, amt) in holders {
        if let Some(e) = c.sell(u, amt, t) {
            tl.pump(t, vec![e]);
        }
        t += (rng.range(0.4, 1.0) * S as f64) as i64;
    }
}

/// Stream integrity cases: duplicate delivery, failed tx, rollback of a dead slot, raydium logs.
pub fn scenario_integrity(tl: &mut Timeline, start: i64, label: &str) {
    let mut c = Curve::new(label);
    let creator = c.creator;
    let create = c.create(label, label, start);
    let b = c.buy(creator, lam(0.3), start).unwrap();
    tl.pump(start, vec![create, b]);
    // duplicate delivery of the same transaction (e.g. two providers)
    let t1 = start + 2 * S;
    let e = c.buy(wallet("dup-buyer"), lam(0.5), t1).unwrap();
    let s1 = tl.pump(t1, vec![e.clone()]);
    if let Some(StreamMsg::Event(last)) = tl.msgs.last().cloned() {
        let mut dup = last;
        dup.provider = "fixture-redundant".into();
        dup.recv_us += 30_000;
        assert_eq!(dup.signature, s1);
        tl.msgs.push(StreamMsg::Event(dup));
    }
    // failed transaction: must not change state
    let t2 = start + 3 * S;
    tl.push_tx(t2, build::pump_logs(&[PumpEvent::Trade(match c.buy(wallet("failed"), lam(5.0), t2).unwrap() {
        PumpEvent::Trade(t) => t,
        _ => unreachable!(),
    })]), vec![], Some("{\"InstructionError\":[0,{\"Custom\":6002}]}".into()));
    // undo the simulated failed buy in our curve model
    let mut c2 = Curve::new(label);
    c2.buy(creator, lam(0.3), start);
    c2.buy(wallet("dup-buyer"), lam(0.5), t1);
    // events in a slot that later dies
    let t3 = start + 5 * S;
    let mut fork = Curve { ..Curve::new(label) };
    fork.vs = c2.vs;
    fork.vt = c2.vt;
    fork.rs = c2.rs;
    fork.rt = c2.rt;
    let fe = fork.buy(wallet("fork-buyer"), lam(3.0), t3).unwrap();
    tl.pump(t3, vec![fe]);
    tl.msgs.push(StreamMsg::Slot { slot: slot_at(t3), status: SlotStatus::Dead, recv_us: t3 + 800_000 });
    // raydium v4 logs (decode-only)
    tl.push_tx(start + 6 * S, build::raydium_logs(&RayLog::SwapBaseIn { amount_in: 1_000_000, minimum_out: 1, direction: 2, user_source: 1_000_000, pool_coin: 5_000_000_000, pool_pc: 9_000_000_000, out_amount: 1_700_000 }), vec![], None);
}

/// The full mixed session used by `sniper run --mode fixture` and the end-to-end tests.
pub fn hybrid_session() -> Vec<StreamMsg> {
    let mut tl = Timeline::new();
    scenario_organic(&mut tl, T0_US, "ORGA", 11);
    scenario_creator_rug(&mut tl, T0_US + 20 * S, "RUGB", 22);
    scenario_bundled(&mut tl, T0_US + 35 * S, "BNDL", 33);
    scenario_migration(&mut tl, T0_US + 50 * S, "MIGR", 44);
    scenario_momentum_pool(&mut tl, T0_US + 10 * S, "MOMO", 55);
    scenario_discovered_curve(&mut tl, T0_US + 120 * S, "DISC", 66);
    scenario_integrity(&mut tl, T0_US + 5 * S, "INTG");
    scenario_fade(&mut tl, T0_US + 260 * S, "FADE", 77);
    // integrity scenario relies on insertion order for the dead-slot marker; stable sort keeps it
    tl.sorted()
}

pub fn to_jsonl(msgs: &[StreamMsg]) -> String {
    let mut s = String::from("# SYNTHETIC FIXTURE — generated by `sniper gen-fixtures`. Not market data.\n");
    for m in msgs {
        s.push_str(&serde_json::to_string(m).unwrap());
        s.push('\n');
    }
    s
}
