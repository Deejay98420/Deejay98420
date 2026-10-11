//! Transaction-evidence verification of decoded events.
//!
//! Decoded event fields are cross-checked against INDEPENDENT evidence in the same transaction:
//! `meta.preTokenBalances/postTokenBalances` (token deltas per owner+mint), `meta.pre/postBalances`
//! (lamports), and arithmetic consistency of fees. Hard checks must pass for a layout to count as
//! mainnet-verified; soft checks are informative (other instructions in a transaction can move SOL).

use crate::decode::pumpfun::PumpEvent;
use crate::decode::pumpswap::PumpSwapEvent;
use crate::decode::{decode_raw, VenueEvent};
use crate::stream::ws::parse_tx_object;
use crate::types::{Commitment, WSOL_MINT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub event_index: usize,
    pub name: String,
    pub ok: bool,
    pub hard: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TxVerification {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    /// launch | trade_buy | trade_sell | curve_complete | migration | pool_create | pool_swap_buy | pool_swap_sell | liquidity
    pub classification: Vec<String>,
    /// "venue/Event" per decoded event, aligned with event_index
    pub items: Vec<String>,
    pub checks: Vec<Check>,
    pub passed: bool,
    pub decode_warnings: Vec<String>,
}

/// (owner, mint) → signed raw token delta
fn token_deltas(meta: &Value) -> HashMap<(String, String), i128> {
    let mut m: HashMap<(String, String), i128> = HashMap::new();
    let amt = |x: &Value| x["uiTokenAmount"]["amount"].as_str().and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
    let key = |x: &Value| (x["owner"].as_str().unwrap_or("").to_string(), x["mint"].as_str().unwrap_or("").to_string());
    for x in meta["preTokenBalances"].as_array().into_iter().flatten() {
        *m.entry(key(x)).or_default() -= amt(x);
    }
    for x in meta["postTokenBalances"].as_array().into_iter().flatten() {
        *m.entry(key(x)).or_default() += amt(x);
    }
    m
}

fn account_keys(tx: &Value) -> Vec<String> {
    let mut keys: Vec<String> = tx["transaction"]["message"]["accountKeys"]
        .as_array()
        .map(|a| a.iter().filter_map(|k| k.get("pubkey").and_then(|p| p.as_str()).or_else(|| k.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    for t in ["writable", "readonly"] {
        for k in tx["meta"]["loadedAddresses"][t].as_array().into_iter().flatten() {
            if let Some(s) = k.as_str() {
                keys.push(s.to_string());
            }
        }
    }
    keys
}

fn lamport_delta(tx: &Value, keys: &[String], who: &str) -> Option<i128> {
    let i = keys.iter().position(|k| k == who)?;
    let pre = tx["meta"]["preBalances"].get(i)?.as_i64()? as i128;
    let post = tx["meta"]["postBalances"].get(i)?.as_i64()? as i128;
    Some(post - pre)
}

fn ceil_fee(amount: u64, bps: u64) -> u64 {
    (amount as u128 * bps as u128).div_ceil(10_000) as u64
}

/// Verify one getTransaction (jsonParsed) result.
pub fn verify_tx(rpc_result: &Value, signature: &str) -> TxVerification {
    let mut v = TxVerification {
        signature: signature.to_string(),
        slot: rpc_result["slot"].as_u64().unwrap_or(0),
        block_time: rpc_result["blockTime"].as_i64(),
        ..Default::default()
    };
    let Some(raw) = parse_tx_object(rpc_result, Some(signature), v.slot, "verify", Commitment::Confirmed, 0) else {
        v.checks.push(Check { event_index: 0, name: "parse".into(), ok: false, hard: true, detail: "unparsable transaction".into() });
        return v;
    };
    let out = decode_raw(&raw);
    v.decode_warnings = out.warnings.clone();
    let meta = &rpc_result["meta"];
    let deltas = token_deltas(meta);
    let keys = account_keys(rpc_result);
    let tx_fee = meta["fee"].as_u64().unwrap_or(0) as i128;
    let wsol = WSOL_MINT.to_string();
    // aggregate expected user token deltas across all trade events in the tx (multiple fills per tx)
    let mut expected_user: HashMap<(String, String), i128> = HashMap::new();
    for d in &out.events {
        if let VenueEvent::Pump(PumpEvent::Trade(t)) = &d.event {
            let s = if t.is_buy { t.token_amount as i128 } else { -(t.token_amount as i128) };
            *expected_user.entry((t.user.to_string(), t.mint.to_string())).or_default() += s;
        }
    }
    let mut check = |i: usize, name: &str, ok: bool, hard: bool, detail: String| v.checks.push(Check { event_index: i, name: name.into(), ok, hard, detail });
    for (i, d) in out.events.iter().enumerate() {
        match &d.event {
            VenueEvent::Pump(PumpEvent::Create(c)) => {
                v.classification.push("launch".into());
                v.items.push("pump_bonding_curve/CreateEvent".into());
                let mint = c.mint.to_string();
                let seen = deltas.keys().any(|(_, m)| *m == mint);
                check(i, "create.mint_has_token_balances", seen, true, format!("mint {mint} in token balance evidence: {seen}"));
            }
            VenueEvent::Pump(PumpEvent::Trade(t)) => {
                v.classification.push(if t.is_buy { "trade_buy".into() } else { "trade_sell".into() });
                v.items.push("pump_bonding_curve/TradeEvent".into());
                let (user, mint) = (t.user.to_string(), t.mint.to_string());
                let actual = deltas.get(&(user.clone(), mint.clone())).copied().unwrap_or(0);
                let exp = expected_user[&(user.clone(), mint.clone())];
                check(i, "trade.user_token_delta", actual == exp, true, format!("user {} token delta {actual} vs decoded {exp}", &user[..6.min(user.len())]));
                check(i, "trade.direction", (actual > 0) == t.is_buy || actual == 0, true, format!("is_buy={} delta={actual}", t.is_buy));
                let net: i128 = deltas.iter().filter(|((_, m), _)| *m == mint).map(|(_, d)| *d).sum();
                check(i, "trade.token_conservation", net == 0, true, format!("sum of {mint} deltas across owners = {net}"));
                if let (Some(bps), Some(fee)) = (t.fee_basis_points, t.fee) {
                    let e = ceil_fee(t.sol_amount, bps);
                    check(i, "trade.protocol_fee", fee.abs_diff(e) <= 1, true, format!("fee {fee} vs ceil(sol*{bps}bps) {e}"));
                }
                if let (Some(bps), Some(fee), Some(cr)) = (t.creator_fee_basis_points, t.creator_fee, t.creator) {
                    if cr != crate::types::Pubkey::default() {
                        let e = ceil_fee(t.sol_amount, bps);
                        check(i, "trade.creator_fee", fee.abs_diff(e) <= 1, true, format!("creator fee {fee} vs ceil(sol*{bps}bps) {e}"));
                    }
                }
                // user lamports: buys pay sol + fees (+ tx fee/rent/tips → soft bound)
                if let Some(dl) = lamport_delta(rpc_result, &keys, &user) {
                    let fees = (t.fee.unwrap_or(0) + t.creator_fee.unwrap_or(0)) as i128;
                    let model = if t.is_buy { -(t.sol_amount as i128) - fees } else { t.sol_amount as i128 - fees };
                    let residual = dl - model; // ≈ -(tx fee + rent + tips + other ix)
                    let ok = residual <= 0 && residual >= -(tx_fee + 50_000_000);
                    check(i, "trade.user_sol_delta", ok, false, format!("user lamport delta {dl}, model {model}, residual {residual} (tx fee {tx_fee})"));
                }
                if t.is_sol_quoted() {
                    let (vs, vt) = (t.virtual_sol_reserves as i128, t.virtual_token_reserves as i128);
                    let (ds, dt) = (t.sol_amount as i128, t.token_amount as i128);
                    let (ps, pt) = if t.is_buy { (vs - ds, vt + dt) } else { (vs + ds, vt - dt) };
                    let ok = vs * vt * 1_000_000 >= ps * pt * 999_999;
                    check(i, "trade.curve_invariant", ok, true, format!("k_post {} vs k_pre {}", vs * vt, ps * pt));
                }
            }
            VenueEvent::Pump(PumpEvent::Complete(c)) => {
                v.classification.push("curve_complete".into());
                v.items.push("pump_bonding_curve/CompleteEvent".into());
                check(i, "complete.has_mint", true, false, format!("mint {}", c.mint));
            }
            VenueEvent::Pump(PumpEvent::Migration(m)) => {
                v.classification.push("migration".into());
                v.items.push("pump_bonding_curve/CompletePumpAmmMigrationEvent".into());
                let pool = m.pool.to_string();
                let moved = deltas.get(&(pool.clone(), m.mint.to_string())).copied().unwrap_or(0);
                check(i, "migration.pool_received_tokens", moved == m.mint_amount as i128, true, format!("pool token delta {moved} vs mint_amount {}", m.mint_amount));
            }
            VenueEvent::PumpSwap(PumpSwapEvent::CreatePool(c)) => {
                v.classification.push("pool_create".into());
                v.items.push("pumpswap/CreatePoolEvent".into());
                let pool = c.pool.to_string();
                let base = deltas.get(&(pool.clone(), c.base_mint.to_string())).copied().unwrap_or(0);
                check(i, "create_pool.base_deposit", base == c.pool_base_amount as i128, true, format!("pool base delta {base} vs {}", c.pool_base_amount));
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) => {
                v.classification.push(if s.is_buy { "pool_swap_buy".into() } else { "pool_swap_sell".into() });
                v.items.push(format!("pumpswap/{}", if s.is_buy { "BuyEvent" } else { "SellEvent" }));
                let (pool, user) = (s.pool.to_string(), s.user.to_string());
                // base mint inferred from the pool vault delta (pool-owned token account, non-WSOL)
                let pool_base: Vec<(&String, i128)> = deltas.iter().filter(|((o, m), _)| *o == pool && *m != wsol).map(|((_, m), d)| (m, *d)).collect();
                let want = if s.is_buy { -(s.base_amount as i128) } else { s.base_amount as i128 };
                let base_ok = pool_base.iter().any(|(_, d)| *d == want);
                check(i, "swap.pool_base_delta", base_ok, true, format!("pool base deltas {:?} vs {want}", pool_base.iter().map(|x| x.1).collect::<Vec<_>>()));
                if let Some((mint, _)) = pool_base.iter().find(|(_, d)| *d == want) {
                    let ud = deltas.get(&(user.clone(), (*mint).clone())).copied().unwrap_or(0);
                    check(i, "swap.user_base_delta", ud == -want, false, format!("user base delta {ud} vs {}", -want));
                }
                let pq = deltas.get(&(pool.clone(), wsol.clone())).copied().unwrap_or(0);
                let fees_kept = (s.protocol_fee + s.coin_creator_fee.unwrap_or(0)) as i128;
                let expect_q = if s.is_buy { s.quote_amount_pool as i128 } else { -(s.quote_amount_pool as i128) };
                let q_ok = pq == expect_q || pq == expect_q + fees_kept;
                check(i, "swap.pool_quote_delta", q_ok, true, format!("pool WSOL delta {pq} vs {expect_q} (or +{fees_kept} retained fees)"));
                let gross = if s.is_buy { s.quote_amount_pool - s.lp_fee } else { s.quote_amount_pool + s.lp_fee };
                let e = ceil_fee(gross, s.lp_fee_bps);
                check(i, "swap.lp_fee", s.lp_fee.abs_diff(e) <= 1, true, format!("lp fee {} vs ceil({gross}*{}bps) {e}", s.lp_fee, s.lp_fee_bps));
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Liquidity(l)) => {
                v.classification.push("liquidity".into());
                v.items.push(format!("pumpswap/{}", if l.is_deposit { "DepositEvent" } else { "WithdrawEvent" }));
                let pq = deltas.get(&(l.pool.to_string(), wsol.clone())).copied().unwrap_or(0);
                let want = if l.is_deposit { l.quote_amount as i128 } else { -(l.quote_amount as i128) };
                check(i, "liquidity.pool_quote_delta", pq == want, true, format!("pool WSOL delta {pq} vs {want}"));
            }
            VenueEvent::Raydium(_) => {
                v.classification.push("raydium_decode_only".into());
                v.items.push("raydium_amm_v4/ray_log".into());
            }
        }
    }
    v.passed = !v.checks.is_empty() && v.checks.iter().filter(|c| c.hard).all(|c| c.ok) && v.decode_warnings.is_empty();
    v
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ItemStatus {
    pub transactions: u64,
    pub events: u64,
    pub hard_checks_passed: u64,
    pub hard_checks_failed: u64,
    pub mainnet_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct VerificationSummary {
    pub generated_us: i64,
    pub files: u64,
    pub min_events_for_verified: u64,
    pub items: BTreeMap<String, ItemStatus>,
    pub failures: Vec<String>,
    /// venue → true when every traded item of the venue is mainnet-verified
    pub venues_mainnet_verified: BTreeMap<String, bool>,
}

pub const TRADED_ITEMS: &[(&str, &[&str])] = &[
    ("pump_bonding_curve", &["pump_bonding_curve/TradeEvent"]),
    ("pumpswap", &["pumpswap/BuyEvent", "pumpswap/SellEvent"]),
];

pub fn summarize(results: &[TxVerification], min_events: u64) -> VerificationSummary {
    let mut s = VerificationSummary { generated_us: crate::time::wall_us(), files: results.len() as u64, min_events_for_verified: min_events, ..Default::default() };
    for r in results {
        let mut seen_tx: std::collections::HashSet<&str> = Default::default();
        for (i, item) in r.items.iter().enumerate() {
            let e = s.items.entry(item.clone()).or_default();
            e.events += 1;
            if seen_tx.insert(item) {
                e.transactions += 1;
            }
            for c in r.checks.iter().filter(|c| c.event_index == i && c.hard) {
                if c.ok {
                    e.hard_checks_passed += 1;
                } else {
                    e.hard_checks_failed += 1;
                    s.failures.push(format!("{} {}: {} — {}", r.signature, item, c.name, c.detail));
                }
            }
        }
    }
    for e in s.items.values_mut() {
        e.mainnet_verified = e.events >= min_events && e.hard_checks_failed == 0 && e.hard_checks_passed > 0;
    }
    for (venue, items) in TRADED_ITEMS {
        let ok = items.iter().all(|it| s.items.get(*it).is_some_and(|x| x.mainnet_verified));
        s.venues_mainnet_verified.insert(venue.to_string(), ok);
    }
    s
}

/// Load `fixtures/real/verification.json` written by `sniper verify-fixtures` (None if absent).
pub fn load_summary(path: &std::path::Path) -> Option<VerificationSummary> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}
