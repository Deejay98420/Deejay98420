//! `sniper` CLI — PAPER ONLY. There is no command, flag or code path that signs or sends a transaction.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use sniper_core::config::EngineConfig;
use sniper_core::engine::{Control, Engine, EngineOptions, Snapshot};
use sniper_core::quote::jupiter::{JupiterClient, JupiterConfig};
use sniper_core::store::{self, pg::PgStore, MemData, Recovered};
use sniper_core::stream::{fixture, Ingestor};
use sniper_core::time::wall_us;
use sniper_core::types::{programs, sol, DataMode};
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
        #[arg(long, default_value = "../fixtures/synthetic")]
        out: PathBuf,
    },
    /// Connect to the configured live providers for N seconds (memory store, no trading decisions are
    /// persisted) and write a validation report: connectivity, decode coverage, latency, feed health.
    VerifyLive {
        #[arg(long, default_value_t = 120)]
        seconds: u64,
        #[arg(long, default_value = "../reports")]
        out: PathBuf,
    },
    /// Capture real mainnet transactions (read-only getTransaction) as regression fixtures.
    Capture {
        /// comma-separated signatures to capture
        #[arg(long, value_delimiter = ',')]
        signatures: Vec<String>,
        /// or: sample N recent signatures per supported program
        #[arg(long)]
        from_live: Option<usize>,
        #[arg(long, default_value = "../fixtures/real")]
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
            let p = out.join("hybrid_session.jsonl"); // synthetic fixtures live in fixtures/synthetic
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
            let snap = run_engine(cfg, DataMode::Replay, profile_key, StoreKind::Postgres, None, false, msgs, None, None).await?;
            print_summary(&snap);
            Ok(())
        }
        Cmd::Run { mode, fixture, profile, store, api, hold, config } => {
            let cfg = EngineConfig::load(config.as_deref())?;
            match mode {
                Mode::Fixture => {
                    let default = PathBuf::from("../fixtures/synthetic/hybrid_session.jsonl");
                    let msgs = match fixture {
                        Some(p) => fixture::load_jsonl(&p, DataMode::Fixture)?,
                        None if default.exists() => fixture::load_jsonl(&default, DataMode::Fixture)?,
                        None => sniper_core::fixtures::hybrid_session(),
                    };
                    let profile = profile.unwrap_or_else(|| format!("run{}", wall_us() / 1_000_000));
                    let key = format!("FIXTURE:{profile}");
                    let snap = run_engine(cfg, DataMode::Fixture, key, store, Some(api), hold, msgs, None, None).await?;
                    print_summary(&snap);
                    Ok(())
                }
                Mode::Live => {
                    let setup = sniper_core::live::LiveSetup::from_env()?;
                    let profile = profile.unwrap_or_else(|| "main".into());
                    let key = format!("LIVE:{profile}");
                    let snap = run_engine(cfg, DataMode::Live, key, store, Some(api), true, vec![], Some(setup), None).await?;
                    print_summary(&snap);
                    Ok(())
                }
            }
        }
        Cmd::VerifyLive { seconds, out } => {
            let setup = sniper_core::live::LiveSetup::from_env()?;
            let cfg = EngineConfig::default();
            let key = format!("LIVE:verify{}", wall_us() / 1_000_000);
            let snap = run_engine(cfg, DataMode::Live, key, StoreKind::Memory, None, true, vec![], Some(setup), Some(Duration::from_secs(seconds))).await?;
            let report = live_report(&snap, seconds);
            std::fs::create_dir_all(&out)?;
            let path = out.join(format!("live-validation-{}.json", wall_us() / 1_000_000));
            std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
            println!("{}", serde_json::to_string_pretty(&report["verdict"])?);
            println!("full report: {}", path.display());
            Ok(())
        }
        Cmd::Capture { signatures, from_live, out } => capture(signatures, from_live, out).await,
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
    live: Option<sniper_core::live::LiveSetup>,
    stop_after: Option<Duration>,
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
    let rpc = match &live {
        Some(l) => l.rpc()?,
        None => None,
    };
    let fetch_stats = Arc::new(sniper_core::live::FetchStats::default());
    engine.live = sniper_core::engine::LiveServices { rpc: rpc.clone(), fetch_stats: Some(fetch_stats.clone()), backfill: None };
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
    match live {
        Some(setup) => {
            tracing::info!(providers = setup.ws.len(), rpc_endpoints = setup.rpc_urls.len(), "starting live sources (read-only)");
            sniper_core::live::start_sources(setup, rpc, ingestor.clone(), ctl_tx.clone(), fetch_stats);
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
    if let Some(d) = stop_after {
        let stop = ctl_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            let _ = stop.send(Control::Shutdown).await;
        });
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
    let fwd = pg
        .client
        .query(
            "select path, decision, horizon_s, count(*), count(net_return), avg(net_return), avg((net_return > 0)::int::float8)
             from forward_outcomes where profile_key=$1 group by 1,2,3 order by 1,2,3",
            &[&profile],
        )
        .await?;
    let exec = pg
        .client
        .query(
            "select count(*), count(adverse_bps), percentile_cont(0.5) within group (order by adverse_bps), avg(adverse_bps)
             from execution_checks where profile_key=$1",
            &[&profile],
        )
        .await?;
    let gaps = pg
        .client
        .query("select count(*), coalesce(sum(coalesce(end_us, start_us) - start_us),0)::bigint from observation_gaps where profile_key=$1", &[&profile])
        .await?;
    let mut out = serde_json::json!({"profile_key": profile, "paper_only": true, "books": []});
    out["forward_evaluation"] = fwd
        .iter()
        .map(|r| {
            serde_json::json!({"path": r.get::<_, String>(0), "decision": r.get::<_, String>(1), "horizon_s": r.get::<_, i32>(2),
                "n": r.get::<_, i64>(3), "sellable": r.get::<_, i64>(4), "mean_net_return": r.get::<_, Option<f64>>(5), "positive_fraction": r.get::<_, Option<f64>>(6)})
        })
        .collect();
    out["execution_checks"] = exec
        .first()
        .map(|r| serde_json::json!({"checks": r.get::<_, i64>(0), "compared": r.get::<_, i64>(1), "median_adverse_bps": r.get::<_, Option<f64>>(2), "mean_adverse_bps": r.get::<_, Option<f64>>(3)}))
        .unwrap_or_default();
    out["observation_gaps"] = gaps.first().map(|r| serde_json::json!({"count": r.get::<_, i64>(0), "total_s": r.get::<_, i64>(1) as f64 / 1e6})).unwrap_or_default();
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
        println!("  forward evaluation (probe, net of fees): {}", out["forward_evaluation"]);
        println!("  execution vs observed market trades: {}", out["execution_checks"]);
        println!("  observation gaps: {}", out["observation_gaps"]);
    }
    Ok(())
}

/// Live validation report: everything observed, nothing inferred.
fn live_report(s: &Snapshot, seconds: u64) -> serde_json::Value {
    let decoded = s.health.decoded_events;
    let events = s.health.events_processed;
    let feed = s.feed_status.as_ref();
    let mut problems = Vec::new();
    if events == 0 {
        problems.push("no transactions received from any provider".to_string());
    }
    if events > 0 && decoded == 0 {
        problems.push("transactions received but no venue events decoded".into());
    }
    if let Some(f) = feed {
        if f.state != sniper_core::feed::FeedState::Healthy {
            problems.push(format!("feed ended {:?}: {}", f.state, f.reasons.join("; ")));
        }
    }
    if s.health.decode_warnings > 0 {
        problems.push(format!("{} decode warnings (last: {:?})", s.health.decode_warnings, s.health.last_decode_warning));
    }
    serde_json::json!({
        "verdict": {
            "seconds": seconds,
            "events_received": events,
            "venue_events_decoded": decoded,
            "decode_warnings": s.health.decode_warnings,
            "tokens_tracked": s.health.tokens_tracked,
            "feed_state": feed.map(|f| format!("{:?}", f.state)),
            "coverage": feed.and_then(|f| f.coverage),
            "problems": problems,
            "live_validated": problems.is_empty() && events > 0,
        },
        "providers": s.providers,
        "rpc_endpoints": s.rpc_endpoints,
        "tx_fetch": s.tx_fetch,
        "backfill": s.backfill,
        "latency": s.latency,
        "counters": s.counters,
        "health": s.health,
        "ingest": s.ingest,
        "feed_status": s.feed_status,
        "routes": s.routes,
    })
}

async fn capture(signatures: Vec<String>, from_live: Option<usize>, out: PathBuf) -> anyhow::Result<()> {
    let setup = sniper_core::live::LiveSetup::from_env()?;
    let rpc = setup.rpc()?.context("capture needs an RPC endpoint (SOLANA_RPC_URLS or a derivable SOLANA_WS_URLS)")?;
    let mut sigs = signatures;
    if let Some(n) = from_live {
        for prog in [programs::PUMP_FUN, programs::PUMP_SWAP] {
            for s in rpc.get_signatures_for_address(prog, n, None).await? {
                if !s.failed {
                    sigs.push(s.signature);
                }
            }
        }
    }
    anyhow::ensure!(!sigs.is_empty(), "nothing to capture: pass --signatures or --from-live N");
    std::fs::create_dir_all(&out)?;
    let mut saved = 0;
    for sig in sigs {
        let raw = rpc.get_transaction_raw(&sig).await?;
        if raw.is_null() {
            println!("{sig}: not found");
            continue;
        }
        let doc = serde_json::json!({
            "provenance": {"source": "getTransaction (jsonParsed)", "captured_at_us": wall_us(), "network": "mainnet-beta", "note": "REAL mainnet transaction captured read-only"},
            "signature": sig,
            "rpc_result": raw,
        });
        std::fs::write(out.join(format!("{sig}.json")), serde_json::to_string_pretty(&doc)?)?;
        saved += 1;
    }
    println!("captured {saved} real transactions into {}", out.display());
    println!("run `cargo test --test real_fixtures` to verify decoders against them");
    Ok(())
}
