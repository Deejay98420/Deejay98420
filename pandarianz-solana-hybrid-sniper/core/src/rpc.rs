//! Minimal READ-ONLY Solana JSON-RPC client with ordered failover across endpoints.
//! Methods: getTransaction, getSignaturesForAddress, getAccountInfo, getSlot. Nothing here can
//! submit anything; the allowed method list is enforced in `call`.

use crate::quote::ratelimit::TokenBucket;
use crate::stream::ws::{parse_tx_object, redact_url};
use crate::stream::RawEvent;
use crate::time::wall_us;
use crate::types::Commitment;
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const ALLOWED_METHODS: &[&str] = &["getTransaction", "getSignaturesForAddress", "getAccountInfo", "getSlot", "getHealth"];

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("method {0} not allowed (read-only client)")]
    NotAllowed(String),
    #[error("all endpoints failed: {0}")]
    AllFailed(String),
    #[error("rate limited locally")]
    RateLimited,
    #[error("rpc error: {0}")]
    Rpc(String),
}

#[derive(Default, Debug, Serialize, Clone)]
pub struct EndpointStats {
    pub host: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub rate_limited: u64,
    pub last_error: Option<String>,
    pub last_latency_us: Option<i64>,
}

#[derive(Default)]
struct EpCounters {
    requests: AtomicU64,
    ok: AtomicU64,
    errors: AtomicU64,
    rate_limited: AtomicU64,
    last_latency_us: AtomicU64,
}

pub struct RpcClient {
    urls: Vec<String>,
    http: reqwest::Client,
    bucket: Mutex<TokenBucket>,
    counters: Vec<EpCounters>,
    last_error: Mutex<Vec<Option<String>>>,
    pub commitment: Commitment,
}

#[derive(Clone, Debug, Serialize)]
pub struct SigInfo {
    pub signature: String,
    pub slot: u64,
    pub failed: bool,
    pub block_time: Option<i64>,
}

impl RpcClient {
    pub fn new(urls: Vec<String>, timeout: Duration, rate_per_s: f64, burst: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(!urls.is_empty(), "no RPC URLs");
        let n = urls.len();
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            bucket: Mutex::new(TokenBucket::new(rate_per_s, burst, wall_us())),
            counters: (0..n).map(|_| EpCounters::default()).collect(),
            last_error: Mutex::new(vec![None; n]),
            urls,
            commitment: Commitment::Confirmed,
        })
    }

    pub fn stats(&self) -> Vec<EndpointStats> {
        let le = self.last_error.lock().unwrap();
        self.urls
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let c = &self.counters[i];
                EndpointStats {
                    host: redact_url(u),
                    requests: c.requests.load(Ordering::Relaxed),
                    ok: c.ok.load(Ordering::Relaxed),
                    errors: c.errors.load(Ordering::Relaxed),
                    rate_limited: c.rate_limited.load(Ordering::Relaxed),
                    last_error: le[i].clone(),
                    last_latency_us: Some(c.last_latency_us.load(Ordering::Relaxed) as i64).filter(|v| *v > 0),
                }
            })
            .collect()
    }

    /// JSON-RPC call with ordered failover (first healthy endpoint wins).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if !ALLOWED_METHODS.contains(&method) {
            return Err(RpcError::NotAllowed(method.into()));
        }
        if self.bucket.lock().unwrap().try_acquire(wall_us()).is_err() {
            return Err(RpcError::RateLimited);
        }
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let mut errs = Vec::new();
        for (i, url) in self.urls.iter().enumerate() {
            let c = &self.counters[i];
            c.requests.fetch_add(1, Ordering::Relaxed);
            let t0 = wall_us();
            let res = self.http.post(url).json(&body).send().await;
            let err = match res {
                Ok(r) if r.status().as_u16() == 429 => {
                    c.rate_limited.fetch_add(1, Ordering::Relaxed);
                    "http 429".to_string()
                }
                Ok(r) if !r.status().is_success() => format!("http {}", r.status()),
                Ok(r) => match r.json::<Value>().await {
                    Ok(v) => {
                        if let Some(e) = v.get("error") {
                            format!("rpc error {}", e)
                        } else {
                            c.ok.fetch_add(1, Ordering::Relaxed);
                            c.last_latency_us.store((wall_us() - t0).max(1) as u64, Ordering::Relaxed);
                            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                    Err(e) => format!("decode: {}", e.without_url()),
                },
                Err(e) => format!("transport: {}", e.without_url()),
            };
            c.errors.fetch_add(1, Ordering::Relaxed);
            self.last_error.lock().unwrap()[i] = Some(err.clone());
            errs.push(format!("{}: {err}", redact_url(url)));
        }
        Err(RpcError::AllFailed(errs.join("; ")))
    }

    fn commitment_str(&self) -> &'static str {
        match self.commitment {
            Commitment::Processed | Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }

    /// Full transaction (jsonParsed) as a provider-neutral RawEvent, or None if not found yet.
    pub async fn get_transaction(&self, signature: &str, provider: &str) -> Result<Option<RawEvent>, RpcError> {
        let v = self
            .call(
                "getTransaction",
                json!([signature, {"encoding":"jsonParsed","commitment":self.commitment_str(),"maxSupportedTransactionVersion":0}]),
            )
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        let slot = v.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
        Ok(parse_tx_object(&v, Some(signature), slot, provider, self.commitment, wall_us()))
    }

    /// Raw getTransaction JSON (for capturing regression fixtures verbatim).
    pub async fn get_transaction_raw(&self, signature: &str) -> Result<Value, RpcError> {
        self.call("getTransaction", json!([signature, {"encoding":"jsonParsed","commitment":self.commitment_str(),"maxSupportedTransactionVersion":0}])).await
    }

    pub async fn get_signatures_for_address(&self, address: &str, limit: usize, before: Option<&str>) -> Result<Vec<SigInfo>, RpcError> {
        let mut opts = json!({"limit": limit.min(1000), "commitment": self.commitment_str()});
        if let Some(b) = before {
            opts["before"] = Value::String(b.into());
        }
        let v = self.call("getSignaturesForAddress", json!([address, opts])).await?;
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        Some(SigInfo {
                            signature: x.get("signature")?.as_str()?.to_string(),
                            slot: x.get("slot")?.as_u64()?,
                            failed: x.get("err").map(|e| !e.is_null()).unwrap_or(false),
                            block_time: x.get("blockTime").and_then(|b| b.as_i64()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Raw account data and owner program, or None if the account does not exist.
    pub async fn get_account(&self, address: &str) -> Result<Option<(Vec<u8>, String)>, RpcError> {
        let v = self.call("getAccountInfo", json!([address, {"encoding":"base64","commitment":self.commitment_str()}])).await?;
        let Some(val) = v.get("value").filter(|x| !x.is_null()) else { return Ok(None) };
        let owner = val.get("owner").and_then(|o| o.as_str()).unwrap_or("").to_string();
        let data = val
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|d| d.as_str())
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .ok_or_else(|| RpcError::Rpc("account data missing".into()))?;
        Ok(Some((data, owner)))
    }

    pub async fn get_slot(&self) -> Result<u64, RpcError> {
        self.call("getSlot", json!([{"commitment": "processed"}])).await?.as_u64().ok_or_else(|| RpcError::Rpc("bad slot".into()))
    }
}
