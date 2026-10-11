//! Jupiter Swap API quote client (read-only GET /quote). The base URL is configurable so the
//! current Swap API version/host can be selected without code changes. This module never calls
//! transaction-building endpoints.

use super::ratelimit::{RateLimitReason, TokenBucket};
use super::{Quote, QuoteError, QuoteSource, Side};
use crate::time::wall_us;
use crate::types::{Pubkey, WSOL_MINT};
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct JupiterConfig {
    /// Full URL of the quote endpoint, e.g. https://lite-api.jup.ag/swap/v1/quote
    pub quote_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub rate_per_s: f64,
    pub burst: u32,
    pub slippage_bps: u32,
}

pub struct JupiterClient {
    cfg: JupiterConfig,
    http: reqwest::Client,
    bucket: Mutex<TokenBucket>,
}

impl JupiterClient {
    pub fn new(cfg: JupiterConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(cfg.timeout).build()?;
        let bucket = Mutex::new(TokenBucket::new(cfg.rate_per_s, cfg.burst, wall_us()));
        Ok(Self { cfg, http, bucket })
    }

    pub async fn quote(&self, mint: Pubkey, side: Side, amount: u64) -> Result<Quote, QuoteError> {
        if let Err(r) = self.bucket.lock().unwrap().try_acquire(wall_us()) {
            return Err(QuoteError::RateLimited(match r {
                RateLimitReason::LocalBudget => "local request budget".into(),
                RateLimitReason::RemoteCooldown { .. } => "remote cooldown after 429".into(),
            }));
        }
        let (input, output) = match side {
            Side::Buy => (WSOL_MINT.to_string(), mint.to_string()),
            Side::Sell => (mint.to_string(), WSOL_MINT.to_string()),
        };
        let t0 = wall_us();
        let mut req = self.http.get(&self.cfg.quote_url).query(&[
            ("inputMint", input.as_str()),
            ("outputMint", output.as_str()),
            ("amount", &amount.to_string()),
            ("slippageBps", &self.cfg.slippage_bps.to_string()),
            ("restrictIntermediateTokens", "true"),
        ]);
        if let Some(k) = &self.cfg.api_key {
            req = req.header("x-api-key", k);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                QuoteError::Timeout
            } else {
                // strip URL (may contain key) from error text
                QuoteError::Http(e.without_url().to_string())
            }
        })?;
        let status = resp.status();
        if status.as_u16() == 429 {
            let retry_ms = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|s| s * 1000)
                .unwrap_or(2000);
            self.bucket.lock().unwrap().cooldown(wall_us(), retry_ms);
            return Err(QuoteError::RateLimited(format!("http 429, retry after {retry_ms} ms")));
        }
        let body = resp.text().await.map_err(|e| QuoteError::Http(e.without_url().to_string()))?;
        let latency = wall_us() - t0;
        if !status.is_success() {
            if body.contains("NO_ROUTES_FOUND") || body.contains("COULD_NOT_FIND_ANY_ROUTE") || body.contains("TOKEN_NOT_TRADABLE") {
                return Err(QuoteError::NoRoute(format!("jupiter: {}", truncate(&body, 160))));
            }
            return Err(QuoteError::Http(format!("{status}: {}", truncate(&body, 160))));
        }
        parse_quote(&body, mint, side, t0, latency)
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn parse_quote(body: &str, mint: Pubkey, side: Side, at_us: i64, latency_us: i64) -> Result<Quote, QuoteError> {
    let v: Value = serde_json::from_str(body).map_err(|e| QuoteError::Parse(e.to_string()))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(QuoteError::NoRoute(format!("jupiter: {err}")));
    }
    let num = |k: &str| -> Option<u64> {
        v.get(k).and_then(|x| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_u64()))
    };
    let in_amount = num("inAmount").ok_or_else(|| QuoteError::Parse("missing inAmount".into()))?;
    let out_amount = num("outAmount").ok_or_else(|| QuoteError::Parse("missing outAmount".into()))?;
    if out_amount == 0 {
        return Err(QuoteError::NoRoute("jupiter returned zero out".into()));
    }
    let impact = v
        .get("priceImpactPct")
        .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64()))
        .map(|p| p * 10_000.0)
        .unwrap_or(f64::NAN);
    let labels: Vec<String> = v
        .get("routePlan")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|h| h.get("swapInfo")?.get("label")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if labels.is_empty() {
        return Err(QuoteError::NoRoute("empty routePlan".into()));
    }
    let fee: u64 = v
        .get("routePlan")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|h| {
                    let si = h.get("swapInfo")?;
                    let fm = si.get("feeMint")?.as_str()?;
                    if fm == WSOL_MINT {
                        si.get("feeAmount")?.as_str()?.parse::<u64>().ok()
                    } else {
                        None
                    }
                })
                .sum()
        })
        .unwrap_or(0);
    Ok(Quote {
        source: QuoteSource::Jupiter,
        venue: None,
        side,
        mint,
        in_amount,
        out_amount,
        venue_fee_lamports: fee,
        price_impact_bps: impact,
        capped_by_reserves: false,
        state_slot: v.get("contextSlot").and_then(|s| s.as_u64()).unwrap_or(0),
        state_age_ms: 0.0,
        quoted_at_us: at_us,
        latency_us,
        route: labels.join(" > "),
    })
}
