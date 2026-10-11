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
        /// replace an existing file whose content differs (default: never; the new file is written
        /// next to it as hybrid_session.generated.jsonl)
        #[arg(long)]
        force: bool,
    },
    /// Connect to the configured live providers for N seconds (memory store, no trading decisions are
    /// persisted) and write a validation report: connectivity, decode coverage, latency, feed health.
    VerifyLive {
        #[arg(long, default_value_t = 120)]
        seconds: u64,
        #[arg(long, default_value = "../reports")]
        out: PathBuf,
    },
    /// Diagnose provider connectivity and subscription capabilities (keys are never printed).
    Doctor {
        #[arg(long, default_value_t = 20)]
        listen_seconds: u64,
        #[arg(long, default_value = "../reports")]
        out: PathBuf,
    },
    /// Verify captured real transactions against transaction evidence. Fixture files are only read;
    /// reports go to --report-dir.
    VerifyFixtures {
        #[arg(long, default_value = "../fixtures/real")]
        dir: PathBuf,
        #[arg(long, default_value = "../reports/verification")]
        report_dir: PathBuf,
    },
    /// Calibration-split analysis of the expected-move model (read-only; never changes config).
    Calibrate {
        #[arg(long)]
        profile: String,
    },
    /// Capture real mainnet transactions (read-only getTransaction) as regression fixtures. Bounded:
    /// stops at the target, the request budget or the time limit, whichever comes first. Existing
    /// fixture files are never modified or replaced.
    Capture {
        /// comma-separated signatures to capture
        #[arg(long, value_delimiter = ',')]
        signatures: Vec<String>,
        /// or: target number of NEW unique transactions (with venue events) to save from recent activity
        #[arg(long)]
        from_live: Option<usize>,
        /// maximum HTTP requests (including retries) this capture may send
        #[arg(long, default_value_t = 150)]
        max_requests: u64,
        /// wall-clock limit in seconds
        #[arg(long, default_value_t = 180)]
        max_seconds: u64,
        /// request rate limit (requests/second) shared by every request of this capture
        #[arg(long, default_value_t = 2.0)]
        rate: f64,
        /// concurrent requests
        #[arg(long, default_value_t = 2)]
        concurrency: usize,
        #[arg(long, default_value = "../fixtures/real")]
        out: PathBuf,
        #[arg(long, default_value = "../reports/verification")]
        report_dir: PathBuf,
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
    /// Show which provider endpoints are actually usable (redacted; keys are never printed).
    Endpoints,
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
        Cmd::GenFixtures { out, force } => {
            std::fs::create_dir_all(&out)?;
            let p = out.join("hybrid_session.jsonl"); // synthetic fixtures live in fixtures/synthetic
            let msgs = sniper_core::fixtures::hybrid_session();
            let text = sniper_core::fixtures::to_jsonl(&msgs);
            match std::fs::read_to_string(&p) {
                Ok(existing) if existing == text => println!("{} is up to date ({} synthetic messages)", p.display(), msgs.len()),
                Ok(_) if !force => {
                    // never overwrite the tracked fixture silently
                    let g = out.join("hybrid_session.generated.jsonl");
                    std::fs::write(&g, &text)?;
                    println!("{} differs from the generator and was left unchanged; new output written to {} (use --force to replace)", p.display(), g.display());
                }
                _ => {
                    std::fs::write(&p, &text)?;
                    println!("wrote {} synthetic messages to {}", msgs.len(), p.display());
                }
            }
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
        Cmd::Endpoints => {
            use sniper_core::live::endpoint_status;
            let (ws, m1) = endpoint_status(if std::env::var("SOLANA_WS_URLS").is_ok() { "SOLANA_WS_URLS" } else { "SOLANA_WS_URL" }, &["wss", "ws"]);
            let (rpc, m2) = endpoint_status(if std::env::var("SOLANA_RPC_URLS").is_ok() { "SOLANA_RPC_URLS" } else { "SOLANA_RPC_URL" }, &["https", "http"]);
            let (_, m3) = endpoint_status("JUPITER_QUOTE_URL", &["https", "http"]);
            println!("SOLANA_WS_URLS     {m1}");
            if rpc == 0 && ws > 0 {
                println!("SOLANA_RPC_URLS    {m2} → RPC derived from the WebSocket URL(s) (same host and key)");
            } else {
                println!("SOLANA_RPC_URLS    {m2}");
            }
            println!("JUPITER_QUOTE_URL  {m3} (optional)");
            println!("maxSupportedTransactionVersion = {}", sniper_core::rpc::max_tx_version());
            if ws == 0 {
                std::process::exit(3);
            }
            Ok(())
        }
        Cmd::Capture { signatures, from_live, max_requests, max_seconds, rate, concurrency, out, report_dir } => {
            let lim = CaptureLimits { max_requests, max_seconds, rate, concurrency };
            capture(signatures, from_live, lim, out, report_dir).await
        }
        Cmd::Doctor { listen_seconds, out } => doctor(listen_seconds, out).await,
        Cmd::VerifyFixtures { dir, report_dir } => {
            verify_fixtures(&dir, &report_dir, EngineConfig::load(std::env::var("SNIPER_CONFIG").ok().map(PathBuf::from).as_deref())?.evidence.min_events_for_mainnet_verified).map(|_| ())
        }
        Cmd::Calibrate { profile } => calibrate(&profile).await,
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
    engine.decoder_verification = sniper_core::verify::load_default_summary();
    if engine.decoder_verification.is_none() {
        tracing::warn!("no decoder verification report (reports/verification/verification.json): decoders are IDL-verified only; every fill is marked unverified");
    }
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
        let validated: Vec<_> = closed.iter().filter(|p| p.validated()).cloned().collect();
        let holdout: Vec<_> = validated.iter().filter(|p| sniper_core::metrics::evidence_split(p.opened_us) == "holdout").cloned().collect();
        let by_ps = {
            let mut m: std::collections::BTreeMap<String, Vec<sniper_core::paper::Position>> = Default::default();
            for p in &validated {
                m.entry(format!("{}@{}", p.path.as_str(), sniper_core::metrics::size_label(p.cost_basis))).or_default().push(p.clone());
            }
            m.into_iter().map(|(k, v)| (k, sniper_core::metrics::perf(&v))).collect::<std::collections::BTreeMap<_, _>>()
        };
        out["books"].as_array_mut().unwrap().push(serde_json::json!({
            "book_id": id, "start_sol": sol(*start as i64), "balance_sol": sol(*start as i64 + sum),
            "open_or_unresolved": open.len(), "unresolved": open.iter().filter(|p| p.status == sniper_core::paper::PositionStatus::Unresolved).count(),
            "perf": perf,
            "validated_closed": validated.len(), "unverified_closed": closed.len() - validated.len(),
            "perf_validated_holdout": sniper_core::metrics::perf(&holdout),
            "perf_validated_by_strategy_size": by_ps,
            "sensitivity_validated": sniper_core::metrics::sensitivity(&validated, 200_000),
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
            let h = &b["perf_validated_holdout"];
            println!(
                "  {}: VALIDATED HOLDOUT (evidence) trades {} net {:+.4} SOL expectancy {} PF {} maxDD {:.4} | validated {} / unverified {} closed | unresolved {}",
                b["book_id"].as_str().unwrap_or(""),
                h["closed_trades"],
                h["realized_pnl_sol"].as_f64().unwrap_or(0.0),
                h["expectancy_sol"],
                h["profit_factor"],
                h["max_drawdown_sol"].as_f64().unwrap_or(0.0),
                b["validated_closed"],
                b["unverified_closed"],
                b["unresolved"]
            );
            let p = &b["perf"];
            println!(
                "    all trades (incl. unverified): balance {:.4} SOL (start {:.4}), closed {}, win_rate {}, PF {}, expectancy {}, realized {:+.4}, maxDD {:.4}, open/unresolved {}",
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

struct CaptureLimits {
    max_requests: u64,
    max_seconds: u64,
    rate: f64,
    concurrency: usize,
}

/// Signatures already captured in `dir` (file stems and recorded signatures). Read-only.
fn existing_signatures(dir: &std::path::Path) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for p in std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()) {
        if p.extension().is_some_and(|x| x == "json") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                set.insert(stem.to_string());
            }
        }
    }
    set
}

#[derive(Default)]
struct CaptureTally {
    saved: usize,
    saved_by_program: std::collections::BTreeMap<&'static str, usize>,
    by_type: std::collections::BTreeMap<String, usize>,
    already_captured: usize,
    failed_on_chain: usize,
    not_found: usize,
    no_venue_events: usize,
    invalid_structure: usize,
    errors: std::collections::BTreeMap<String, usize>,
    error_examples: Vec<String>,
    versions: std::collections::BTreeMap<String, usize>,
    requeued: usize,
}

async fn capture(signatures: Vec<String>, from_live: Option<usize>, lim: CaptureLimits, out: PathBuf, report_dir: PathBuf) -> anyhow::Result<()> {
    use sniper_core::rpc::{RpcClient, RpcError, SchedulerConfig};
    let rpc_urls = sniper_core::live::rpc_urls_from_env();
    anyhow::ensure!(!rpc_urls.is_empty(), "capture needs an RPC endpoint (SOLANA_RPC_URLS, or SOLANA_WS_URLS from which it is derived)");
    anyhow::ensure!(lim.rate > 0.0 && lim.concurrency > 0 && lim.max_requests > 0, "--rate, --concurrency and --max-requests must be positive");
    let explicit = !signatures.is_empty();
    let target = if explicit { signatures.len() } else { from_live.context("nothing to capture: pass --signatures or --from-live N")? };
    let t0 = std::time::Instant::now();
    let deadline = t0 + Duration::from_secs(lim.max_seconds);
    let cfg = SchedulerConfig {
        rate_per_s: lim.rate,
        burst: (lim.rate.ceil() as u32).max(1),
        max_in_flight: lim.concurrency,
        max_queue: lim.concurrency * 4,
        max_wait: Duration::from_secs(lim.max_seconds.clamp(1, 30)),
        request_budget: Some(lim.max_requests),
        timeout: Duration::from_secs(15),
        ..SchedulerConfig::default()
    };
    let rpc = Arc::new(RpcClient::with_config(rpc_urls.clone(), cfg)?);
    std::fs::create_dir_all(&out)?;
    let existing = existing_signatures(&out);
    println!(
        "CAPTURE (read-only getTransaction, maxSupportedTransactionVersion={}): target {target} new transactions, ≤{} requests, ≤{}s, {} req/s, concurrency {}; {} already in {}",
        rpc.max_tx_version,
        lim.max_requests,
        lim.max_seconds,
        lim.rate,
        lim.concurrency,
        existing.len(),
        out.display()
    );
    let mut tally = CaptureTally::default();
    // candidate queues per program (newest first); explicit signatures go to a single queue
    let progs: Vec<&'static str> = if explicit { vec!["explicit"] } else { vec![programs::PUMP_FUN, programs::PUMP_SWAP] };
    let mut queues: std::collections::BTreeMap<&'static str, std::collections::VecDeque<String>> = progs.iter().map(|p| (*p, Default::default())).collect();
    let mut cursors: std::collections::BTreeMap<&'static str, Option<String>> = Default::default();
    let mut exhausted: std::collections::HashSet<&'static str> = Default::default();
    let mut seen: std::collections::HashSet<String> = Default::default();
    if explicit {
        for s in signatures {
            if existing.contains(&s) {
                tally.already_captured += 1;
            } else if seen.insert(s.clone()) {
                queues.get_mut("explicit").unwrap().push_back(s);
            }
        }
        exhausted.insert("explicit");
    }
    let label = |p: &str| match p {
        programs::PUMP_FUN => "pump",
        programs::PUMP_SWAP => "pumpswap",
        _ => "explicit",
    };
    let mut retry_once: std::collections::HashSet<String> = Default::default();
    let stop_reason: String = loop {
        if tally.saved >= target {
            break "target reached".into();
        }
        if std::time::Instant::now() >= deadline {
            break format!("time limit ({}s) reached", lim.max_seconds);
        }
        if rpc.budget_remaining() == Some(0) {
            break format!("request budget ({}) spent", lim.max_requests);
        }
        // pick the program with the fewest saved transactions that still has (or can list) candidates
        let mut order: Vec<&'static str> = progs.clone();
        order.sort_by_key(|p| tally.saved_by_program.get(label(p)).copied().unwrap_or(0));
        let Some(prog) = order.into_iter().find(|p| !queues[p].is_empty() || !exhausted.contains(p)) else {
            break "no more candidate signatures".into();
        };
        if queues[prog].is_empty() {
            // one page of the newest signatures (successful only, not yet captured, not seen)
            match rpc.get_signatures_for_address(prog, 40, cursors.get(prog).cloned().flatten().as_deref()).await {
                Ok(page) => {
                    if page.is_empty() {
                        exhausted.insert(prog);
                    }
                    cursors.insert(prog, page.last().map(|s| s.signature.clone()));
                    for s in page {
                        if s.failed {
                            tally.failed_on_chain += 1;
                        } else if existing.contains(&s.signature) {
                            tally.already_captured += 1;
                        } else if seen.insert(s.signature.clone()) {
                            queues.get_mut(prog).unwrap().push_back(s.signature);
                        }
                    }
                }
                Err(e) => {
                    *tally.errors.entry(format!("getSignaturesForAddress {}", e.class())).or_default() += 1;
                    if tally.error_examples.len() < 3 {
                        tally.error_examples.push(format!("getSignaturesForAddress({}): {e}", label(prog)));
                    }
                    if matches!(e, RpcError::BudgetExhausted) {
                        continue;
                    }
                    if !e.not_sent() {
                        exhausted.insert(prog); // do not hammer a failing listing
                    }
                }
            }
            continue;
        }
        // fetch a small batch concurrently (never more than still needed)
        let need = target - tally.saved;
        let batch: Vec<String> = (0..lim.concurrency.min(need).max(1)).filter_map(|_| queues.get_mut(prog).unwrap().pop_front()).collect();
        let results = futures_util::future::join_all(batch.iter().map(|sig| {
            let rpc = rpc.clone();
            let sig = sig.clone();
            async move { (sig.clone(), rpc.get_transaction_raw(&sig).await) }
        }))
        .await;
        for (sig, res) in results {
            match res {
                Ok(raw) if raw.is_null() => tally.not_found += 1,
                Ok(raw) => {
                    let problems = sniper_core::txmodel::structure_problems(&raw);
                    if !problems.is_empty() {
                        tally.invalid_structure += 1;
                        if tally.error_examples.len() < 3 {
                            tally.error_examples.push(format!("{}…: invalid response: {}", &sig[..8.min(sig.len())], problems.join(", ")));
                        }
                        continue;
                    }
                    let v = sniper_core::verify::verify_tx(&raw, &sig);
                    if v.tx_failed {
                        tally.failed_on_chain += 1;
                        continue;
                    }
                    if v.items.is_empty() {
                        tally.no_venue_events += 1;
                        continue;
                    }
                    if tally.saved >= target {
                        break;
                    }
                    let doc = serde_json::json!({
                        "provenance": {"source": "getTransaction (jsonParsed)", "captured_at_us": wall_us(), "network": "mainnet-beta",
                            "request": {"encoding": "jsonParsed", "commitment": "confirmed", "maxSupportedTransactionVersion": rpc.max_tx_version},
                            "transaction_version": v.version, "slot": v.slot, "block_time": v.block_time, "classification": v.classification,
                            "sampled_from": label(prog),
                            "note": "REAL mainnet transaction captured read-only; full RPC result preserved verbatim"},
                        "signature": sig,
                        "rpc_result": raw,
                    });
                    // create_new: an existing fixture is never modified or replaced
                    let path = out.join(format!("{sig}.json"));
                    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                        Ok(mut f) => {
                            use std::io::Write;
                            f.write_all(serde_json::to_string_pretty(&doc)?.as_bytes())?;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            tally.already_captured += 1;
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                    tally.saved += 1;
                    *tally.saved_by_program.entry(label(prog)).or_default() += 1;
                    *tally.versions.entry(v.version.clone()).or_default() += 1;
                    for c in v.classification {
                        *tally.by_type.entry(c).or_default() += 1;
                    }
                }
                Err(e) => {
                    *tally.errors.entry(e.class()).or_default() += 1;
                    if tally.error_examples.len() < 3 && !tally.error_examples.iter().any(|x| x.contains(&e.class())) {
                        tally.error_examples.push(format!("{}…: [{}] {e}", &sig[..8.min(sig.len())], e.class()));
                    }
                    // a request the scheduler deferred past its wait limit gets one more chance
                    if matches!(e, RpcError::Expired(_) | RpcError::QueueFull) && retry_once.insert(sig.clone()) {
                        tally.requeued += 1;
                        queues.get_mut(prog).unwrap().push_back(sig);
                    }
                }
            }
        }
    };
    let st = rpc.scheduler_stats();
    println!(
        "CAPTURE RESULT: saved {}/{target} new unique transactions {:?} in {:.0}s — stop reason: {stop_reason}",
        tally.saved,
        tally.saved_by_program,
        t0.elapsed().as_secs_f64()
    );
    println!("  by type {:?}; tx versions {:?}", tally.by_type, tally.versions);
    println!(
        "  skipped: {} already captured, {} failed on chain, {} without venue events, {} not found, {} invalid responses",
        tally.already_captured, tally.failed_on_chain, tally.no_venue_events, tally.not_found, tally.invalid_structure
    );
    println!(
        "  requests: {} sent / budget {} (ok {}, http 429 {}, retries {}, cooldowns {}); deferred {}, expired {}, rejected (queue) {}, rejected (budget) {}, requeued {}",
        st.http_sent, lim.max_requests, st.ok, st.http_429, st.retries, st.cooldowns, st.deferred, st.expired, st.queue_rejected, st.budget_rejected, tally.requeued
    );
    if !tally.errors.is_empty() {
        println!("  errors by class: {:?}", tally.errors);
        let secrets: Vec<String> = rpc_urls.clone();
        for e in &tally.error_examples {
            println!("    e.g. {}", sniper_core::doctor::scrub(e, &secrets));
        }
    }
    if tally.saved < target {
        println!("  NOTE: fewer than {target} saved — rerun later (already-captured transactions are skipped), or raise --max-requests / --max-seconds within your plan's limits.");
    }
    verify_fixtures(&out, &report_dir, EngineConfig::load(std::env::var("SNIPER_CONFIG").ok().map(PathBuf::from).as_deref())?.evidence.min_events_for_mainnet_verified).map(|_| ())
}

fn verify_fixtures(dir: &std::path::Path, report_dir: &std::path::Path, min_events: u64) -> anyhow::Result<sniper_core::verify::VerificationSummary> {
    let mut results = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("verification"))).collect())
        .unwrap_or_default();
    files.sort();
    let mut unreadable = 0;
    for f in &files {
        // fixture files are only READ here
        let Ok(doc) = std::fs::read_to_string(f).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str::<serde_json::Value>(&t)?)) else {
            unreadable += 1;
            continue;
        };
        if doc["provenance"]["network"] != "mainnet-beta" {
            continue;
        }
        results.push(sniper_core::verify::verify_tx(&doc["rpc_result"], doc["signature"].as_str().unwrap_or("")));
    }
    let summary = sniper_core::verify::summarize(&results, min_events);
    std::fs::create_dir_all(report_dir)?;
    // keep the previous reports: copied into history/<ts>/ (never deleted), then compared
    let prev_summary: Option<serde_json::Value> = std::fs::read_to_string(report_dir.join("verification.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
    let prev_detail: Option<serde_json::Value> = std::fs::read_to_string(report_dir.join("verification-detail.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
    let ts = wall_us() / 1_000_000;
    if prev_summary.is_some() || prev_detail.is_some() {
        let h = report_dir.join("history").join(ts.to_string());
        std::fs::create_dir_all(&h)?;
        for f in ["verification.json", "verification-detail.json"] {
            if report_dir.join(f).exists() && !h.join(f).exists() {
                std::fs::copy(report_dir.join(f), h.join(f))?;
            }
        }
    }
    std::fs::write(report_dir.join("verification.json"), serde_json::to_string_pretty(&summary)?)?;
    std::fs::write(report_dir.join("verification-detail.json"), serde_json::to_string_pretty(&results)?)?;
    println!("DECODER VERIFICATION — {} captured mainnet transactions in {} (read-only){}", summary.files, dir.display(), if unreadable > 0 { format!(", {unreadable} unreadable files skipped") } else { String::new() });
    if summary.files == 0 {
        println!("  none captured yet — run `./scripts/sniper capture 25` with provider access");
    }
    if summary.failed_transactions_skipped + summary.unparsable > 0 {
        println!("  {} failed-on-chain transactions skipped, {} unparsable", summary.failed_transactions_skipped, summary.unparsable);
    }
    for (venue, r) in &summary.venues {
        if r.status == "disabled" {
            println!("  {venue:<20} DISABLED ({})", r.reason);
            continue;
        }
        println!(
            "  {venue:<20} {:<22} {} signatures, {} events ({} trades, {} duplicate emissions merged); hard checks {} passed / {} failed / {} inconclusive; trades fully verified {}, with failures {}, inconclusive {}",
            r.status.to_uppercase(),
            r.unique_signatures,
            r.events_decoded,
            r.trade_events,
            r.duplicate_emissions_merged,
            r.checks_passed,
            r.checks_failed,
            r.checks_inconclusive,
            r.events_fully_verified,
            r.events_with_failures,
            r.events_inconclusive
        );
        println!("  {:<20} {}", "", r.reason);
        if !r.failures_by_check.is_empty() {
            println!("  {:<20} failed checks: {:?}", "", r.failures_by_check);
        }
        if !r.inconclusive_by_check.is_empty() {
            println!("  {:<20} inconclusive checks: {:?}", "", r.inconclusive_by_check);
        }
        if !r.fee_hypotheses.is_empty() {
            println!("  {:<20} fee routing matched: {:?}", "", r.fee_hypotheses);
        }
        for e in r.examples.iter().take(3) {
            println!("  {:<20} e.g. {}", "", e.chars().take(220).collect::<String>());
        }
    }
    if let (Some(ps), Some(pd)) = (&prev_summary, &prev_detail) {
        let cmp = compare_reports(ps, pd, &summary, &results);
        std::fs::write(report_dir.join(format!("comparison-{ts}.json")), serde_json::to_string_pretty(&cmp)?)?;
    }
    println!(
        "VENUES MAINNET-VERIFIED (validated P&L allowed): {}",
        summary.venues_mainnet_verified.iter().map(|(k, v)| format!("{k}={}", if *v { "YES" } else { "no" })).collect::<Vec<_>>().join(", ")
    );
    println!("report: {}/verification.json (per-transaction details: verification-detail.json)", report_dir.display());
    Ok(summary)
}

/// Print and return a before/after comparison against the previous report: per-venue totals, and the
/// new outcome of every check that previously FAILED or was INCONCLUSIVE.
fn compare_reports(prev: &serde_json::Value, prev_detail: &serde_json::Value, now: &sniper_core::verify::VerificationSummary, now_detail: &[sniper_core::verify::TxVerification]) -> serde_json::Value {
    use serde_json::json;
    let mut venues = serde_json::Map::new();
    println!("BEFORE → AFTER (previous report kept in history/):");
    for (venue, r) in &now.venues {
        if r.status == "disabled" {
            continue;
        }
        let p = &prev["venues"][venue];
        let g = |k: &str| p[k].as_u64().map(|x| x.to_string()).unwrap_or("?".into());
        println!(
            "  {venue:<20} hard checks pass/fail/inconclusive {}/{}/{} → {}/{}/{}; fully verified trades {} → {} ({} buys, {} sells); status {} → {}",
            g("checks_passed"), g("checks_failed"), g("checks_inconclusive"), r.checks_passed, r.checks_failed, r.checks_inconclusive,
            g("events_fully_verified"), r.events_fully_verified, r.fully_verified_buys, r.fully_verified_sells,
            p["status"].as_str().unwrap_or("?"), r.status
        );
        venues.insert(venue.clone(), json!({"before": p, "after": r}));
    }
    let mut changes = Vec::new();
    for tx in prev_detail.as_array().into_iter().flatten() {
        let sig = tx["signature"].as_str().unwrap_or("");
        let now_tx = now_detail.iter().find(|t| t.signature == sig);
        for c in tx["checks"].as_array().into_iter().flatten() {
            let before = c["outcome"].as_str().unwrap_or(if c["ok"].as_bool() == Some(true) { "pass" } else { "fail" });
            if before == "pass" || c["hard"].as_bool() != Some(true) {
                continue;
            }
            let (name, ev) = (c["name"].as_str().unwrap_or(""), c["event_index"].as_u64().unwrap_or(0) as usize);
            let after = now_tx.and_then(|t| t.checks.iter().find(|x| x.name == name && x.event_index == ev));
            let (a_out, a_detail, a_ix) = match after {
                Some(x) => (format!("{:?}", x.outcome).to_lowercase(), x.detail.clone(), x.instruction.clone().unwrap_or_default()),
                None => ("not produced".into(), String::new(), String::new()),
            };
            println!("    {}… {:<30} {:>12} → {:<12} {}", &sig[..8.min(sig.len())], name, before.to_uppercase(), a_out.to_uppercase(), a_detail.chars().take(170).collect::<String>());
            changes.push(json!({"signature": sig, "event_index": ev, "check": name, "before": before, "before_detail": c["detail"], "after": a_out, "after_detail": a_detail, "instruction": a_ix}));
        }
    }
    if changes.is_empty() {
        println!("    (no previously failing or inconclusive checks)");
    }
    json!({"venues": venues, "previously_failed_or_inconclusive": changes})
}

async fn doctor(listen_seconds: u64, out: PathBuf) -> anyhow::Result<()> {
    use sniper_core::live::endpoint_list;
    let ws_urls: Vec<String> = endpoint_list(&std::env::var("SOLANA_WS_URLS").or_else(|_| std::env::var("SOLANA_WS_URL")).unwrap_or_default(), &["wss", "ws"]);
    let mut rpc_urls: Vec<String> = endpoint_list(&std::env::var("SOLANA_RPC_URLS").or_else(|_| std::env::var("SOLANA_RPC_URL")).unwrap_or_default(), &["https", "http"]);
    if rpc_urls.is_empty() {
        rpc_urls = ws_urls.iter().filter_map(|u| sniper_core::live::ws_to_http(u)).collect();
    }
    println!("probing {} WebSocket and {} RPC endpoint(s) (read-only, {listen_seconds}s listen)…", ws_urls.len(), rpc_urls.len());
    let ws = futures_util::future::join_all(ws_urls.iter().map(|u| sniper_core::doctor::probe_ws(u, Duration::from_secs(listen_seconds)))).await;
    let rpc = futures_util::future::join_all(rpc_urls.iter().map(|u| sniper_core::doctor::probe_rpc(u))).await;
    let jup = sniper_core::doctor::probe_jupiter(endpoint_list(&std::env::var("JUPITER_QUOTE_URL").unwrap_or_default(), &["https", "http"]).first().map(|s| s.as_str()), std::env::var("JUPITER_API_KEY").ok().filter(|s| !s.is_empty())).await;
    let rep = sniper_core::doctor::assess(ws, rpc, jup);
    std::fs::create_dir_all(&out)?;
    let path = out.join(format!("doctor-{}.json", wall_us() / 1_000_000));
    let mut secrets = ws_urls.clone();
    secrets.extend(rpc_urls.clone());
    secrets.extend(std::env::var("JUPITER_QUOTE_URL").ok());
    let text = sniper_core::doctor::scrub(&serde_json::to_string_pretty(&rep)?, &secrets);
    std::fs::write(&path, &text)?;
    for w in &rep.ws {
        println!("WS  {}  connected={} connect={:?}ms", w.host, w.connected, w.connect_ms.map(|x| x.round()));
        for (k, v) in &w.subscriptions {
            println!("      {:<24} {:<10} notifications={} {}", k, match v.accepted { Some(true) => "accepted", Some(false) => "REJECTED", None => "no answer" }, v.notifications, sniper_core::doctor::scrub(&v.detail, &secrets));
        }
        if let Some(e) = &w.error {
            println!("      error: {}", sniper_core::doctor::scrub(e, &secrets));
        }
    }
    for r in &rep.rpc {
        println!("RPC {}  ok={} getSlot={:?}ms pump events decoded={} pumpswap events decoded={} {}", r.host, r.ok, r.get_slot_ms.map(|x| x.round()), r.pump_events_decoded, r.pumpswap_events_decoded, r.error.as_ref().map(|e| sniper_core::doctor::scrub(e, &secrets)).unwrap_or_default());
    }
    if rep.jupiter.configured {
        println!("JUP configured ok={} {}", rep.jupiter.ok, rep.jupiter.error.clone().unwrap_or_default());
    } else {
        println!("JUP not configured (JUPITER_QUOTE_URL empty; optional)");
    }
    println!("PumpSwap coverage: {}", rep.pumpswap_coverage);
    println!("recommended SOLANA_STREAM_KIND: {}", rep.recommended_stream_kind.clone().unwrap_or("-".into()));
    for p in &rep.problems {
        println!("PROBLEM: {p}");
    }
    println!("READY FOR LIVE: {}", rep.ready_for_live);
    println!("NEXT: {}", rep.next_action);
    println!("report: {}", path.display());
    Ok(())
}

async fn calibrate(profile: &str) -> anyhow::Result<()> {
    let pg = PgStore::connect(&db_url()?, profile).await?;
    // calibration split only (even UTC days), live data only, 120s horizon
    let rows = pg
        .client
        .query(
            "select (doc->>'expected_move_bps')::float8 as em, net_return from forward_outcomes
             where profile_key=$1 and profile_key like 'LIVE:%' and horizon_s=120 and net_return is not null
               and doc ? 'expected_move_bps' and ((t0_us / 86400000000) % 2) = 0",
            &[&profile],
        )
        .await?;
    let mut v: Vec<(f64, f64)> = rows.iter().filter_map(|r| Some((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<f64>>(1)?))).collect();
    let min = EngineConfig::default().evidence.min_outcomes_for_calibration as usize;
    println!("calibration split (even UTC days), LIVE only, 120s horizon: {} outcomes (need ≥ {min})", v.len());
    if v.len() < min {
        println!("INSUFFICIENT EVIDENCE — expected-move model stays UNCALIBRATED. Keep collecting live data.");
        return Ok(());
    }
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    println!("{:>14} {:>14} {:>16} {:>8}", "predicted bps", "realized bps", "realized − pred", "n");
    for chunk in v.chunks(v.len().div_ceil(5)) {
        let p = chunk.iter().map(|x| x.0).sum::<f64>() / chunk.len() as f64;
        let r = chunk.iter().map(|x| x.1 * 10_000.0).sum::<f64>() / chunk.len() as f64;
        println!("{p:>14.0} {r:>14.0} {:>16.0} {:>8}", r - p, chunk.len());
    }
    let (sp, sr): (f64, f64) = v.iter().fold((0.0, 0.0), |a, x| (a.0 + x.0, a.1 + x.1 * 10_000.0));
    println!("suggested expected_move_scale_bps multiplier (calibration split only): {:.3}", if sp > 0.0 { sr / sp } else { 0.0 });
    println!("This is NOT applied. Evaluate any change only on the holdout split (odd UTC days) collected afterwards.");
    Ok(())
}
