//! `sniper` CLI — PAPER ONLY. There is no command, flag or code path that signs or sends a transaction.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use sniper_core::config::EngineConfig;
use sniper_core::engine::{Control, Engine, EngineOptions, Snapshot};
use sniper_core::quote::jupiter::{JupiterClient, JupiterConfig};
use sniper_core::store::{self, pg::PgStore, MemData, Recovered};
use sniper_core::stream::{backoff::Backoff, fixture, ws, Ingestor};
use sniper_core::time::wall_us;
use sniper_core::types::{programs, sol, Commitment, DataMode};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "sniper", about = "PAPER-ONLY Solana hybrid launch sniper / momentum trader")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Mode {
    Fixture,
    Live,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum StoreKind {
    Postgres,
    Memory,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the engine (fixture replay or live provider stream) with the dashboard API.
    Run {
        #[arg(long, value_enum, default_value = "fixture")]
        mode: Mode,
        /// Fixture JSONL (fixture mode). Generated in memory if omitted and the default file is missing.
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Book/journal namespace. Reuse a profile to resume (restart recovery).
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, value_enum, default_value = "postgres")]
        store: StoreKind,
        #[arg(long, env = "SNIPER_API_ADDR", default_value = "127.0.0.1:8787")]
        api: String,
        /// Keep running (serving the API) after a fixture source ends.
        #[arg(long)]
        hold: bool,
        #[arg(long, env = "SNIPER_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Write deterministic synthetic fixtures.
    GenFixtures {
        #[arg(long, default_value = "../fixtures")]
        out: PathBuf,
    },
    /// Re-run a recorded journal (PostgreSQL) through the current strategy as REPLAY data.
    Replay {
        #[arg(long)]
        from_profile: String,
        #[arg(long, env = "SNIPER_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Print a performance report for a profile from PostgreSQL.
    Report {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        json: bool,
    },
    /// Apply database migrations.
    Migrate,
    /// Query a running engine's health endpoint.
    Status {
        #[arg(long, env = "SNIPER_API_ADDR", default_value = "127.0.0.1:8787")]
        api: String,
    },
}

fn db_url() -> anyhow::Result<String> {
    std::env::var("DATABASE_URL").context("DATABASE_URL is not set (see .env.example)")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::GenFixtures { out } => {
            std::fs::create_dir_all(&out)?;
            let p = out.join("hybrid_session.jsonl");
            let msgs = sniper_core::fixtures::hybrid_session();
            std::fs::write(&p, sniper_core::fixtures::to_jsonl(&msgs))?;
            println!("wrote {} synthetic messages to {}", msgs.len(), p.display());
            Ok(())
        }
        Cmd::Migrate => {
            let pg = PgStore::connect(&db_url()?, "migrate").await?;
            pg.migrate().await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::Status { api } => {
            let url = format!("http://{api}/api/health");
            match reqwest::get(&url).await {
                Ok(r) => {
                    let v: serde_json::Value = r.json().await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                Err(_) => println!("engine API not reachable at {api} (not running?)"),
            }
            Ok(())
        }
        Cmd::Report { profile, json } => report(&profile, json).await,
        Cmd::Replay { from_profile, config } => {
            let cfg = EngineConfig::load(config.as_deref())?;
            let src = PgStore::connect(&db_url()?, &from_profile).await?;
            let events = src.load_journal().await?;
            if events.is_empty() {
                bail!("no journaled events for profile {from_profile}");
            }
            let profile_key = format!("REPLAY:{}:{}", from_profile, wall_us() / 1_000_000);
            let msgs: Vec<_> = events
                .into_iter()
                .map(|mut e| {
                    // the original mode is preserved in the source journal; replay runs are labelled REPLAY
                    e.data_mode = DataMode::Replay;
                    e.into_stream_msg()
                })
                .collect();
            let snap = run_engine(cfg, DataMode::Replay, profile_key, StoreKind::Postgres, None, false, msgs, None).await?;
            print_summary(&snap);
            Ok(())
        }
        Cmd::Run { mode, fixture, profile, store, api, hold, config } => {
            let cfg = EngineConfig::load(config.as_deref())?;
            match mode {
                Mode::Fixture => {
                    let default = PathBuf::from("../fixtures/hybrid_session.jsonl");
                    let msgs = match fixture {
                        Some(p) => fixture::load_jsonl(&p, DataMode::Fixture)?,
                        None if default.exists() => fixture::load_jsonl(&default, DataMode::Fixture)?,
                        None => sniper_core::fixtures::hybrid_session(),
                    };
                    let profile = profile.unwrap_or_else(|| format!("run{}", wall_us() / 1_000_000));
                    let key = format!("FIXTURE:{profile}");
                    let snap = run_engine(cfg, DataMode::Fixture, key, store, Some(api), hold, msgs, None).await?;
                    print_summary(&snap);
                    Ok(())
                }
                Mode::Live => {
                    let url = std::env::var("SOLANA_WS_URL").ok().filter(|s| !s.trim().is_empty());
                    let Some(url) = url else {
                        bail!("LIVE mode blocked: SOLANA_WS_URL is not set. Provide a provider WebSocket URL in .env, or use --mode fixture.");
                    };
                    let kind = match std::env::var("SOLANA_STREAM_KIND").unwrap_or_default().as_str() {
                        "transactions" => ws::SubscriptionKind::Transactions {
                            account_include: vec![programs::PUMP_FUN.into(), programs::PUMP_SWAP.into()],
                        },
                        _ => ws::SubscriptionKind::Logs { mentions: vec![programs::PUMP_FUN.into(), programs::PUMP_SWAP.into(), programs::RAYDIUM_AMM_V4.into()] },
                    };
                    let wscfg = ws::WsConfig {
                        provider: std::env::var("SOLANA_PROVIDER_NAME").unwrap_or_else(|_| "rpc-ws".into()),
                        url,
                        kind,
                        commitment: Commitment::Confirmed,
                        idle_timeout: Duration::from_secs(45),
                        ping_interval: Duration::from_secs(15),
                        backoff: Backoff::new(250, 15_000, None),
                    };
                    let profile = profile.unwrap_or_else(|| "main".into());
                    let key = format!("LIVE:{profile}");
                    let snap = run_engine(cfg, DataMode::Live, key, store, Some(api), true, vec![], Some(wscfg)).await?;
                    print_summary(&snap);
                    Ok(())
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_engine(
    cfg: EngineConfig,
    mode: DataMode,
    profile_key: String,
    store_kind: StoreKind,
    api: Option<String>,
    hold: bool,
    fixture_msgs: Vec<sniper_core::stream::StreamMsg>,
    ws_cfg: Option<ws::WsConfig>,
) -> anyhow::Result<Snapshot> {
    let run_id = format!("r{}", wall_us());
    let (store, rec, warm) = match store_kind {
        StoreKind::Postgres => {
            let pg = PgStore::connect(&db_url()?, &profile_key).await?;
            pg.migrate().await?;
            let rec = pg.recover().await?;
            let since = if mode == DataMode::Live { wall_us() - 30 * 60 * 1_000_000 } else { i64::MIN };
            let warm = if rec.books.is_empty() { vec![] } else { pg.load_events_since(since).await? };
            (store::spawn_pg(pg, 16_384), rec, warm)
        }
        StoreKind::Memory => {
            tracing::warn!("memory store: nothing is persisted (tests / quick look only)");
            (store::spawn_mem(Arc::new(Mutex::new(MemData::default())), profile_key.clone(), 16_384), Recovered::default(), vec![])
        }
    };
    let (ingestor, rx) = Ingestor::new(cfg.queue_capacity, cfg.dedup_window);
    ingestor.seed(rec.signatures.clone());
    let jupiter = match (mode, std::env::var("JUPITER_QUOTE_URL").ok().filter(|s| !s.is_empty())) {
        (DataMode::Live, Some(u)) => Some(Arc::new(JupiterClient::new(JupiterConfig {
            quote_url: u,
            api_key: std::env::var("JUPITER_API_KEY").ok().filter(|s| !s.is_empty()),
            timeout: Duration::from_millis(1500),
            rate_per_s: std::env::var("JUPITER_RATE_PER_S").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0),
            burst: 2,
            slippage_bps: 1000,
        })?)),
        _ => None,
    };
    let opts = EngineOptions { data_mode: mode, profile_key: profile_key.clone(), run_id, jupiter, ingest_stats: Some(ingestor.clone()) };
    let cfg_json = serde_json::to_value(&cfg)?;
    let mut engine = Engine::new(cfg, opts, store, Some(rec));
    engine.warmup(&warm);
    engine.boot(cfg_json).await;
    engine.exit_on_end = !hold;
    engine.publish(true);
    let dash = engine.dash.clone();
    if let Some(addr) = api {
        let d = dash.clone();
        tokio::spawn(async move {
            if let Err(e) = sniper_core::api::serve(&addr, d).await {
                tracing::error!(error = %e, "API server failed");
            }
        });
    }
    let (ctl_tx, ctl_rx) = mpsc::channel(1024);
    engine.set_control_sender(ctl_tx.clone());
    tracing::info!(profile = %profile_key, mode = mode.as_str(), version = %engine.version, "engine starting (PAPER ONLY)");
    match ws_cfg {
        Some(w) => {
            let ing = ingestor.clone();
            tokio::spawn(ws::run(w, ing));
        }
        None => {
            let ing = ingestor.clone();
            tokio::spawn(async move {
                if let Err(e) = fixture::replay(&ing, fixture_msgs, "fixture").await {
                    tracing::error!(error = %e, "fixture replay failed");
                }
            });
        }
    }
    let stop = ctl_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown requested; flushing store");
        let _ = stop.send(Control::Shutdown).await;
    });
    let engine = engine.run(rx, ctl_rx).await;
    let snap = engine.snapshot();
    Ok(snap)
}

fn print_summary(s: &Snapshot) {
    println!("\n=== PAPER RUN SUMMARY ({}) ===", s.data_mode.map(|m| m.as_str()).unwrap_or("?"));
    println!("{}", s.data_mode_note);
    println!("profile {}  strategy {}", s.profile_key, s.strategy_version);
    println!(
        "events {}  decoded {}  dup-dropped {}  failed-tx skipped {}  rollbacks {}  evaluations {}  entry decisions {}",
        s.health.events_processed, s.health.decoded_events, s.health.engine_duplicates_dropped, s.health.failed_tx_skipped, s.health.rollbacks, s.evaluations, s.entries_decided
    );
    for b in &s.books {
        let p = &b.perf;
        println!(
            "[{:<11}] equity {:.4} SOL (pnl {:+.4})  realized {:+.4}  trades {}  win {}  PF {}  expectancy {}  maxDD {:.4}  open {}  failed-entry-fees {:.5}  accounting {}",
            b.scenario,
            b.equity_sol,
            b.equity_pnl_sol,
            p.realized_pnl_sol,
            p.closed_trades,
            p.win_rate.map(|w| format!("{:.0}%", w * 100.0)).unwrap_or("-".into()),
            p.profit_factor.map(|w| format!("{w:.2}")).unwrap_or("-".into()),
            p.expectancy_sol.map(|w| format!("{w:+.5}")).unwrap_or("-".into()),
            p.max_drawdown_sol,
            b.open_positions.len(),
            b.failed_entry_costs_sol,
            if b.accounting_ok { "OK".to_string() } else { format!("ERROR {:?}", b.accounting_error) }
        );
    }
    let mut rc: Vec<_> = s.rejection_counts.iter().collect();
    rc.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
    println!("top rejection reasons: {}", rc.iter().take(8).map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "));
    for (k, r) in &s.routes {
        println!("route {k}: attempts {} ok {} no_route {} disabled {} errors {} rate_limited {}", r.attempts, r.ok, r.no_route, r.disabled, r.errors, r.rate_limited);
    }
}

async fn report(profile: &str, json: bool) -> anyhow::Result<()> {
    let pg = PgStore::connect(&db_url()?, profile).await?;
    let rec = pg.recover().await?;
    if rec.books.is_empty() {
        bail!("no books for profile key {profile} (use the full key, e.g. FIXTURE:myprofile or LIVE:main)");
    }
    let rows = pg
        .client
        .query(
            "select decision, path, count(*) from evaluations where profile_key=$1 group by 1,2 order by 1,2",
            &[&profile],
        )
        .await?;
    let reasons = pg
        .client
        .query(
            "select r->>'code' as code, count(*) from evaluations, jsonb_array_elements(reasons) r where profile_key=$1 group by 1 order by 2 desc limit 15",
            &[&profile],
        )
        .await?;
    let orders = pg
        .client
        .query("select kind, status, count(*) from orders where profile_key=$1 group by 1,2 order by 1,2", &[&profile])
        .await?;
    let mut out = serde_json::json!({"profile_key": profile, "paper_only": true, "books": []});
    for (id, (start, sum, _)) in &rec.books {
        let closed: Vec<_> = rec.positions.iter().filter(|p| &p.book_id == id && p.realized_pnl.is_some()).cloned().collect();
        let open: Vec<_> = rec.positions.iter().filter(|p| &p.book_id == id && p.is_live()).cloned().collect();
        let perf = sniper_core::metrics::perf(&closed);
        out["books"].as_array_mut().unwrap().push(serde_json::json!({
            "book_id": id, "start_sol": sol(*start as i64), "balance_sol": sol(*start as i64 + sum),
            "open_or_unresolved": open.len(), "perf": perf,
        }));
    }
    out["evaluations"] = rows.iter().map(|r| serde_json::json!({"decision": r.get::<_, String>(0), "path": r.get::<_, String>(1), "n": r.get::<_, i64>(2)})).collect();
    out["top_rejections"] = reasons.iter().map(|r| serde_json::json!({"code": r.get::<_, Option<String>>(0), "n": r.get::<_, i64>(1)})).collect();
    out["orders"] = orders.iter().map(|r| serde_json::json!({"kind": r.get::<_, String>(0), "status": r.get::<_, String>(1), "n": r.get::<_, i64>(2)})).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("PAPER report — profile {profile}");
        if profile.starts_with("FIXTURE") {
            println!("NOTE: FIXTURE profile — synthetic data, results say nothing about real profitability.");
        }
        for b in out["books"].as_array().unwrap() {
            let p = &b["perf"];
            println!(
                "  {}: balance {:.4} SOL (start {:.4}), closed {}, win_rate {}, PF {}, expectancy {}, realized {:+.4}, maxDD {:.4}, open/unresolved {}",
                b["book_id"].as_str().unwrap_or(""),
                b["balance_sol"].as_f64().unwrap_or(0.0),
                b["start_sol"].as_f64().unwrap_or(0.0),
                p["closed_trades"],
                p["win_rate"],
                p["profit_factor"],
                p["expectancy_sol"],
                p["realized_pnl_sol"].as_f64().unwrap_or(0.0),
                p["max_drawdown_sol"].as_f64().unwrap_or(0.0),
                b["open_or_unresolved"]
            );
        }
        println!("  evaluations: {}", out["evaluations"]);
        println!("  top rejections: {}", out["top_rejections"]);
        println!("  orders: {}", out["orders"]);
    }
    Ok(())
}
