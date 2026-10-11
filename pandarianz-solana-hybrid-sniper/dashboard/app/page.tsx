"use client";

import { useEffect, useMemo, useState } from "react";
import type { BookView, Candidate, Position, Reason, Snapshot } from "../lib/types";
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
          {mode && <span className={`badge ${mode}`}>{mode} DATA</span>}
          <span className="badge neutral mono">{snap?.strategy_version ?? "…"}</span>
          <span className={`badge neutral`}>{err ? "engine unreachable" : snap?.health.ended ? "source ended" : snap ? "streaming" : "connecting"}</span>
        </div>
      </header>

      {err && <div className="note"><span className="err">Engine API unreachable:</span> {err}. Start it with <code className="mono">./scripts/sniper start</code>. Showing last known data{fetchedAt ? ` from ${new Date(fetchedAt).toLocaleTimeString()}` : ""}.</div>}
      {snap && <div className={`note ${mode ?? ""}`}>{snap.data_mode_note} Profile <span className="mono">{snap.profile_key}</span>.</div>}

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
