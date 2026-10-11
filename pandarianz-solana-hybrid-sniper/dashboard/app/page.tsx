"use client";

import { useEffect, useMemo, useState } from "react";
import type { BookView, Candidate, PerfStats, Position, Reason, Sensitivity, Snapshot } from "../lib/types";
import { PnlChart } from "./PnlChart";

const POLL_MS = 2000;
const LAMPORTS = 1e9;

const sol = (v: number | null | undefined, d = 4) => (v == null || Number.isNaN(v) ? "–" : v.toFixed(d));
const lam = (v: number | null | undefined, d = 4) => (v == null ? "–" : (v / LAMPORTS).toFixed(d));
const pct = (v: number | null | undefined) => (v == null ? "–" : `${(v * 100).toFixed(1)}%`);
const signed = (v: number | null | undefined, d = 4) => (v == null ? "–" : `${v >= 0 ? "+" : "−"}${Math.abs(v).toFixed(d)}`);
const cls = (v: number | null | undefined) => (v == null ? "" : v > 0 ? "pos" : v < 0 ? "neg" : "");
const ms = (us: number | null | undefined) => (us == null ? "–" : us >= 1e6 ? `${(us / 1e6).toFixed(2)} s` : us >= 1000 ? `${(us / 1000).toFixed(1)} ms` : `${us} µs`);
const time = (us: number | null | undefined) => (us ? new Date(us / 1000).toISOString().slice(11, 19) : "–");
const short = (m: string) => `${m.slice(0, 4)}…${m.slice(-4)}`;
const name = (sym: string | null, mint: string) => (sym ? `${sym} · ${short(mint)}` : short(mint));

function reasonText(r: Reason): string {
  const { code, ...rest } = r;
  const parts = Object.entries(rest).map(([k, v]) => `${k}=${typeof v === "number" ? (Math.abs(v) < 100 ? v.toFixed(3) : v.toFixed(0)) : String(v)}`);
  return parts.length ? `${code} (${parts.join(", ")})` : code;
}

export default function Page() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [scenario, setScenario] = useState("base");
  const [fetchedAt, setFetchedAt] = useState<number>(0);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const r = await fetch("/engine/snapshot", { cache: "no-store" });
        if (!r.ok) throw new Error(`engine API ${r.status}`);
        const s = (await r.json()) as Snapshot;
        if (alive) {
          setSnap(s);
          setErr(null);
          setFetchedAt(Date.now());
        }
      } catch (e) {
        if (alive) setErr(e instanceof Error ? e.message : String(e));
      }
    };
    load();
    const id = setInterval(load, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const book: BookView | undefined = useMemo(() => snap?.books.find((b) => b.scenario === scenario) ?? snap?.books[0], [snap, scenario]);
  const trades = useMemo(() => (snap?.trades ?? []).filter((t) => t.scenario === (book?.scenario ?? scenario)), [snap, book, scenario]);
  const mode = snap?.data_mode ?? null;

  return (
    <div className="wrap">
      <header className="top">
        <h1>Pandarianz Solana Hybrid Sniper</h1>
        <div className="badges">
          <span className="badge paper" title="No keys, no signing, no broadcasting">PAPER ONLY</span>
          {mode && <span className={`badge ${mode}`}>{mode === "LIVE" ? "LIVE DATA / PAPER TRADING" : mode === "FIXTURE" ? "OFFLINE FIXTURE" : "REPLAY"}</span>}
          {snap?.feed_status && <span className={`badge feed-${snap.feed_status.state}`} title={snap.feed_status.reasons.join("; ")}>FEED {snap.feed_status.state}</span>}
          <span className="badge neutral mono">{snap?.strategy_version ?? "…"}</span>
          <span className={`badge neutral`}>{err ? "engine unreachable" : snap?.health.ended ? "source ended" : snap ? "streaming" : "connecting"}</span>
        </div>
      </header>

      {err && <div className="note"><span className="err">Engine API unreachable:</span> {err}. Start it with <code className="mono">./scripts/sniper start</code>. Showing last known data{fetchedAt ? ` from ${new Date(fetchedAt).toLocaleTimeString()}` : ""}.</div>}
      {snap && <div className={`note ${mode ?? ""}`}><strong>{snap.status_banner}.</strong> {snap.data_mode_note} Profile <span className="mono">{snap.profile_key}</span>.</div>}
      {snap?.feed_status && snap.feed_status.state !== "HEALTHY" && (
        <div className="note DOWN"><strong>Feed {snap.feed_status.state}: new entries are blocked.</strong> {snap.feed_status.reasons.join("; ")}</div>
      )}

      {snap && book && (
        <div className="grid">
          <section className="card span-12">
            <div style={{ display: "flex", justifyContent: "space-between", flexWrap: "wrap", gap: 8, marginBottom: 10 }}>
              <h2 style={{ margin: 0 }}>Performance (SOL) — scenario</h2>
              <div className="tabs" role="group" aria-label="Simulation scenario">
                {snap.books.map((b) => (
                  <button key={b.scenario} aria-pressed={b.scenario === book.scenario} onClick={() => setScenario(b.scenario)}>{b.scenario}</button>
                ))}
              </div>
            </div>
            <div className="tiles">
              <Tile label="Equity" value={sol(book.equity_sol)} sub={`start ${sol(book.start_sol, 2)} · unresolved valued at 0`} />
              <Tile label="Equity P&L" value={signed(book.equity_pnl_sol)} tone={book.equity_pnl_sol} />
              <Tile label="Realized P&L" value={signed(book.perf.realized_pnl_sol)} tone={book.perf.realized_pnl_sol} sub={`fees ${sol(book.perf.total_fees_sol, 5)}`} />
              <Tile label="Trades" value={String(book.perf.closed_trades)} sub={`${book.perf.wins}W / ${book.perf.losses}L`} />
              <Tile label="Win rate" value={pct(book.perf.win_rate)} />
              <Tile label="Expectancy" value={signed(book.perf.expectancy_sol, 5)} tone={book.perf.expectancy_sol} sub="per closed trade" />
              <Tile label="Profit factor" value={book.perf.profit_factor == null ? "–" : book.perf.profit_factor.toFixed(2)} sub={book.perf.profit_factor == null && book.perf.closed_trades ? "no losing trades yet" : ""} />
              <Tile label="Max drawdown" value={sol(book.perf.max_drawdown_sol)} sub="realized" />
              <Tile label="Open / pending" value={`${book.open_positions.length} / ${book.pending_orders}`} sub={`exposure ${sol(book.exposure_sol, 3)}`} />
              <Tile label="Failed-entry fees" value={sol(book.failed_entry_costs_sol, 5)} />
              <Tile label="Accounting" value={book.accounting_ok ? "OK" : "ERROR"} tone={book.accounting_ok ? 1 : -1} sub={book.accounting_error ?? "balance = start + Σ ledger"} />
            </div>
            <div style={{ marginTop: 14 }}>
              <h2>Cumulative realized P&L (SOL) by closed trade</h2>
              <PnlChart trades={trades} />
            </div>
          </section>

          <section className="card span-12">
            <h2>Profitability evidence — {book.scenario}</h2>
            <div className="tiles">
              <Tile label="Validated holdout P&L" value={signed(book.perf_validated_holdout.realized_pnl_sol)} tone={book.perf_validated_holdout.realized_pnl_sol} sub={`${book.perf_validated_holdout.closed_trades} trades · the only figure to read as evidence`} />
              <Tile label="Validated P&L (all splits)" value={signed(book.perf_validated.realized_pnl_sol)} tone={book.perf_validated.realized_pnl_sol} sub={`${book.perf_validated.closed_trades} trades · calibration ${book.perf_validated_calibration.closed_trades}`} />
              <Tile label="Unverified P&L" value={signed(book.perf_unverified.realized_pnl_sol)} sub={`${book.perf_unverified.closed_trades} trades · NOT evidence`} />
              <Tile label="Execution failure rate" value={pct(book.execution_failure_rate)} sub="dropped + slippage fails + no route" />
              <Tile label="Unresolved positions" value={String(book.unresolved_positions)} tone={book.unresolved_positions ? -1 : null} sub="valued at 0" />
              <Tile label="Expected-move model" value={snap.expected_move_model_status.split(" — ")[0]} sub={snap.expected_move_model_status.split(" — ")[1] ?? ""} />
            </div>
            <p className="empty" style={{ fontStyle: "normal" }}>A fill is <b>validated</b> only on LIVE data, with a decoder verified against captured mainnet transactions, venue state ≤ 2 s old at fill and fee rates observed on-chain. Positions count as validated only if entry and exit both validate. Holdout = odd UTC days; thresholds may only be tuned on calibration days.</p>
            <div className="grid" style={{ marginTop: 8 }}>
              <div className="span-6" style={{ minWidth: 0 }}>
                <h2>By strategy and size</h2>
                <PerfTable rows={book.perf_by_path_size} />
              </div>
              <div className="span-6" style={{ minWidth: 0 }}>
                <h2>Fee &amp; slippage sensitivity (all closed trades)</h2>
                <SensTable rows={book.sensitivity_all} />
              </div>
            </div>
          </section>

          <section className="card span-8">
            <h2>Open & unresolved positions — {book.scenario}</h2>
            <Positions rows={book.open_positions} now={snap.generated_us} />
          </section>

          <section className="card span-4">
            <h2>Health</h2>
            <dl className="kv">
              {Object.entries(snap.health.connections).map(([p, c]) => (
                <Row key={p} k={`stream ${p}`} v={`${c.state ?? "?"} · ${c.reconnects} reconnects`} />
              ))}
              {Object.keys(snap.health.connections).length === 0 && <Row k="stream" v={mode === "LIVE" ? "no provider connected" : "offline source"} />}
              <Row k="last event" v={snap.health.last_event_us ? `${((snap.generated_us - snap.health.last_event_us) / 1e6).toFixed(1)} s ago` : "–"} />
              <Row k="events / decoded" v={`${snap.health.events_processed} / ${snap.health.decoded_events}`} />
              <Row k="duplicates dropped" v={`${(snap.ingest?.duplicates ?? 0) + snap.health.engine_duplicates_dropped}`} />
              <Row k="failed tx skipped" v={String(snap.health.failed_tx_skipped)} />
              <Row k="slot rollbacks" v={String(snap.health.rollbacks)} />
              <Row k="queue high-water" v={snap.ingest ? `${snap.ingest.queue_high_water} / ${snap.ingest.queue_capacity}` : "–"} />
              <Row k="backpressure waits" v={String(snap.ingest?.backpressure_waits ?? 0)} />
              <Row k="tokens tracked" v={String(snap.health.tokens_tracked)} />
              <Row k="unresolved pool events" v={String(snap.health.unresolved_pool_events)} />
              <Row k="store" v={`${snap.store_backend} · ${snap.store_written} writes · ${snap.store_errors} errors`} />
              <Row k="evaluations / entries" v={`${snap.evaluations} / ${snap.entries_decided}`} />
            </dl>
            {snap.health.recovery_notes.length > 0 && (
              <>
                <h2 style={{ marginTop: 12 }}>Recovery</h2>
                <ul style={{ margin: 0, paddingLeft: 18, fontSize: 12 }}>{snap.health.recovery_notes.slice(0, 8).map((n, i) => <li key={i}>{n}</li>)}</ul>
              </>
            )}
          </section>

          <section className="card span-6">
            <h2>Live feed &amp; coverage</h2>
            {snap.feed_status ? (
              <>
                <dl className="kv">
                  <Row k="state" v={`${snap.feed_status.state} (${snap.feed_status.providers_up}/${snap.feed_status.providers_total} providers up)`} />
                  <Row k="coverage (healthy time)" v={pct(snap.feed_status.coverage)} />
                  <Row k="chain tip" v={snap.feed_status.tip_slot ? `${snap.feed_status.tip_slot} · ${ms((snap.feed_status.tip_age_ms ?? 0) * 1000)} ago` : "no tip subscription"} />
                  <Row k="slot lag (tip − newest event)" v={snap.feed_status.slot_lag == null ? "–" : `${snap.feed_status.slot_lag} slots`} />
                  <Row k="last event" v={snap.feed_status.last_event_age_ms == null ? "–" : ms(snap.feed_status.last_event_age_ms * 1000) + " ago"} />
                  {snap.tx_fetch && <Row k="tx fetch (logs→getTransaction)" v={`${snap.tx_fetch.fetched}/${snap.tx_fetch.requested} · ${snap.tx_fetch.not_found} missing · mean ${snap.tx_fetch.mean_fetch_ms?.toFixed(0) ?? "–"} ms`} />}
                  {snap.backfill && <Row k="startup backfill" v={`${snap.backfill.transactions_loaded} txs in ${snap.backfill.duration_ms.toFixed(0)} ms`} />}
                  {Object.entries(snap.coverage).map(([k, v]) => <Row key={k} k={`coverage ${k}`} v={v} />)}
                </dl>
                <div className="tablewrap" style={{ marginTop: 8 }}>
                  <table>
                    <thead><tr><th>provider</th><th>state</th><th className="num">first deliveries</th><th className="num">reconnects</th><th className="num">tip</th><th>subscriptions</th></tr></thead>
                    <tbody>{Object.entries(snap.providers).map(([k, p]) => (
                      <tr key={k}><td>{k}</td><td>{p.state ?? "?"}</td><td className="num">{p.first_deliveries}</td><td className="num">{p.reconnects}</td><td className="num">{p.tip_slot ?? "–"}</td>
                        <td style={{ whiteSpace: "normal" }}>{Object.entries(p.capabilities ?? {}).map(([m, v]) => <span key={m} className={`chip ${v.startsWith("accepted") ? "enter" : ""}`} title={v}>{m}: {v.startsWith("accepted") ? "ok" : "REJECTED"}</span>)}</td></tr>
                    ))}</tbody>
                  </table>
                </div>
                {snap.rpc_endpoints.length > 0 && (
                  <div className="tablewrap" style={{ marginTop: 8 }}>
                    <table>
                      <thead><tr><th>RPC (read-only)</th><th className="num">ok</th><th className="num">errors</th><th className="num">429</th><th>last error</th></tr></thead>
                      <tbody>{snap.rpc_endpoints.map((r) => (
                        <tr key={r.host}><td>{r.host}</td><td className="num">{r.ok}</td><td className="num">{r.errors}</td><td className="num">{r.rate_limited}</td><td>{r.last_error?.slice(0, 40) ?? "–"}</td></tr>
                      ))}</tbody>
                    </table>
                  </div>
                )}
                <h2 style={{ marginTop: 12 }}>Observation gaps</h2>
                {[...(snap.feed_status.open_gap ? [snap.feed_status.open_gap] : []), ...snap.feed_status.recent_gaps].length === 0 ? <div className="empty">No gaps.</div> : (
                  <ul style={{ margin: 0, paddingLeft: 18, fontSize: 12 }}>
                    {[...(snap.feed_status.open_gap ? [snap.feed_status.open_gap] : []), ...snap.feed_status.recent_gaps].slice(0, 8).map((g, i) => (
                      <li key={i}>{time(g.start_us)}–{g.end_us ? time(g.end_us) : "ongoing"} · {g.state} · {g.reason}</li>
                    ))}
                  </ul>
                )}
              </>
            ) : <div className="empty">Offline source — no live feed in this mode.</div>}
          </section>

          <section className="card span-6">
            <h2>Execution quality &amp; fill outcomes</h2>
            <div className="tiles">
              <Tile label="Fills" value={String(snap.counters.fills_ok ?? 0)} sub={`${snap.counters.fills_failed_slippage ?? 0} slippage fails · ${snap.counters.fills_dropped ?? 0} dropped`} />
              <Tile label="No fill (no route)" value={String(snap.counters.fills_no_route ?? 0)} sub={`${snap.counters.fills_stale_state ?? 0} refused: stale state`} />
              <Tile label="Aggregator observations" value={String(snap.counters.order_observations_attached ?? 0)} sub="live Jupiter quotes attached to orders" />
              <Tile label="Vs real trades" value={`${snap.execution_quality.compared}/${snap.execution_quality.checks}`} sub="fills compared with next market trade" />
              <Tile label="Median adverse" value={snap.execution_quality.median_adverse_bps == null ? "–" : `${snap.execution_quality.median_adverse_bps.toFixed(0)} bps`} sub="+ = simulation worse than market" />
              <Tile label="Within ±150 bps" value={pct(snap.execution_quality.within_150bps)} />
            </div>
            <p className="empty" style={{ fontStyle: "normal" }}>Each order keeps three things apart: the decision quote, the simulated fill (venue simulation, or conservative min with a live aggregator quote) and the observed market execution of the next same-side trade.</p>
          </section>

          <section className="card span-6">
            <h2>Strategy performance — {book.scenario}</h2>
            <div className="tablewrap">
              <table>
                <thead><tr><th>strategy</th><th className="num">trades</th><th className="num">win</th><th className="num">net SOL</th><th className="num">expectancy</th><th className="num">PF</th><th className="num">max DD</th></tr></thead>
                <tbody>{Object.entries(book.perf_by_path ?? {}).map(([k, p]) => (
                  <tr key={k}><td>{k === "launch" ? "launch sniper" : "momentum scanner"}</td><td className="num">{p.closed_trades}</td><td className="num">{pct(p.win_rate)}</td>
                    <td className={`num ${cls(p.realized_pnl_sol)}`}>{signed(p.realized_pnl_sol)}</td><td className={`num ${cls(p.expectancy_sol)}`}>{signed(p.expectancy_sol, 5)}</td>
                    <td className="num">{p.profit_factor == null ? "–" : p.profit_factor.toFixed(2)}</td><td className="num">{sol(p.max_drawdown_sol)}</td></tr>
                ))}</tbody>
              </table>
            </div>
          </section>

          <section className="card span-6">
            <h2>Forward evaluation (probe {"0.05"} SOL, net of fees)</h2>
            <ForwardTable rows={snap.forward_summary} pending={snap.forward_pending} />
          </section>

          <section className="card span-6">
            <h2>Latency (measured, this process)</h2>
            <div className="tablewrap">
              <table>
                <thead><tr><th>stage</th><th className="num">n</th><th className="num">p50</th><th className="num">p95</th><th className="num">p99</th><th className="num">max</th></tr></thead>
                <tbody>
                  {Object.entries(snap.latency).map(([k, l]) => (
                    <tr key={k}><td>{k}</td><td className="num">{l.count}</td><td className="num">{ms(l.p50_us)}</td><td className="num">{ms(l.p95_us)}</td><td className="num">{ms(l.p99_us)}</td><td className="num">{ms(l.max_us)}</td></tr>
                  ))}
                </tbody>
              </table>
            </div>
            <p className="empty" style={{ fontStyle: "normal" }}>Fixture/replay runs use event time; provider lag and queue wait are only measured in LIVE mode. fill_latency_sim is the scenario&apos;s simulated landing latency, not a measurement.</p>
          </section>

          <section className="card span-6">
            <h2>Route availability</h2>
            <div className="tablewrap">
              <table>
                <thead><tr><th>source</th><th className="num">attempts</th><th className="num">ok</th><th className="num">avail.</th><th className="num">no route</th><th className="num">rate-ltd</th><th className="num">disabled</th><th>last error</th></tr></thead>
                <tbody>
                  {Object.entries(snap.routes).map(([k, r]) => {
                    const denom = r.attempts - r.disabled;
                    return (
                      <tr key={k}>
                        <td>{k}</td><td className="num">{r.attempts}</td><td className="num">{r.ok}</td>
                        <td className="num">{denom > 0 ? pct(r.ok / denom) : "n/a"}</td><td className="num">{r.no_route}</td><td className="num">{r.rate_limited}</td><td className="num">{r.disabled}</td>
                        <td title={r.last_error ?? ""}>{r.last_error ? r.last_error.slice(0, 48) : "–"}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </section>

          <section className="card span-8">
            <h2>Candidates (latest evaluation per token)</h2>
            <Candidates rows={snap.candidates} />
          </section>

          <section className="card span-4">
            <h2>Rejection reasons (all evaluations)</h2>
            <RejectionBars counts={snap.rejection_counts} />
          </section>

          <section className="card span-8">
            <h2>Trade journal — {book.scenario}</h2>
            <Journal rows={trades} />
          </section>

          <section className="card span-12">
            <h2>Decoder verification</h2>
            <div className="tablewrap">
              <table>
                <thead><tr><th>venue</th><th>item</th><th>status</th><th>evidence</th></tr></thead>
                <tbody>{snap.decoder_verification.map((v, i) => (
                  <tr key={i}><td>{v.venue}</td><td>{v.item}</td><td className={v.status === "mainnet_verified" ? "pos" : v.status === "idl_verified" ? "" : "warn"}>{v.status}</td><td style={{ whiteSpace: "normal" }}>{v.evidence}</td></tr>
                ))}</tbody>
              </table>
            </div>
          </section>

          <section className="card span-4">
            <h2>Feed</h2>
            {snap.feed.length === 0 ? <div className="empty">No events yet.</div> : (
              <ul className="feed">
                {snap.feed.slice(0, 120).map((f) => (
                  <li key={`${f.signature}-${f.kind}-${f.mint}`}>
                    <span className="mono">{time(f.ts_us)}</span>
                    <span>{f.kind}</span>
                    <span title={f.mint}>{name(f.symbol, f.mint)} — {f.detail}</span>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      )}
      <footer>Read-only dashboard. The engine exposes GET endpoints only; there is no order, wallet or signing interface anywhere in this system. Numbers are paper simulations and are not a promise of future results.</footer>
    </div>
  );
}

function Tile({ label, value, sub, tone }: { label: string; value: string; sub?: string; tone?: number | null }) {
  return (
    <div className="tile">
      <div className="label">{label}</div>
      <div className={`value num ${tone == null ? "" : cls(tone)}`}>{value}</div>
      {sub ? <div className="sub">{sub}</div> : null}
    </div>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (<><dt>{k}</dt><dd className="mono">{v}</dd></>);
}

function Positions({ rows, now }: { rows: Position[]; now: number }) {
  if (!rows.length) return <div className="empty">No open positions.</div>;
  return (
    <div className="tablewrap">
      <table>
        <thead><tr><th>token</th><th>path</th><th>status</th><th className="num">cost</th><th className="num">exit value</th><th className="num">P&L</th><th className="num">held</th><th>exit / issue</th></tr></thead>
        <tbody>
          {rows.map((p) => {
            const v = p.route_lost_since_us != null ? null : p.last_value;
            const pnl = v == null ? null : (v - p.cost_basis) / LAMPORTS;
            return (
              <tr key={p.position_id}>
                <td title={p.mint}>{name(p.symbol, p.mint)}</td><td>{p.path}</td>
                <td className={p.status === "unresolved" ? "neg" : p.status === "exiting" ? "warn" : ""}>{p.status}</td>
                <td className="num">{lam(p.cost_basis)}</td>
                <td className="num">{v == null ? "no route" : lam(v)}</td>
                <td className={`num ${cls(pnl)}`}>{signed(pnl)}</td>
                <td className="num">{((now - p.opened_us) / 1e6).toFixed(0)}s</td>
                <td title={p.unresolved_reason ?? ""}>{p.unresolved_reason ?? (p.exit_reason ? `${p.exit_reason.code}${p.exit_attempts ? ` · ${p.exit_attempts} failed` : ""}` : "–")}{p.trigger_rolled_back ? " · trigger rolled back" : ""}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function Candidates({ rows }: { rows: Candidate[] }) {
  if (!rows.length) return <div className="empty">No evaluations yet.</div>;
  return (
    <div className="tablewrap">
      <table>
        <thead><tr><th>token</th><th>path</th><th className="num">score</th><th className="num">liq</th><th className="num">buyers 30s</th><th className="num">buy %</th><th className="num">net flow</th><th>decision / reasons</th></tr></thead>
        <tbody>
          {rows.map((c) => (
            <tr key={c.mint}>
              <td title={c.mint}>{name(c.symbol, c.mint)}</td><td>{c.path}</td>
              <td className="num">{c.score.toFixed(2)}</td><td className="num">{c.liquidity_sol.toFixed(1)}</td>
              <td className="num">{c.unique_buyers_30s}</td><td className="num">{(c.buy_ratio_30s * 100).toFixed(0)}%</td>
              <td className={`num ${cls(c.net_flow_sol_30s)}`}>{signed(c.net_flow_sol_30s, 2)}</td>
              <td style={{ whiteSpace: "normal", minWidth: 260 }}>
                {c.decision === "enter" ? <span className="chip enter">enter {c.size_sol.toFixed(2)} SOL</span> : c.reasons.slice(0, 4).map((r, i) => <span className="chip" key={i} title={reasonText(r)}>{r.code}</span>)}
                {c.decision !== "enter" && c.reasons.length > 4 ? <span className="chip">+{c.reasons.length - 4}</span> : null}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function PerfTable({ rows }: { rows: Record<string, PerfStats> }) {
  const e = Object.entries(rows ?? {});
  if (!e.length) return <div className="empty">No closed trades yet.</div>;
  return (
    <div className="tablewrap">
      <table>
        <thead><tr><th>strategy@size</th><th className="num">trades</th><th className="num">win</th><th className="num">net SOL</th><th className="num">expectancy</th><th className="num">PF</th><th className="num">max DD</th></tr></thead>
        <tbody>{e.map(([k, p]) => (
          <tr key={k}><td>{k}</td><td className="num">{p.closed_trades}</td><td className="num">{pct(p.win_rate)}</td><td className={`num ${cls(p.realized_pnl_sol)}`}>{signed(p.realized_pnl_sol)}</td>
            <td className={`num ${cls(p.expectancy_sol)}`}>{signed(p.expectancy_sol, 5)}</td><td className="num">{p.profit_factor == null ? "–" : p.profit_factor.toFixed(2)}</td><td className="num">{sol(p.max_drawdown_sol)}</td></tr>
        ))}</tbody>
      </table>
    </div>
  );
}

function SensTable({ rows }: { rows: Sensitivity[] }) {
  if (!rows?.length || rows[0].trades === 0) return <div className="empty">No closed trades yet.</div>;
  return (
    <div className="tablewrap">
      <table>
        <thead><tr><th>assumption</th><th className="num">net SOL</th><th className="num">expectancy</th><th className="num">PF</th><th className="num">win</th></tr></thead>
        <tbody>{rows.map((r) => (
          <tr key={r.label}><td>{r.label}</td><td className={`num ${cls(r.net_sol)}`}>{signed(r.net_sol)}</td><td className={`num ${cls(r.expectancy_sol)}`}>{signed(r.expectancy_sol, 5)}</td>
            <td className="num">{r.profit_factor == null ? "–" : r.profit_factor.toFixed(2)}</td><td className="num">{pct(r.win_rate)}</td></tr>
        ))}</tbody>
      </table>
    </div>
  );
}

function ForwardTable({ rows, pending }: { rows: Record<string, { n: number; unsellable: number; mean_net_return: number | null; median_net_return: number | null; positive_fraction: number | null }>; pending: number }) {
  const entries = Object.entries(rows).filter(([k]) => !k.startsWith("reject:")).sort();
  const rejects = Object.entries(rows).filter(([k]) => k.startsWith("reject:") && k.endsWith("@120s")).sort((a, b) => b[1].n - a[1].n).slice(0, 8);
  if (!entries.length) return <div className="empty">No forward outcomes yet ({pending} probes pending).</div>;
  const row = ([k, b]: [string, { n: number; unsellable: number; mean_net_return: number | null; median_net_return: number | null; positive_fraction: number | null }]) => (
    <tr key={k}><td>{k}</td><td className="num">{b.n}</td><td className={`num ${cls(b.mean_net_return)}`}>{b.mean_net_return == null ? "–" : `${(b.mean_net_return * 100).toFixed(1)}%`}</td>
      <td className="num">{b.median_net_return == null ? "–" : `${(b.median_net_return * 100).toFixed(1)}%`}</td><td className="num">{pct(b.positive_fraction)}</td><td className="num">{b.unsellable}</td></tr>
  );
  return (
    <div className="tablewrap">
      <table>
        <thead><tr><th>bucket</th><th className="num">n</th><th className="num">mean</th><th className="num">median</th><th className="num">&gt; 0</th><th className="num">unsellable</th></tr></thead>
        <tbody>{entries.map(row)}{rejects.length > 0 && <tr><td colSpan={6} className="empty">top rejection reasons @120s (would rejected candidates have paid?)</td></tr>}{rejects.map(row)}</tbody>
      </table>
      <p className="empty" style={{ fontStyle: "normal" }}>{pending} probes pending. Outcomes are recorded after each horizon and never feed back into decisions.</p>
    </div>
  );
}

function RejectionBars({ counts }: { counts: Record<string, number> }) {
  const rows = Object.entries(counts).sort((a, b) => b[1] - a[1]).slice(0, 14);
  if (!rows.length) return <div className="empty">No rejections yet.</div>;
  const max = rows[0][1];
  return (
    <div className="bars">
      {rows.map(([k, v]) => (
        <div className="row" key={k} title={`${k}: ${v}`}>
          <span>{k}</span>
          <div className="track"><div className="fill" style={{ width: `${(v / max) * 100}%` }} /></div>
          <span className="num" style={{ textAlign: "right" }}>{v}</span>
        </div>
      ))}
    </div>
  );
}

function Journal({ rows }: { rows: Position[] }) {
  if (!rows.length) return <div className="empty">No closed trades yet.</div>;
  return (
    <div className="tablewrap">
      <table>
        <thead><tr><th>closed</th><th>token</th><th>path</th><th className="num">cost</th><th className="num">proceeds</th><th className="num">fees</th><th className="num">P&L</th><th className="num">hold</th><th>exit reason</th></tr></thead>
        <tbody>
          {rows.map((p) => {
            const pnl = p.realized_pnl == null ? null : p.realized_pnl / LAMPORTS;
            const fees = p.entry_tx_costs + p.exit_tx_costs + p.failed_exit_costs + p.venue_fees_paid;
            return (
              <tr key={p.position_id}>
                <td className="mono">{time(p.closed_us)}</td><td title={p.mint}>{name(p.symbol, p.mint)}</td><td>{p.path}</td>
                <td className="num">{lam(p.cost_basis)}</td><td className="num">{lam(p.proceeds)}</td><td className="num">{lam(fees, 5)}</td>
                <td className={`num ${cls(pnl)}`}>{signed(pnl)}</td>
                <td className="num">{p.closed_us ? `${((p.closed_us - p.opened_us) / 1e6).toFixed(0)}s` : "–"}</td>
                <td title={p.exit_reason ? reasonText(p.exit_reason) : ""}>{p.exit_reason?.code ?? "–"}{p.exit_attempts ? ` · ${p.exit_attempts} failed` : ""}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
