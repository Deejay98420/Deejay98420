//! Standard Solana JSON-RPC WebSocket source with reconnection, resubscription and idle detection.
//!
//! * `logsSubscribe` (any RPC provider): logs only. Sees Pump.fun `emit!` events; cannot see
//!   PumpSwap `emit_cpi!` events (those live in inner instructions).
//! * `transactionSubscribe` (providers that offer it, jsonParsed encoding): logs + inner instructions.
//!
//! The URL may embed an API key; only `redact_url` output is ever logged.

use super::backoff::Backoff;
use super::{ConnState, InnerIx, Ingestor, RawEvent, SlotStatus, StreamMsg};
use crate::types::programs;
use tokio::sync::mpsc;
use crate::time::wall_us;
use crate::types::{Commitment, DataMode};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
pub enum SubscriptionKind {
    Logs { mentions: Vec<String> },
    Transactions { account_include: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct WsConfig {
    pub provider: String,
    pub url: String,
    pub kind: SubscriptionKind,
    pub commitment: Commitment,
    pub idle_timeout: Duration,
    pub ping_interval: Duration,
    pub backoff: Backoff,
    /// also subscribe to `slotSubscribe` (chain tip → slot lag + liveness)
    pub tip: bool,
    /// also subscribe to `slotsUpdatesSubscribe` (dead slots → rollback); many providers disable it
    pub slot_updates: bool,
}

/// Sends a signature for full-transaction fetch (logs-only providers cannot see emit_cpi events).
pub type FetchSender = mpsc::Sender<FetchReq>;

#[derive(Clone, Debug)]
pub struct FetchReq {
    pub signature: String,
    pub slot: u64,
    pub first_seen_us: i64,
    pub provider: String,
}

pub fn redact_url(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(p) => format!("{}://{}", p.scheme(), p.host_str().unwrap_or("?")),
        Err(_) => "<invalid url>".into(),
    }
}

fn commitment_str(c: Commitment) -> &'static str {
    match c {
        Commitment::Processed => "processed",
        Commitment::Confirmed => "confirmed",
        Commitment::Finalized => "finalized",
    }
}

/// Subscription methods keyed by request id (for capability checks).
pub fn subscribe_methods(cfg: &WsConfig) -> Vec<(u64, &'static str, bool /*primary*/)> {
    let mut v: Vec<(u64, &'static str, bool)> = match &cfg.kind {
        SubscriptionKind::Logs { mentions } => (0..mentions.len()).map(|i| (i as u64 + 1, "logsSubscribe", true)).collect(),
        SubscriptionKind::Transactions { .. } => vec![(1, "transactionSubscribe", true)],
    };
    if cfg.tip {
        v.push((900, "slotSubscribe", false));
    }
    if cfg.slot_updates {
        v.push((901, "slotsUpdatesSubscribe", false));
    }
    v
}

/// Classify a JSON-RPC response to a subscription request: Some((id, ok, detail)).
pub fn parse_ack(text: &str) -> Option<(u64, bool, String)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let id = v.get("id")?.as_u64()?;
    if v.get("method").is_some() {
        return None;
    }
    if let Some(e) = v.get("error") {
        let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("error");
        let code = e.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        return Some((id, false, format!("{code}: {}", msg.chars().take(120).collect::<String>())));
    }
    v.get("result").map(|r| (id, true, format!("subscription id {r}")))
}

pub fn subscribe_requests(cfg: &WsConfig) -> Vec<String> {
    let c = commitment_str(cfg.commitment);
    let mut out: Vec<String> = match &cfg.kind {
        SubscriptionKind::Logs { mentions } => mentions
            .iter()
            .enumerate()
            .map(|(i, m)| {
                json!({"jsonrpc":"2.0","id":i+1,"method":"logsSubscribe","params":[{"mentions":[m]},{"commitment":c}]})
                    .to_string()
            })
            .collect(),
        SubscriptionKind::Transactions { account_include } => vec![json!({
            "jsonrpc":"2.0","id":1,"method":"transactionSubscribe",
            "params":[{"accountInclude":account_include,"failed":false,"vote":false},
                      {"commitment":c,"encoding":"jsonParsed","transactionDetails":"full","maxSupportedTransactionVersion":0}]
        })
        .to_string()],
    };
    if cfg.tip {
        out.push(json!({"jsonrpc":"2.0","id":900,"method":"slotSubscribe"}).to_string());
    }
    if cfg.slot_updates {
        out.push(json!({"jsonrpc":"2.0","id":901,"method":"slotsUpdatesSubscribe"}).to_string());
    }
    out
}

/// Parse a full transaction object (getTransaction result or transactionNotification `transaction`).
pub fn parse_tx_object(tx: &Value, signature: Option<&str>, slot: u64, provider: &str, commitment: Commitment, recv_us: i64) -> Option<RawEvent> {
    let meta = tx.get("meta")?;
    let sig = signature
        .map(String::from)
        .or_else(|| tx.get("transaction")?.get("signatures")?.get(0)?.as_str().map(String::from))?;
    let mut inner = Vec::new();
    if let Some(groups) = meta.get("innerInstructions").and_then(|g| g.as_array()) {
        for g in groups {
            for ix in g.get("instructions").and_then(|i| i.as_array()).into_iter().flatten() {
                let (Some(pid), Some(data)) = (ix.get("programId").and_then(|p| p.as_str()), ix.get("data").and_then(|d| d.as_str())) else {
                    continue;
                };
                // jsonParsed encodes unparsed instruction data as base58
                if let Ok(bytes) = bs58::decode(data).into_vec() {
                    inner.push(InnerIx { program_id: pid.to_string(), data_b64: base64::engine::general_purpose::STANDARD.encode(bytes) });
                }
            }
        }
    }
    Some(RawEvent {
        provider: provider.into(),
        data_mode: DataMode::Live,
        slot: tx.get("slot").and_then(|s| s.as_u64()).unwrap_or(slot),
        signature: sig,
        commitment,
        err: meta.get("err").filter(|e| !e.is_null()).map(|e| e.to_string()),
        logs: meta
            .get("logMessages")
            .and_then(|l| l.as_array())
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        inner_ix: inner,
        block_time: tx.get("blockTime").and_then(|b| b.as_i64()),
        recv_us,
        backfill: false,
        fetch_us: None,
    })
}

/// Parsed provider message.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Event(RawEvent),
    Tip(u64),
    DeadSlot(u64),
}

/// Parse one provider message (None for acks / unrelated messages).
pub fn parse_message(text: &str, provider: &str, commitment: Commitment, recv_us: i64) -> Option<Parsed> {
    let v: Value = serde_json::from_str(text).ok()?;
    let method = v.get("method")?.as_str()?;
    let result = v.get("params")?.get("result")?;
    match method {
        "logsNotification" => {
            let slot = result.get("context")?.get("slot")?.as_u64()?;
            let val = result.get("value")?;
            Some(Parsed::Event(RawEvent {
                provider: provider.into(),
                data_mode: DataMode::Live,
                slot,
                signature: val.get("signature")?.as_str()?.to_string(),
                commitment,
                err: val.get("err").filter(|e| !e.is_null()).map(|e| e.to_string()),
                logs: val.get("logs")?.as_array()?.iter().filter_map(|l| l.as_str().map(String::from)).collect(),
                inner_ix: vec![],
                block_time: None,
                recv_us,
                backfill: false,
                fetch_us: None,
            }))
        }
        "transactionNotification" => {
            let tx = result.get("transaction")?;
            let slot = result.get("slot")?.as_u64()?;
            parse_tx_object(tx, result.get("signature").and_then(|s| s.as_str()), slot, provider, commitment, recv_us).map(Parsed::Event)
        }
        "slotNotification" => Some(Parsed::Tip(result.get("slot")?.as_u64()?)),
        "slotsUpdatesNotification" => {
            if result.get("type")?.as_str()? == "dead" {
                Some(Parsed::DeadSlot(result.get("slot")?.as_u64()?))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Back-compat helper: transaction/log notifications only.
pub fn parse_notification(text: &str, provider: &str, commitment: Commitment, recv_us: i64) -> Option<RawEvent> {
    match parse_message(text, provider, commitment, recv_us)? {
        Parsed::Event(e) => Some(e),
        _ => None,
    }
}

/// True when a logs-only event invokes PumpSwap (its events are in inner instructions → fetch tx).
pub fn needs_full_tx(ev: &RawEvent) -> bool {
    ev.inner_ix.is_empty() && ev.logs.iter().any(|l| l.starts_with(&format!("Program {} invoke", programs::PUMP_SWAP)))
}

/// Run until the ingestor closes or reconnect attempts are exhausted.
pub async fn run(cfg: WsConfig, ingestor: Ingestor) {
    run_with_fetch(cfg, ingestor, None).await
}

pub async fn run_with_fetch(mut cfg: WsConfig, ingestor: Ingestor, fetch: Option<FetchSender>) {
    let host = redact_url(&cfg.url);
    loop {
        let _ = ingestor.push(ingestor.connection(&cfg.provider, ConnState::Reconnecting, format!("connecting {host}"))).await;
        let mut got_data = false;
        match session(&cfg, &ingestor, &mut got_data, fetch.as_ref()).await {
            Ok(()) => tracing::warn!(provider = %cfg.provider, %host, "stream closed by server"),
            Err(e) => tracing::warn!(provider = %cfg.provider, %host, error = %e, "stream error"),
        }
        if got_data {
            // a session that delivered data resets the backoff schedule
            cfg.backoff.reset();
        }
        if ingestor.push(ingestor.connection(&cfg.provider, ConnState::Disconnected, "session ended")).await.is_err() {
            return;
        }
        match cfg.backoff.next_delay() {
            Some(d) => tokio::time::sleep(d).await,
            None => {
                let _ = ingestor.push(ingestor.connection(&cfg.provider, ConnState::Exhausted, "reconnect attempts exhausted")).await;
                return;
            }
        }
    }
}

async fn session(cfg: &WsConfig, ingestor: &Ingestor, got_data: &mut bool, fetch: Option<&FetchSender>) -> anyhow::Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(cfg.url.as_str()).await?;
    let _ = ingestor.push(ingestor.connection(&cfg.provider, ConnState::Connected, redact_url(&cfg.url))).await;
    for req in subscribe_requests(cfg) {
        ws.send(Message::Text(req)).await?;
    }
    let mut ping = tokio::time::interval(cfg.ping_interval);
    ping.tick().await;
    let methods = subscribe_methods(cfg);
    let mut primary_pending = methods.iter().filter(|m| m.2).count();
    let mut primary_ok = 0usize;
    let mut acked: std::collections::HashSet<u64> = Default::default();
    loop {
        tokio::select! {
            _ = ping.tick() => { ws.send(Message::Ping(vec![])).await?; }
            msg = tokio::time::timeout(cfg.idle_timeout, ws.next()) => {
                let msg = match msg {
                    Err(_) => anyhow::bail!("idle timeout ({:?}) — no messages", cfg.idle_timeout),
                    Ok(None) => return Ok(()),
                    Ok(Some(m)) => m?,
                };
                match msg {
                    Message::Text(t) => {
                        let recv = wall_us();
                        if let Some((id, ok, detail)) = parse_ack(&t) {
                            if let Some((_, method, primary)) = methods.iter().find(|m| m.0 == id) {
                                if acked.insert(id) {
                                    let _ = ingestor.push(StreamMsg::Capability { provider: cfg.provider.clone(), method: method.to_string(), ok, detail: detail.clone(), recv_us: recv }).await;
                                    if *primary {
                                        primary_pending -= 1;
                                        if ok {
                                            primary_ok += 1;
                                            if primary_ok == 1 {
                                                let _ = ingestor.push(ingestor.connection(&cfg.provider, ConnState::Subscribed, format!("{method} accepted"))).await;
                                            }
                                        } else if primary_pending == 0 && primary_ok == 0 {
                                            anyhow::bail!("provider rejected {method}: {detail}");
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        match parse_message(&t, &cfg.provider, cfg.commitment, recv) {
                            Some(Parsed::Event(ev)) => {
                                *got_data = true;
                                if let (Some(f), true) = (fetch, needs_full_tx(&ev)) {
                                    // forward when fetched (with inner instructions); the logs-only copy is not used
                                    let _ = f.try_send(FetchReq { signature: ev.signature.clone(), slot: ev.slot, first_seen_us: recv, provider: cfg.provider.clone() });
                                } else {
                                    ingestor.push(StreamMsg::Event(ev)).await.map_err(|_| anyhow::anyhow!("engine closed"))?;
                                }
                            }
                            Some(Parsed::Tip(slot)) => {
                                ingestor.push(StreamMsg::Tip { provider: cfg.provider.clone(), slot, recv_us: recv }).await.map_err(|_| anyhow::anyhow!("engine closed"))?;
                            }
                            Some(Parsed::DeadSlot(slot)) => {
                                ingestor.push(StreamMsg::Slot { slot, status: SlotStatus::Dead, recv_us: recv }).await.map_err(|_| anyhow::anyhow!("engine closed"))?;
                            }
                            None => {}
                        }
                    }
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}
