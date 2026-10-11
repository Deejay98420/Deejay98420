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
    /// emitting instruction of the event this check belongs to ("ix 3.1 buy")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
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
    /// observations that are recorded but not counted as checks (e.g. extra vault funding)
    #[serde(default)]
    pub notes: Vec<String>,
    /// checks whose balance-only evidence reconciles exactly but whose accounts are also named by a
    /// calling (wrapper) instruction; counted as PASS only with --accept-ancestor-scoped
    #[serde(default)]
    pub reconciled_via_ancestor: Vec<String>,
    /// per event: emitting instruction variant ("" if unresolved) and whether the curve is quoted in
    /// a non-SOL token (such trades are verified but never traded)
    #[serde(default)]
    pub event_instruction: Vec<String>,
    #[serde(default)]
    pub token_quoted: Vec<bool>,
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

fn hex8(d: [u8; 8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_event_cpi(ix: &crate::txmodel::Ix) -> bool {
    ix.data.as_ref().is_some_and(|d| d.len() >= 8 && d[..8] == EVENT_IX_TAG)
}

/// Resolve the emitting instruction of one event:
/// 1. self-CPI channel: the event's inner instruction is a child of the emitting instruction, so the
///    parent is the nearest preceding instruction one call level up (exact, works at any CPI depth);
/// 2. log channel: the n-th invocation of the program inside the top-level instruction (from the
///    log invoke records, self-CPIs excluded) is the n-th instruction of that program in the
///    instruction tree.
fn map_one(m: &TxModel, d: &crate::decode::Decoded) -> Mapping {
    let Some(class) = event_class(&d.event) else { return Err("event type is not tied to an instruction".into()) };
    let program = event_program(&d.event);
    let Some(o) = d.provenance.outer_ix else { return Err("no instruction provenance (logs without invoke records)".into()) };
    let group: Vec<usize> = (0..m.ixs.len()).filter(|&i| m.ixs[i].outer == o).collect();
    let parent = d.provenance.inner_pos.and_then(|p| {
        let e = group.iter().copied().find(|&i| m.ixs[i].inner_pos == Some(p))?;
        let h = m.ixs[e].stack_height.filter(|h| *h >= 2)?;
        group.iter().copied().filter(|&i| i < e).rev().find(|&i| m.ixs[i].stack_height == Some(h - 1))
    });
    let (i, via) = if let Some(pi) = parent {
        (pi, "self-CPI parent")
    } else if let Some(n) = d.provenance.invoke_ordinal {
        let calls: Vec<usize> = group.iter().copied().filter(|&i| m.ixs[i].program_id == program && !is_event_cpi(&m.ixs[i])).collect();
        match calls.get(n as usize) {
            Some(&i) => (i, "log invoke order"),
            None => return Err(format!("log invoke #{n} of the program has no matching instruction in top-level instruction {o}")),
        }
    } else {
        return Err("neither a self-CPI position (with call depth) nor a log invoke order is available".into());
    };
    let ix = &m.ixs[i];
    if ix.program_id != program {
        return Err(format!("{via} resolved to {} of another program ({})", m.ix_ref(i), short(&ix.program_id)));
    }
    let Some(disc) = ix.disc() else { return Err(format!("{} has no instruction data", m.ix_ref(i))) };
    let Some(def) = ixlayout::lookup(program, disc) else {
        return Err(format!("{} ({via}): unrecognized instruction discriminator {} — not in the official IDL subset", m.ix_ref(i), hex8(disc)));
    };
    match ix_class(program, &def.name) {
        Some(c) if c == class => Ok((i, def)),
        Some("pump_trade_multi") => Err(format!("{} multi_hop_curve_swap emits several trades per instruction; per-trade accounts are not in the instruction", m.ix_ref(i))),
        _ => Err(format!("{} ({via}) is `{}`, not an instruction of class {class}", m.ix_ref(i), def.name)),
    }
}

fn map_events(m: &TxModel, events: &[crate::decode::Decoded]) -> Vec<Mapping> {
    let mut out: Vec<Mapping> = events.iter().map(|d| map_one(m, d)).collect();
    // one instruction emits one event of its class; anything else is ambiguous
    let mut used: HashMap<(usize, &'static str), Vec<usize>> = HashMap::new();
    for (k, r) in out.iter().enumerate() {
        if let (Ok((i, _)), Some(c)) = (r, event_class(&events[k].event)) {
            used.entry((*i, c)).or_default().push(k);
        }
    }
    for ((i, _), ks) in used {
        if ks.len() > 1 {
            for k in ks.iter() {
                out[*k] = Err(format!("{} events attributed to the same instruction {}", ks.len(), m.ix_ref(i)));
            }
        }
    }
    out
}

/// Official Pump.fun fee-recipient lists (pump-sdk bondingCurve.ts, with provenance).
pub const PUMP_FEE_RECIPIENTS: &str = include_str!("../../fixtures/idl/pump.fee-recipients.json");

fn buyback_recipients() -> &'static Vec<String> {
    static L: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    L.get_or_init(|| {
        let v: Value = serde_json::from_str(PUMP_FEE_RECIPIENTS).expect("bundled fee recipient JSON");
        v["buyback_fee_recipients"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
    })
}

/// Lamport credit evidence for one account relative to one trade instruction.
struct Credit {
    /// parsed System transfers into the account inside the instruction's subtree
    parsed: Vec<(usize, i128)>,
    balance: Option<i128>,
    /// instructions outside the trade instruction (and its CPIs) that name the account
    others: Vec<usize>,
}

fn credit(m: &TxModel, acct: &str, ix: usize, sub: &std::ops::Range<usize>) -> Credit {
    Credit {
        parsed: m.lamport_credits_in(acct, sub),
        balance: m.lamport_delta(acct),
        others: m.referencing(acct).into_iter().filter(|r| *r != ix && !sub.contains(r)).collect(),
    }
}

fn refs(m: &TxModel, v: &[usize]) -> String {
    v.iter().map(|i| m.ix_ref(*i)).collect::<Vec<_>>().join(", ")
}

/// Protocol fee: every recipient bound to the trade instruction (its `fee_recipient` account and any
/// buyback recipient — an instruction account, or a remaining account listed in the official
/// `Global.buybackFeeRecipients`) is credited inside that instruction; the credits must add up to
/// the decoded fee exactly. Evidence: parsed System transfers inside the instruction, or — when there
/// are none (sells pay from the program-owned curve) — balance deltas of accounts no other
/// instruction names. Anything weaker is inconclusive.
#[allow(clippy::too_many_arguments)]
fn fee_route_check(m: &TxModel, o: &mut Out, i: usize, ix: usize, def: &IxDef, t: &crate::decode::pumpfun::PumpTrade, notes: &mut Vec<String>, routes: &mut Vec<String>, recon: &mut Vec<String>) {
    let accts = &m.ixs[ix].accounts;
    let name = &def.name;
    let Some(sub) = m.subtree(ix) else {
        return o.add(i, "trade.protocol_fee_observed", Outcome::Inconclusive, true, format!("{}: call depth unavailable", m.ix_ref(ix)));
    };
    let Some(fr) = def.get(accts, "fee_recipient") else {
        return o.add(i, "trade.protocol_fee_observed", Outcome::Inconclusive, true, format!("{name}: no fee_recipient account in this instruction layout"));
    };
    let mut recips: Vec<(String, String)> = vec![("fee_recipient".into(), fr.to_string())];
    if let Some(bb) = def.get(accts, "buyback_fee_recipient") {
        recips.push(("buyback_fee_recipient (instruction account)".into(), bb.to_string()));
    }
    for (k, a) in accts.iter().enumerate().skip(def.accounts.len()) {
        if buyback_recipients().contains(a) && !recips.iter().any(|r| &r.1 == a) {
            recips.push((format!("buyback recipient (remaining account #{}, official Global.buybackFeeRecipients)", k - def.accounts.len()), a.clone()));
        }
    }
    let fee = t.fee.unwrap_or(0) as i128;
    let credits: Vec<Credit> = recips.iter().map(|(_, a)| credit(m, a, ix, &sub)).collect();
    let parsed_total: i128 = credits.iter().flat_map(|c| c.parsed.iter().map(|x| x.1)).sum();
    let any_parsed = credits.iter().any(|c| !c.parsed.is_empty());
    let exclusive = credits.iter().all(|c| c.others.is_empty() && c.balance.is_some());
    let bal: i128 = credits.iter().map(|c| c.balance.unwrap_or(0)).sum();
    let split = |parsed: bool| {
        recips
            .iter()
            .zip(&credits)
            .map(|((l, a), c)| format!("{l} {}: {}", short(a), if parsed { c.parsed.iter().map(|x| x.1).sum::<i128>() } else { c.balance.unwrap_or(0) }))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let here = format!("{} {name}", m.ix_ref(ix));
    let others: Vec<usize> = credits.iter().flat_map(|c| c.others.clone()).collect();
    if let Some(tail) = t.tail.as_ref() {
        if recips.len() > 1 {
            let bb_obs: i128 = credits[1..].iter().map(|c| if c.parsed.is_empty() { c.balance.unwrap_or(0) } else { c.parsed.iter().map(|x| x.1).sum() }).sum();
            o.add(i, "trade.buyback_fee_field", Outcome::Pass, false, format!("event buyback_fee {} ({} bps) vs buyback recipient credit {bb_obs} (information only)", tail.buyback_fee, tail.buyback_fee_basis_points));
        }
    }
    // parsed credits must also agree with the balance change of any recipient no other instruction names
    let inconsistent: Vec<String> = recips
        .iter()
        .zip(&credits)
        .filter(|(_, c)| c.others.is_empty() && !c.parsed.is_empty() && c.balance.is_some_and(|b| b != c.parsed.iter().map(|x| x.1).sum::<i128>()))
        .map(|((l, a), c)| format!("{l} {}: parsed {} vs balance delta {:?}", short(a), c.parsed.iter().map(|x| x.1).sum::<i128>(), c.balance))
        .collect();
    let (outcome, detail, route) = if !inconsistent.is_empty() {
        (Outcome::Fail, format!("parsed transfers disagree with balance evidence: {}", inconsistent.join("; ")), String::new())
    } else if any_parsed && parsed_total == fee {
        (Outcome::Pass, format!("decoded fee {fee} = {} [parsed System transfers inside {here}]", split(true)), format!("{}_recipient(s):parsed_transfers", recips.len()))
    } else if exclusive && bal == fee {
        (Outcome::Pass, format!("decoded fee {fee} = {} [balance deltas; accounts named only by {here}]", split(false)), format!("{}_recipient(s):exclusive_balance", recips.len()))
    } else if any_parsed && (parsed_total > fee || exclusive) {
        (Outcome::Fail, format!("decoded fee {fee} vs parsed credits {parsed_total} = {} inside {here}", split(true)), String::new())
    } else if exclusive {
        (Outcome::Fail, format!("decoded fee {fee} vs balance deltas {bal} = {} (accounts named only by {here})", split(false)), String::new())
    } else if any_parsed {
        (
            Outcome::Inconclusive,
            format!("parsed credits {parsed_total} cover only part of decoded fee {fee} ({}); the rest is balance-only and the accounts are also named by {}", split(true), refs(m, &others)),
            String::new(),
        )
    } else {
        // balance-only evidence: attributable only within the scope rules (strict by default)
        let sc = recips.iter().map(|(_, a)| scope_of(m, a, ix)).fold(Scope::Exclusive, combine);
        let (oc, d) = balance_verdict(m, &sc, bal, fee, &format!("decoded fee {fee} vs recipient credits {} inside {here}", split(false)), "trade.protocol_fee_observed", recon);
        let route = if oc == Outcome::Pass { format!("{}_recipient(s):ancestor_reconciled", recips.len()) } else { String::new() };
        (oc, d, route)
    };
    if !route.is_empty() {
        routes.push(route);
    }
    if outcome != Outcome::Pass && recips.len() == 1 {
        notes.push(format!("{here}: no buyback recipient found among the instruction's remaining accounts"));
    }
    o.add(i, "trade.protocol_fee_observed", outcome, true, detail);
}

/// Creator fee: the creator-fee credit inside the trade instruction is compared with the decoded
/// creator fee; other transfers into the vault are reported separately (never counted as fee).
#[allow(clippy::too_many_arguments)]
fn creator_fee_check(m: &TxModel, o: &mut Out, i: usize, ix: usize, def: &IxDef, t: &crate::decode::pumpfun::PumpTrade, notes: &mut Vec<String>, recon: &mut Vec<String>) {
    let accts = &m.ixs[ix].accounts;
    let name = &def.name;
    let Some(cv) = def.get(accts, "creator_vault") else {
        return o.add(i, "trade.creator_fee_observed", Outcome::Inconclusive, true, format!("{name}: no creator_vault account in this instruction layout"));
    };
    let Some(sub) = m.subtree(ix) else {
        return o.add(i, "trade.creator_fee_observed", Outcome::Inconclusive, true, format!("{}: call depth unavailable", m.ix_ref(ix)));
    };
    let creator_set = t.creator.is_some_and(|c| c != Pubkey::default());
    let expected = if creator_set { t.creator_fee.unwrap_or(0) as i128 } else { 0 };
    let cashback = t.tail.as_ref().is_some_and(|x| x.cashback > 0);
    let c = credit(m, cv, ix, &sub);
    let here = format!("{} {name}", m.ix_ref(ix));
    let total: i128 = c.parsed.iter().map(|x| x.1).sum();
    let (outcome, detail) = if !c.parsed.is_empty() && c.others.is_empty() && c.balance.is_some_and(|b| b != total) {
        (Outcome::Fail, format!("creator vault {}: parsed credits {total} disagree with balance delta {:?}", short(cv), c.balance))
    } else if !c.parsed.is_empty() {
        if total == expected {
            (Outcome::Pass, format!("creator vault {}: parsed credits {total} = decoded creator fee {expected} inside {here}", short(cv)))
        } else if let Some(pos) = c.parsed.iter().position(|x| x.1 == expected).filter(|_| expected > 0) {
            let extra: Vec<String> = c.parsed.iter().enumerate().filter(|(k, _)| *k != pos).map(|(_, (ix2, a))| format!("{a} lamports ({})", m.ix_ref(*ix2))).collect();
            notes.push(format!("{here}: creator vault {} received additional funding {} besides the creator fee — purpose not established, not counted as fee", short(cv), extra.join(", ")));
            (Outcome::Pass, format!("creator vault {}: creator-fee transfer {expected} ({}) = decoded creator fee; additional vault funding {} tracked separately", short(cv), m.ix_ref(c.parsed[pos].0), extra.join(", ")))
        } else if cashback {
            (Outcome::Inconclusive, format!("creator vault {}: parsed credits {total} vs decoded {expected}; cashback coin — routing undocumented", short(cv)))
        } else {
            (Outcome::Fail, format!("creator vault {}: parsed credits {:?} inside {here}, none equals decoded creator fee {expected}", short(cv), c.parsed.iter().map(|x| x.1).collect::<Vec<_>>()))
        }
    } else if let (true, Some(b)) = (c.others.is_empty(), c.balance) {
        match (b == expected, cashback) {
            (true, _) => (Outcome::Pass, format!("creator vault {}: balance delta {b} = decoded creator fee {expected} [account named only by {here}]", short(cv))),
            (false, true) => (Outcome::Inconclusive, format!("creator vault {}: balance delta {b} vs decoded {expected}; cashback coin — routing undocumented", short(cv))),
            (false, false) => (Outcome::Fail, format!("creator vault {}: balance delta {b} vs decoded creator fee {expected} (account named only by {here})", short(cv))),
        }
    } else if let Some(b) = c.balance {
        if cashback && b != expected {
            (Outcome::Inconclusive, format!("creator vault {}: balance delta {b} vs decoded {expected}; cashback coin — routing undocumented", short(cv)))
        } else {
            balance_verdict(m, &scope_of(m, cv, ix), b, expected, &format!("creator vault {} vs decoded creator fee", short(cv)), "trade.creator_fee_observed", recon)
        }
    } else {
        (Outcome::Inconclusive, format!("creator vault {}: no balance evidence", short(cv)))
    };
    o.add(i, "trade.creator_fee_observed", outcome, true, detail);
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
}

struct Out<'a> {
    checks: &'a mut Vec<Check>,
}

impl Out<'_> {
    fn add(&mut self, i: usize, name: &str, outcome: Outcome, hard: bool, detail: String) {
        self.checks.push(Check { event_index: i, name: name.into(), ok: outcome == Outcome::Pass, outcome, hard, detail, instruction: None });
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
        v.checks.push(Check { event_index: 0, name: "parse".into(), outcome: Outcome::Fail, ok: false, hard: true, detail: format!("unparsable transaction: {:?}", v.structure_problems), instruction: None });
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
    let (mut curve_vault, mut user_ata) = (BTreeMap::new(), BTreeMap::new());
    let mut mints: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, d) in events.iter().enumerate() {
        v.event_instruction.push(mapping[i].as_ref().map(|(_, def)| def.name.clone()).unwrap_or_default());
        v.token_quoted.push(matches!(&d.event, VenueEvent::Pump(PumpEvent::Trade(t)) if !t.is_sol_quoted()));
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
                if let Ok((ix, def)) = mp {
                    // another trade in this transaction on the same curve makes curve balances non-attributable
                    let curve = def.get(&m.ixs[*ix].accounts, "bonding_curve");
                    let shared_curve = mapping.iter().enumerate().any(|(k, mk)| {
                        k != i && matches!(events[k].event, VenueEvent::Pump(PumpEvent::Trade(_))) && mk.as_ref().is_ok_and(|(ix2, d2)| ix2 != ix && d2.get(&m.ixs[*ix2].accounts, "bonding_curve") == curve)
                    });
                    let curve_ok = quote_payment_check(&m, &mut o, i, *ix, def, t, shared_curve, &mut v.reconciled_via_ancestor);
                    if def.get(&m.ixs[*ix].accounts, "fee_recipient").is_some() {
                        if t.is_sol_quoted() {
                            fee_route_check(&m, &mut o, i, *ix, def, t, &mut v.notes, &mut v.fee_hypotheses, &mut v.reconciled_via_ancestor);
                            creator_fee_check(&m, &mut o, i, *ix, def, t, &mut v.notes, &mut v.reconciled_via_ancestor);
                        } else {
                            for n in ["trade.protocol_fee_observed", "trade.creator_fee_observed"] {
                                o.add(i, n, Outcome::Inconclusive, true, format!("{}: token-quoted v1/v2 fee routing is not modelled (outside trading scope)", def.name));
                            }
                        }
                    } else {
                        v3_fee_checks(&m, &mut o, i, *ix, def, t, curve_ok, &mut v.fee_hypotheses, &mut v.reconciled_via_ancestor);
                    }
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
                if let Ok((ix, def)) = mp {
                    amm_swap_checks(&m, &mut o, i, *ix, def, s, &mut v.fee_hypotheses);
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
    // ---- supply conservation per mint: Σ balance changes = minted − burned ----
    // Wrapped SOL is not minted or burned: a native token account holds lamports = rent reserve +
    // amount, so wrapping/unwrapping is checked per account as token Δ = lamport Δ − rent Δ.
    for (mint, evs) in &mints {
        let accts: Vec<&String> = m.tokens.iter().filter(|(_, t)| &t.mint == mint).map(|(a, _)| a).collect();
        if mint == crate::types::WSOL_MINT {
            let mut bad = Vec::new();
            let mut unsynced = Vec::new();
            for a in &accts {
                let t = &m.tokens[*a];
                let rent = match (t.pre, t.post) {
                    (None, Some(_)) => TOKEN_ACCOUNT_RENT,
                    (Some(_), None) => -TOKEN_ACCOUNT_RENT,
                    _ => 0,
                };
                match m.lamport_delta(a) {
                    Some(l) if l - rent == t.delta() => {}
                    Some(l) => {
                        // lamports sent without a syncNative are legal (not yet wrapped): evidence gap, not a contradiction
                        let synced_later = m.ixs.iter().any(|x| x.parsed_type.as_deref() == Some("syncNative") && x.info["account"].as_str() == Some(a.as_str()));
                        let credited = m.flows.iter().any(|f| matches!(f, crate::txmodel::Flow::Lamports { to, .. } if to == *a));
                        if credited && !synced_later {
                            unsynced.push(format!("{} (token Δ {}, lamport Δ {l})", short(a), t.delta()));
                        } else {
                            bad.push(format!("{}: token Δ {} vs lamport Δ {l} − rent Δ {rent}", short(a), t.delta()));
                        }
                    }
                    None => unsynced.push(format!("{} (no lamport evidence)", short(a))),
                }
            }
            for &i in evs {
                if !bad.is_empty() {
                    o.add(i, "token.mint_conservation", Outcome::Fail, true, format!("wrapped SOL accounts inconsistent with their lamports: {}", bad.join("; ")));
                } else if !unsynced.is_empty() {
                    o.add(i, "token.mint_conservation", Outcome::Inconclusive, true, format!("wrapped SOL: unsynced lamports / missing evidence for {}", unsynced.join(", ")));
                } else {
                    o.add(i, "token.mint_conservation", Outcome::Pass, true, format!("wrapped SOL: all {} native token accounts satisfy token Δ = lamport Δ − rent Δ (wrap/unwrap, not mint/burn)", accts.len()));
                }
            }
            continue;
        }
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
    for c in checks.iter_mut() {
        c.instruction = match mapping.get(c.event_index) {
            Some(Ok((ix, def))) => Some(format!("{} {} ({})", m.ix_ref(*ix), def.name, short(&m.ixs[*ix].program_id))),
            _ => None,
        };
    }
    v.checks = checks;
    let hard = || v.checks.iter().filter(|c| c.hard);
    v.passed = hard().any(|c| c.outcome == Outcome::Pass) && hard().all(|c| c.outcome != Outcome::Fail) && v.decode_warnings.is_empty();
    v
}


// ------------------------------------------------------------------------------------------------
// PumpSwap swaps: every amount from parsed transfers inside the swap instruction

/// Rent-exempt reserve of a 165-byte SPL token account (lamports): wrapped-SOL accounts hold
/// lamports = reserve + amount.
pub const TOKEN_ACCOUNT_RENT: i128 = 2_039_280;

fn flow_between(m: &TxModel, from: &str, to: &str, range: &std::ops::Range<usize>) -> i128 {
    m.flows
        .iter()
        .filter_map(|f| match f {
            crate::txmodel::Flow::Token { ix, from: Some(a), to: Some(b), amount, .. } if a == from && b == to && range.contains(ix) => Some(*amount),
            _ => None,
        })
        .sum()
}

/// PumpSwap fee routing per the official SDK (`offlinePumpAmm.ts`):
/// * v1 `buy`/`sell`/`buy_exact_quote_in`: fees leave around the pool — protocol fee to
///   `protocol_fee_recipient_token_account` plus the buyback slice to the buyback token account
///   passed as a remaining account right after its (official) buyback wallet; creator fee to
///   `coin_creator_vault_ata`. The quote vault moves by exactly `quote_amount_pool`.
/// * v2: "protocol and coin-creator fees stay in the pool's quote vault … only the buyback slice
///   of the protocol fee is paid in the trade … to a listed buyback fee recipient's quote ATA".
fn amm_swap_checks(m: &TxModel, o: &mut Out, i: usize, ix: usize, def: &IxDef, s: &crate::decode::pumpswap::Swap, routes: &mut Vec<String>) {
    let accts = &m.ixs[ix].accounts;
    let name = &def.name;
    let here = format!("{} {name}", m.ix_ref(ix));
    let names = ["swap.base_transfer", "swap.pool_base_delta", "swap.user_base_delta", "swap.pool_quote_delta", "swap.protocol_fee_observed", "swap.creator_fee_observed"];
    // no inner-instruction record → the transfers inside the swap are unknown: evidence gap, never 0
    let sub = if m.inner_recorded { m.subtree(ix) } else { None };
    let Some(sub) = sub else {
        let why = if m.inner_recorded { "call depth unavailable" } else { "transaction has no inner-instruction record" };
        for n in names {
            o.add(i, n, Outcome::Inconclusive, true, format!("{here}: {why}"));
        }
        return;
    };
    let get = |n: &str| def.get(accts, n).map(String::from);
    let v2 = name.ends_with("_v2");
    let base = s.base_amount as i128;
    // event-named accounts must be the instruction's accounts
    let mut mismatch = Vec::new();
    for (ev, role) in [(&s.user_base_token_account, "user_base_token_account"), (&s.user_quote_token_account, "user_quote_token_account"), (&s.protocol_fee_recipient_token_account, "protocol_fee_recipient_token_account")] {
        if let (Some(e), Some(a)) = (ev.filter(|k| *k != Pubkey::default()), get(role)) {
            if e.to_string() != a {
                mismatch.push(format!("event {role} {} ≠ instruction {}", short(&e.to_string()), short(&a)));
            }
        }
    }
    if !mismatch.is_empty() {
        o.add(i, "swap.event_accounts", Outcome::Fail, true, mismatch.join("; "));
    }
    // base leg
    match (get("user_base_token_account"), get("pool_base_token_account")) {
        (Some(ub), Some(pb)) => {
            let moved = if s.is_buy { flow_between(m, &pb, &ub, &sub) } else { flow_between(m, &ub, &pb, &sub) };
            o.cmp(i, "swap.base_transfer", Ok(moved), base, &format!("base tokens {} inside {here}", if s.is_buy { "pool → user" } else { "user → pool" }));
            o.cmp(i, "swap.pool_base_delta", Ok(token_net_in(m, &pb, &sub)), if s.is_buy { -base } else { base }, &format!("pool base vault {} net inside {here}", short(&pb)));
            o.cmp(i, "swap.user_base_delta", Ok(token_net_in(m, &ub, &sub)), if s.is_buy { base } else { -base }, &format!("user base account {} net inside {here}", short(&ub)));
        }
        _ => {
            for n in ["swap.base_transfer", "swap.pool_base_delta", "swap.user_base_delta"] {
                o.add(i, n, Outcome::Inconclusive, true, format!("{here}: base accounts not in instruction"));
            }
        }
    }
    let (protocol, creator) = (s.protocol_fee as i128, s.coin_creator_fee.unwrap_or(0) as i128);
    let cashback = s.cashback.unwrap_or(0) as i128;
    let bb = s.buyback_fee.map(|x| x as i128);
    let q = s.quote_amount_pool as i128;
    // quote vault movement inside the instruction
    let vault_ok: Option<bool> = match get("pool_quote_token_account") {
        None => {
            o.add(i, "swap.pool_quote_delta", Outcome::Inconclusive, true, format!("{here}: pool quote vault not in instruction"));
            None
        }
        Some(pq) => {
            let net = token_net_in(m, &pq, &sub);
            let signed = if s.is_buy { q } else { -q };
            let (expected, formula) = if !v2 {
                (Some(signed), format!("{}quote_amount_pool {q}", if s.is_buy { "+" } else { "−" }))
            } else {
                match bb {
                    Some(b) => (Some(signed + protocol + creator - b), format!("{}quote_amount_pool {q} + protocol fee {protocol} + creator fee {creator} − buyback slice {b} (v2: fees stay in the vault)", if s.is_buy { "+" } else { "−" })),
                    None => (None, String::new()),
                }
            };
            match expected {
                None => {
                    o.add(i, "swap.pool_quote_delta", Outcome::Inconclusive, true, format!("{here}: older event layout without buyback_fee"));
                    None
                }
                Some(e) if e == net => {
                    o.add(i, "swap.pool_quote_delta", Outcome::Pass, true, format!("pool quote vault {} net {net} = {formula} inside {here}", short(&pq)));
                    Some(true)
                }
                Some(e) if cashback != 0 => {
                    o.add(i, "swap.pool_quote_delta", Outcome::Inconclusive, true, format!("pool quote vault {} net {net} vs {formula} = {e}; cashback {cashback} — routing undocumented", short(&pq)));
                    None
                }
                Some(e) => {
                    o.add(i, "swap.pool_quote_delta", Outcome::Fail, true, format!("pool quote vault {} net {net} vs {formula} = {e} (diff {}) inside {here}", short(&pq), net - e));
                    Some(false)
                }
            }
        }
    };
    if v2 {
        // buyback slice to the instruction's buyback_fee_recipient (a quote token account whose owner
        // must be an official buyback wallet); the rest of the protocol fee and the creator fee stay
        // in the vault and are proven by the vault identity
        let (pf, cf) = match (get("buyback_fee_recipient"), bb) {
            (Some(bta), Some(b)) => {
                let owner = m.tokens.get(&bta).and_then(|t| t.owner.clone());
                let official = owner.as_ref().is_some_and(|w| buyback_recipients().contains(w));
                let got: i128 = token_credits_in(m, &bta, &sub).iter().map(|x| x.1).sum();
                let pf = if b != 0 && !official {
                    (Outcome::Fail, format!("buyback slice {b} paid to {} whose owner {} is not an official buyback wallet", short(&bta), owner.as_deref().map(short).unwrap_or("?".into())))
                } else if got != b {
                    (Outcome::Fail, format!("buyback token account {}: parsed credit {got} vs event buyback_fee {b} inside {here}", short(&bta)))
                } else {
                    match vault_ok {
                        Some(true) => {
                            routes.push("amm_v2:buyback_parsed+retained_in_vault".into());
                            (Outcome::Pass, format!("protocol fee {protocol} = buyback slice {b} ({} bps; parsed to official {}'s ATA {}) + {} retained in the pool vault (vault identity proven)", s.buyback_fee_bps.unwrap_or(0), owner.as_deref().map(short).unwrap_or_default(), short(&bta), protocol - b))
                        }
                        Some(false) => (Outcome::Fail, format!("buyback slice {b} matches, but the pool vault identity failed")),
                        None => (Outcome::Inconclusive, format!("buyback slice {b} matches, but the pool vault identity is not proven")),
                    }
                };
                let cf = match vault_ok {
                    Some(true) => (Outcome::Pass, format!("creator fee {creator} retained in the pool quote vault (v2; SDK Pool.creatorFees) — included exactly in the vault identity inside {here}")),
                    _ => (Outcome::Inconclusive, format!("creator fee {creator} stays in the vault in v2; the vault identity is not proven")),
                };
                (pf, cf)
            }
            _ => (
                (Outcome::Inconclusive, format!("{here}: buyback account or event buyback_fee missing")),
                (Outcome::Inconclusive, format!("{here}: buyback account or event buyback_fee missing")),
            ),
        };
        o.add(i, "swap.protocol_fee_observed", pf.0, true, pf.1);
        o.add(i, "swap.creator_fee_observed", cf.0, true, cf.1);
        return;
    }
    // v1: protocol fee = fee recipient token account + buyback token account (SDK remaining accounts:
    // [cashback…] [pool_v2…] buyback wallet, buyback token account)
    let pf = match get("protocol_fee_recipient_token_account") {
        None => (Outcome::Inconclusive, format!("{here}: no protocol fee token account in the instruction")),
        Some(pfta) => {
            let rem: Vec<&String> = accts.iter().skip(def.accounts.len()).collect();
            let bta = rem.iter().enumerate().find_map(|(k, w)| {
                if !buyback_recipients().contains(*w) {
                    return None;
                }
                rem.iter().skip(k + 1).find(|t| m.tokens.get(t.as_str()).and_then(|x| x.owner.as_deref()) == Some(w.as_str())).map(|t| ((*w).clone(), (*t).clone()))
            });
            let cp: i128 = token_credits_in(m, &pfta, &sub).iter().map(|x| x.1).sum();
            let cb: i128 = bta.as_ref().map(|(_, t)| token_credits_in(m, t, &sub).iter().map(|x| x.1).sum()).unwrap_or(0);
            let split = format!(
                "protocol fee token account {}: {cp}{}",
                short(&pfta),
                bta.as_ref().map(|(w, t)| format!(" + buyback token account {} (owner {}, official buyback wallet, remaining account): {cb}", short(t), short(w))).unwrap_or_default()
            );
            if cp == 0 && cb == 0 && protocol != 0 {
                (Outcome::Inconclusive, format!("{here}: no parsed fee transfer"))
            } else if cp + cb != protocol {
                (Outcome::Fail, format!("decoded protocol fee {protocol} vs parsed {} = {split} inside {here}", cp + cb))
            } else if bb.is_some_and(|b| b != cb) {
                (Outcome::Fail, format!("total matches ({split}) but buyback credit {cb} ≠ event buyback_fee {}", bb.unwrap()))
            } else {
                routes.push(format!("amm_v1:{}", if bta.is_some() { "fee_recipient+buyback_parsed" } else { "fee_recipient_parsed" }));
                (Outcome::Pass, format!("decoded protocol fee {protocol} = {split}{} [parsed transfers inside {here}]", bb.map(|b| format!("; event buyback_fee {b} ({} bps)", s.buyback_fee_bps.unwrap_or(0))).unwrap_or_default()))
            }
        }
    };
    o.add(i, "swap.protocol_fee_observed", pf.0, true, pf.1);
    let cf = match get("coin_creator_vault_ata") {
        None if creator == 0 => (Outcome::Pass, "no coin creator fee".to_string()),
        None => (Outcome::Inconclusive, format!("{here}: no coin_creator_vault_ata account")),
        Some(cva) => {
            let got: i128 = token_credits_in(m, &cva, &sub).iter().map(|x| x.1).sum();
            if got == creator {
                (Outcome::Pass, format!("coin creator vault ATA {}: parsed credit {got} = decoded creator fee inside {here}", short(&cva)))
            } else {
                (Outcome::Fail, format!("coin creator vault ATA {}: parsed credit {got} vs decoded creator fee {creator} inside {here}", short(&cva)))
            }
        }
    };
    o.add(i, "swap.creator_fee_observed", cf.0, true, cf.1);
}

// ------------------------------------------------------------------------------------------------
// Attribution scope of balance-only evidence

static ACCEPT_ANCESTOR_SCOPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Count exact balance reconciliations whose accounts are also named by a calling (wrapper)
/// instruction as PASS. Off by default (strict): they are reported as INCONCLUSIVE.
pub fn set_accept_ancestor_scoped(on: bool) {
    ACCEPT_ANCESTOR_SCOPED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn accept_ancestor_scoped() -> bool {
    ACCEPT_ANCESTOR_SCOPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Which instructions besides the trade instruction (and its own CPIs) name an account.
#[derive(Debug, Clone)]
enum Scope {
    /// only the trade instruction and its CPIs
    Exclusive,
    /// additionally the instruction(s) that called it (a wrapper must pass the account down, and
    /// could in principle move lamports itself)
    Ancestor(Vec<usize>),
    /// unrelated instructions (other swaps, other top-level instructions)
    Shared(Vec<usize>),
}

fn scope_of(m: &TxModel, acct: &str, ix: usize) -> Scope {
    let Some(sub) = m.subtree(ix) else { return Scope::Shared(vec![]) };
    let anc = m.ancestors(ix);
    let others: Vec<usize> = m.referencing(acct).into_iter().filter(|r| *r != ix && !sub.contains(r)).collect();
    if others.is_empty() {
        Scope::Exclusive
    } else if others.iter().all(|r| anc.contains(r)) {
        Scope::Ancestor(others)
    } else {
        Scope::Shared(others)
    }
}

fn combine(a: Scope, b: Scope) -> Scope {
    match (a, b) {
        (Scope::Shared(mut x), Scope::Shared(y)) => {
            x.extend(y);
            Scope::Shared(x)
        }
        (Scope::Shared(x), _) | (_, Scope::Shared(x)) => Scope::Shared(x),
        (Scope::Ancestor(mut x), Scope::Ancestor(y)) => {
            x.extend(y);
            Scope::Ancestor(x)
        }
        (Scope::Ancestor(x), _) | (_, Scope::Ancestor(x)) => Scope::Ancestor(x),
        _ => Scope::Exclusive,
    }
}

/// Verdict for balance-only evidence of `what` (observed vs expected, exact integers).
fn balance_verdict(m: &TxModel, scope: &Scope, observed: i128, expected: i128, what: &str, name: &str, recon: &mut Vec<String>) -> (Outcome, String) {
    let dedup = |v: &[usize]| {
        let mut v = v.to_vec();
        v.sort();
        v.dedup();
        refs(m, &v)
    };
    match scope {
        Scope::Exclusive if observed == expected => (Outcome::Pass, format!("{what}: balance Δ {observed} = {expected} [accounts named only by this instruction and its CPIs]")),
        Scope::Exclusive => (Outcome::Fail, format!("{what}: balance Δ {observed} vs {expected} (diff {}) [accounts named only by this instruction and its CPIs]", observed - expected)),
        Scope::Ancestor(a) if observed == expected => {
            recon.push(name.to_string());
            if accept_ancestor_scoped() {
                (Outcome::Pass, format!("{what}: balance Δ {observed} = {expected} [also named by calling instruction(s) {}; exact reconciliation accepted]", dedup(a)))
            } else {
                (Outcome::Inconclusive, format!("{what}: balance Δ {observed} reconciles exactly with {expected}, but calling instruction(s) {} also name the account(s) — strict attribution (--accept-ancestor-scoped counts it)", dedup(a)))
            }
        }
        Scope::Ancestor(a) => (Outcome::Inconclusive, format!("{what}: balance Δ {observed} vs {expected} (diff {}); calling instruction(s) {} also name the account(s), so the difference is not attributable", observed - expected, dedup(a))),
        Scope::Shared(o) => (Outcome::Inconclusive, format!("{what}: balance Δ {observed} vs {expected}; also named by unrelated instruction(s) {} — not attributable to this trade", dedup(o))),
    }
}

/// Parsed token credits into `acct` inside `range` (instruction index, amount).
fn token_credits_in(m: &TxModel, acct: &str, range: &std::ops::Range<usize>) -> Vec<(usize, i128)> {
    m.flows
        .iter()
        .filter_map(|f| match f {
            crate::txmodel::Flow::Token { ix, to: Some(t), amount, .. } if t == acct && range.contains(ix) => Some((*ix, *amount)),
            _ => None,
        })
        .collect()
}

fn token_net_in(m: &TxModel, acct: &str, range: &std::ops::Range<usize>) -> i128 {
    m.token_flow_sum(acct, |ix| range.contains(&ix))
}

/// The trade's quote payment into the bonding curve, tied to the trade instruction.
///
/// * v1/v2 layouts (with `fee_recipient` / `creator_vault` accounts): fees are paid to those
///   accounts, so the curve receives exactly the base amount (`sol_amount`).
/// * v3 layouts (no fee accounts): per the official SDK (`state.ts`), protocol fees net of the
///   buyback slice and creator fees accrue ON the curve; the buyback slice is paid out by the trade.
///   The gross payment into the curve is therefore base + fee + creator fee − any buyback slice the
///   payer transfers directly to the buyback recipient inside the instruction.
///
/// Evidence order: parsed transfers inside the instruction (exact); otherwise balance deltas, which
/// count only with an attributable scope. Returns Some(true) when the payment identity is proven.
#[allow(clippy::too_many_arguments)]
fn quote_payment_check(m: &TxModel, o: &mut Out, i: usize, ix: usize, def: &IxDef, t: &crate::decode::pumpfun::PumpTrade, shared_curve: bool, recon: &mut Vec<String>) -> Option<bool> {
    let accts = &m.ixs[ix].accounts;
    let legacy = def.get(accts, "fee_recipient").is_some();
    let v3 = !legacy && def.name.ends_with("_v3");
    let sol = t.is_sol_quoted();
    let name = if sol { "trade.sol_amount" } else { "trade.quote_amount" };
    let here = format!("{} {}", m.ix_ref(ix), def.name);
    let tail = t.tail.as_ref();
    let Some(q) = (if sol { Some(t.sol_amount) } else { tail.map(|x| x.quote_amount) }) else {
        o.add(i, name, Outcome::Inconclusive, true, "older TradeEvent layout without quote_amount".into());
        return None;
    };
    let (q, fee, c, bb) = (q as i128, t.fee.unwrap_or(0) as i128, t.creator_fee.unwrap_or(0) as i128, tail.map(|x| x.buyback_fee).unwrap_or(0) as i128);
    if !legacy && !v3 {
        o.add(i, name, Outcome::Inconclusive, true, format!("{here}: quote routing of this instruction variant is not modelled"));
        return None;
    }
    if legacy && !sol {
        o.add(i, name, Outcome::Inconclusive, true, format!("{here}: token-quoted v1/v2 quote routing is not modelled (outside trading scope)"));
        return None;
    }
    let Some(sub) = m.subtree(ix) else {
        o.add(i, name, Outcome::Inconclusive, true, format!("{here}: call depth unavailable"));
        return None;
    };
    let Some(target) = (if sol { def.get(accts, "bonding_curve") } else { def.get(accts, "associated_quote_bonding_curve") }) else {
        o.add(i, name, Outcome::Inconclusive, true, format!("{here}: curve quote account not in instruction"));
        return None;
    };
    let bb_acct = def.get(accts, "buyback_fee_recipient");
    let credits = |a: &str| if sol { m.lamport_credits_in(a, &sub) } else { token_credits_in(m, a, &sub) };
    let what = if sol { "bonding-curve SOL" } else { "bonding-curve quote vault" };
    if t.is_buy {
        let paid = credits(target);
        let bb_direct: i128 = if v3 { bb_acct.map(|a| credits(a).iter().map(|x| x.1).sum()).unwrap_or(0) } else { 0 };
        let (expected, formula) = if legacy {
            (q, format!("base {q}"))
        } else if bb_direct != 0 {
            (q + fee + c - bb_direct, format!("base {q} + protocol fee {fee} + creator fee {c} − buyback slice {bb_direct} paid directly"))
        } else {
            (q + fee + c, format!("base {q} + protocol fee {fee} + creator fee {c} (v3: fees accrue on the curve)"))
        };
        if !paid.is_empty() {
            let total: i128 = paid.iter().map(|x| x.1).sum();
            let at = paid.iter().map(|x| m.ix_ref(x.0)).collect::<Vec<_>>().join(", ");
            let ok = total == expected;
            o.add(
                i,
                name,
                if ok { Outcome::Pass } else { Outcome::Fail },
                true,
                format!("{what} {}: parsed payment {total} ({at}) {} {formula} = {expected} inside {here}", short(target), if ok { "=" } else { "≠" }),
            );
            // the curve's transaction-wide balance is a separate consistency check: in v3 the
            // buyback slice leaves the curve again (SDK: "the trade pays out directly")
            if v3 && sol {
                let out = bb - bb_direct;
                let net = m.lamport_delta(target).unwrap_or(0);
                let (oc, d) = balance_verdict(m, &scope_of(m, target, ix), net, total - out, &format!("bonding-curve net balance (parsed payment {total} − buyback slice {out} paid out)"), "trade.curve_net_balance", recon);
                o.add(i, "trade.curve_net_balance", oc, true, d);
            }
            return Some(ok);
        }
        if !sol {
            o.add(i, name, Outcome::Inconclusive, true, format!("{here}: no parsed quote transfer into the curve vault"));
            return None;
        }
    }
    // sells (and buys without a parsed payment): balance evidence only
    let expected = match (t.is_buy, legacy) {
        (true, true) => q,
        (true, false) => q + fee + c - bb,
        (false, true) => -q,
        (false, false) => -q + fee + c - bb,
    };
    if !sol {
        // token-quoted v3 sell: every movement of the quote vault inside the instruction is parsed
        let net = token_net_in(m, target, &sub);
        let ok = net == expected;
        o.add(i, name, if ok { Outcome::Pass } else { Outcome::Fail }, true, format!("{what} {}: parsed net {net} {} expected {expected} inside {here}", short(target), if ok { "=" } else { "≠" }));
        return Some(ok);
    }
    if shared_curve {
        o.add(i, name, Outcome::Inconclusive, true, format!("{what} {}: several trades in this transaction share the curve; balance-only evidence is not attributable per trade", short(target)));
        return None;
    }
    let observed = m.lamport_delta(target).unwrap_or(0) - m.lamport_flow_sum(target, |k| k != ix && !sub.contains(&k));
    let model = if legacy { if t.is_buy { "base" } else { "−base (fees paid out of the curve)" } } else { "v3: base ± retained fees − buyback slice" };
    let (oc, d) = balance_verdict(m, &scope_of(m, target, ix), observed, expected, &format!("{what} {} ({model})", short(target)), name, recon);
    o.add(i, name, oc, true, d);
    Some(oc == Outcome::Pass)
}

/// v3 fee evidence: the buyback slice goes to the instruction's `buyback_fee_recipient` (must be an
/// official Global.buybackFeeRecipients wallet, or its quote token account); the rest of the
/// protocol fee and the creator fee accrue on the curve, so they are proven only jointly by the
/// curve payment identity (`curve_ok`).
#[allow(clippy::too_many_arguments)]
fn v3_fee_checks(m: &TxModel, o: &mut Out, i: usize, ix: usize, def: &IxDef, t: &crate::decode::pumpfun::PumpTrade, curve_ok: Option<bool>, routes: &mut Vec<String>, recon: &mut Vec<String>) {
    let accts = &m.ixs[ix].accounts;
    let here = format!("{} {}", m.ix_ref(ix), def.name);
    let sol = t.is_sol_quoted();
    let Some(tail) = t.tail.as_ref() else {
        for n in ["trade.protocol_fee_observed", "trade.creator_fee_observed"] {
            o.add(i, n, Outcome::Inconclusive, true, "older TradeEvent layout without buyback fields".into());
        }
        return;
    };
    let (fee, c, bb) = (t.fee.unwrap_or(0) as i128, t.creator_fee.unwrap_or(0) as i128, tail.buyback_fee as i128);
    let joint = match curve_ok {
        Some(true) => "proven by the curve payment identity",
        Some(false) => "the curve payment identity FAILED",
        None => "the curve payment identity is not proven",
    };
    // protocol fee: buyback slice + retained part
    let pf = match def.get(accts, "buyback_fee_recipient") {
        None => (Outcome::Inconclusive, format!("{here}: no buyback_fee_recipient account")),
        Some(r) => {
            let owner = if sol { Some(r.to_string()) } else { m.tokens.get(r).and_then(|x| x.owner.clone()) };
            let official = owner.as_ref().is_some_and(|w| buyback_recipients().contains(w));
            let sub = m.subtree(ix);
            let parsed: Vec<(usize, i128)> = match &sub {
                Some(sb) => if sol { m.lamport_credits_in(r, sb) } else { token_credits_in(m, r, sb) },
                None => vec![],
            };
            if !official && bb != 0 {
                (Outcome::Fail, format!("buyback slice {bb} goes to {} which is not an official Global.buybackFeeRecipients wallet", short(r)))
            } else if !parsed.is_empty() {
                let got: i128 = parsed.iter().map(|x| x.1).sum();
                match (got == bb, curve_ok) {
                    (false, _) => (Outcome::Fail, format!("official buyback recipient {}: parsed {got} vs event buyback_fee {bb}", short(r))),
                    (true, Some(true)) => {
                        routes.push("v3:buyback_parsed+retained_on_curve".into());
                        (Outcome::Pass, format!("protocol fee {fee} = buyback slice {bb} ({} bps; parsed to official {} at {}) + {} retained on the curve ({joint})", tail.buyback_fee_basis_points, short(r), refs(m, &parsed.iter().map(|x| x.0).collect::<Vec<_>>()), fee - bb))
                    }
                    (true, Some(false)) => (Outcome::Fail, format!("buyback slice {bb} matches, but {joint}")),
                    (true, None) => (Outcome::Inconclusive, format!("buyback slice {bb} matches, but {joint}")),
                }
            } else if !sol {
                (Outcome::Inconclusive, format!("{here}: no parsed buyback transfer"))
            } else {
                // SOL v3: the program pays the slice out of the curve directly (no System instruction)
                let observed = m.lamport_delta(r).unwrap_or(0);
                let (oc, d) = balance_verdict(m, &scope_of(m, r, ix), observed, bb, &format!("official buyback recipient {} (event buyback_fee {bb} = {} bps of fee {fee})", short(r), tail.buyback_fee_basis_points), "trade.protocol_fee_observed", recon);
                match (oc, curve_ok) {
                    (Outcome::Pass, Some(true)) => {
                        routes.push("v3:buyback_balance+retained_on_curve".into());
                        (Outcome::Pass, format!("{d}; remaining {} retained on the curve ({joint})", fee - bb))
                    }
                    (Outcome::Pass, _) => (Outcome::Inconclusive, format!("{d}; but {joint}")),
                    (oc, _) => (oc, format!("{d}; remaining {} retained on the curve ({joint})", fee - bb)),
                }
            }
        }
    };
    o.add(i, "trade.protocol_fee_observed", pf.0, true, pf.1);
    // creator fee: accrues on the curve in v3 (BondingCurve.creator_fee bucket)
    let (oc, d) = match curve_ok {
        Some(true) => (Outcome::Pass, format!("creator fee {c} accrued on the curve (v3 creator bucket, now {}) — included exactly in the parsed curve payment inside {here}", tail.creator_fee_unclaimed)),
        _ => (Outcome::Inconclusive, format!("creator fee {c} accrues on the curve in v3; {joint}")),
    };
    o.add(i, "trade.creator_fee_observed", oc, true, d);
}

/// Event-internal arithmetic: SDK fee formula and the constant-product invariant.
fn trade_math_checks(o: &mut Out, i: usize, t: &crate::decode::pumpfun::PumpTrade) {
    // fees are computed on the quote amount: SOL curves report it as sol_amount, token-quoted curves
    // as quote_amount (with their own virtual quote reserves)
    let (base, vq, label) = if t.is_sol_quoted() {
        (t.sol_amount, t.virtual_sol_reserves, "sol_amount")
    } else {
        match t.tail.as_ref() {
            Some(x) => (x.quote_amount, x.virtual_quote_reserves, "quote_amount"),
            None => return,
        }
    };
    if let (Some(bps), Some(fee)) = (t.fee_basis_points, t.fee) {
        let e = ceil_fee(base, bps);
        let detail = format!(
            "fee {fee} vs ceil({label} {base}×{bps}bps) {e}{}",
            if fee == e { String::new() } else { format!("; implied {:.2} bps, floor would be {}", fee as f64 * 10_000.0 / base.max(1) as f64, base as u128 * bps as u128 / 10_000) }
        );
        o.add(i, "trade.protocol_fee_formula", if fee == e { Outcome::Pass } else { Outcome::Fail }, true, detail);
    }
    if let (Some(bps), Some(fee)) = (t.creator_fee_basis_points, t.creator_fee) {
        let set = t.creator.is_some_and(|c| c != Pubkey::default());
        let e = if set { ceil_fee(base, bps) } else { 0 };
        o.add(i, "trade.creator_fee_formula", if fee == e { Outcome::Pass } else { Outcome::Fail }, true, format!("creator fee {fee} vs {} {e}", if set { format!("ceil({label}×{bps}bps)") } else { "no creator →".into() }));
    }
    let (vs, vt) = (vq as i128, t.virtual_token_reserves as i128);
    let (ds, dt) = (base as i128, t.token_amount as i128);
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
    #[serde(default)]
    pub fully_verified_buys: u64,
    #[serde(default)]
    pub fully_verified_sells: u64,
    pub events_with_failures: u64,
    pub events_inconclusive: u64,
    pub checks_passed: u64,
    pub checks_failed: u64,
    pub checks_inconclusive: u64,
    pub failures_by_check: BTreeMap<String, u64>,
    pub inconclusive_by_check: BTreeMap<String, u64>,
    pub fee_hypotheses: BTreeMap<String, u64>,
    /// balance-only checks that reconcile exactly but whose accounts a calling (wrapper)
    /// instruction also names: INCONCLUSIVE unless --accept-ancestor-scoped
    #[serde(default)]
    pub reconciled_via_ancestor: BTreeMap<String, u64>,
    /// fully verified trade events by emitting instruction variant
    #[serde(default)]
    pub fully_verified_by_instruction: BTreeMap<String, u64>,
    /// fully verified trades on non-SOL-quoted curves (never traded; not counted for the venue status)
    #[serde(default)]
    pub fully_verified_token_quoted: u64,
    #[serde(default)]
    pub attribution_mode: String,
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
                    let token_quoted = r.token_quoted.get(i).copied().unwrap_or(false);
                    if p > 0 && f == 0 && n == 0 && token_quoted {
                        // verified decoding, but non-SOL-quoted curves are never traded: not counted for the venue
                        vr.fully_verified_token_quoted += 1;
                    } else if p > 0 && f == 0 && n == 0 {
                        vr.events_fully_verified += 1;
                        *vr.fully_verified_by_instruction.entry(r.event_instruction.get(i).cloned().unwrap_or_default()).or_default() += 1;
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
                for c in &r.reconciled_via_ancestor {
                    *vr.reconciled_via_ancestor.entry(c.clone()).or_default() += 1;
                }
            }
        }
    }
    for e in s.items.values_mut() {
        e.mainnet_verified = e.events_fully_verified >= min_events && e.hard_checks_failed == 0;
    }
    for (venue, vr) in s.venues.iter_mut() {
        vr.attribution_mode = if accept_ancestor_scoped() { "accept-ancestor-scoped".into() } else { "strict".into() };
        let (buys, sells) = sides.get(venue).copied().unwrap_or_default();
        (vr.fully_verified_buys, vr.fully_verified_sells) = (buys, sells);
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

/// Human-readable forensic view of one transaction: every decoded event, its emitting instruction,
/// the instruction's accounts by IDL role with lamport / token deltas, the accounts beyond the
/// fixed layout, and the parsed movements inside the instruction. Read-only.
pub fn explain_tx(rpc_result: &Value, signature: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let (Some(raw), Some(m)) = (parse_tx_object(rpc_result, Some(signature), 0, "explain", Commitment::Confirmed, 0), TxModel::parse(rpc_result, signature)) else {
        return "unparsable transaction".into();
    };
    let out = decode_raw(&raw);
    let mapping = map_events(&m, &out.events);
    let _ = writeln!(s, "{signature} version {} fee {} — {} event(s), {} duplicate emission(s) merged", m.version.label(), m.fee, out.events.len(), out.duplicates_merged);
    for (k, d) in out.events.iter().enumerate() {
        let _ = writeln!(s, "\nEVENT {k} via {} (outer {:?}, inner {:?}, invoke #{:?}):", d.provenance.channel, d.provenance.outer_ix, d.provenance.inner_pos, d.provenance.invoke_ordinal);
        let _ = writeln!(s, "  {}", serde_json::to_string(&d.event).unwrap_or_default().chars().take(1500).collect::<String>());
        match &mapping[k] {
            Err(e) => {
                let _ = writeln!(s, "  instruction: UNRESOLVED — {e}");
            }
            Ok((ix, def)) => {
                let i = &m.ixs[*ix];
                let _ = writeln!(s, "  instruction: {} {} (depth {:?})", m.ix_ref(*ix), def.name, i.stack_height);
                for (n, a) in i.accounts.iter().enumerate() {
                    let role = def.accounts.get(n).map(|x| x.name.clone()).unwrap_or_else(|| {
                        format!("remaining #{}{}", n - def.accounts.len(), if buyback_recipients().contains(a) { " [official buyback recipient]" } else { "" })
                    });
                    let lam = m.lamport_delta(a).map(|x| x.to_string()).unwrap_or("-".into());
                    let tok = m.token_delta(a).map(|x| format!(" token Δ {x}")).unwrap_or_default();
                    let refs = m.referencing(a).iter().map(|r| m.ix_ref(*r)).collect::<Vec<_>>().join(",");
                    let _ = writeln!(s, "    [{n:>2}] {role:<40} {a}  lamport Δ {lam}{tok}  named by {refs}");
                }
                if let Some(r) = m.subtree(*ix) {
                    for f in m.flows.iter().filter(|f| matches!(f, crate::txmodel::Flow::Token { ix, .. } | crate::txmodel::Flow::Lamports { ix, .. } if r.contains(ix))) {
                        let _ = writeln!(s, "    movement inside: {f:?}");
                    }
                }
            }
        }
    }
    s
}
