//! Structured, read-only view of a `getTransaction` (jsonParsed) result: account keys (including
//! address-lookup-table loads), lamport and token balances, and every outer and inner instruction in
//! execution order with its call depth. Verification uses this view to check decoded events against
//! the accounts and transfers a specific instruction touched, not wallet-wide balance changes.
//!
//! Parsed SPL Token / Token-2022 and System instructions are turned into exact flows. Token balances
//! can only change through the token program, so parsed token flows explain every token balance
//! change (except native-SOL syncs, which are reported separately).

use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// Transaction format version as reported by the RPC (`"legacy"`, `0`, `1`, ...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum TxVersion {
    Legacy,
    V(u64),
    /// No `version` field: the request did not set `maxSupportedTransactionVersion`.
    Absent,
    Other(String),
}

impl TxVersion {
    pub fn parse(v: &Value) -> Self {
        match v.get("version") {
            None | Some(Value::Null) => TxVersion::Absent,
            Some(Value::String(s)) if s == "legacy" => TxVersion::Legacy,
            Some(Value::Number(n)) => n.as_u64().map(TxVersion::V).unwrap_or_else(|| TxVersion::Other(n.to_string())),
            Some(x) => TxVersion::Other(x.to_string()),
        }
    }
    pub fn label(&self) -> String {
        match self {
            TxVersion::Legacy => "legacy".into(),
            TxVersion::V(n) => n.to_string(),
            TxVersion::Absent => "absent".into(),
            TxVersion::Other(s) => format!("other:{s}"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Ix {
    /// Index of the top-level instruction this belongs to.
    pub outer: u16,
    /// Position inside `innerInstructions[outer].instructions`; None for the top-level instruction.
    pub inner_pos: Option<u16>,
    /// 1 for top-level instructions; from `stackHeight` for inner ones (None when the provider omits it).
    pub stack_height: Option<u8>,
    pub program_id: String,
    pub accounts: Vec<String>,
    /// Raw instruction data (unparsed instructions only).
    #[serde(skip)]
    pub data: Option<Vec<u8>>,
    /// `program` / `parsed.type` / `parsed.info` for instructions the RPC parsed.
    pub parsed_program: Option<String>,
    pub parsed_type: Option<String>,
    #[serde(skip)]
    pub info: Value,
}

impl Ix {
    pub fn disc(&self) -> Option<[u8; 8]> {
        self.data.as_ref().filter(|d| d.len() >= 8).map(|d| d[..8].try_into().unwrap())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TokenBal {
    pub mint: String,
    pub owner: Option<String>,
    pub pre: Option<i128>,
    pub post: Option<i128>,
    pub decimals: Option<u8>,
}

impl TokenBal {
    /// post − pre; an account created in the transaction has pre 0, a closed one post 0.
    pub fn delta(&self) -> i128 {
        self.post.unwrap_or(0) - self.pre.unwrap_or(0)
    }
}

/// One parsed value movement.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Flow {
    /// Token amount (raw units) of `mint`; `from`/`to` are token accounts. None = minted / burned.
    Token { ix: usize, mint: Option<String>, from: Option<String>, to: Option<String>, amount: i128 },
    /// Lamports between system accounts (transfer / createAccount funding).
    Lamports { ix: usize, from: String, to: String, amount: i128 },
}

#[derive(Clone, Debug, Serialize)]
pub struct TxModel {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub version: TxVersion,
    pub fee: u64,
    pub err: Option<String>,
    pub keys: Vec<String>,
    pub pre_lamports: Vec<i128>,
    pub post_lamports: Vec<i128>,
    pub tokens: HashMap<String, TokenBal>,
    pub ixs: Vec<Ix>,
    /// `meta.innerInstructions` was present (it is null when the node did not record CPIs).
    pub inner_recorded: bool,
    pub inner_stack_heights: bool,
    pub flows: Vec<Flow>,
    /// Token accounts touched by parsed token instructions that change balances without a transfer
    /// (syncNative, unrecognised token instructions): their deltas cannot be fully explained.
    pub unexplained_token_accounts: Vec<String>,
    /// token accounts closed by a parsed closeAccount, with their balance at that moment
    pub closed_accounts: Vec<ClosedAccount>,
    pub log_messages: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClosedAccount {
    pub account: String,
    pub ix: usize,
    pub balance_at_close: i128,
}

/// Net parsed token flow of `acct` over the flows collected so far (all earlier instructions).
fn flow_sum_before(flows: &[Flow], acct: &str) -> i128 {
    flows
        .iter()
        .map(|f| match f {
            Flow::Token { from, to, amount, .. } => {
                let mut v = 0;
                if to.as_deref() == Some(acct) {
                    v += amount;
                }
                if from.as_deref() == Some(acct) {
                    v -= amount;
                }
                v
            }
            _ => 0,
        })
        .sum()
}

fn b58_or_str(k: &Value) -> Option<String> {
    k.get("pubkey").and_then(|p| p.as_str()).or_else(|| k.as_str()).map(String::from)
}

fn amount_of(info: &Value) -> Option<i128> {
    info.get("amount")
        .and_then(|a| a.as_str().map(String::from).or_else(|| a.as_u64().map(|x| x.to_string())))
        .or_else(|| info.get("tokenAmount")?.get("amount")?.as_str().map(String::from))
        .and_then(|s| s.parse().ok())
}

/// Structural problems that make a result unusable as evidence (empty when it is well formed).
pub fn structure_problems(v: &Value) -> Vec<String> {
    let mut p = Vec::new();
    if v.is_null() {
        p.push("result is null (transaction not found / not yet available)".into());
        return p;
    }
    if !v.is_object() {
        p.push("result is not an object".into());
        return p;
    }
    let meta = &v["meta"];
    if !meta.is_object() {
        p.push("meta missing".into());
    }
    let msg = &v["transaction"]["message"];
    if !msg.is_object() {
        p.push("transaction.message missing (not jsonParsed encoding?)".into());
    }
    let nkeys = msg["accountKeys"].as_array().map(|a| a.len()).unwrap_or(0)
        + ["writable", "readonly"].iter().map(|t| meta["loadedAddresses"][t].as_array().map(|a| a.len()).unwrap_or(0)).sum::<usize>();
    if nkeys == 0 {
        p.push("no account keys".into());
    }
    for b in ["preBalances", "postBalances"] {
        match meta[b].as_array() {
            Some(a) if a.len() == nkeys => {}
            Some(a) => p.push(format!("{b} has {} entries for {nkeys} account keys", a.len())),
            None if meta.is_object() => p.push(format!("{b} missing")),
            None => {}
        }
    }
    if meta.is_object() && meta.get("logMessages").map(|l| l.is_null()).unwrap_or(true) {
        p.push("logMessages missing".into());
    }
    if v["transaction"]["signatures"].as_array().map(|a| a.is_empty()).unwrap_or(true) {
        p.push("signatures missing".into());
    }
    p
}

impl TxModel {
    pub fn parse(v: &Value, signature: &str) -> Option<TxModel> {
        let meta = v.get("meta")?;
        let msg = v.get("transaction")?.get("message")?;
        let mut keys: Vec<String> = msg.get("accountKeys")?.as_array()?.iter().filter_map(b58_or_str).collect();
        for t in ["writable", "readonly"] {
            for k in meta["loadedAddresses"][t].as_array().into_iter().flatten() {
                if let Some(s) = k.as_str() {
                    keys.push(s.to_string());
                }
            }
        }
        let lam = |name: &str| -> Vec<i128> { meta[name].as_array().map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i128).collect()).unwrap_or_default() };
        let mut tokens: HashMap<String, TokenBal> = HashMap::new();
        for (field, post) in [("preTokenBalances", false), ("postTokenBalances", true)] {
            for b in meta[field].as_array().into_iter().flatten() {
                let Some(acct) = b["accountIndex"].as_u64().and_then(|i| keys.get(i as usize)).cloned() else { continue };
                let amt = b["uiTokenAmount"]["amount"].as_str().and_then(|s| s.parse::<i128>().ok());
                let e = tokens.entry(acct).or_default();
                e.mint = b["mint"].as_str().unwrap_or("").to_string();
                e.owner = b["owner"].as_str().map(String::from).or(e.owner.take());
                e.decimals = b["uiTokenAmount"]["decimals"].as_u64().map(|d| d as u8).or(e.decimals);
                if post {
                    e.post = amt;
                } else {
                    e.pre = amt;
                }
            }
        }
        let b58 = |s: &str| bs58::decode(s).into_vec().ok();
        let mk = |x: &Value, outer: u16, inner_pos: Option<u16>, sh: Option<u8>| -> Ix {
            let parsed = x.get("parsed").filter(|p| p.is_object());
            Ix {
                outer,
                inner_pos,
                stack_height: sh,
                program_id: x["programId"].as_str().unwrap_or("").to_string(),
                accounts: x["accounts"].as_array().map(|a| a.iter().filter_map(b58_or_str).collect()).unwrap_or_default(),
                data: x["data"].as_str().and_then(b58),
                parsed_program: x["program"].as_str().map(String::from),
                parsed_type: parsed.and_then(|p| p["type"].as_str()).map(String::from),
                info: parsed.map(|p| p["info"].clone()).unwrap_or(Value::Null),
            }
        };
        let inner_groups = meta.get("innerInstructions").and_then(|g| g.as_array());
        let mut by_outer: HashMap<u64, &Vec<Value>> = HashMap::new();
        let mut stack_heights = true;
        for g in inner_groups.into_iter().flatten() {
            if let (Some(i), Some(list)) = (g["index"].as_u64(), g["instructions"].as_array()) {
                by_outer.insert(i, list);
                stack_heights &= list.iter().all(|x| x["stackHeight"].as_u64().is_some());
            }
        }
        let mut ixs = Vec::new();
        for (o, x) in msg.get("instructions").and_then(|i| i.as_array()).into_iter().flatten().enumerate() {
            ixs.push(mk(x, o as u16, None, Some(1)));
            for (p, y) in by_outer.get(&(o as u64)).map(|l| l.iter()).into_iter().flatten().enumerate() {
                let sh = y["stackHeight"].as_u64().map(|h| h.min(255) as u8);
                ixs.push(mk(y, o as u16, Some(p as u16), sh));
            }
        }
        let mut m = TxModel {
            signature: signature.to_string(),
            slot: v["slot"].as_u64().unwrap_or(0),
            block_time: v["blockTime"].as_i64(),
            version: TxVersion::parse(v),
            fee: meta["fee"].as_u64().unwrap_or(0),
            err: meta.get("err").filter(|e| !e.is_null()).map(|e| e.to_string()),
            pre_lamports: lam("preBalances"),
            post_lamports: lam("postBalances"),
            keys,
            tokens,
            ixs,
            inner_recorded: inner_groups.is_some(),
            inner_stack_heights: stack_heights,
            flows: vec![],
            unexplained_token_accounts: vec![],
            closed_accounts: vec![],
            log_messages: meta["logMessages"].as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect()).unwrap_or_default(),
        };
        m.collect_flows();
        Some(m)
    }

    fn token_mint_of(&self, acct: &str) -> Option<String> {
        self.tokens.get(acct).map(|t| t.mint.clone())
    }

    fn collect_flows(&mut self) {
        let mut flows = Vec::new();
        let mut unexplained = Vec::new();
        let mut closed = Vec::new();
        for (i, ix) in self.ixs.iter().enumerate() {
            let (Some(prog), Some(ty)) = (ix.parsed_program.as_deref(), ix.parsed_type.as_deref()) else { continue };
            let info = &ix.info;
            let s = |k: &str| info[k].as_str().map(String::from);
            match prog {
                "spl-token" | "spl-token-2022" => match ty {
                    "transfer" | "transferChecked" => {
                        if let Some(a) = amount_of(info) {
                            let mint = s("mint").or_else(|| s("source").and_then(|x| self.token_mint_of(&x)));
                            flows.push(Flow::Token { ix: i, mint, from: s("source"), to: s("destination"), amount: a });
                        }
                    }
                    "transferCheckedWithFee" => {
                        if let Some(a) = amount_of(info) {
                            let fee = info["feeAmount"]["amount"].as_str().and_then(|x| x.parse::<i128>().ok()).unwrap_or(0);
                            flows.push(Flow::Token { ix: i, mint: s("mint"), from: s("source"), to: None, amount: a });
                            flows.push(Flow::Token { ix: i, mint: s("mint"), from: None, to: s("destination"), amount: a - fee });
                        }
                    }
                    "mintTo" | "mintToChecked" => {
                        if let Some(a) = amount_of(info) {
                            flows.push(Flow::Token { ix: i, mint: s("mint"), from: None, to: s("account"), amount: a });
                        }
                    }
                    "burn" | "burnChecked" => {
                        if let Some(a) = amount_of(info) {
                            flows.push(Flow::Token { ix: i, mint: s("mint"), from: s("account"), to: None, amount: a });
                        }
                    }
                    "syncNative" => unexplained.extend(s("account")),
                    // no token balance change: account setup / authority / metadata
                    "closeAccount" => {
                        // SPL Token only closes an account whose balance is zero AT THAT POINT (native WSOL
                        // accounts excepted). The balance at close = pre balance + parsed flows before it.
                        // Closing never burns tokens by itself.
                        if let Some(a) = s("account") {
                            let pre = self.tokens.get(&a).and_then(|t| t.pre).unwrap_or(0);
                            let at_close = pre + flow_sum_before(&flows, &a);
                            closed.push(ClosedAccount { account: a.clone(), ix: i, balance_at_close: at_close });
                            if at_close != 0 {
                                unexplained.push(a);
                            }
                        }
                    }
                    "initializeAccount" | "initializeAccount2" | "initializeAccount3" | "initializeImmutableOwner" | "approve" | "approveChecked" | "revoke"
                    | "setAuthority" | "initializeMint" | "initializeMint2" | "getAccountDataSize" | "freezeAccount" | "thawAccount" | "amountToUiAmount"
                    | "uiAmountToAmount" | "initializeMintCloseAuthority" | "initializeMetadataPointer" | "initializeTokenMetadata" | "updateTokenMetadataField"
                    | "initializeTransferFeeConfig" | "reallocate" => {}
                    _ => {
                        for k in ["account", "source", "destination"] {
                            unexplained.extend(s(k));
                        }
                    }
                },
                "system" => match ty {
                    "transfer" | "transferWithSeed" => {
                        if let (Some(f), Some(t), Some(l)) = (s("source"), s("destination"), info["lamports"].as_u64()) {
                            flows.push(Flow::Lamports { ix: i, from: f, to: t, amount: l as i128 });
                        }
                    }
                    "createAccount" | "createAccountWithSeed" => {
                        if let (Some(f), Some(t), Some(l)) = (s("source"), s("newAccount"), info["lamports"].as_u64()) {
                            flows.push(Flow::Lamports { ix: i, from: f, to: t, amount: l as i128 });
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        self.flows = flows;
        self.closed_accounts = closed;
        unexplained.sort();
        unexplained.dedup();
        self.unexplained_token_accounts = unexplained;
    }

    pub fn key_index(&self, k: &str) -> Option<usize> {
        self.keys.iter().position(|x| x == k)
    }

    pub fn lamport_delta(&self, acct: &str) -> Option<i128> {
        let i = self.key_index(acct)?;
        Some(self.post_lamports.get(i)? - self.pre_lamports.get(i)?)
    }

    pub fn pre_lamports_of(&self, acct: &str) -> Option<i128> {
        self.pre_lamports.get(self.key_index(acct)?).copied()
    }

    /// Token delta of a token account, if it appears in the balance evidence.
    pub fn token_delta(&self, acct: &str) -> Option<i128> {
        self.tokens.get(acct).map(|t| t.delta())
    }

    /// Instruction indices executed inside `i` (its CPI subtree). None when call depth is unknown.
    pub fn subtree(&self, i: usize) -> Option<std::ops::Range<usize>> {
        let me = self.ixs.get(i)?;
        let h = me.stack_height?;
        let mut j = i + 1;
        while j < self.ixs.len() && self.ixs[j].outer == me.outer && self.ixs[j].inner_pos.is_some() {
            match self.ixs[j].stack_height {
                Some(hj) if hj > h => j += 1,
                Some(_) => break,
                None if me.inner_pos.is_none() => j += 1, // whole group belongs to a top-level ix
                None => return None,
            }
        }
        Some(i + 1..j)
    }

    /// Sum of parsed token flows into (+) / out of (−) a token account, restricted by `include(ix)`.
    pub fn token_flow_sum(&self, acct: &str, include: impl Fn(usize) -> bool) -> i128 {
        self.flows
            .iter()
            .filter_map(|f| match f {
                Flow::Token { ix, from, to, amount, .. } if include(*ix) => {
                    let mut s = 0;
                    if to.as_deref() == Some(acct) {
                        s += amount;
                    }
                    if from.as_deref() == Some(acct) {
                        s -= amount;
                    }
                    Some(s)
                }
                _ => None,
            })
            .sum()
    }

    /// Sum of parsed lamport flows into (+) / out of (−) an account, restricted by `include(ix)`.
    pub fn lamport_flow_sum(&self, acct: &str, include: impl Fn(usize) -> bool) -> i128 {
        self.flows
            .iter()
            .filter_map(|f| match f {
                Flow::Lamports { ix, from, to, amount } if include(*ix) => {
                    let mut s = 0;
                    if to == acct {
                        s += amount;
                    }
                    if from == acct {
                        s -= amount;
                    }
                    Some(s)
                }
                _ => None,
            })
            .sum()
    }

    /// Net minted (mintTo − burn) of a mint according to parsed instructions.
    pub fn net_minted(&self, mint: &str) -> i128 {
        self.flows
            .iter()
            .filter_map(|f| match f {
                Flow::Token { mint: Some(m), from: None, to: Some(_), amount, .. } if m == mint => Some(*amount),
                Flow::Token { mint: Some(m), from: Some(_), to: None, amount, .. } if m == mint => Some(-*amount),
                _ => None,
            })
            .sum()
    }

    /// Token transfers of `mint` executed inside instruction `i`'s subtree.
    pub fn transfers_in(&self, i: usize) -> Option<Vec<&Flow>> {
        let r = self.subtree(i)?;
        Some(self.flows.iter().filter(|f| matches!(f, Flow::Token { ix, .. } if r.contains(ix))).collect())
    }

    /// Instruction indices that name `acct`, in their account list or (parsed instructions) their info.
    pub fn referencing(&self, acct: &str) -> Vec<usize> {
        self.ixs
            .iter()
            .enumerate()
            .filter(|(_, ix)| ix.accounts.iter().any(|a| a == acct) || ix.info.as_object().is_some_and(|o| o.values().any(|v| v.as_str() == Some(acct))))
            .map(|(i, _)| i)
            .collect()
    }

    /// Lamport flows (parsed System transfers / account creation) into `acct` inside `range`.
    pub fn lamport_credits_in(&self, acct: &str, range: &std::ops::Range<usize>) -> Vec<(usize, i128)> {
        self.flows
            .iter()
            .filter_map(|f| match f {
                Flow::Lamports { ix, to, amount, .. } if to == acct && range.contains(ix) => Some((*ix, *amount)),
                _ => None,
            })
            .collect()
    }

    /// Human-readable instruction reference: "ix 3" or "ix 3.2".
    pub fn ix_ref(&self, i: usize) -> String {
        match self.ixs.get(i) {
            Some(ix) => match ix.inner_pos {
                Some(p) => format!("ix {}.{}", ix.outer, p),
                None => format!("ix {}", ix.outer),
            },
            None => "ix ?".into(),
        }
    }

    /// Base64 of an instruction's raw data (for logs / reports).
    pub fn data_b64(&self, i: usize) -> Option<String> {
        self.ixs.get(i)?.data.as_ref().map(|d| base64::engine::general_purpose::STANDARD.encode(d))
    }
}
