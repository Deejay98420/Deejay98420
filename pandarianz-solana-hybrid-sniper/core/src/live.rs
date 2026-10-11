//! Live-mode plumbing: full-transaction fetcher (for logs-only providers) and bounded startup
//! backfill so tokens launched before startup can be evaluated. Read-only RPC only.

use crate::rpc::RpcClient;
use crate::stream::ws::FetchReq;
use crate::stream::{Ingestor, RawEvent, StreamMsg};
use crate::time::wall_us;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};

#[derive(Default, Debug)]
pub struct FetchStats {
    pub requested: AtomicU64,
    pub fetched: AtomicU64,
    pub not_found: AtomicU64,
    pub errors: AtomicU64,
    pub dropped_queue_full: AtomicU64,
    pub total_fetch_us: AtomicU64,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct FetchSnapshot {
    pub requested: u64,
    pub fetched: u64,
    pub not_found: u64,
    pub errors: u64,
    pub mean_fetch_ms: Option<f64>,
}

impl FetchStats {
    pub fn snapshot(&self) -> FetchSnapshot {
        let f = self.fetched.load(Ordering::Relaxed);
        FetchSnapshot {
            requested: self.requested.load(Ordering::Relaxed),
            fetched: f,
            not_found: self.not_found.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            mean_fetch_ms: if f > 0 { Some(self.total_fetch_us.load(Ordering::Relaxed) as f64 / f as f64 / 1e3) } else { None },
        }
    }
}

/// Fetch full transactions for signatures seen in logs (bounded concurrency, retries while the
/// transaction is not yet queryable at the requested commitment).
pub fn spawn_fetcher(rpc: Arc<RpcClient>, ingestor: Ingestor, mut rx: mpsc::Receiver<FetchReq>, concurrency: usize, stats: Arc<FetchStats>) {
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            stats.requested.fetch_add(1, Ordering::Relaxed);
            let permit = sem.clone().acquire_owned().await.expect("semaphore");
            let (rpc, ing, st) = (rpc.clone(), ingestor.clone(), stats.clone());
            tokio::spawn(async move {
                let _p = permit;
                for attempt in 0..4u32 {
                    match rpc.get_transaction(&req.signature, &format!("{}+rpc", req.provider)).await {
                        Ok(Some(mut ev)) => {
                            let now = wall_us();
                            ev.recv_us = now;
                            ev.fetch_us = Some(now - req.first_seen_us);
                            st.fetched.fetch_add(1, Ordering::Relaxed);
                            st.total_fetch_us.fetch_add((now - req.first_seen_us).max(0) as u64, Ordering::Relaxed);
                            let _ = ing.push(StreamMsg::Event(ev)).await;
                            return;
                        }
                        Ok(None) => tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await,
                        Err(e) => {
                            tracing::debug!(error = %e, "getTransaction failed");
                            tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
                        }
                    }
                }
                st.not_found.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
}

#[derive(Clone, Debug)]
pub struct BackfillConfig {
    pub programs: Vec<String>,
    pub max_signatures_per_program: usize,
    pub max_age_s: i64,
    pub concurrency: usize,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct BackfillReport {
    pub signatures_listed: usize,
    pub transactions_loaded: usize,
    pub failed_or_missing: usize,
    pub oldest_block_time: Option<i64>,
    pub duration_ms: f64,
    pub errors: Vec<String>,
}

/// Load recent history before the live stream starts. Events are marked `backfill` (state only).
pub async fn backfill(rpc: Arc<RpcClient>, ingestor: &Ingestor, cfg: &BackfillConfig) -> BackfillReport {
    let t0 = wall_us();
    let mut rep = BackfillReport::default();
    let cutoff = t0 / 1_000_000 - cfg.max_age_s;
    let mut sigs = Vec::new();
    for prog in &cfg.programs {
        let mut before: Option<String> = None;
        let mut n = 0;
        while n < cfg.max_signatures_per_program {
            let page = match rpc.get_signatures_for_address(prog, (cfg.max_signatures_per_program - n).min(1000), before.as_deref()).await {
                Ok(p) => p,
                Err(e) => {
                    rep.errors.push(format!("{prog}: {e}"));
                    break;
                }
            };
            if page.is_empty() {
                break;
            }
            before = page.last().map(|s| s.signature.clone());
            let mut too_old = false;
            for s in page {
                n += 1;
                if s.block_time.is_some_and(|b| b < cutoff) {
                    too_old = true;
                    continue;
                }
                if !s.failed {
                    sigs.push(s);
                }
            }
            if too_old {
                break;
            }
        }
    }
    sigs.sort_by(|a, b| a.signature.cmp(&b.signature));
    sigs.dedup_by(|a, b| a.signature == b.signature);
    rep.signatures_listed = sigs.len();
    let sem = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
    let mut handles = Vec::new();
    for s in sigs {
        let permit = sem.clone().acquire_owned().await.expect("semaphore");
        let rpc = rpc.clone();
        handles.push(tokio::spawn(async move {
            let _p = permit;
            rpc.get_transaction(&s.signature, "backfill+rpc").await
        }));
    }
    let mut events: Vec<RawEvent> = Vec::new();
    for h in handles {
        match h.await {
            Ok(Ok(Some(mut ev))) => {
                ev.backfill = true;
                if let Some(bt) = ev.block_time {
                    ev.recv_us = bt * 1_000_000;
                }
                events.push(ev);
            }
            Ok(Ok(None)) => rep.failed_or_missing += 1,
            Ok(Err(e)) => {
                rep.failed_or_missing += 1;
                if rep.errors.len() < 10 {
                    rep.errors.push(e.to_string());
                }
            }
            Err(e) => rep.errors.push(e.to_string()),
        }
    }
    events.sort_by_key(|e| (e.slot, e.recv_us));
    rep.transactions_loaded = events.len();
    rep.oldest_block_time = events.iter().filter_map(|e| e.block_time).min();
    for ev in events {
        let _ = ingestor.push(StreamMsg::Event(ev)).await;
    }
    rep.duration_ms = (wall_us() - t0) as f64 / 1e3;
    rep
}

// ---------------------------------------------------------------------------------------------
// Live source orchestration (multi-provider failover + fetcher + backfill)

use crate::engine::Control;
use crate::stream::backoff::Backoff;
use crate::stream::ws::{self, SubscriptionKind, WsConfig};
use crate::types::{programs, Commitment};

#[derive(Clone, Debug)]
pub struct LiveSetup {
    /// one WsConfig per provider; all run concurrently (redundancy + failover, deduplicated)
    pub ws: Vec<WsConfig>,
    pub rpc_urls: Vec<String>,
    pub rpc_rate_per_s: f64,
    pub fetch_concurrency: usize,
    pub backfill: Option<BackfillConfig>,
}

fn env_list(k: &str) -> Vec<String> {
    std::env::var(k).unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}
fn env_num<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// wss://host/path?query → https://host/path?query (most providers serve both on the same URL).
pub fn ws_to_http(u: &str) -> Option<String> {
    let mut p = url::Url::parse(u).ok()?;
    let scheme = match p.scheme() {
        "wss" => "https",
        "ws" => "http",
        _ => return None,
    };
    p.set_scheme(scheme).ok()?;
    Some(p.to_string())
}

impl LiveSetup {
    pub fn new(ws_urls: Vec<String>, rpc_urls: Vec<String>, transactions: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(!ws_urls.is_empty(), "no WebSocket provider URL");
        let kind = if transactions {
            SubscriptionKind::Transactions { account_include: vec![programs::PUMP_FUN.into(), programs::PUMP_SWAP.into()] }
        } else {
            SubscriptionKind::Logs { mentions: vec![programs::PUMP_FUN.into(), programs::PUMP_SWAP.into(), programs::RAYDIUM_AMM_V4.into()] }
        };
        let ws = ws_urls
            .iter()
            .enumerate()
            .map(|(i, u)| WsConfig {
                provider: format!("ws{}:{}", i + 1, ws::redact_url(u).trim_start_matches("wss://").trim_start_matches("ws://")),
                url: u.clone(),
                kind: kind.clone(),
                commitment: Commitment::Confirmed,
                idle_timeout: Duration::from_secs(30),
                ping_interval: Duration::from_secs(10),
                backoff: Backoff::new(250, 15_000, None),
                tip: true,
                slot_updates: false,
            })
            .collect();
        let rpc_urls = if rpc_urls.is_empty() { ws_urls.iter().filter_map(|u| ws_to_http(u)).collect() } else { rpc_urls };
        Ok(Self {
            ws,
            rpc_urls,
            rpc_rate_per_s: 10.0,
            fetch_concurrency: 8,
            backfill: Some(BackfillConfig {
                programs: vec![programs::PUMP_FUN.into(), programs::PUMP_SWAP.into()],
                max_signatures_per_program: 300,
                max_age_s: 900,
                concurrency: 8,
            }),
        })
    }

    /// From environment variables (see .env.example). Errors explain exactly what is missing.
    pub fn from_env() -> anyhow::Result<Self> {
        let mut ws_urls = env_list("SOLANA_WS_URLS");
        if ws_urls.is_empty() {
            ws_urls = env_list("SOLANA_WS_URL");
        }
        anyhow::ensure!(!ws_urls.is_empty(), "LIVE mode blocked: set SOLANA_WS_URLS (comma-separated provider WebSocket URLs) in .env");
        let mut rpc = env_list("SOLANA_RPC_URLS");
        if rpc.is_empty() {
            rpc = env_list("SOLANA_RPC_URL");
        }
        let transactions = std::env::var("SOLANA_STREAM_KIND").map(|k| k == "transactions").unwrap_or(false);
        let mut s = Self::new(ws_urls, rpc, transactions)?;
        s.rpc_rate_per_s = env_num("SOLANA_RPC_RATE_PER_S", 10.0);
        s.fetch_concurrency = env_num("TX_FETCH_CONCURRENCY", 8);
        let slot_updates = std::env::var("SOLANA_SLOT_UPDATES").map(|v| v == "1").unwrap_or(false);
        for w in s.ws.iter_mut() {
            w.slot_updates = slot_updates;
        }
        let max_sigs: usize = env_num("BACKFILL_MAX_SIGNATURES", 300);
        if max_sigs == 0 {
            s.backfill = None;
        } else if let Some(b) = s.backfill.as_mut() {
            b.max_signatures_per_program = max_sigs;
            b.max_age_s = env_num("BACKFILL_MAX_AGE_S", 900);
        }
        Ok(s)
    }

    pub fn rpc(&self) -> anyhow::Result<Option<Arc<RpcClient>>> {
        if self.rpc_urls.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(RpcClient::new(self.rpc_urls.clone(), Duration::from_secs(8), self.rpc_rate_per_s, (self.rpc_rate_per_s as u32).max(2) * 2)?)))
    }
}

/// Start backfill (first), then every WebSocket provider. Returns immediately.
pub fn start_sources(setup: LiveSetup, rpc: Option<Arc<RpcClient>>, ingestor: Ingestor, ctl: mpsc::Sender<Control>, fetch_stats: Arc<FetchStats>) {
    tokio::spawn(async move {
        let logs_mode = setup.ws.iter().any(|w| matches!(w.kind, SubscriptionKind::Logs { .. }));
        let fetch_tx = match (&rpc, logs_mode) {
            (Some(r), true) => {
                let (tx, rx) = mpsc::channel(4096);
                spawn_fetcher(r.clone(), ingestor.clone(), rx, setup.fetch_concurrency, fetch_stats);
                Some(tx)
            }
            _ => None,
        };
        if let (Some(r), Some(b)) = (&rpc, &setup.backfill) {
            let rep = backfill(r.clone(), &ingestor, b).await;
            tracing::info!(loaded = rep.transactions_loaded, ms = rep.duration_ms, errors = rep.errors.len(), "backfill done");
            let _ = ctl.send(Control::BackfillDone(rep)).await;
        }
        for w in setup.ws {
            tokio::spawn(ws::run_with_fetch(w, ingestor.clone(), fetch_tx.clone()));
        }
    });
}
