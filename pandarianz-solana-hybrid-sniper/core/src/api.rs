//! Read-only HTTP API for the dashboard. There are no order, wallet or execution endpoints.

use crate::engine::Snapshot;
use axum::{extract::State, routing::get, Json, Router};
use std::sync::{Arc, RwLock};
use tower_http::cors::{Any, CorsLayer};

pub type Shared = Arc<RwLock<Snapshot>>;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/health", get(health))
        .route("/api/safety", get(safety))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods([axum::http::Method::GET]))
        .with_state(state)
}

async fn snapshot(State(s): State<Shared>) -> Json<Snapshot> {
    Json(s.read().map(|g| g.clone()).unwrap_or_default())
}

async fn health(State(s): State<Shared>) -> Json<serde_json::Value> {
    let g = s.read().map(|g| g.clone()).unwrap_or_default();
    Json(serde_json::json!({
        "paper_only": true,
        "data_mode": g.data_mode,
        "data_mode_note": g.data_mode_note,
        "profile_key": g.profile_key,
        "run_id": g.run_id,
        "strategy_version": g.strategy_version,
        "health": g.health,
        "ingest": g.ingest,
        "store_backend": g.store_backend,
        "store_errors": g.store_errors,
        "generated_us": g.generated_us,
    }))
}

async fn safety() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "paper_only": true,
        "key_material": "none — no wallet key loading, generation or storage exists in this codebase",
        "signing": "not implemented",
        "broadcasting": "not implemented (no transaction submission or bundle code exists)",
        "order_endpoints": "none — this API is GET-only and read-only",
        "verification": "scripts/check_no_live.sh and the `no_live_execution` test enforce this",
    }))
}

pub async fn serve(addr: &str, state: Shared) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "dashboard API listening (read-only)");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
