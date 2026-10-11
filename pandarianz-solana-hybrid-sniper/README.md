# pandarianz-solana-hybrid-sniper

A **PAPER-ONLY** Solana memecoin launch sniper and momentum trader. It has an event-driven Rust core, PostgreSQL persistence with a replayable journal, and a read-only Next.js dashboard.

> **Safety.** This system cannot trade. There is no wallet or private-key handling, no transaction building or signing, no sending or bundle submission, and no order endpoint. Every trade is a simulated fill. `scripts/check_no_live.sh` and `core/tests/no_live_execution.rs` enforce this statically.
>
> **No profitability claims.** The bundled data is **synthetic fixture data**, labelled `FIXTURE` everywhere. P&L from it shows only that the pipeline and accounting work. It says nothing about real-market profitability.

## Quick start (macOS)

Prerequisites: Rust (https://rustup.rs), Node.js 20+, and Docker Desktop. Docker can be skipped if `DATABASE_URL` points at a local PostgreSQL 14+.

```bash
cd pandarianz-solana-hybrid-sniper
./scripts/sniper setup            # .env from template, release build, npm install, fixtures, DB up + migrate
./scripts/sniper start            # offline FIXTURE replay + dashboard → http://127.0.0.1:3000
./scripts/sniper status           # processes, DB, engine health JSON
./scripts/sniper report           # performance report for the last profile (from PostgreSQL)
./scripts/sniper stop             # graceful: engine flushes its store, then exits  (--db also stops PostgreSQL)
./scripts/sniper test             # static safety check + all Rust tests (incl. PostgreSQL) + dashboard typecheck/build
```

Other commands:

| Command | Purpose |
|---|---|
| `start fixture --profile NAME` | Reuse a profile to resume it. Balances, positions and dedup state are recovered from PostgreSQL. |
| `start live` | Live provider stream. Needs `SOLANA_WS_URL` in `.env`; refuses to start without it. |
| `replay PROFILE_KEY` | Re-run a recorded journal through the current strategy as `REPLAY` data. |
| `logs` | Tail the engine log. |
| `db-up` / `db-down` | Start or stop the Docker PostgreSQL. |

The engine API is read-only (GET only) at `127.0.0.1:8787/api/{snapshot,health,safety}`.

## What it does

```
provider stream (WS logsSubscribe | transactionSubscribe | fixture JSONL | journal replay)
   → Ingestor: dedup by signature (multi-provider), commitment upgrades, bounded queue + backpressure
   → Engine (single owner of state, no hot-path locks)
       decode (Pump.fun curve, PumpSwap AMM, Raydium v4 ray_log) → token state / analytics
       → unified entry evaluation (launch path | momentum path) → size selection 0.02/0.05/0.1 SOL
       → PAPER books ×3 scenarios (optimistic/base/pessimistic): latency, fill-time state, fees,
         adverse slippage, drops, slippage failures, own-impact-aware exits, ledger
       → dynamic exit manager (rug/LP pull/flow deterioration/trailing/stagnation/max-hold, retries,
         migration hold, explicit Unresolved)
   → async store writer (PostgreSQL, fills written atomically) ; dashboard snapshot
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the modules and [docs/ASSUMPTIONS.md](docs/ASSUMPTIONS.md) for venue layouts, cost model and known limitations.

### Requirement checklist

1. **Event-driven core.** Provider-neutral `StreamMsg`/`RawEvent`, reconnect with backoff and jitter, idle timeout, resubscribe, signature dedup across providers, rollback via `Slot{Dead}` (state rebuilt without the dead slot, pending entries cancelled), bounded queues with measured backpressure, and structured `Stamps`.
2. **Discovery.** Pump.fun create/trade/complete/migration events, PumpSwap pool create/swaps/deposits/withdrawals, and curve completion → migration. A separate momentum path covers tokens first seen mid-life. Raydium v4 `ray_log` is decoded but not traded (see the limitations below).
3. **Token analytics.** Reserves, liquidity, unique buyers, 10/30/60/300 s flow windows, top-holder and creator concentration, bundled-launch detection, wash round-trips, creator dumping, LP removal, and sellability (a successful non-creator sell must have been seen). Every rejection reason is kept per evaluation.
4. **Decision engine.** One evaluator for both paths with a configurable score and filters. Size is chosen by score tier, a liquidity cap, entry impact, and the estimated round-trip cost (both swap legs, own impact, tx fees). Exits are dynamic; there are no fixed take-profits.
5. **Quotes.** Venue-local exact integer simulation (bonding curve and CPMM, with fee bps taken from trade events when reported). The Jupiter quote client has a configurable URL, local token bucket, 429 cooldown, timeout and no-route handling. Coverage, timestamps, latency and errors are recorded per quote (`route_checks`).
6. **PAPER execution.** Size-aware quotes at fill time, not decision time. Includes fees, priority fee, tip, ATA rent (deposit and refund), impact, scenario adverse slippage, dropped transactions (no fee), slippage failures (fees charged), and no-route handling. Last price is never used as the fill price.
7. **Exit manager.** Emergency slippage on rug warnings, escalating slippage with backoff on retries, hold-through-migration, `Unresolved` with a reason, and unresolved positions valued at 0 in equity.
8. **PostgreSQL.** Append-only journal, evaluations, order submissions and completions, ledger, position events and route checks; database triggers reject UPDATE/DELETE. Each fill's ledger, position and order are written in one transaction. Records carry the strategy version (a hash of the config), source timestamps, fill quotes and cost breakdowns.
9. **Dashboard.** PAPER and data-mode badges, health, feed, candidates with reasons, rejection histogram, open/unresolved positions, trade journal, SOL P&L, win rate, expectancy, profit factor, drawdown, measured latency and route availability. Responsive, with light and dark modes.
10. **Tests.** 57 tests plus 1 ignored diagnostic; see below.
11. **Safety.** Static source and dependency scans, GET-only API checks, and a `.env.example` placeholder check.
12. **Tooling.** `scripts/sniper` (bash 3.2 compatible), `.env.example` with placeholders, and `docker-compose.yml` for PostgreSQL.

## Data modes

| Mode | Source | Shown as |
|---|---|---|
| `FIXTURE` | `fixtures/hybrid_session.jsonl`, generated deterministically by `sniper gen-fixtures`. Synthetic events encoded with the venue byte layouts and a reserve-consistent curve/pool simulation. | Amber `FIXTURE DATA` badge and a "NOT market data" banner |
| `LIVE` | Your provider's WebSocket (`SOLANA_WS_URL`) | Green `LIVE DATA` badge; trades are still PAPER |
| `REPLAY` | A profile's PostgreSQL journal | Blue `REPLAY DATA` badge |

Labels cannot be crossed: the fixture loader relabels everything as FIXTURE, a live engine rejects non-live events, and a non-live engine rejects live-labelled events.

## Tests

`./scripts/sniper test` runs:

- `decode` (12): round-trips per venue; legacy/short layouts; invoke-stack attribution (a router's own `Program data` is ignored); garbage/truncation → warnings; WS `logsNotification` / `transactionNotification` parsing; URL key redaction; fixture-never-live.
- `stream` (7): dedup window / upgrades / eviction; multi-provider duplicates; bounded queue backpressure with no loss and order kept; backoff; **real reconnection** against a local WebSocket server (close and idle timeout); reconnect exhaustion; journaled rollback markers.
- `state_strategy` (12): reserve tracking; creator detection; **exact state restore after slot rollback**; pool mapping, unresolved and non-SOL pools; migration route gap; stale-data rejection; size selection and liquidity cap; specific rejection reasons; momentum path; no fixed take-profit (+400% with strong flow holds); trailing / rug / stagnation exits; own-impact sell modelling.
- `paper_engine` (10): exact entry cost breakdown with fill-time state; slippage-failure fees vs drop (free); no-route; **end-to-end lifecycle where each closed position's realized P&L equals the sum of its ledger entries**; determinism; re-delivered stream → zero new orders; rollback cancels a pending entry; concurrent signals never open two positions for one mint; **restart recovery** (balances, positions, contiguous ledger, no duplicates); in-flight order at crash → `abandoned_restart`, never filled.
- `pg` (1): PostgreSQL migrations, immutability triggers, idempotency uniqueness, restart recovery from PostgreSQL, and ledger reconciliation in SQL.
- `quote_api` (5): Jupiter success / 429 cooldown (no hammering) / no-route / timeout / local budget; API is GET-only.
- `no_live_execution` (3): source and dependency scans, `.env.example` placeholders.
- Unit tests (7): swap math, rate limiter, Anchor discriminators (checked against the published Pump.fun `TradeEvent` value).

## What is verified vs blocked

**Verified in this build environment:** everything above. That includes a full `start` → dashboard → `stop` → restart-with-same-profile (recovered from PostgreSQL) → `report` → `replay` cycle, with replay reproducing the original run's books.

**Not verified / blocked:**

- **LIVE mainnet streaming has not been run.** No provider credentials were available, and the build container's network policy blocks Solana RPC and Jupiter hosts. The WebSocket client is tested only against a local server. The first live run is the real test of message shapes and throughput.
- **Fixture byte layouts come from public IDLs and were not checked against captured mainnet transactions.** Only the Pump.fun `TradeEvent` discriminator is checked against a published value. Capture real transactions and add them as fixtures before trusting live decoding.
- **PumpSwap events are invisible to plain `logsSubscribe`.** They are emitted via `emit_cpi`, so live PumpSwap tracking needs `SOLANA_STREAM_KIND=transactions` (a provider supporting `transactionSubscribe`). PumpSwap swaps on pools whose creation or migration was not observed are counted as `unresolved_pool_events`; RPC pool-account lookup is not implemented.
- **Raydium v4 is decode-only.** `ray_log` has no pool or mint ids.
- **Docker Compose was not exercised here** (no Docker daemon in the container). The same scripts were run against a local PostgreSQL 16.
- **Latency figures** come from this machine on fixture data. The provider-lag and queue-wait series only fill in LIVE mode.
