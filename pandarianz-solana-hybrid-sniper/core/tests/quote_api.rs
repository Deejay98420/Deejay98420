//! Jupiter quote client against a local mock server, and the read-only dashboard API.

use axum::routing::get;
use axum::Router;
use sniper_core::quote::jupiter::{JupiterClient, JupiterConfig};
use sniper_core::quote::{QuoteError, QuoteSource, Side};
use sniper_core::types::Pubkey;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

async fn mock() -> (String, Arc<AtomicU32>) {
    let hits = Arc::new(AtomicU32::new(0));
    let h = hits.clone();
    let app = Router::new()
        .route(
            "/ok/quote",
            get(|| async {
                axum::Json(serde_json::json!({
                    "inputMint":"So11111111111111111111111111111111111111112","inAmount":"50000000",
                    "outputMint":"x","outAmount":"1234567","otherAmountThreshold":"1100000",
                    "priceImpactPct":"0.0123","contextSlot": 999,
                    "routePlan":[{"swapInfo":{"ammKey":"a","label":"Pump.fun Amm","feeAmount":"125000","feeMint":"So11111111111111111111111111111111111111112"},"percent":100}]
                }))
            }),
        )
        .route(
            "/limited/quote",
            get(move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    (axum::http::StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], "slow down")
                }
            }),
        )
        .route("/noroute/quote", get(|| async { (axum::http::StatusCode::BAD_REQUEST, r#"{"error":"Could not find any route","errorCode":"COULD_NOT_FIND_ANY_ROUTE"}"#) }))
        .route(
            "/slow/quote",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                "{}"
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

fn client(url: String, rate: f64, burst: u32) -> JupiterClient {
    JupiterClient::new(JupiterConfig { quote_url: url, api_key: None, timeout: Duration::from_millis(200), rate_per_s: rate, burst, slippage_bps: 500 }).unwrap()
}

#[tokio::test]
async fn jupiter_quote_success_is_recorded_with_route_and_latency() {
    let (base, _) = mock().await;
    let c = client(format!("{base}/ok/quote"), 10.0, 5);
    let q = c.quote(Pubkey::synthetic("m"), Side::Buy, 50_000_000).await.unwrap();
    assert_eq!(q.source, QuoteSource::Jupiter);
    assert_eq!((q.in_amount, q.out_amount), (50_000_000, 1_234_567));
    assert_eq!(q.route, "Pump.fun Amm");
    assert_eq!(q.venue_fee_lamports, 125_000);
    assert!((q.price_impact_bps - 123.0).abs() < 1e-6);
    assert_eq!(q.state_slot, 999);
    assert!(q.latency_us > 0);
}

#[tokio::test]
async fn jupiter_429_triggers_cooldown_without_hammering() {
    let (base, hits) = mock().await;
    let c = client(format!("{base}/limited/quote"), 100.0, 10);
    let e = c.quote(Pubkey::synthetic("m"), Side::Sell, 1).await.unwrap_err();
    assert!(matches!(e, QuoteError::RateLimited(ref s) if s.contains("429")), "{e:?}");
    // during the Retry-After cooldown no request is sent
    for _ in 0..5 {
        assert!(matches!(c.quote(Pubkey::synthetic("m"), Side::Sell, 1).await, Err(QuoteError::RateLimited(_))));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn jupiter_no_route_timeout_and_local_budget() {
    let (base, _) = mock().await;
    let c = client(format!("{base}/noroute/quote"), 10.0, 5);
    assert!(matches!(c.quote(Pubkey::synthetic("m"), Side::Sell, 1).await, Err(QuoteError::NoRoute(_))));
    let c = client(format!("{base}/slow/quote"), 10.0, 5);
    assert_eq!(c.quote(Pubkey::synthetic("m"), Side::Sell, 1).await.unwrap_err(), QuoteError::Timeout);
    let c = client(format!("{base}/ok/quote"), 0.001, 1);
    assert!(c.quote(Pubkey::synthetic("m"), Side::Buy, 1).await.is_ok());
    assert!(matches!(c.quote(Pubkey::synthetic("m"), Side::Buy, 1).await, Err(QuoteError::RateLimited(ref s)) if s.contains("local")));
}

#[test]
fn jupiter_parse_rejects_empty_routes() {
    let body = r#"{"inAmount":"1","outAmount":"2","routePlan":[]}"#;
    assert!(matches!(sniper_core::quote::jupiter::parse_quote(body, Pubkey::synthetic("m"), Side::Buy, 0, 0), Err(QuoteError::NoRoute(_))));
}

#[tokio::test]
async fn dashboard_api_is_get_only() {
    let state = Arc::new(std::sync::RwLock::new(sniper_core::engine::Snapshot { paper_only: true, ..Default::default() }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, sniper_core::api::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let r = http.get(format!("http://{addr}/api/snapshot")).send().await.unwrap();
    assert!(r.status().is_success());
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["paper_only"], true);
    let s: serde_json::Value = http.get(format!("http://{addr}/api/safety")).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["signing"], "not implemented");
    for path in ["/api/snapshot", "/api/order", "/api/swap", "/api/execute"] {
        let r = http.post(format!("http://{addr}{path}")).body("{}").send().await.unwrap();
        assert!(r.status() == 405 || r.status() == 404, "{path} -> {}", r.status());
    }
}
