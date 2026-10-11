//! Provider diagnostics: explicit connectivity and subscription-capability checks for each configured
//! WebSocket / RPC / Jupiter endpoint, with a concrete recommendation. Read-only. Every string that
//! leaves this module is scrubbed of URLs, paths and query parameters (API keys).

use crate::decode::{decode_raw, VenueEvent};
use crate::rpc::RpcClient;
use crate::stream::ws::{parse_ack, redact_url};
use crate::time::wall_us;
use crate::types::{programs, Commitment};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Remove every configured URL (and its query/path secrets) from free text.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if s.is_empty() {
            continue;
        }
        out = out.replace(s.as_str(), &redact_url(s));
        if let Ok(u) = url::Url::parse(s) {
            for (_, v) in u.query_pairs() {
                if v.len() >= 6 {
                    out = out.replace(v.as_ref(), "<redacted>");
                }
            }
            let path = u.path().trim_matches('/');
            if path.len() >= 12 {
                out = out.replace(path, "<redacted>");
            }
            // individual long path segments (providers often put the key in the path)
            for seg in u.path().split('/').filter(|x| x.len() >= 12) {
                out = out.replace(seg, "<redacted>");
            }
        }
    }
    out
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct SubscriptionResult {
    pub accepted: Option<bool>,
    pub detail: String,
    pub notifications: u64,
    pub first_notification_ms: Option<f64>,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct WsProbe {
    pub host: String,
    pub connected: bool,
    pub connect_ms: Option<f64>,
    pub error: Option<String>,
    pub subscriptions: BTreeMap<String, SubscriptionResult>,
    /// transactionSubscribe notifications that carried inner instructions (needed for PumpSwap)
    pub tx_notifications_with_inner_ix: u64,
    pub decoded_pump_events: u64,
    pub decoded_pumpswap_events: u64,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct RpcProbe {
    pub host: String,
    pub ok: bool,
    pub get_slot_ms: Option<f64>,
    pub slot: Option<u64>,
    pub pump_tx_checked: Option<String>,
    pub pump_events_decoded: u64,
    pub pumpswap_tx_checked: Option<String>,
    pub pumpswap_events_decoded: u64,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct JupiterProbe {
    pub configured: bool,
    pub ok: bool,
    pub latency_ms: Option<f64>,
    pub route: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct DoctorReport {
    pub generated_us: i64,
    pub ws: Vec<WsProbe>,
    pub rpc: Vec<RpcProbe>,
    pub jupiter: JupiterProbe,
    /// "logs" (+getTransaction fetch) or "transactions"
    pub recommended_stream_kind: Option<String>,
    pub pumpswap_coverage: String,
    pub ready_for_live: bool,
    pub problems: Vec<String>,
    pub next_action: String,
}

const IDS: &[(u64, &str)] = &[(1, "logsSubscribe:pump"), (2, "logsSubscribe:pumpswap"), (3, "transactionSubscribe"), (900, "slotSubscribe"), (901, "slotsUpdatesSubscribe")];

/// Connect to one WebSocket provider, request every subscription type and listen for `listen`.
pub async fn probe_ws(url: &str, listen: Duration) -> WsProbe {
    let mut p = WsProbe { host: redact_url(url), ..Default::default() };
    let secrets = vec![url.to_string()];
    let t0 = wall_us();
    let conn = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(url)).await;
    let mut ws = match conn {
        Err(_) => {
            p.error = Some("connect timeout (10s)".into());
            return p;
        }
        Ok(Err(e)) => {
            p.error = Some(scrub(&e.to_string(), &secrets));
            return p;
        }
        Ok(Ok((ws, _))) => ws,
    };
    p.connected = true;
    p.connect_ms = Some((wall_us() - t0) as f64 / 1e3);
    let reqs = [
        json!({"jsonrpc":"2.0","id":1,"method":"logsSubscribe","params":[{"mentions":[programs::PUMP_FUN]},{"commitment":"confirmed"}]}),
        json!({"jsonrpc":"2.0","id":2,"method":"logsSubscribe","params":[{"mentions":[programs::PUMP_SWAP]},{"commitment":"confirmed"}]}),
        json!({"jsonrpc":"2.0","id":3,"method":"transactionSubscribe","params":[{"accountInclude":[programs::PUMP_FUN, programs::PUMP_SWAP],"failed":false,"vote":false},{"commitment":"confirmed","encoding":"jsonParsed","transactionDetails":"full","maxSupportedTransactionVersion":0}]}),
        json!({"jsonrpc":"2.0","id":900,"method":"slotSubscribe"}),
        json!({"jsonrpc":"2.0","id":901,"method":"slotsUpdatesSubscribe"}),
    ];
    for r in reqs {
        if let Err(e) = ws.send(Message::Text(r.to_string())).await {
            p.error = Some(scrub(&e.to_string(), &secrets));
            return p;
        }
    }
    for (_, name) in IDS {
        p.subscriptions.insert(name.to_string(), SubscriptionResult::default());
    }
    // subscription id (server) → our method name
    let mut sub_ids: BTreeMap<u64, String> = BTreeMap::new();
    let start = wall_us();
    let deadline = tokio::time::Instant::now() + listen;
    loop {
        let msg = match tokio::time::timeout_at(deadline, ws.next()).await {
            Err(_) | Ok(None) => break,
            Ok(Some(Err(e))) => {
                p.error = Some(scrub(&e.to_string(), &secrets));
                break;
            }
            Ok(Some(Ok(m))) => m,
        };
        let Message::Text(t) = msg else { continue };
        if let Some((id, ok, detail)) = parse_ack(&t) {
            if let Some((_, name)) = IDS.iter().find(|x| x.0 == id) {
                let e = p.subscriptions.get_mut(*name).unwrap();
                e.accepted = Some(ok);
                e.detail = scrub(&detail, &secrets);
                if ok {
                    if let Some(sid) = serde_json::from_str::<Value>(&t).ok().and_then(|v| v["result"].as_u64()) {
                        sub_ids.insert(sid, name.to_string());
                    }
                }
            }
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
        let sid = v["params"]["subscription"].as_u64().unwrap_or(u64::MAX);
        let method = v["method"].as_str().unwrap_or("");
        let name = sub_ids.get(&sid).cloned().unwrap_or_else(|| match method {
            "slotNotification" => "slotSubscribe".into(),
            "slotsUpdatesNotification" => "slotsUpdatesSubscribe".into(),
            "transactionNotification" => "transactionSubscribe".into(),
            _ => "logsSubscribe:pump".into(),
        });
        if let Some(e) = p.subscriptions.get_mut(&name) {
            e.notifications += 1;
            if e.first_notification_ms.is_none() {
                e.first_notification_ms = Some((wall_us() - start) as f64 / 1e3);
            }
        }
        if let Some(Ok(ev)) = (method == "logsNotification" || method == "transactionNotification")
            .then(|| crate::stream::ws::parse_message(&t, "doctor", Commitment::Confirmed, wall_us()).ok_or(()))
        {
            if let crate::stream::ws::Parsed::Event(raw) = ev {
                if method == "transactionNotification" && !raw.inner_ix.is_empty() {
                    p.tx_notifications_with_inner_ix += 1;
                }
                for d in decode_raw(&raw).events {
                    match d.event {
                        VenueEvent::Pump(_) => p.decoded_pump_events += 1,
                        VenueEvent::PumpSwap(_) => p.decoded_pumpswap_events += 1,
                        _ => {}
                    }
                }
            }
        }
    }
    let _ = ws.close(None).await;
    p
}

/// RPC probe: latency + real getTransaction decode of one Pump.fun and one PumpSwap transaction.
pub async fn probe_rpc(url: &str) -> RpcProbe {
    let mut p = RpcProbe { host: redact_url(url), ..Default::default() };
    let secrets = vec![url.to_string()];
    let rpc = match RpcClient::new(vec![url.to_string()], Duration::from_secs(10), 5.0, 10) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            p.error = Some(scrub(&e.to_string(), &secrets));
            return p;
        }
    };
    let t0 = wall_us();
    match rpc.get_slot().await {
        Ok(s) => {
            p.slot = Some(s);
            p.get_slot_ms = Some((wall_us() - t0) as f64 / 1e3);
            p.ok = true;
        }
        Err(e) => {
            p.error = Some(scrub(&e.to_string(), &secrets));
            return p;
        }
    }
    for (prog, is_swap) in [(programs::PUMP_FUN, false), (programs::PUMP_SWAP, true)] {
        let sigs = match rpc.get_signatures_for_address(prog, 10, None).await {
            Ok(s) => s,
            Err(e) => {
                p.error = Some(scrub(&e.to_string(), &secrets));
                continue;
            }
        };
        for s in sigs.into_iter().filter(|s| !s.failed).take(5) {
            if let Ok(Some(raw)) = rpc.get_transaction(&s.signature, "doctor").await {
                let n = decode_raw(&raw).events.len() as u64;
                if n > 0 {
                    if is_swap {
                        p.pumpswap_tx_checked = Some(s.signature.clone());
                        p.pumpswap_events_decoded += n;
                    } else {
                        p.pump_tx_checked = Some(s.signature.clone());
                        p.pump_events_decoded += n;
                    }
                    break;
                }
            }
        }
    }
    p
}

pub async fn probe_jupiter(quote_url: Option<&str>, api_key: Option<String>) -> JupiterProbe {
    let Some(u) = quote_url.filter(|u| !u.is_empty()) else {
        return JupiterProbe { configured: false, error: Some("JUPITER_QUOTE_URL not set (optional)".into()), ..Default::default() };
    };
    let secrets = vec![u.to_string()];
    let c = match crate::quote::jupiter::JupiterClient::new(crate::quote::jupiter::JupiterConfig {
        quote_url: u.into(),
        api_key,
        timeout: Duration::from_secs(5),
        rate_per_s: 1.0,
        burst: 1,
        slippage_bps: 100,
    }) {
        Ok(c) => c,
        Err(e) => return JupiterProbe { configured: true, error: Some(scrub(&e.to_string(), &secrets)), ..Default::default() },
    };
    // 0.01 SOL → USDC: a liquid route that must exist if the endpoint works
    let usdc = crate::types::Pubkey::from_b58("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
    match c.quote(usdc, crate::quote::Side::Buy, 10_000_000).await {
        Ok(q) => JupiterProbe { configured: true, ok: true, latency_ms: Some(q.latency_us as f64 / 1e3), route: Some(q.route), error: None },
        Err(e) => JupiterProbe { configured: true, error: Some(scrub(&e.to_string(), &secrets)), ..Default::default() },
    }
}

/// Combine probes into a verdict and a single next action.
pub fn assess(ws: Vec<WsProbe>, rpc: Vec<RpcProbe>, jupiter: JupiterProbe) -> DoctorReport {
    let mut problems = Vec::new();
    let ws_ok: Vec<&WsProbe> = ws.iter().filter(|w| w.connected).collect();
    if ws.is_empty() {
        problems.push("no WebSocket provider configured (SOLANA_WS_URLS)".to_string());
    }
    for w in &ws {
        if !w.connected {
            problems.push(format!("{}: cannot connect ({})", w.host, w.error.clone().unwrap_or_default()));
        }
    }
    let accepted = |w: &WsProbe, k: &str| w.subscriptions.get(k).and_then(|s| s.accepted) == Some(true);
    let tx_native = ws_ok.iter().any(|w| accepted(w, "transactionSubscribe") && (w.tx_notifications_with_inner_ix > 0 || w.decoded_pumpswap_events > 0));
    let tx_accepted = ws_ok.iter().any(|w| accepted(w, "transactionSubscribe"));
    let logs = ws_ok.iter().any(|w| accepted(w, "logsSubscribe:pump"));
    let logs_data = ws_ok.iter().any(|w| w.subscriptions.get("logsSubscribe:pump").is_some_and(|s| s.notifications > 0));
    let rpc_fetch = rpc.iter().any(|r| r.ok && r.pumpswap_events_decoded > 0);
    let rpc_ok = rpc.iter().any(|r| r.ok);
    if !ws_ok.is_empty() && !logs && !tx_accepted {
        problems.push("no provider accepted logsSubscribe or transactionSubscribe".into());
    }
    if logs && !logs_data && !tx_native {
        problems.push("logsSubscribe accepted but no Pump.fun notifications arrived during the probe window".into());
    }
    if !rpc_ok {
        problems.push("no RPC endpoint answered getSlot (needed for backfill, pool discovery, mint checks, PumpSwap fetch)".into());
    }
    let (kind, coverage) = if tx_native {
        (Some("transactions".to_string()), "native (transactionSubscribe with inner instructions)".to_string())
    } else if logs && rpc_fetch {
        (Some("logs".to_string()), "via getTransaction fetch (logsSubscribe + RPC)".to_string())
    } else if logs {
        (Some("logs".to_string()), "NOT VERIFIED: no PumpSwap transaction could be fetched and decoded".to_string())
    } else {
        (None, "NONE".to_string())
    };
    if coverage.starts_with("NOT") {
        problems.push("PumpSwap coverage not confirmed".into());
    }
    let ready = problems.is_empty() && kind.is_some();
    let next_action = if ws.is_empty() {
        "Add SOLANA_WS_URLS (and optionally SOLANA_RPC_URLS) to .env, then rerun".to_string()
    } else if !ready {
        format!("Fix: {}", problems.first().cloned().unwrap_or_default())
    } else {
        format!("Set SOLANA_STREAM_KIND={} in .env, then run `./scripts/sniper capture 25`", kind.clone().unwrap())
    };
    DoctorReport {
        generated_us: wall_us(),
        ws,
        rpc,
        jupiter,
        recommended_stream_kind: kind,
        pumpswap_coverage: coverage,
        ready_for_live: ready,
        problems,
        next_action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrub_removes_keys_paths_and_full_urls() {
        let url = "wss://mainnet.example-rpc.com/v2/abcdef0123456789secretpath?api-key=SUPERSECRETKEY123";
        let text = format!("error connecting to {url}: 401 for key SUPERSECRETKEY123 at abcdef0123456789secretpath");
        let s = scrub(&text, &[url.to_string()]);
        assert!(!s.contains("SUPERSECRETKEY123"), "{s}");
        assert!(!s.contains("abcdef0123456789secretpath"), "{s}");
        assert!(s.contains("wss://mainnet.example-rpc.com"));
    }
    #[test]
    fn assessment_prefers_native_transactions_and_flags_gaps() {
        let mut w = WsProbe { host: "wss://a".into(), connected: true, ..Default::default() };
        w.subscriptions.insert("logsSubscribe:pump".into(), SubscriptionResult { accepted: Some(true), notifications: 5, ..Default::default() });
        w.subscriptions.insert("transactionSubscribe".into(), SubscriptionResult { accepted: Some(false), detail: "-32601: Method not found".into(), ..Default::default() });
        let r = RpcProbe { host: "https://a".into(), ok: true, pumpswap_events_decoded: 2, ..Default::default() };
        let rep = assess(vec![w.clone()], vec![r], JupiterProbe::default());
        assert!(rep.ready_for_live, "{:?}", rep.problems);
        assert_eq!(rep.recommended_stream_kind.as_deref(), Some("logs"));
        assert!(rep.pumpswap_coverage.contains("getTransaction"));
        // without a working RPC, PumpSwap coverage is not confirmed
        let rep = assess(vec![w], vec![], JupiterProbe::default());
        assert!(!rep.ready_for_live);
        assert!(rep.pumpswap_coverage.starts_with("NOT"));
        let rep = assess(vec![], vec![], JupiterProbe::default());
        assert!(rep.next_action.contains("SOLANA_WS_URLS"));
    }
}
