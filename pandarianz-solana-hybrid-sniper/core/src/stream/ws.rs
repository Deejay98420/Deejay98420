//! Standard Solana JSON-RPC WebSocket source with reconnection, resubscription and idle detection.
//!
//! * `logsSubscribe` (any RPC provider): logs only. Sees Pump.fun `emit!` events; cannot see
//!   PumpSwap `emit_cpi!` events (those live in inner instructions).
//! * `transactionSubscribe` (providers that offer it, jsonParsed encoding): logs + inner instructions.
//!
//! The URL may embed an API key; only `redact_url` output is ever logged.

use super::backoff::Backoff;
use super::{ConnState, InnerIx, Ingestor, RawEvent, StreamMsg};
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

pub fn subscribe_requests(cfg: &WsConfig) -> Vec<String> {
    let c = commitment_str(cfg.commitment);
    match &cfg.kind {
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
    }
}

/// Parse one provider message into a RawEvent (None for acks / unrelated messages).
pub fn parse_notification(text: &str, provider: &str, commitment: Commitment, recv_us: i64) -> Option<RawEvent> {
    let v: Value = serde_json::from_str(text).ok()?;
    let method = v.get("method")?.as_str()?;
    let result = v.get("params")?.get("result")?;
    match method {
        "logsNotification" => {
            let slot = result.get("context")?.get("slot")?.as_u64()?;
            let val = result.get("value")?;
            Some(RawEvent {
                provider: provider.into(),
                data_mode: DataMode::Live,
                slot,
                signature: val.get("signature")?.as_str()?.to_string(),
                commitment,
                err: val.get("err").filter(|e| !e.is_null()).map(|e| e.to_string()),
                logs: val
                    .get("logs")?
                    .as_array()?
                    .iter()
                    .filter_map(|l| l.as_str().map(String::from))
                    .collect(),
                inner_ix: vec![],
                block_time: None,
                recv_us,
            })
        }
        "transactionNotification" => {
            let tx = result.get("transaction")?;
            let meta = tx.get("meta")?;
            let sig = result
                .get("signature")
                .and_then(|s| s.as_str())
                .or_else(|| tx.get("transaction")?.get("signatures")?.get(0)?.as_str())?
                .to_string();
            let mut inner = Vec::new();
            if let Some(groups) = meta.get("innerInstructions").and_then(|g| g.as_array()) {
                for g in groups {
                    for ix in g.get("instructions").and_then(|i| i.as_array()).into_iter().flatten() {
                        let (Some(pid), Some(data)) =
                            (ix.get("programId").and_then(|p| p.as_str()), ix.get("data").and_then(|d| d.as_str()))
                        else {
                            continue;
                        };
                        // jsonParsed encodes unparsed instruction data as base58
                        if let Ok(bytes) = bs58::decode(data).into_vec() {
                            inner.push(InnerIx {
                                program_id: pid.to_string(),
                                data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                            });
                        }
                    }
                }
            }
            Some(RawEvent {
                provider: provider.into(),
                data_mode: DataMode::Live,
                slot: result.get("slot")?.as_u64()?,
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
            })
        }
        _ => None,
    }
}

/// Run until the ingestor closes or reconnect attempts are exhausted.
pub async fn run(mut cfg: WsConfig, ingestor: Ingestor) {
    let host = redact_url(&cfg.url);
    loop {
        let _ = ingestor
            .push(ingestor.connection(&cfg.provider, ConnState::Reconnecting, format!("connecting {host}")))
            .await;
        let mut got_data = false;
        match session(&cfg, &ingestor, &mut got_data).await {
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
                let _ = ingestor
                    .push(ingestor.connection(&cfg.provider, ConnState::Exhausted, "reconnect attempts exhausted"))
                    .await;
                return;
            }
        }
    }
}

async fn session(cfg: &WsConfig, ingestor: &Ingestor, got_data: &mut bool) -> anyhow::Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(cfg.url.as_str()).await?;
    let _ = ingestor.push(ingestor.connection(&cfg.provider, ConnState::Connected, redact_url(&cfg.url))).await;
    for req in subscribe_requests(cfg) {
        ws.send(Message::Text(req)).await?;
    }
    let mut ping = tokio::time::interval(cfg.ping_interval);
    ping.tick().await;
    let mut subscribed = false;
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
                        if !subscribed && t.contains("\"result\"") && t.contains("\"id\"") {
                            subscribed = true;
                            let _ = ingestor.push(ingestor.connection(&cfg.provider, ConnState::Subscribed, "subscription ack")).await;
                        }
                        if let Some(ev) = parse_notification(&t, &cfg.provider, cfg.commitment, recv) {
                            *got_data = true;
                            ingestor.push(StreamMsg::Event(ev)).await.map_err(|_| anyhow::anyhow!("engine closed"))?;
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
