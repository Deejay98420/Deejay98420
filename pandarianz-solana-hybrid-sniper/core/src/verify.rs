//! Transaction-evidence verification of decoded events.
//!
//! Each decoded event is tied to the venue instruction that emitted it (same top-level instruction,
//! same program, execution order), and the instruction's accounts are located with the official IDL
//! layouts (`ixlayout`). Decoded amounts are then checked against INDEPENDENT evidence in the same
//! transaction:
//! * token transfers executed inside that instruction (parsed SPL Token / Token-2022 instructions);
//! * token balance changes of the specific token accounts involved (bonding-curve vault, user token
//!   account, pool vaults), after accounting for every other parsed token movement in the
//!   transaction (minting at token creation, burns, unrelated transfers);
//! * lamport changes of the specific accounts involved (bonding curve, fee recipient, creator vault),
//!   after accounting for parsed System transfers / account creation outside the trade;
//! * per-mint supply conservation: Σ balance changes = minted − burned;
//! * fee arithmetic as implemented in the official SDKs (ceil(amount × bps / 10 000)).
//!
//! Every check is Pass, Fail or Inconclusive. Inconclusive means the evidence needed is missing or
//! ambiguous (e.g. no inner-instruction record, unknown instruction layout, an account also moved
//! by instructions whose effect cannot be quantified); it never counts as a pass. A venue is
//! mainnet-verified only when no hard check fails and enough events pass every hard check.

use crate::decode::anchor::EVENT_IX_TAG;
use crate::decode::pumpfun::PumpEvent;
use crate::decode::pumpswap::PumpSwapEvent;
use crate::decode::{decode_raw, Provenance, VenueEvent};
use crate::ixlayout::{self, IxDef};
use crate::stream::ws::parse_tx_object;
use crate::txmodel::{structure_problems, TxModel};
use crate::types::{programs, Commitment, Pubkey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    #[default]
    Fail,
    Inconclusive,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub event_index: usize,
    pub name: String,
    #[serde(default)]
    pub outcome: Outcome,
    /// outcome == Pass (kept for older readers)
    pub ok: bool,
    pub hard: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TxVerification {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    /// transaction format version reported by the RPC ("legacy", "0", "1", "absent")
    #[serde(default)]
    pub version: String,
    /// launch | trade_buy | trade_sell | curve_complete | migration | pool_create | pool_swap_buy | pool_swap_sell | liquidity
    pub classification: Vec<String>,
    /// "venue/Event" per decoded event, aligned with event_index
    pub items: Vec<String>,
    /// stable event ids (signature:outer:inner:ordinal), aligned with event_index
    #[serde(default)]
    pub event_ids: Vec<String>,
    #[serde(default)]
    pub provenance: Vec<Provenance>,
    pub checks: Vec<Check>,
    /// no hard check failed, no decode warning, at least one hard check passed
    pub passed: bool,
    pub decode_warnings: Vec<String>,
    /// events emitted through both log and self-CPI channels, merged into one
    #[serde(default)]
    pub duplicates_merged: u32,
    #[serde(default)]
    pub structure_problems: Vec<String>,
    /// fee-routing hypotheses that matched the observed transfers
    #[serde(default)]
    pub fee_hypotheses: Vec<String>,
    #[serde(default)]
    pub tx_failed: bool,
}

impl TxVerification {
    /// Hard-check outcome counts for one event: (pass, fail, inconclusive).
    pub fn event_outcomes(&self, i: usize) -> (u64, u64, u64) {
        let mut r = (0, 0, 0);
        for c in self.checks.iter().filter(|c| c.event_index == i && c.hard) {
            match c.outcome {
                Outcome::Pass => r.0 += 1,
                Outcome::Fail => r.1 += 1,
                Outcome::Inconclusive => r.2 += 1,
            }
        }
        r
    }
}

fn ceil_fee(amount: u64, bps: u64) -> u64 {
    (amount as u128 * bps as u128).div_ceil(10_000) as u64
}

fn short(s: &str) -> String {
    s.chars().take(6).collect()
}

/// Venue instruction classes; an event is matched to an instruction of the same class.
fn ix_class(program: &str, name: &str) -> Option<&'static str> {
    match (program, name) {
        (programs::PUMP_FUN, "multi_hop_curve_swap") => Some("pump_trade_multi"),
        (programs::PUMP_FUN, n) if n.starts_with("buy") || n.starts_with("sell") => Some("pump_trade"),
        (programs::PUMP_FUN, "create" | "create_v2") => Some("pump_create"),
        (programs::PUMP_FUN, "migrate" | "migrate_v2") => Some("pump_migrate"),
        (programs::PUMP_SWAP, "buy" | "sell" | "buy_exact_quote_in" | "buy_v2" | "sell_v2" | "buy_exact_quote_in_v2") => Some("amm_swap"),
        (programs::PUMP_SWAP, "create_pool") => Some("amm_create_pool"),
        (programs::PUMP_SWAP, "deposit" | "withdraw") => Some("amm_liquidity"),
        _ => None,
    }
}

fn event_class(e: &VenueEvent) -> Option<&'static str> {
    match e {
        VenueEvent::Pump(PumpEvent::Trade(_)) => Some("pump_trade"),
        VenueEvent::Pump(PumpEvent::Create(_)) => Some("pump_create"),
        VenueEvent::Pump(PumpEvent::Migration(_)) => Some("pump_migrate"),
        VenueEvent::PumpSwap(PumpSwapEvent::Swap(_)) => Some("amm_swap"),
        VenueEvent::PumpSwap(PumpSwapEvent::CreatePool(_)) => Some("amm_create_pool"),
        VenueEvent::PumpSwap(PumpSwapEvent::Liquidity(_)) => Some("amm_liquidity"),
        _ => None,
    }
}

fn event_program(e: &VenueEvent) -> &'static str {
    match e {
        VenueEvent::Pump(_) => programs::PUMP_FUN,
        VenueEvent::PumpSwap(_) => programs::PUMP_SWAP,
        VenueEvent::Raydium(_) => programs::RAYDIUM_AMM_V4,
    }
}

/// Instruction an event was emitted by: (ix index, layout) or the reason it cannot be determined.
type Mapping = Result<(usize, &'static IxDef), String>;

fn map_events(m: &TxModel, events: &[crate::decode::Decoded]) -> Vec<Mapping> {
    // venue instructions with a known layout (event self-CPIs excluded), grouped by (outer, class)
    let mut groups: HashMap<(u16, &'static str), Vec<(usize, &'static IxDef)>> = HashMap::new();
    let mut multi: HashSet<u16> = HashSet::new();
    for (i, ix) in m.ixs.iter().enumerate() {
        let Some(d) = ix.data.as_ref() else { continue };
        if d.len() >= 8 && d[..8] == EVENT_IX_TAG {
            continue;
        }
        let Some(def) = ix.disc().and_then(|disc| ixlayout::lookup(&ix.program_id, disc)) else { continue };
        let Some(class) = ix_class(&ix.program_id, &def.name) else { continue };
        if class == "pump_trade_multi" {
            multi.insert(ix.outer);
            continue;
        }
        groups.entry((ix.outer, class)).or_default().push((i, def));
    }
    let mut ev_groups: HashMap<(u16, &'static str), Vec<usize>> = HashMap::new();
    let mut out: Vec<Mapping> = vec![Err("event type is not tied to an instruction".into()); events.len()];
    for (k, d) in events.iter().enumerate() {
        let Some(class) = event_class(&d.event) else { continue };
        match d.provenance.outer_ix {
            Some(o) => ev_groups.entry((o, class)).or_default().push(k),
            None => out[k] = Err("no instruction provenance (logs without invoke records)".into()),
        }
    }
    for (key, evs) in ev_groups {
        let ixs = groups.get(&key).cloned().unwrap_or_default();
        let ixs: Vec<_> = ixs.into_iter().filter(|(_, def)| !def.name.is_empty()).collect();
        if key.1 == "pump_trade" && multi.contains(&key.0) {
            for k in evs {
                out[k] = Err("multi_hop_curve_swap emits several trades per instruction; per-trade accounts are not in the instruction".into());
            }
            continue;
        }
        let same_program = |k: usize| ixs.iter().all(|(i, _)| m.ixs[*i].program_id == event_program(&events[k].event));
        if ixs.len() != evs.len() || !evs.iter().all(|k| same_program(*k)) {
            for k in &evs {
                out[*k] = Err(format!("{} {} event(s) vs {} matching instruction(s) in top-level instruction {}", evs.len(), key.1, ixs.len(), key.0));
            }
            continue;
        }
        for (k, ix) in evs.into_iter().zip(ixs) {
            out[k] = Ok(ix);
        }
    }
    out
}

/// Accumulates expected deltas for an account across events.
#[derive(Default)]
struct Expect {
    expected: i128,
    events: Vec<usize>,
    note: Vec<String>,
}

struct Ctx<'a> {
    m: &'a TxModel,
    /// instruction indices inside mapped trade/swap instructions (including themselves)
    inside: HashSet<usize>,
    /// every mapped trade/swap instruction's subtree is known
    subtrees_known: bool,
}

impl Ctx<'_> {
    fn outside(&self) -> impl Fn(usize) -> bool + '_ {
        move |ix| !self.inside.contains(&ix)
    }
    /// Observed token delta of a token account minus parsed movements outside the trades.
    fn token_net(&self, acct: &str) -> Result<i128, String> {
        if !self.subtrees_known {
            return Err("call depth (stackHeight) unavailable: trade subtrees unknown".into());
        }
        if self.m.unexplained_token_accounts.iter().any(|a| a == acct) {
            return Err(format!("token account {} changed by an instruction without an exact amount (syncNative / close / unknown)", short(acct)));
        }
        let d = self.m.token_delta(acct).ok_or_else(|| format!("token account {} not in balance evidence", short(acct)))?;
        Ok(d - self.m.token_flow_sum(acct, self.outside()))
    }
    /// Observed value delta (lamports of `sys` + tokens of `ata`) minus parsed movements outside the trades.
    fn value_net(&self, sys: Option<&str>, ata: Option<&str>) -> Result<i128, String> {
        if !self.subtrees_known {
            return Err("call depth (stackHeight) unavailable: trade subtrees unknown".into());
        }
        let mut v = 0;
        let mut any = false;
        if let Some(s) = sys {
            if let Some(d) = self.m.lamport_delta(s) {
                v += d - self.m.lamport_flow_sum(s, self.outside());
                any = true;
                if self.m.key_index(s) == Some(0) {
                    v += self.m.fee as i128; // fee payer: transaction fee is not part of the trade
                }
            }
        }
        if let Some(a) = ata {
            if self.m.tokens.contains_key(a) {
                v += self.token_net(a)?;
                any = true;
            }
        }
        if any {
            Ok(v)
        } else {
            Err("account not present in balance evidence".into())
        }
    }
}

struct Out<'a> {
    checks: &'a mut Vec<Check>,
}

impl Out<'_> {
    fn add(&mut self, i: usize, name: &str, outcome: Outcome, hard: bool, detail: String) {
        self.checks.push(Check { event_index: i, name: name.into(), ok: outcome == Outcome::Pass, outcome, hard, detail });
    }
    fn cmp(&mut self, i: usize, name: &str, observed: Result<i128, String>, expected: i128, what: &str) {
        match observed {
            Ok(o) if o == expected => self.add(i, name, Outcome::Pass, true, format!("{what}: observed {o} = decoded {expected}")),
            Ok(o) => self.add(i, name, Outcome::Fail, true, format!("{what}: observed {o} vs decoded {expected} (diff {})", o - expected)),
            Err(e) => self.add(i, name, Outcome::Inconclusive, true, format!("{what}: {e}")),
        }
    }
    fn group(&mut self, name: &str, groups: &BTreeMap<String, Expect>, f: impl Fn(&str) -> Result<i128, String>, what: &str) {
        for (acct, e) in groups {
            let obs = f(acct);
            for &i in &e.events {
                let suffix = if e.events.len() > 1 { format!(" ({} events share this account)", e.events.len()) } else { String::new() };
                let n = if e.note.is_empty() { String::new() } else { format!(" [{}]", e.note.join("; ")) };
                self.cmp(i, name, obs.clone(), e.expected, &format!("{what} {}{suffix}{n}", short(acct)));
            }
        }
    }
}

fn add_expect(map: &mut BTreeMap<String, Expect>, acct: Option<&str>, amount: i128, ev: usize) {
    if let Some(a) = acct {
        let e = map.entry(a.to_string()).or_default();
        e.expected += amount;
        e.events.push(ev);
    }
}

/// Verify one getTransaction (jsonParsed) result.
pub fn verify_tx(rpc_result: &Value, signature: &str) -> TxVerification {
    let mut v = TxVerification {
        signature: signature.to_string(),
        slot: rpc_result["slot"].as_u64().unwrap_or(0),
        block_time: rpc_result["blockTime"].as_i64(),
        version: crate::txmodel::TxVersion::parse(rpc_result).label(),
        ..Default::default()
    };
    v.structure_problems = structure_problems(rpc_result);
    let (Some(raw), Some(m)) = (parse_tx_object(rpc_result, Some(signature), v.slot, "verify", Commitment::Confirmed, 0), TxModel::parse(rpc_result, signature)) else {
        v.checks.push(Check { event_index: 0, name: "parse".into(), outcome: Outcome::Fail, ok: false, hard: true, detail: format!("unparsable transaction: {:?}", v.structure_problems) });
        return v;
    };
    if m.err.is_some() {
        v.tx_failed = true;
        return v; // failed on chain: no state change, nothing to verify
    }
    let out = decode_raw(&raw);
    v.decode_warnings = out.warnings.clone();
    v.duplicates_merged = out.duplicates_merged;
    let events = &out.events;
    let mapping = map_events(&m, events);
    let mut inside = HashSet::new();
    let mut subtrees_known = true;
    for mp in mapping.iter().flatten() {
        let (i, def) = mp;
        if !def.name.starts_with("buy") && !def.name.starts_with("sell") {
            continue;
        }
        inside.insert(*i);
        match m.subtree(*i) {
            Some(r) => inside.extend(r),
            None => subtrees_known = false,
        }
    }
    let ctx = Ctx { m: &m, inside, subtrees_known: subtrees_known && m.inner_recorded };
    let mut checks = Vec::new();
    let mut o = Out { checks: &mut checks };
    // aggregated per-account expectations
    let (mut curve_vault, mut user_ata, mut curve_val, mut fee_val, mut creator_val, mut buyback_val) =
        (BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let (mut pool_base, mut pool_quote, mut swap_user_base, mut amm_proto, mut amm_creator) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let mut mints: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut fee_quote_ata: HashMap<String, Option<String>> = HashMap::new();
    let mut cashback_events: HashSet<usize> = HashSet::new();
    let mut amm_v2_events: HashSet<usize> = HashSet::new();

    for (i, d) in events.iter().enumerate() {
        v.event_ids.push(d.event_id(signature));
        v.provenance.push(d.provenance.clone());
        let mp = &mapping[i];
        let acct = |name: &str| -> Option<String> {
            let (ix, def) = mp.as_ref().ok()?;
            def.get(&m.ixs[*ix].accounts, name).map(String::from)
        };
        let ix_name = mp.as_ref().map(|(_, d)| d.name.clone()).unwrap_or_default();
        match &d.event {
            VenueEvent::Pump(PumpEvent::Create(c)) => {
                v.classification.push("launch".into());
                v.items.push("pump_bonding_curve/CreateEvent".into());
                let mint = c.mint.to_string();
                mints.entry(mint.clone()).or_default().push(i);
                match mp {
                    Ok(_) => {
                        let ok = acct("mint").as_deref() == Some(mint.as_str()) && acct("bonding_curve").as_deref() == Some(c.bonding_curve.to_string().as_str());
                        o.add(i, "create.instruction", if ok { Outcome::Pass } else { Outcome::Fail }, true, format!("{ix_name}: mint/bonding_curve accounts match event: {ok}"));
                    }
                    Err(e) => o.add(i, "create.instruction", Outcome::Inconclusive, true, e.clone()),
                }
                match c.token_total_supply {
                    Some(sup) => o.cmp(i, "create.minted_supply", Ok(m.net_minted(&mint)), sup as i128, "minted (parsed mintTo − burn) vs token_total_supply"),
                    None => o.add(i, "create.minted_supply", Outcome::Inconclusive, false, "older CreateEvent layout without token_total_supply".into()),
                }
            }
            VenueEvent::Pump(PumpEvent::Trade(t)) => {
                v.classification.push(if t.is_buy { "trade_buy".into() } else { "trade_sell".into() });
                v.items.push("pump_bonding_curve/TradeEvent".into());
                let mint = t.mint.to_string();
                mints.entry(mint.clone()).or_default().push(i);
                let tok = t.token_amount as i128;
                if t.tail.as_ref().is_some_and(|x| x.cashback > 0) {
                    cashback_events.insert(i);
                }
                let (ix, _def) = match mp {
                    Ok(x) => *x,
                    Err(e) => {
                        o.add(i, "trade.instruction", Outcome::Inconclusive, true, e.clone());
                        trade_math_checks(&mut o, i, t);
                        continue;
                    }
                };
                let ix_buy = ix_name.starts_with("buy");
                let ix_mint = acct("mint").or_else(|| acct("base_mint"));
                let ix_user = acct("user");
                let mut problems = Vec::new();
                if ix_buy != t.is_buy {
                    problems.push(format!("direction: instruction {ix_name} vs is_buy={}", t.is_buy));
                }
                if ix_mint.as_deref() != Some(mint.as_str()) {
                    problems.push(format!("mint {} vs instruction {}", short(&mint), ix_mint.as_deref().map(short).unwrap_or("-".into())));
                }
                if ix_user.as_deref() != Some(t.user.to_string().as_str()) {
                    problems.push(format!("user {} vs instruction {}", short(&t.user.to_string()), ix_user.as_deref().map(short).unwrap_or("-".into())));
                }
                if let (Some(fr), Some(ixfr)) = (t.fee_recipient, acct("fee_recipient")) {
                    if fr.to_string() != ixfr {
                        problems.push("fee_recipient differs from instruction account".into());
                    }
                }
                o.add(i, "trade.instruction", if problems.is_empty() { Outcome::Pass } else { Outcome::Fail }, true, if problems.is_empty() { format!("{ix_name}: direction, mint and user match") } else { problems.join("; ") });

                let vault = acct("associated_bonding_curve").or_else(|| acct("associated_base_bonding_curve"));
                let uata = acct("associated_user").or_else(|| acct("associated_base_user"));
                // token transfer executed inside this instruction
                match (m.inner_recorded, m.transfers_in(ix), &vault, &uata) {
                    (false, _, _, _) => o.add(i, "trade.token_transfer", Outcome::Inconclusive, true, "transaction has no inner-instruction record".into()),
                    (_, None, _, _) => o.add(i, "trade.token_transfer", Outcome::Inconclusive, true, "call depth unavailable for this instruction".into()),
                    (_, _, None, _) | (_, _, _, None) => o.add(i, "trade.token_transfer", Outcome::Inconclusive, true, format!("{ix_name}: vault/user token account not in instruction (older layout?)")),
                    (true, Some(fl), Some(va), Some(ua)) => {
                        let (from, to) = if t.is_buy { (va, ua) } else { (ua, va) };
                        let moved: i128 = fl
                            .iter()
                            .filter_map(|f| match f {
                                crate::txmodel::Flow::Token { from: Some(a), to: Some(b), amount, .. } if a == from && b == to => Some(*amount),
                                _ => None,
                            })
                            .sum();
                        let what = format!("tokens moved {} → {} inside {ix_name}", short(from), short(to));
                        o.cmp(i, "trade.token_transfer", Ok(moved), tok, &what);
                    }
                }
                add_expect(&mut curve_vault, vault.as_deref(), if t.is_buy { -tok } else { tok }, i);
                add_expect(&mut user_ata, uata.as_deref(), if t.is_buy { tok } else { -tok }, i);
                if t.is_sol_quoted() {
                    let sol = t.sol_amount as i128;
                    let curve = acct("bonding_curve");
                    if let Some(c) = &curve {
                        // buys: the curve receives the net amount; sells: the curve pays out the gross amount (fees included)
                        add_expect(&mut curve_val, Some(c), if t.is_buy { sol } else { -sol }, i);
                        fee_quote_ata.insert(c.clone(), acct("associated_quote_bonding_curve"));
                    }
                    if let Some(fr) = acct("fee_recipient") {
                        add_expect(&mut fee_val, Some(&fr), t.fee.unwrap_or(0) as i128, i);
                        fee_quote_ata.insert(fr.clone(), acct("associated_quote_fee_recipient"));
                    } else {
                        o.add(i, "trade.protocol_fee_observed", Outcome::Inconclusive, true, format!("{ix_name}: no fee_recipient account in this instruction layout"));
                    }
                    let creator_set = t.creator.is_some_and(|c| c != Pubkey::default());
                    if let Some(cv) = acct("creator_vault") {
                        add_expect(&mut creator_val, Some(&cv), if creator_set { t.creator_fee.unwrap_or(0) as i128 } else { 0 }, i);
                        fee_quote_ata.insert(cv.clone(), acct("associated_creator_vault"));
                    } else {
                        o.add(i, "trade.creator_fee_observed", Outcome::Inconclusive, true, format!("{ix_name}: no creator_vault account in this instruction layout"));
                    }
                    if let (Some(bb), Some(tail)) = (acct("buyback_fee_recipient"), t.tail.as_ref()) {
                        add_expect(&mut buyback_val, Some(&bb), tail.buyback_fee as i128, i);
                        fee_quote_ata.insert(bb.clone(), acct("associated_quote_buyback_fee_recipient"));
                    }
                } else {
                    o.add(i, "trade.sol_amount", Outcome::Inconclusive, true, "non-SOL quote mint: outside trading scope, quote accounting not verified".into());
                }
                trade_math_checks(&mut o, i, t);
                // informative: the user's wallet-wide lamport change also includes rent, tips, other instructions
                if let Some(dl) = m.lamport_delta(&t.user.to_string()) {
                    let fees = (t.fee.unwrap_or(0) + t.creator_fee.unwrap_or(0)) as i128;
                    let model = if t.is_buy { -(t.sol_amount as i128) - fees } else { t.sol_amount as i128 - fees };
                    o.add(i, "trade.user_sol_delta", Outcome::Pass, false, format!("user lamport delta {dl}, trade model {model}, residual {} (tx fee {}, rent, tips, other instructions)", dl - model, m.fee));
                }
            }
            VenueEvent::Pump(PumpEvent::Complete(c)) => {
                v.classification.push("curve_complete".into());
                v.items.push("pump_bonding_curve/CompleteEvent".into());
                o.add(i, "complete.has_mint", Outcome::Pass, false, format!("mint {}", c.mint));
            }
            VenueEvent::Pump(PumpEvent::Migration(mg)) => {
                v.classification.push("migration".into());
                v.items.push("pump_bonding_curve/CompletePumpAmmMigrationEvent".into());
                match mp {
                    Ok(_) => {
                        let ok = acct("pool").as_deref() == Some(mg.pool.to_string().as_str());
                        o.add(i, "migration.instruction", if ok { Outcome::Pass } else { Outcome::Fail }, true, format!("{ix_name}: pool account matches event: {ok}"));
                        match acct("pool_base_token_account") {
                            Some(pb) => o.cmp(i, "migration.pool_received_tokens", m.token_delta(&pb).ok_or("pool base vault not in balance evidence".into()), mg.mint_amount as i128, "pool base vault delta"),
                            None => o.add(i, "migration.pool_received_tokens", Outcome::Inconclusive, true, "pool vault account not in instruction".into()),
                        }
                    }
                    Err(e) => o.add(i, "migration.instruction", Outcome::Inconclusive, true, e.clone()),
                }
            }
            VenueEvent::PumpSwap(PumpSwapEvent::CreatePool(c)) => {
                v.classification.push("pool_create".into());
                v.items.push("pumpswap/CreatePoolEvent".into());
                match acct("pool_base_token_account") {
                    Some(pb) => o.cmp(i, "create_pool.base_deposit", m.token_delta(&pb).ok_or("pool base vault not in balance evidence".into()), c.pool_base_amount as i128, "pool base vault delta"),
                    None => o.add(i, "create_pool.base_deposit", Outcome::Inconclusive, true, mp.as_ref().err().cloned().unwrap_or_else(|| "pool vault account not in instruction".into())),
                }
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) => {
                v.classification.push(if s.is_buy { "pool_swap_buy".into() } else { "pool_swap_sell".into() });
                v.items.push(format!("pumpswap/{}", if s.is_buy { "BuyEvent" } else { "SellEvent" }));
                let gross = if s.is_buy { s.quote_amount_pool.saturating_sub(s.lp_fee) } else { s.quote_amount_pool + s.lp_fee };
                let e = ceil_fee(gross, s.lp_fee_bps);
                o.add(i, "swap.lp_fee", if s.lp_fee.abs_diff(e) == 0 { Outcome::Pass } else { Outcome::Fail }, true, format!("lp fee {} vs ceil({gross}×{}bps) {e}", s.lp_fee, s.lp_fee_bps));
                let ep = ceil_fee(gross, s.protocol_fee_bps);
                o.add(i, "swap.protocol_fee_formula", if s.protocol_fee == ep { Outcome::Pass } else { Outcome::Fail }, true, format!("protocol fee {} vs ceil({gross}×{}bps) {ep}", s.protocol_fee, s.protocol_fee_bps));
                if let (Some(cf), Some(cb)) = (s.coin_creator_fee, s.coin_creator_fee_bps) {
                    let set = s.coin_creator.is_some_and(|c| c != Pubkey::default());
                    let ec = if set { ceil_fee(gross, cb) } else { 0 };
                    o.add(i, "swap.creator_fee_formula", if cf == ec { Outcome::Pass } else { Outcome::Fail }, true, format!("coin creator fee {cf} vs {} {ec}", if set { format!("ceil({gross}×{cb}bps)") } else { "no creator →".into() }));
                }
                // constant product on effective reserves (vault + virtual quote), as in the SDK
                let q_eff = s.pool_quote_token_reserves as i128 + s.virtual_quote_reserves.unwrap_or(0);
                let b = s.pool_base_token_reserves as i128;
                let (model, actual, what) = if s.is_buy {
                    let net = gross as i128;
                    (if q_eff + net > 0 { b * net / (q_eff + net) } else { 0 }, s.base_amount as i128, "base out")
                } else {
                    let x = s.base_amount as i128;
                    (if b + x > 0 { q_eff * x / (b + x) } else { 0 }, gross as i128, "gross quote out")
                };
                let tol = 1 + model.abs() / 10_000;
                o.add(i, "swap.amm_math", if (model - actual).abs() <= tol { Outcome::Pass } else { Outcome::Fail }, true, format!("{what} {actual} vs constant-product model {model} (±{tol})"));
                let (ix, _) = match mp {
                    Ok(x) => *x,
                    Err(e) => {
                        o.add(i, "swap.instruction", Outcome::Inconclusive, true, e.clone());
                        continue;
                    }
                };
                let _ = ix;
                let mut problems = Vec::new();
                if ix_name.starts_with("buy") != s.is_buy {
                    problems.push(format!("direction: instruction {ix_name} vs is_buy={}", s.is_buy));
                }
                if acct("pool").as_deref() != Some(s.pool.to_string().as_str()) {
                    problems.push("pool differs from instruction account".into());
                }
                if acct("user").as_deref() != Some(s.user.to_string().as_str()) {
                    problems.push("user differs from instruction account".into());
                }
                o.add(i, "swap.instruction", if problems.is_empty() { Outcome::Pass } else { Outcome::Fail }, true, if problems.is_empty() { format!("{ix_name}: direction, pool and user match") } else { problems.join("; ") });
                if let Some(bm) = acct("base_mint") {
                    mints.entry(bm).or_default().push(i);
                }
                let base = s.base_amount as i128;
                add_expect(&mut pool_base, acct("pool_base_token_account").as_deref(), if s.is_buy { -base } else { base }, i);
                add_expect(&mut swap_user_base, acct("user_base_token_account").as_deref(), if s.is_buy { base } else { -base }, i);
                let q = s.quote_amount_pool as i128;
                add_expect(&mut pool_quote, acct("pool_quote_token_account").as_deref(), if s.is_buy { q } else { -q }, i);
                if ix_name.ends_with("_v2") {
                    amm_v2_events.insert(i);
                    o.add(i, "swap.fee_transfers", Outcome::Inconclusive, true, format!("{ix_name}: v2 fee routing (buyback / fee config) has no per-fee recipient accounts in the instruction"));
                } else {
                    add_expect(&mut amm_proto, acct("protocol_fee_recipient_token_account").as_deref(), s.protocol_fee as i128, i);
                    add_expect(&mut amm_creator, acct("coin_creator_vault_ata").as_deref(), s.coin_creator_fee.unwrap_or(0) as i128, i);
                }
            }
            VenueEvent::PumpSwap(PumpSwapEvent::Liquidity(l)) => {
                v.classification.push("liquidity".into());
                v.items.push(format!("pumpswap/{}", if l.is_deposit { "DepositEvent" } else { "WithdrawEvent" }));
                let want = if l.is_deposit { l.quote_amount as i128 } else { -(l.quote_amount as i128) };
                match acct("pool_quote_token_account") {
                    Some(pq) => o.cmp(i, "liquidity.pool_quote_delta", m.token_delta(&pq).ok_or("pool quote vault not in balance evidence".into()), want, "pool quote vault delta"),
                    None => o.add(i, "liquidity.pool_quote_delta", Outcome::Inconclusive, true, mp.as_ref().err().cloned().unwrap_or_else(|| "pool vault not in instruction".into())),
                }
            }
            VenueEvent::Raydium(_) => {
                v.classification.push("raydium_decode_only".into());
                v.items.push("raydium_amm_v4/ray_log".into());
            }
        }
    }
    // ---- aggregated account checks (several trades may share an account) ----
    o.group("trade.curve_vault_delta", &curve_vault, |a| ctx.token_net(a), "bonding-curve token vault");
    o.group("trade.user_token_delta", &user_ata, |a| ctx.token_net(a), "user token account");
    o.group("trade.sol_amount", &curve_val, |a| ctx.value_net(Some(a), fee_quote_ata.get(a).and_then(|x| x.as_deref())), "bonding-curve SOL");
    o.group("swap.pool_base_delta", &pool_base, |a| ctx.token_net(a), "pool base vault");
    o.group("swap.user_base_delta", &swap_user_base, |a| ctx.token_net(a), "user base token account");
    // pool quote vault: v1 moves every fee out of the vault; v2 may retain protocol/creator fees
    for (acct, e) in &pool_quote {
        let obs = ctx.token_net(acct);
        for &i in &e.events {
            let retained: i128 = e
                .events
                .iter()
                .filter(|k| amm_v2_events.contains(k))
                .filter_map(|k| match &events[*k].event {
                    VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) => Some((s.protocol_fee + s.coin_creator_fee.unwrap_or(0)) as i128),
                    _ => None,
                })
                .sum();
            match &obs {
                Ok(x) if *x == e.expected => o.add(i, "swap.pool_quote_delta", Outcome::Pass, true, format!("pool quote vault {}: observed {x} = decoded {}", short(acct), e.expected)),
                Ok(x) if retained != 0 && *x == e.expected + retained => {
                    o.add(i, "swap.pool_quote_delta", Outcome::Pass, true, format!("pool quote vault {}: observed {x} = decoded {} + {retained} protocol/creator fees retained (v2)", short(acct), e.expected));
                    v.fee_hypotheses.push("amm_v2_fees_retained_in_vault".into());
                }
                Ok(x) => o.add(i, "swap.pool_quote_delta", Outcome::Fail, true, format!("pool quote vault {}: observed {x} vs decoded {} (diff {})", short(acct), e.expected, x - e.expected)),
                Err(er) => o.add(i, "swap.pool_quote_delta", Outcome::Inconclusive, true, er.clone()),
            }
        }
    }
    o.group("swap.protocol_fee_observed", &amm_proto, |a| ctx.token_net(a), "protocol fee token account");
    o.group("swap.creator_fee_observed", &amm_creator, |a| ctx.token_net(a), "coin creator vault token account");
    // ---- curve fee routing: test explicit hypotheses against observed transfers ----
    let obs = |map: &BTreeMap<String, Expect>| -> BTreeMap<String, Result<i128, String>> {
        map.keys().map(|a| (a.clone(), ctx.value_net(Some(a), fee_quote_ata.get(a).and_then(|x| x.as_deref())))).collect()
    };
    let (fee_obs, bb_obs) = (obs(&fee_val), obs(&buyback_val));
    let bb_total: i128 = buyback_val.values().map(|e| e.expected).sum();
    let all_eq = |o: &BTreeMap<String, Result<i128, String>>, e: &BTreeMap<String, Expect>, adj: &dyn Fn(&str) -> i128| e.iter().all(|(a, x)| o.get(a).and_then(|r| r.as_ref().ok()) == Some(&(x.expected + adj(a))));
    let inconclusive_fee = fee_obs.values().chain(bb_obs.values()).find_map(|r| r.as_ref().err().cloned());
    let shared_roles = fee_val.keys().any(|a| curve_val.contains_key(a) || creator_val.contains_key(a));
    let hyp: Option<&str> = if fee_val.is_empty() {
        None
    } else if bb_total == 0 && all_eq(&fee_obs, &fee_val, &|_| 0) {
        Some("protocol_fee_to_fee_recipient")
    } else if bb_total != 0 && all_eq(&fee_obs, &fee_val, &|_| 0) && all_eq(&bb_obs, &buyback_val, &|_| 0) {
        Some("buyback_fee_additional_to_protocol_fee")
    } else if bb_total != 0 && fee_val.len() == 1 && all_eq(&fee_obs, &fee_val, &|_| -bb_total) && all_eq(&bb_obs, &buyback_val, &|_| 0) {
        Some("buyback_fee_carved_from_protocol_fee")
    } else {
        None
    };
    for (acct, e) in &fee_val {
        for &i in &e.events {
            let (outcome, detail) = match (hyp, &inconclusive_fee, shared_roles) {
                (_, _, true) => (Outcome::Inconclusive, "fee recipient also has another role in this transaction".to_string()),
                (Some(h), _, _) => (Outcome::Pass, format!("fee recipient {}: observed {:?} matches hypothesis {h}", short(acct), fee_obs[acct].as_ref().ok())),
                (None, Some(er), _) => (Outcome::Inconclusive, er.clone()),
                (None, None, _) => (
                    Outcome::Fail,
                    format!(
                        "fee recipient {}: observed {:?} vs decoded fee {} (buyback decoded {bb_total}, buyback observed {:?}); no documented fee-routing hypothesis matches",
                        short(acct),
                        fee_obs[acct].as_ref().ok(),
                        e.expected,
                        bb_obs.values().filter_map(|r| r.as_ref().ok()).sum::<i128>()
                    ),
                ),
            };
            o.add(i, "trade.protocol_fee_observed", outcome, true, detail);
        }
    }
    if let Some(h) = hyp {
        v.fee_hypotheses.push(h.into());
    }
    for (acct, e) in &creator_val {
        let r = ctx.value_net(Some(acct), fee_quote_ata.get(acct).and_then(|x| x.as_deref()));
        for &i in &e.events {
            match &r {
                Ok(x) if *x == e.expected => o.add(i, "trade.creator_fee_observed", Outcome::Pass, true, format!("creator vault {}: observed {x} = decoded {}", short(acct), e.expected)),
                Ok(x) if e.events.iter().any(|k| cashback_events.contains(k)) => {
                    o.add(i, "trade.creator_fee_observed", Outcome::Inconclusive, true, format!("creator vault {}: observed {x} vs decoded {}; cashback coin — cashback routing is not documented", short(acct), e.expected))
                }
                Ok(x) => o.add(i, "trade.creator_fee_observed", Outcome::Fail, true, format!("creator vault {}: observed {x} vs decoded creator fee {} (diff {})", short(acct), e.expected, x - e.expected)),
                Err(er) => o.add(i, "trade.creator_fee_observed", Outcome::Inconclusive, true, er.clone()),
            }
        }
    }
    // ---- supply conservation per mint: Σ balance changes = minted − burned ----
    for (mint, evs) in &mints {
        let accts: Vec<&String> = m.tokens.iter().filter(|(_, t)| &t.mint == mint).map(|(a, _)| a).collect();
        let net: i128 = accts.iter().map(|a| m.tokens[*a].delta()).sum();
        let minted = m.net_minted(mint);
        let unexplained = accts.iter().any(|a| m.unexplained_token_accounts.contains(a));
        for &i in evs {
            if accts.is_empty() {
                o.add(i, "token.mint_conservation", Outcome::Inconclusive, true, format!("mint {} has no balance evidence", short(mint)));
            } else if unexplained {
                o.add(i, "token.mint_conservation", Outcome::Inconclusive, true, format!("mint {}: an account was changed without an exact parsed amount", short(mint)));
            } else {
                let what = format!("Σ {} balance changes over {} accounts vs parsed minted−burned", short(mint), accts.len());
                o.cmp(i, "token.mint_conservation", Ok(net), minted, &what);
            }
        }
    }
    v.fee_hypotheses.sort();
    v.fee_hypotheses.dedup();
    v.checks = checks;
    let hard = || v.checks.iter().filter(|c| c.hard);
    v.passed = hard().any(|c| c.outcome == Outcome::Pass) && hard().all(|c| c.outcome != Outcome::Fail) && v.decode_warnings.is_empty();
    v
}

/// Event-internal arithmetic: SDK fee formula and the constant-product invariant.
fn trade_math_checks(o: &mut Out, i: usize, t: &crate::decode::pumpfun::PumpTrade) {
    if !t.is_sol_quoted() {
        return;
    }
    if let (Some(bps), Some(fee)) = (t.fee_basis_points, t.fee) {
        let e = ceil_fee(t.sol_amount, bps);
        let detail = format!(
            "fee {fee} vs ceil(sol_amount {}×{bps}bps) {e}{}",
            t.sol_amount,
            if fee == e { String::new() } else { format!("; implied {:.2} bps, floor would be {}", fee as f64 * 10_000.0 / t.sol_amount.max(1) as f64, t.sol_amount as u128 * bps as u128 / 10_000) }
        );
        o.add(i, "trade.protocol_fee_formula", if fee == e { Outcome::Pass } else { Outcome::Fail }, true, detail);
    }
    if let (Some(bps), Some(fee)) = (t.creator_fee_basis_points, t.creator_fee) {
        let set = t.creator.is_some_and(|c| c != Pubkey::default());
        let e = if set { ceil_fee(t.sol_amount, bps) } else { 0 };
        o.add(i, "trade.creator_fee_formula", if fee == e { Outcome::Pass } else { Outcome::Fail }, true, format!("creator fee {fee} vs {} {e}", if set { format!("ceil(sol_amount×{bps}bps)") } else { "no creator →".into() }));
    }
    let (vs, vt) = (t.virtual_sol_reserves as i128, t.virtual_token_reserves as i128);
    let (ds, dt) = (t.sol_amount as i128, t.token_amount as i128);
    let (ps, pt) = if t.is_buy { (vs - ds, vt + dt) } else { (vs + ds, vt - dt) };
    let ok = vs * vt * 1_000_000 >= ps * pt * 999_999;
    o.add(i, "trade.curve_invariant", if ok { Outcome::Pass } else { Outcome::Fail }, true, format!("k_post {} vs k_pre {}", vs * vt, ps * pt));
}

// ------------------------------------------------------------------------------------------------
// Summary

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ItemStatus {
    pub transactions: u64,
    pub events: u64,
    pub hard_checks_passed: u64,
    pub hard_checks_failed: u64,
    #[serde(default)]
    pub hard_checks_inconclusive: u64,
    /// events whose every hard check passed
    #[serde(default)]
    pub events_fully_verified: u64,
    pub mainnet_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct VenueReport {
    /// mainnet_verified | MISMATCH | insufficient_evidence | disabled
    pub status: String,
    pub reason: String,
    pub unique_signatures: u64,
    pub events_decoded: u64,
    pub trade_events: u64,
    pub duplicate_emissions_merged: u64,
    pub events_fully_verified: u64,
    pub events_with_failures: u64,
    pub events_inconclusive: u64,
    pub checks_passed: u64,
    pub checks_failed: u64,
    pub checks_inconclusive: u64,
    pub failures_by_check: BTreeMap<String, u64>,
    pub inconclusive_by_check: BTreeMap<String, u64>,
    pub fee_hypotheses: BTreeMap<String, u64>,
    pub tx_versions: BTreeMap<String, u64>,
    /// a few representative failures (signature prefix, check, detail)
    pub examples: Vec<String>,
    pub provenance: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct VerificationSummary {
    pub generated_us: i64,
    pub files: u64,
    pub min_events_for_verified: u64,
    pub items: BTreeMap<String, ItemStatus>,
    pub failures: Vec<String>,
    /// venue → true when the venue's status is mainnet_verified
    pub venues_mainnet_verified: BTreeMap<String, bool>,
    #[serde(default)]
    pub venues: BTreeMap<String, VenueReport>,
    #[serde(default)]
    pub failed_transactions_skipped: u64,
    #[serde(default)]
    pub unparsable: u64,
}

pub const TRADED_ITEMS: &[(&str, &[&str])] = &[
    ("pump_bonding_curve", &["pump_bonding_curve/TradeEvent"]),
    ("pumpswap", &["pumpswap/BuyEvent", "pumpswap/SellEvent"]),
];

fn venue_of_item(item: &str) -> &str {
    item.split('/').next().unwrap_or("")
}

pub fn summarize(results: &[TxVerification], min_events: u64) -> VerificationSummary {
    let mut s = VerificationSummary { generated_us: crate::time::wall_us(), files: results.len() as u64, min_events_for_verified: min_events, ..Default::default() };
    for venue in ["pump_bonding_curve", "pumpswap"] {
        s.venues.insert(venue.into(), VenueReport { provenance: provenance_of(venue).into(), ..Default::default() });
    }
    // per venue: (fully verified buys, fully verified sells)
    let mut sides: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for r in results {
        if r.tx_failed {
            s.failed_transactions_skipped += 1;
            continue;
        }
        if r.checks.iter().any(|c| c.name == "parse") {
            s.unparsable += 1;
            s.failures.push(format!("{} parse — {}", r.signature, r.checks[0].detail));
            continue;
        }
        let mut venues_in_tx: HashSet<&str> = HashSet::new();
        let mut seen_tx: HashSet<&str> = HashSet::new();
        for (i, item) in r.items.iter().enumerate() {
            let venue = venue_of_item(item);
            venues_in_tx.insert(venue);
            let e = s.items.entry(item.clone()).or_default();
            e.events += 1;
            if seen_tx.insert(item) {
                e.transactions += 1;
            }
            let (p, f, n) = r.event_outcomes(i);
            e.hard_checks_passed += p;
            e.hard_checks_failed += f;
            e.hard_checks_inconclusive += n;
            if p > 0 && f == 0 && n == 0 {
                e.events_fully_verified += 1;
            }
            if let Some(vr) = s.venues.get_mut(venue) {
                vr.events_decoded += 1;
                vr.checks_passed += p;
                vr.checks_failed += f;
                vr.checks_inconclusive += n;
                let traded = TRADED_ITEMS.iter().any(|(_, its)| its.contains(&item.as_str()));
                if traded {
                    vr.trade_events += 1;
                    if p > 0 && f == 0 && n == 0 {
                        vr.events_fully_verified += 1;
                        let sd = sides.entry(venue.to_string()).or_default();
                        if r.classification.get(i).is_some_and(|c| c.ends_with("buy")) {
                            sd.0 += 1;
                        } else {
                            sd.1 += 1;
                        }
                    } else if f > 0 {
                        vr.events_with_failures += 1;
                    } else {
                        vr.events_inconclusive += 1;
                    }
                }
                for c in r.checks.iter().filter(|c| c.event_index == i && c.hard) {
                    match c.outcome {
                        Outcome::Fail => {
                            *vr.failures_by_check.entry(c.name.clone()).or_default() += 1;
                            if vr.examples.len() < 5 && !vr.examples.iter().any(|x| x.contains(&format!(" {} ", c.name))) {
                                vr.examples.push(format!("{}… {} {} — {}", &r.signature[..8.min(r.signature.len())], c.name, item, c.detail));
                            }
                        }
                        Outcome::Inconclusive => *vr.inconclusive_by_check.entry(c.name.clone()).or_default() += 1,
                        Outcome::Pass => {}
                    }
                }
            }
            for c in r.checks.iter().filter(|c| c.event_index == i && c.hard && c.outcome == Outcome::Fail) {
                s.failures.push(format!("{} {}: {} — {}", r.signature, item, c.name, c.detail));
            }
        }
        for venue in venues_in_tx {
            if let Some(vr) = s.venues.get_mut(venue) {
                vr.unique_signatures += 1;
                vr.duplicate_emissions_merged += r.duplicates_merged as u64;
                *vr.tx_versions.entry(r.version.clone()).or_default() += 1;
                for h in &r.fee_hypotheses {
                    *vr.fee_hypotheses.entry(h.clone()).or_default() += 1;
                }
            }
        }
    }
    for e in s.items.values_mut() {
        e.mainnet_verified = e.events_fully_verified >= min_events && e.hard_checks_failed == 0;
    }
    for (venue, vr) in s.venues.iter_mut() {
        let (buys, sells) = sides.get(venue).copied().unwrap_or_default();
        (vr.status, vr.reason) = if vr.checks_failed > 0 {
            ("MISMATCH".into(), format!("{} hard check(s) failed; see failures_by_check", vr.checks_failed))
        } else if vr.events_fully_verified >= min_events && buys > 0 && sells > 0 {
            ("mainnet_verified".into(), format!("{} trade events passed every hard check ({buys} buys, {sells} sells), none failed", vr.events_fully_verified))
        } else {
            (
                "insufficient_evidence".into(),
                format!("{} fully verified trade events ({buys} buys, {sells} sells); need ≥ {min_events} incl. at least one buy and one sell; {} inconclusive", vr.events_fully_verified, vr.events_inconclusive),
            )
        };
    }
    s.venues.insert(
        "raydium_amm_v4".into(),
        VenueReport { status: "disabled".into(), reason: "decode-only; never traded".into(), provenance: "ray_log layout from public program source; unverified".into(), ..Default::default() },
    );
    for (venue, _) in TRADED_ITEMS {
        let ok = s.venues.get(*venue).is_some_and(|v| v.status == "mainnet_verified");
        s.venues_mainnet_verified.insert(venue.to_string(), ok);
    }
    s
}

fn provenance_of(venue: &str) -> &'static str {
    match venue {
        "pump_bonding_curve" => "events + instruction layouts: official IDL @pump-fun/pump-sdk@4.0.0 (fixtures/idl); fee math: SDK fees.ts",
        "pumpswap" => "events + instruction layouts: official IDL @pump-fun/pump-swap-sdk@2.1.0 (fixtures/idl); fee math: SDK util.ts/buy.ts",
        _ => "",
    }
}

/// Load a verification summary written by `sniper verify-fixtures` (None if absent).
pub fn load_summary(path: &std::path::Path) -> Option<VerificationSummary> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Where the current verification summary is written / read.
pub const SUMMARY_PATHS: &[&str] = &["../reports/verification/verification.json", "../fixtures/real/verification.json"];

/// First existing summary among `SUMMARY_PATHS`.
pub fn load_default_summary() -> Option<VerificationSummary> {
    // the legacy location is only trusted when written by this version (per-venue reports present)
    SUMMARY_PATHS.iter().find_map(|p| load_summary(std::path::Path::new(p)).filter(|s| !s.venues.is_empty()))
}
