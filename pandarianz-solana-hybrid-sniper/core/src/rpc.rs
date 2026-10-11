//! Minimal READ-ONLY Solana JSON-RPC client with ordered failover across endpoints, built around a
//! single quota-aware request scheduler. Methods: getTransaction, getSignaturesForAddress,
//! getAccountInfo, getSlot, getHealth. Nothing here can submit anything; the allowed method list is
//! enforced in `call`.
//!
//! One `RpcClient` is shared (Arc) by everything in a process that talks to the RPC provider —
//! full-transaction fetches, startup backfill, mint/pool checks, capture and verification — so all
//! of them draw from one budget:
//! * a token bucket (requests/s + burst) that callers WAIT on, up to `max_wait`, instead of failing;
//! * bounded concurrency (`max_in_flight`) and a bounded wait queue (`max_queue`): when the queue is
//!   full new requests are rejected immediately (`QueueFull`), so no unbounded backlog can build up;
//! * HTTP 429 handling: honours `Retry-After`, otherwise exponential backoff with jitter; the whole
//!   client cools down (not just the one request), then the request is retried up to `max_retries`;
//! * an optional total request budget (`BudgetExhausted` once spent);
//! * in-flight deduplication: identical concurrent requests share one HTTP call.
//!
//! Every outcome is counted separately (sent / ok / retried / deferred / expired / rejected / ...).

use crate::quote::ratelimit::TokenBucket;
use crate::stream::ws::{parse_tx_object, redact_url};
use crate::stream::RawEvent;
use crate::time::wall_us;
use crate::types::Commitment;
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{watch, Semaphore};

const ALLOWED_METHODS: &[&str] = &["getTransaction", "getSignaturesForAddress", "getAccountInfo", "getSlot", "getHealth"];

/// `maxSupportedTransactionVersion` sent with getTransaction / transactionSubscribe. Version 1
/// transactions are rejected with JSON-RPC error -32015 unless this is ≥ 1. Override with
/// `SOLANA_MAX_TX_VERSION` (default 1; 0 only for providers that reject 1).
pub fn max_tx_version() -> u8 {
    static V: OnceLock<u8> = OnceLock::new();
    *V.get_or_init(|| std::env::var("SOLANA_MAX_TX_VERSION").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(DEFAULT_MAX_TX_VERSION))
}
pub const DEFAULT_MAX_TX_VERSION: u8 = 1;

/// getTransaction request parameters (jsonParsed, explicit max supported version).
pub fn get_transaction_params(signature: &str, commitment: &str, max_version: u8) -> Value {
    json!([signature, {"encoding":"jsonParsed","commitment":commitment,"maxSupportedTransactionVersion":max_version}])
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RpcError {
    #[error("method {0} not allowed (read-only client)")]
    NotAllowed(String),
    #[error("request queue full (rejected, not sent)")]
    QueueFull,
    #[error("expired after waiting {0} ms for rate-limit capacity (not sent)")]
    Expired(u64),
    #[error("request budget exhausted (not sent)")]
    BudgetExhausted,
    #[error("provider rate limit (HTTP 429) persisted after {0} retries")]
    RateLimited(u32),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("all endpoints failed: {0}")]
    AllFailed(String),
}

impl RpcError {
    /// Short class used for aggregated error reporting.
    pub fn class(&self) -> String {
        match self {
            RpcError::NotAllowed(_) => "not_allowed".into(),
            RpcError::QueueFull => "queue_full".into(),
            RpcError::Expired(_) => "expired".into(),
            RpcError::BudgetExhausted => "budget_exhausted".into(),
            RpcError::RateLimited(_) => "http_429".into(),
            RpcError::Rpc { code, .. } => format!("rpc_{code}"),
            RpcError::AllFailed(_) => "transport".into(),
        }
    }
    /// The request never reached the provider (local scheduling decision).
    pub fn not_sent(&self) -> bool {
        matches!(self, RpcError::QueueFull | RpcError::Expired(_) | RpcError::BudgetExhausted | RpcError::NotAllowed(_))
    }
}

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    pub rate_per_s: f64,
    pub burst: u32,
    pub max_in_flight: usize,
    pub max_queue: usize,
    pub max_wait: Duration,
    pub max_retries: u32,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    pub request_budget: Option<u64>,
    pub timeout: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            rate_per_s: 10.0,
            burst: 10,
            max_in_flight: 4,
            max_queue: 64,
            max_wait: Duration::from_secs(15),
            max_retries: 3,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            request_budget: None,
            timeout: Duration::from_secs(10),
        }
    }
}

impl SchedulerConfig {
    /// Defaults overridden by `SOLANA_RPC_RATE_PER_S`, `SOLANA_RPC_BURST`, `SOLANA_RPC_MAX_IN_FLIGHT`,
    /// `SOLANA_RPC_MAX_QUEUE`, `SOLANA_RPC_MAX_WAIT_MS`, `SOLANA_RPC_MAX_RETRIES`.
    pub fn from_env() -> Self {
        let n = |k: &str, d: f64| std::env::var(k).ok().and_then(|s| s.trim().parse::<f64>().ok()).filter(|x| *x > 0.0).unwrap_or(d);
        let d = Self::default();
        let rate = n("SOLANA_RPC_RATE_PER_S", d.rate_per_s);
        Self {
            rate_per_s: rate,
            burst: n("SOLANA_RPC_BURST", rate.max(1.0)) as u32,
            max_in_flight: n("SOLANA_RPC_MAX_IN_FLIGHT", d.max_in_flight as f64) as usize,
            max_queue: n("SOLANA_RPC_MAX_QUEUE", d.max_queue as f64) as usize,
            max_wait: Duration::from_millis(n("SOLANA_RPC_MAX_WAIT_MS", d.max_wait.as_millis() as f64) as u64),
            max_retries: std::env::var("SOLANA_RPC_MAX_RETRIES").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(d.max_retries),
            ..d
        }
    }
}

/// Scheduler counters (all requests through this client).
#[derive(Default, Debug, Serialize, Clone)]
pub struct SchedulerStats {
    /// calls made by callers
    pub calls: u64,
    /// HTTP requests actually sent (including retries and failover attempts)
    pub http_sent: u64,
    pub ok: u64,
    pub http_429: u64,
    pub retries: u64,
    /// calls that had to wait for rate-limit capacity before being sent
    pub deferred: u64,
    /// calls dropped after waiting longer than `max_wait` (never sent)
    pub expired: u64,
    /// calls rejected because the wait queue was full (never sent)
    pub queue_rejected: u64,
    /// calls rejected because the request budget was spent (never sent)
    pub budget_rejected: u64,
    /// calls answered by an identical in-flight request
    pub deduplicated: u64,
    pub rpc_errors: u64,
    pub transport_errors: u64,
    /// times the client entered a provider-imposed cooldown
    pub cooldowns: u64,
    pub total_wait_ms: u64,
    pub queue_depth: u64,
    pub max_queue_depth: u64,
}

#[derive(Default)]
struct Counters {
    calls: AtomicU64,
    http_sent: AtomicU64,
    ok: AtomicU64,
    http_429: AtomicU64,
    retries: AtomicU64,
    deferred: AtomicU64,
    expired: AtomicU64,
    queue_rejected: AtomicU64,
    budget_rejected: AtomicU64,
    deduplicated: AtomicU64,
    rpc_errors: AtomicU64,
    transport_errors: AtomicU64,
    cooldowns: AtomicU64,
    total_wait_ms: AtomicU64,
    max_queue_depth: AtomicU64,
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

type Shared = watch::Receiver<Option<Result<Value, RpcError>>>;

pub struct RpcClient {
    urls: Vec<String>,
    http: reqwest::Client,
    cfg: SchedulerConfig,
    bucket: Mutex<TokenBucket>,
    in_flight: Semaphore,
    waiting: AtomicUsize,
    sent_budget: AtomicU64,
    inflight: Mutex<HashMap<String, Shared>>,
    counters: Vec<EpCounters>,
    c: Counters,
    last_error: Mutex<Vec<Option<String>>>,
    jitter_state: AtomicU64,
    pub commitment: Commitment,
    pub max_tx_version: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct SigInfo {
    pub signature: String,
    pub slot: u64,
    pub failed: bool,
    pub block_time: Option<i64>,
}

struct InflightGuard<'a> {
    map: &'a Mutex<HashMap<String, Shared>>,
    key: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.key);
    }
}

/// One HTTP attempt's outcome.
enum Attempt {
    Ok(Value),
    RateLimited { retry_after: Option<Duration> },
    Rpc { code: i64, message: String },
    Failed(String),
}

/// Parse a `Retry-After` header value (delta-seconds; HTTP dates are ignored → None).
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0).map(|s| Duration::from_millis((s * 1000.0) as u64))
}

impl RpcClient {
    /// Back-compat constructor: rate/burst/timeout with the other scheduler settings from defaults.
    pub fn new(urls: Vec<String>, timeout: Duration, rate_per_s: f64, burst: u32) -> anyhow::Result<Self> {
        Self::with_config(urls, SchedulerConfig { rate_per_s, burst, timeout, ..SchedulerConfig::default() })
    }

    pub fn with_config(urls: Vec<String>, cfg: SchedulerConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(!urls.is_empty(), "no RPC URLs");
        anyhow::ensure!(cfg.rate_per_s > 0.0 && cfg.max_in_flight > 0, "invalid scheduler config");
        let n = urls.len();
        Ok(Self {
            http: reqwest::Client::builder().timeout(cfg.timeout).build()?,
            bucket: Mutex::new(TokenBucket::new(cfg.rate_per_s, cfg.burst.max(1), wall_us())),
            in_flight: Semaphore::new(cfg.max_in_flight),
            waiting: AtomicUsize::new(0),
            sent_budget: AtomicU64::new(0),
            inflight: Mutex::new(HashMap::new()),
            counters: (0..n).map(|_| EpCounters::default()).collect(),
            c: Counters::default(),
            last_error: Mutex::new(vec![None; n]),
            jitter_state: AtomicU64::new(wall_us() as u64 | 1),
            urls,
            cfg,
            commitment: Commitment::Confirmed,
            max_tx_version: max_tx_version(),
        })
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
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

    pub fn scheduler_stats(&self) -> SchedulerStats {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SchedulerStats {
            calls: l(&self.c.calls),
            http_sent: l(&self.c.http_sent),
            ok: l(&self.c.ok),
            http_429: l(&self.c.http_429),
            retries: l(&self.c.retries),
            deferred: l(&self.c.deferred),
            expired: l(&self.c.expired),
            queue_rejected: l(&self.c.queue_rejected),
            budget_rejected: l(&self.c.budget_rejected),
            deduplicated: l(&self.c.deduplicated),
            rpc_errors: l(&self.c.rpc_errors),
            transport_errors: l(&self.c.transport_errors),
            cooldowns: l(&self.c.cooldowns),
            total_wait_ms: l(&self.c.total_wait_ms),
            queue_depth: self.waiting.load(Ordering::Relaxed) as u64,
            max_queue_depth: l(&self.c.max_queue_depth),
        }
    }

    /// HTTP requests still allowed by the budget (None = unlimited).
    pub fn budget_remaining(&self) -> Option<u64> {
        self.cfg.request_budget.map(|b| b.saturating_sub(self.sent_budget.load(Ordering::Relaxed)))
    }

    fn jitter(&self, d: Duration) -> Duration {
        // xorshift; ±25% jitter so concurrent callers do not retry in lockstep
        let mut x = self.jitter_state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.jitter_state.store(x, Ordering::Relaxed);
        let f = 0.75 + (x % 1000) as f64 / 2000.0;
        d.mul_f64(f)
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let d = self.cfg.base_backoff.saturating_mul(1u32 << attempt.min(16));
        self.jitter(d.min(self.cfg.max_backoff))
    }

    /// Wait (bounded by `deadline_us`) until the bucket grants a token. Errors without sending.
    async fn acquire_token(&self, deadline_us: i64) -> Result<(), RpcError> {
        let start = wall_us();
        let mut deferred = false;
        loop {
            let now = wall_us();
            let wait = {
                let mut b = self.bucket.lock().unwrap();
                if b.try_acquire(now).is_ok() {
                    0
                } else {
                    b.wait_us(now).max(1_000)
                }
            };
            if wait == 0 {
                if deferred {
                    self.c.total_wait_ms.fetch_add(((wall_us() - start) / 1000) as u64, Ordering::Relaxed);
                }
                return Ok(());
            }
            if !deferred {
                deferred = true;
                self.c.deferred.fetch_add(1, Ordering::Relaxed);
            }
            if now + wait > deadline_us {
                self.c.expired.fetch_add(1, Ordering::Relaxed);
                self.c.total_wait_ms.fetch_add(((now - start) / 1000) as u64, Ordering::Relaxed);
                return Err(RpcError::Expired(((now - start) / 1000).max(0) as u64));
            }
            tokio::time::sleep(Duration::from_micros(wait as u64)).await;
        }
    }

    fn take_budget(&self) -> Result<(), RpcError> {
        if let Some(b) = self.cfg.request_budget {
            let used = self.sent_budget.fetch_add(1, Ordering::Relaxed);
            if used >= b {
                self.sent_budget.fetch_sub(1, Ordering::Relaxed);
                self.c.budget_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(RpcError::BudgetExhausted);
            }
        } else {
            self.sent_budget.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn attempt(&self, i: usize, body: &Value) -> Attempt {
        let url = &self.urls[i];
        let c = &self.counters[i];
        c.requests.fetch_add(1, Ordering::Relaxed);
        self.c.http_sent.fetch_add(1, Ordering::Relaxed);
        let t0 = wall_us();
        let res = self.http.post(url).json(body).send().await;
        let out = match res {
            Ok(r) if r.status().as_u16() == 429 => {
                c.rate_limited.fetch_add(1, Ordering::Relaxed);
                self.c.http_429.fetch_add(1, Ordering::Relaxed);
                let ra = r.headers().get(reqwest::header::RETRY_AFTER).and_then(|h| h.to_str().ok()).and_then(parse_retry_after);
                Attempt::RateLimited { retry_after: ra }
            }
            Ok(r) if !r.status().is_success() => Attempt::Failed(format!("http {}", r.status().as_u16())),
            Ok(r) => match r.json::<Value>().await {
                Ok(v) => match v.get("error") {
                    Some(e) => Attempt::Rpc {
                        code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                        message: e.get("message").and_then(|m| m.as_str()).unwrap_or("").chars().take(200).collect(),
                    },
                    None => {
                        c.ok.fetch_add(1, Ordering::Relaxed);
                        c.last_latency_us.store((wall_us() - t0).max(1) as u64, Ordering::Relaxed);
                        Attempt::Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    }
                },
                Err(e) => Attempt::Failed(format!("decode: {}", e.without_url())),
            },
            Err(e) => Attempt::Failed(format!("transport: {}", e.without_url())),
        };
        let err = match &out {
            Attempt::Ok(_) => None,
            Attempt::RateLimited { .. } => Some("http 429".to_string()),
            Attempt::Rpc { code, message } => Some(format!("rpc error {code}: {message}")),
            Attempt::Failed(e) => Some(e.clone()),
        };
        if let Some(e) = err {
            c.errors.fetch_add(1, Ordering::Relaxed);
            self.last_error.lock().unwrap()[i] = Some(e);
        }
        out
    }

    /// One logical call: rate limit, concurrency, failover, 429 cooldown + retries, budget.
    async fn execute(&self, body: &Value) -> Result<Value, RpcError> {
        let deadline = wall_us() + self.cfg.max_wait.as_micros() as i64;
        let depth = self.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        if depth > self.cfg.max_queue {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            self.c.queue_rejected.fetch_add(1, Ordering::Relaxed);
            return Err(RpcError::QueueFull);
        }
        self.c.max_queue_depth.fetch_max(depth as u64, Ordering::Relaxed);
        let permit = match tokio::time::timeout(Duration::from_micros((deadline - wall_us()).max(0) as u64), self.in_flight.acquire()).await {
            Ok(Ok(p)) => p,
            _ => {
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                self.c.expired.fetch_add(1, Ordering::Relaxed);
                return Err(RpcError::Expired(self.cfg.max_wait.as_millis() as u64));
            }
        };
        let r = self.execute_permitted(body, deadline).await;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        drop(permit);
        r
    }

    async fn execute_permitted(&self, body: &Value, deadline: i64) -> Result<Value, RpcError> {
        let mut rate_limited_retries = 0u32;
        let mut transport_retries = 0u32;
        loop {
            self.acquire_token(deadline).await?;
            self.take_budget()?;
            let mut errs = Vec::new();
            let mut saw_429: Option<Option<Duration>> = None;
            for i in 0..self.urls.len() {
                match self.attempt(i, body).await {
                    Attempt::Ok(v) => {
                        self.c.ok.fetch_add(1, Ordering::Relaxed);
                        return Ok(v);
                    }
                    Attempt::Rpc { code, message } => {
                        // a JSON-RPC error is an answer, not an outage: no failover / retry
                        self.c.rpc_errors.fetch_add(1, Ordering::Relaxed);
                        return Err(RpcError::Rpc { code, message });
                    }
                    Attempt::RateLimited { retry_after } => {
                        saw_429 = Some(retry_after);
                        errs.push(format!("{}: http 429", redact_url(&self.urls[i])));
                    }
                    Attempt::Failed(e) => {
                        self.c.transport_errors.fetch_add(1, Ordering::Relaxed);
                        errs.push(format!("{}: {e}", redact_url(&self.urls[i])));
                    }
                }
                // failover consumes budget too
                if i + 1 < self.urls.len() {
                    self.take_budget()?;
                }
            }
            if let Some(retry_after) = saw_429 {
                if rate_limited_retries >= self.cfg.max_retries {
                    return Err(RpcError::RateLimited(rate_limited_retries));
                }
                let wait = retry_after.map(|d| d.max(self.cfg.base_backoff)).unwrap_or_else(|| self.backoff(rate_limited_retries));
                rate_limited_retries += 1;
                self.c.cooldowns.fetch_add(1, Ordering::Relaxed);
                self.bucket.lock().unwrap().cooldown(wall_us(), wait.as_millis() as u64);
            } else {
                if transport_retries >= self.cfg.max_retries.min(2) {
                    return Err(RpcError::AllFailed(errs.join("; ")));
                }
                let wait = self.backoff(transport_retries);
                transport_retries += 1;
                if wall_us() + wait.as_micros() as i64 > deadline {
                    return Err(RpcError::AllFailed(errs.join("; ")));
                }
                tokio::time::sleep(wait).await;
            }
            self.c.retries.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// JSON-RPC call through the shared scheduler. Identical concurrent calls share one request.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if !ALLOWED_METHODS.contains(&method) {
            return Err(RpcError::NotAllowed(method.into()));
        }
        self.c.calls.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let key = body.to_string();
        let leader = {
            let mut m = self.inflight.lock().unwrap();
            match m.get(&key) {
                Some(rx) => Err(rx.clone()),
                None => {
                    let (tx, rx) = watch::channel(None);
                    m.insert(key.clone(), rx);
                    Ok(tx)
                }
            }
        };
        match leader {
            Ok(tx) => {
                // the guard also clears the entry if this future is cancelled mid-request
                let _guard = InflightGuard { map: &self.inflight, key: key.clone() };
                let r = self.execute(&body).await;
                let _ = tx.send(Some(r.clone()));
                r
            }
            Err(mut rx) => {
                self.c.deduplicated.fetch_add(1, Ordering::Relaxed);
                loop {
                    if let Some(r) = rx.borrow().clone() {
                        return r;
                    }
                    if rx.changed().await.is_err() {
                        return rx.borrow().clone().unwrap_or(Err(RpcError::AllFailed("shared request dropped".into())));
                    }
                }
            }
        }
    }

    fn commitment_str(&self) -> &'static str {
        match self.commitment {
            Commitment::Processed | Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }

    /// Full transaction (jsonParsed) as a provider-neutral RawEvent, or None if not found yet.
    pub async fn get_transaction(&self, signature: &str, provider: &str) -> Result<Option<RawEvent>, RpcError> {
        let v = self.get_transaction_raw(signature).await?;
        if v.is_null() {
            return Ok(None);
        }
        let slot = v.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
        Ok(parse_tx_object(&v, Some(signature), slot, provider, self.commitment, wall_us()))
    }

    /// Raw getTransaction JSON (complete, for capturing regression fixtures verbatim).
    pub async fn get_transaction_raw(&self, signature: &str) -> Result<Value, RpcError> {
        self.call("getTransaction", get_transaction_params(signature, self.commitment_str(), self.max_tx_version)).await
    }

    pub async fn get_signatures_for_address(&self, address: &str, limit: usize, before: Option<&str>) -> Result<Vec<SigInfo>, RpcError> {
        let mut opts = json!({"limit": limit.clamp(1, 1000), "commitment": self.commitment_str()});
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
            .ok_or_else(|| RpcError::Rpc { code: 0, message: "account data missing".into() })?;
        Ok(Some((data, owner)))
    }

    pub async fn get_slot(&self) -> Result<u64, RpcError> {
        self.call("getSlot", json!([{"commitment": "processed"}])).await?.as_u64().ok_or_else(|| RpcError::Rpc { code: 0, message: "bad slot".into() })
    }
}
