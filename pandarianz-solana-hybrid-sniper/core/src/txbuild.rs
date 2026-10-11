//! SYNTHETIC `getTransaction` (jsonParsed) builder for regression tests and self-tests. It
//! reproduces the structure of real mainnet transactions — top-level and inner instructions with
//! `stackHeight`, parsed System / SPL Token instructions, Anchor events emitted through BOTH the
//! `Program data:` log and the self-CPI channel, token creation (mintTo), versioned-transaction
//! `version` and address-lookup-table `loadedAddresses` — so verification is exercised on the same
//! shapes as real data. Output is always labelled synthetic; it is never mainnet evidence.

use crate::decode::anchor::EVENT_IX_TAG;
use crate::decode::pumpfun::{self, PumpEvent, PumpTrade};
use crate::decode::pumpswap::{self, PumpSwapEvent};
use crate::types::{programs, Pubkey, WSOL_MINT};
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
pub const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

fn hexd(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[derive(Default, Clone)]
struct Group {
    outer: Value,
    inner: Vec<Value>,
    logs: Vec<String>,
}

#[derive(Clone)]
pub struct SynthTx {
    pub version: Value,
    pub fee: u64,
    payer: String,
    lamports: BTreeMap<String, (i128, i128)>,
    tokens: BTreeMap<String, (String, String, Option<i128>, Option<i128>)>,
    groups: Vec<Group>,
    /// accounts to place in `meta.loadedAddresses.writable` (v0/v1 lookup tables)
    pub loaded: Vec<String>,
}

impl SynthTx {
    pub fn new(payer: &str, version: Value) -> Self {
        let mut s = Self { version, fee: 5_000, payer: payer.into(), lamports: BTreeMap::new(), tokens: BTreeMap::new(), groups: vec![], loaded: vec![] };
        s.lamports.insert(payer.into(), (10_000_000_000, 10_000_000_000));
        s
    }
    /// Add `delta` lamports to an account (created with `pre` if new).
    pub fn lamports(&mut self, acct: &str, delta: i128) -> &mut Self {
        let e = self.lamports.entry(acct.into()).or_insert((1_000_000_000, 1_000_000_000));
        e.1 += delta;
        self
    }
    /// Account that does not exist before the transaction (pre balance 0).
    pub fn new_account(&mut self, acct: &str) -> &mut Self {
        self.lamports.insert(acct.into(), (0, 0));
        self
    }
    /// Token account balance (pre None = created in this tx; post None = closed).
    pub fn token(&mut self, acct: &str, mint: &str, owner: &str, pre: Option<i128>, post: Option<i128>) -> &mut Self {
        self.tokens.insert(acct.into(), (mint.into(), owner.into(), pre, post));
        self
    }
    pub fn token_add(&mut self, acct: &str, delta: i128) -> &mut Self {
        if let Some(t) = self.tokens.get_mut(acct) {
            t.3 = Some(t.3.unwrap_or(0) + delta);
        }
        self
    }
    /// Start a top-level instruction (unparsed). Returns its index.
    pub fn outer(&mut self, program: &str, accounts: &[&str], data: &[u8]) -> usize {
        self.groups.push(Group {
            outer: json!({"programId": program, "accounts": accounts, "data": bs58::encode(data).into_string(), "stackHeight": null}),
            inner: vec![],
            logs: vec![format!("Program {program} invoke [1]")],
        });
        self.groups.len() - 1
    }
    pub fn inner(&mut self, o: usize, stack: u8, program: &str, accounts: &[&str], data: &[u8]) -> &mut Self {
        self.groups[o].inner.push(json!({"programId": program, "accounts": accounts, "data": bs58::encode(data).into_string(), "stackHeight": stack}));
        self
    }
    pub fn inner_parsed(&mut self, o: usize, stack: u8, program_name: &str, program_id: &str, ty: &str, info: Value) -> &mut Self {
        self.groups[o].inner.push(json!({"program": program_name, "programId": program_id, "parsed": {"type": ty, "info": info}, "stackHeight": stack}));
        self
    }
    pub fn log(&mut self, o: usize, line: String) -> &mut Self {
        self.groups[o].logs.push(line);
        self
    }
    /// Finish a top-level instruction's logs.
    pub fn close(&mut self, o: usize, program: &str) -> &mut Self {
        self.groups[o].logs.push(format!("Program {program} success"));
        self
    }
    pub fn sys_transfer(&mut self, o: usize, stack: u8, from: &str, to: &str, lamports: u64) -> &mut Self {
        self.lamports(from, -(lamports as i128));
        self.lamports(to, lamports as i128);
        self.inner_parsed(o, stack, "system", SYSTEM_PROGRAM, "transfer", json!({"source": from, "destination": to, "lamports": lamports}))
    }
    pub fn token_transfer(&mut self, o: usize, stack: u8, from: &str, to: &str, mint: &str, amount: u64) -> &mut Self {
        self.token_add(from, -(amount as i128));
        self.token_add(to, amount as i128);
        self.inner_parsed(o, stack, "spl-token", TOKEN_PROGRAM, "transferChecked", json!({"source": from, "destination": to, "mint": mint, "authority": "auth", "tokenAmount": {"amount": amount.to_string(), "decimals": 6}}))
    }

    pub fn build(&self, signature: &str, slot: u64) -> Value {
        let mut keys: Vec<String> = vec![self.payer.clone()];
        let add = |k: &str, keys: &mut Vec<String>| {
            if !keys.iter().any(|x| x == k) && !self.loaded.iter().any(|x| x == k) {
                keys.push(k.to_string());
            }
        };
        for g in &self.groups {
            for ix in std::iter::once(&g.outer).chain(g.inner.iter()) {
                add(ix["programId"].as_str().unwrap(), &mut keys);
                for a in ix["accounts"].as_array().into_iter().flatten() {
                    add(a.as_str().unwrap(), &mut keys);
                }
                for f in ["source", "destination", "newAccount", "account", "mint"] {
                    if let Some(a) = ix["parsed"]["info"][f].as_str() {
                        add(a, &mut keys);
                    }
                }
            }
        }
        for a in self.lamports.keys().chain(self.tokens.keys()) {
            add(a, &mut keys);
        }
        let all: Vec<String> = keys.iter().cloned().chain(self.loaded.iter().cloned()).collect();
        let fee = self.fee as i128;
        let bal = |k: &str, post: bool| -> i64 {
            let (pre, p) = self.lamports.get(k).copied().unwrap_or((1_000_000, 1_000_000));
            let mut v = if post { p } else { pre };
            if post && k == self.payer {
                v -= fee;
            }
            v as i64
        };
        let tb = |post: bool| -> Vec<Value> {
            self.tokens
                .iter()
                .filter_map(|(a, (mint, owner, pre, pst))| {
                    let amt = if post { *pst } else { *pre }?;
                    let idx = all.iter().position(|k| k == a)?;
                    Some(json!({"accountIndex": idx, "mint": mint, "owner": owner, "programId": TOKEN_PROGRAM, "uiTokenAmount": {"amount": amt.to_string(), "decimals": 6}}))
                })
                .collect()
        };
        let logs: Vec<String> = self.groups.iter().flat_map(|g| g.logs.clone()).collect();
        let inner: Vec<Value> = self.groups.iter().enumerate().filter(|(_, g)| !g.inner.is_empty()).map(|(i, g)| json!({"index": i, "instructions": g.inner})).collect();
        json!({
            "slot": slot,
            "blockTime": 1_760_000_000,
            "version": self.version,
            "meta": {
                "err": null,
                "fee": self.fee,
                "preBalances": all.iter().map(|k| bal(k, false)).collect::<Vec<_>>(),
                "postBalances": all.iter().map(|k| bal(k, true)).collect::<Vec<_>>(),
                "preTokenBalances": tb(false),
                "postTokenBalances": tb(true),
                "innerInstructions": inner,
                "logMessages": logs,
                "loadedAddresses": {"writable": self.loaded, "readonly": []},
                "computeUnitsConsumed": 50_000,
            },
            "transaction": {
                "signatures": [signature],
                "message": {
                    "accountKeys": keys.iter().enumerate().map(|(i, k)| json!({"pubkey": k, "signer": i == 0, "writable": true, "source": "transaction"})).collect::<Vec<_>>(),
                    "instructions": self.groups.iter().map(|g| g.outer.clone()).collect::<Vec<_>>(),
                    "recentBlockhash": "11111111111111111111111111111111",
                }
            }
        })
    }
}

/// Accounts of one synthetic pump.fun curve (labels → deterministic synthetic keys).
pub struct CurveAccounts {
    pub mint: String,
    pub curve: String,
    pub vault: String,
    pub fee_recipient: String,
    pub creator_vault: String,
}

impl CurveAccounts {
    pub fn of(c: &crate::fixtures::Curve) -> Self {
        let l = c.mint.to_string();
        Self {
            mint: l.clone(),
            curve: c.bonding_curve.to_string(),
            vault: Pubkey::synthetic(&format!("vault:{l}")).to_string(),
            fee_recipient: Pubkey::synthetic("fee-recipient").to_string(),
            creator_vault: Pubkey::synthetic(&format!("creator-vault:{}", c.creator)).to_string(),
        }
    }
}

pub fn user_ata(user: &str, mint: &str) -> String {
    Pubkey::synthetic(&format!("ata:{user}:{mint}")).to_string()
}

/// How a pump event is emitted in a synthetic transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Emission {
    LogOnly,
    CpiOnly,
    /// both `Program data:` and self-CPI — what produced doubled amounts on mainnet
    Both,
}

fn emit_pump(tx: &mut SynthTx, o: usize, depth: u8, ev: &PumpEvent, how: Emission) {
    let payload = pumpfun::encode(ev);
    if how != Emission::CpiOnly {
        tx.log(o, format!("Program data: {}", base64::engine::general_purpose::STANDARD.encode(&payload)));
    }
    if how != Emission::LogOnly {
        let ea = Pubkey::synthetic("pump-event-authority").to_string();
        let mut data = EVENT_IX_TAG.to_vec();
        data.extend(&payload);
        tx.inner(o, depth + 1, programs::PUMP_FUN, &[&ea], &data);
        tx.log(o, format!("Program {} invoke [{}]", programs::PUMP_FUN, depth + 1));
        tx.log(o, format!("Program {} success", programs::PUMP_FUN));
    }
}

/// Append a pump.fun `buy`/`sell` top-level instruction carrying `ev` with consistent effects:
/// parsed SOL/token transfers (buy) or direct lamport moves (sell), balances and the event.
pub fn add_pump_trade(tx: &mut SynthTx, c: &CurveAccounts, ev: &PumpEvent, how: Emission) -> usize {
    let PumpEvent::Trade(t) = ev else { panic!("trade event expected") };
    add_pump_trade_as(tx, c, t, ev, how)
}

/// Like `add_pump_trade`, but the on-chain effects follow `t` while the emitted event is `emitted`
/// (lets tests model a decoder/event mismatch).
pub fn add_pump_trade_as(tx: &mut SynthTx, c: &CurveAccounts, t: &PumpTrade, emitted: &PumpEvent, how: Emission) -> usize {
    add_pump_trade_opts(tx, c, t, emitted, how, &TradeOpts::default())
}

/// Variations observed in real mainnet transactions (forensic audit of 15 captures).
#[derive(Clone, Debug, Default)]
pub struct TradeOpts {
    /// buyback recipient passed as a remaining account after `bonding_curve_v2` (official SDK order),
    /// receiving `share` lamports of the decoded fee; the fee recipient receives the rest
    pub buyback: Option<(String, u64)>,
    /// buys only: fee shares paid by direct lamport moves instead of parsed System transfers
    pub direct_fee_moves: bool,
    /// the trade is a CPI from this (non-Pump) wrapper program's top-level instruction
    pub wrapper: Option<String>,
    /// an extra System transfer into the creator vault inside the trade (not a fee)
    pub creator_extra: Option<u64>,
    /// replace the instruction discriminator (unrecognized layout)
    pub disc: Option<[u8; 8]>,
    /// sells: close the user's token account in a later top-level instruction
    pub close_user_ata: bool,
}

pub fn add_pump_trade_opts(tx: &mut SynthTx, c: &CurveAccounts, t: &PumpTrade, emitted: &PumpEvent, how: Emission, opts: &TradeOpts) -> usize {
    let user = t.user.to_string();
    let ata = user_ata(&user, &c.mint);
    tx.lamports(&user, 0);
    let mut data = opts.disc.map(|d| d.to_vec()).unwrap_or_else(|| hexd(if t.is_buy { "66063d1201daebea" } else { "33e685a4017f83ad" }));
    data.extend(t.token_amount.to_le_bytes());
    data.extend(u64::MAX.to_le_bytes());
    let g = Pubkey::synthetic("pump-global").to_string();
    let ea = Pubkey::synthetic("pump-event-authority").to_string();
    let uva = Pubkey::synthetic(&format!("uva:{user}")).to_string();
    let gva = Pubkey::synthetic("gva").to_string();
    let fc = Pubkey::synthetic("fee-config").to_string();
    let fp = Pubkey::synthetic("fee-program").to_string();
    let bc2 = Pubkey::synthetic(&format!("bc2:{}", c.mint)).to_string();
    let mut accts: Vec<String> = if t.is_buy {
        vec![&g, &c.fee_recipient, &c.mint, &c.curve, &c.vault, &ata, &user, SYSTEM_PROGRAM, TOKEN_PROGRAM, &c.creator_vault, &ea, programs::PUMP_FUN, &gva, &uva, &fc, &fp]
    } else {
        vec![&g, &c.fee_recipient, &c.mint, &c.curve, &c.vault, &ata, &user, SYSTEM_PROGRAM, &c.creator_vault, TOKEN_PROGRAM, &ea, programs::PUMP_FUN, &fc, &fp]
    }
    .into_iter()
    .map(String::from)
    .collect();
    if let Some((bb, _)) = &opts.buyback {
        accts.push(bc2.clone());
        accts.push(bb.clone());
    }
    let refs: Vec<&str> = accts.iter().map(|s| s.as_str()).collect();
    // top level: the pump instruction itself, or a wrapper whose CPI runs it at depth 2
    let (o, d) = match &opts.wrapper {
        Some(w) => {
            let mut wa = refs.clone();
            wa.push(programs::PUMP_FUN);
            let o = tx.outer(w, &wa, &[9, 9, 9, 9, 9, 9, 9, 9]);
            tx.inner(o, 2, programs::PUMP_FUN, &refs, &data);
            tx.log(o, format!("Program {} invoke [2]", programs::PUMP_FUN));
            (o, 2u8)
        }
        None => (tx.outer(programs::PUMP_FUN, &refs, &data), 1u8),
    };
    tx.log(o, format!("Program log: Instruction: {}", if t.is_buy { "Buy" } else { "Sell" }));
    let (fee, cfee) = (t.fee.unwrap_or(0), t.creator_fee.unwrap_or(0));
    let share = opts.buyback.as_ref().map(|b| b.1).unwrap_or(0);
    let child = d + 1;
    if t.is_buy {
        tx.token_transfer(o, child, &c.vault, &ata, &c.mint, t.token_amount);
        tx.sys_transfer(o, child, &user, &c.curve, t.sol_amount);
        if opts.direct_fee_moves {
            tx.lamports(&user, -(fee as i128));
            tx.lamports(&c.fee_recipient, (fee - share) as i128);
            if let Some((bb, _)) = &opts.buyback {
                tx.lamports(bb, share as i128);
            }
        } else {
            tx.sys_transfer(o, child, &user, &c.fee_recipient, fee - share);
            if let Some((bb, _)) = &opts.buyback {
                tx.sys_transfer(o, child, &user, bb, share);
            }
        }
        if let Some(x) = opts.creator_extra {
            tx.sys_transfer(o, child, &user, &c.creator_vault, x);
        }
        tx.sys_transfer(o, child, &user, &c.creator_vault, cfee);
    } else {
        tx.token_transfer(o, child, &ata, &c.vault, &c.mint, t.token_amount);
        // program-owned curve pays out directly (no System instruction)
        tx.lamports(&c.curve, -(t.sol_amount as i128));
        tx.lamports(&user, (t.sol_amount - fee - cfee) as i128);
        tx.lamports(&c.fee_recipient, (fee - share) as i128);
        if let Some((bb, _)) = &opts.buyback {
            tx.lamports(bb, share as i128);
        }
        tx.lamports(&c.creator_vault, cfee as i128);
    }
    emit_pump(tx, o, d, emitted, how);
    if opts.wrapper.is_some() {
        tx.log(o, format!("Program {} success", programs::PUMP_FUN));
        let w = opts.wrapper.clone().unwrap();
        tx.close(o, &w);
    } else {
        tx.close(o, programs::PUMP_FUN);
    }
    if opts.close_user_ata {
        let o2 = tx.outer(TOKEN_PROGRAM, &[&ata, &user, &user], &[9]);
        tx.groups[o2].outer = json!({"program": "spl-token", "programId": TOKEN_PROGRAM, "parsed": {"type": "closeAccount", "info": {"account": ata, "destination": user, "owner": user}}, "stackHeight": null});
        tx.close(o2, TOKEN_PROGRAM);
        if let Some(e) = tx.tokens.get_mut(&ata) {
            e.3 = None; // closed: absent from postTokenBalances
        }
        tx.lamports(&user, 2_039_280);
    }
    o
}

/// Append a pump.fun `create` instruction: curve account creation (rent), 1e15 supply minted into
/// the curve vault, CreateEvent.
pub fn add_pump_create(tx: &mut SynthTx, c: &CurveAccounts, ev: &PumpEvent, how: Emission) -> usize {
    let PumpEvent::Create(cr) = ev else { panic!("create event expected") };
    let user = cr.user.to_string();
    let g = Pubkey::synthetic("pump-global").to_string();
    let ea = Pubkey::synthetic("pump-event-authority").to_string();
    let ma = Pubkey::synthetic("pump-mint-authority").to_string();
    let md = Pubkey::synthetic(&format!("metadata:{}", c.mint)).to_string();
    let mpl = Pubkey::synthetic("mpl").to_string();
    let rent = Pubkey::synthetic("rent").to_string();
    let ata_prog = Pubkey::synthetic("ata-program").to_string();
    let accts = [&c.mint, &ma, &c.curve, &c.vault, &g, &mpl, &md, &user, SYSTEM_PROGRAM, TOKEN_PROGRAM, &ata_prog, &rent, &ea, programs::PUMP_FUN];
    let accts: Vec<&str> = accts.iter().map(|s| s.as_ref()).collect();
    let o = tx.outer(programs::PUMP_FUN, &accts, &hexd("181ec828051c0777"));
    tx.log(o, "Program log: Instruction: Create".into());
    tx.new_account(&c.curve);
    let rent_curve = 1_461_600u64;
    tx.inner_parsed(o, 2, "system", SYSTEM_PROGRAM, "createAccount", json!({"source": user, "newAccount": c.curve, "lamports": rent_curve, "space": 82, "owner": programs::PUMP_FUN}));
    tx.lamports(&user, -(rent_curve as i128));
    tx.lamports(&c.curve, rent_curve as i128);
    tx.token(&c.vault, &c.mint, &c.curve, None, Some(0));
    let supply = cr.token_total_supply.unwrap_or(1_000_000_000_000_000);
    tx.token_add(&c.vault, supply as i128);
    tx.inner_parsed(o, 2, "spl-token", TOKEN_PROGRAM, "mintTo", json!({"mint": c.mint, "account": c.vault, "amount": supply.to_string(), "mintAuthority": ma}));
    emit_pump(tx, o, 1, ev, how);
    tx.close(o, programs::PUMP_FUN);
    o
}

/// Prepare token accounts for a trade's user (existing ATA with `held` tokens).
pub fn ensure_user_ata(tx: &mut SynthTx, user: &str, mint: &str, held: Option<i128>) {
    let ata = user_ata(user, mint);
    tx.token(&ata, mint, user, held, Some(held.unwrap_or(0)));
}

/// A complete synthetic pump.fun trade transaction for `t` (compute-budget ix, the trade).
pub fn pump_trade_tx(c: &CurveAccounts, t: &PumpTrade, vault_before: i128, how: Emission, version: Value, signature: &str) -> Value {
    let user = t.user.to_string();
    let mut tx = SynthTx::new(&user, version);
    tx.outer(COMPUTE_BUDGET, &[], &[2, 0x40, 0x0d, 0x03, 0]);
    tx.close(0, COMPUTE_BUDGET);
    tx.token(&c.vault, &c.mint, &c.curve, Some(vault_before), Some(vault_before));
    ensure_user_ata(&mut tx, &user, &c.mint, Some(if t.is_buy { 0 } else { t.token_amount as i128 }));
    add_pump_trade(&mut tx, c, &PumpEvent::Trade(t.clone()), how);
    tx.build(signature, 300_000_000)
}

/// A synthetic PumpSwap v1 swap transaction (WSOL quote) with parsed transfers and the CPI event.
pub fn pumpswap_swap_tx(ev: &PumpSwapEvent, mint: &str, version: Value, signature: &str) -> Value {
    let PumpSwapEvent::Swap(s) = ev else { panic!("swap expected") };
    let (pool, user) = (s.pool.to_string(), s.user.to_string());
    let k = |l: &str| Pubkey::synthetic(&format!("{l}:{pool}")).to_string();
    let (ub, uq, pb, pq, pf, pft, cva, cvauth) = (k("ub"), k("uq"), k("pb"), k("pq"), k("pf"), k("pft"), k("cva"), k("cvauth"));
    let gc = Pubkey::synthetic("amm-global").to_string();
    let ea = Pubkey::synthetic("amm-event-authority").to_string();
    let mut tx = SynthTx::new(&user, version);
    tx.token(&ub, mint, &user, Some(if s.is_buy { 0 } else { s.base_amount as i128 }), None)
        .token(&uq, WSOL_MINT, &user, Some(50_000_000_000), None)
        .token(&pb, mint, &pool, Some(s.pool_base_token_reserves as i128), None)
        .token(&pq, WSOL_MINT, &pool, Some(s.pool_quote_token_reserves as i128), None)
        .token(&pft, WSOL_MINT, &pf, Some(1_000), None)
        .token(&cva, WSOL_MINT, &cvauth, Some(0), None);
    for a in [&ub, &uq, &pb, &pq, &pft, &cva] {
        let pre = tx.tokens[a.as_str()].2;
        tx.tokens.get_mut(a.as_str()).unwrap().3 = pre;
    }
    let disc = if s.is_buy { "66063d1201daebea" } else { "33e685a4017f83ad" };
    let accts = [&pool, &user, &gc, mint, WSOL_MINT, &ub, &uq, &pb, &pq, &pf, &pft, TOKEN_PROGRAM, TOKEN_PROGRAM, SYSTEM_PROGRAM, "ata", &ea, programs::PUMP_SWAP, &cva, &cvauth];
    let accts: Vec<&str> = accts.iter().map(|s| s.as_ref()).collect();
    let o = tx.outer(programs::PUMP_SWAP, &accts, &hexd(disc));
    let (proto, cr) = (s.protocol_fee, s.coin_creator_fee.unwrap_or(0));
    if s.is_buy {
        tx.token_transfer(o, 2, &pb, &ub, mint, s.base_amount);
        tx.token_transfer(o, 2, &uq, &pq, WSOL_MINT, s.quote_amount_pool);
        tx.token_transfer(o, 2, &uq, &pft, WSOL_MINT, proto);
        tx.token_transfer(o, 2, &uq, &cva, WSOL_MINT, cr);
    } else {
        tx.token_transfer(o, 2, &ub, &pb, mint, s.base_amount);
        tx.token_transfer(o, 2, &pq, &uq, WSOL_MINT, s.quote_amount_user);
        tx.token_transfer(o, 2, &pq, &pft, WSOL_MINT, proto);
        tx.token_transfer(o, 2, &pq, &cva, WSOL_MINT, cr);
    }
    let mut data = EVENT_IX_TAG.to_vec();
    data.extend(pumpswap::encode(ev));
    tx.inner(o, 2, programs::PUMP_SWAP, &[&ea], &data);
    tx.log(o, format!("Program {} invoke [2]", programs::PUMP_SWAP)).log(o, format!("Program {} success", programs::PUMP_SWAP));
    tx.close(o, programs::PUMP_SWAP);
    tx.build(signature, 300_000_001)
}
