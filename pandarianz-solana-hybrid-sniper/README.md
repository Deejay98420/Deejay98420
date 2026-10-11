# pandarianz-solana-hybrid-sniper (v1.2)

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
| `verify-live [SECONDS]` | Connect to your live providers, then write a validation report to `reports/`: connectivity, decode coverage, measured latency, feed health. |
| `capture [N]` | Capture a diverse sample of real mainnet transactions (launches, trades, completions, migrations, swaps) into `fixtures/real/`, then verify them. |
| `check` | One-command environment → tests → providers → decoders → next action. |
| `env-check` / `doctor [SECS]` | Dependency check / provider connectivity and capability diagnostics. |
| `verify-decoders` | Re-verify `fixtures/real/` against transaction evidence and run the decoder test suites. |
| `restart` | Stop and start the last mode and profile. Positions, balances and dedup state are recovered from PostgreSQL. |
| `calibrate [PROFILE_KEY]` | Calibration-split analysis of the expected-move model (read-only). |
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

## One command (Mac)

```bash
./scripts/sniper check
```

`check` runs these steps in order:

1. Verifies tools and `.env` (values are never printed).
2. Builds the project and runs every automated test plus the PAPER-only safety check.
3. Probes each configured provider with `doctor`: connection, which subscriptions are accepted or rejected, and PumpSwap coverage.
4. Captures real mainnet transactions and verifies the decoders against transaction evidence.
5. Prints the **single next required action**.

## V1.2: real-data evidence

- **`doctor`** gives an explicit capability check for every provider: `logsSubscribe` per program, `transactionSubscribe` (and whether its notifications carry inner instructions), `slotSubscribe`, `slotsUpdatesSubscribe`. It also checks RPC `getTransaction` decode of real Pump.fun and PumpSwap transactions and Jupiter quote availability, then recommends `SOLANA_STREAM_KIND`. Reports go to `reports/doctor-*.json` with every URL, path segment and key scrubbed. The live engine records the same per-provider subscription results and never reports a provider as subscribed unless its primary subscription was accepted.
- **Decoder verification against transaction evidence** (`capture` → `verify-fixtures`). Every decoded event is cross-checked against independent data in the same transaction:
  - per-owner token-balance deltas (amount and direction)
  - token conservation
  - pool vault deltas
  - per-component fee arithmetic
  - the curve invariant
  - SOL deltas (a soft check)

  Captures keep their signature, slot and block time, and are classified as launch, trade, curve completion, migration, pool creation, swap or liquidity. A layout counts as **mainnet-verified** only with at least 5 events and zero hard-check failures. The engine reads `fixtures/real/verification.json`.
- **Execution realism.** The v1.1 next-trade comparison used a future trade. It now compares only with the latest same-side trade **at or before** the fill. Every fill is classified **validated** or **unverified**, with reasons. Validated requires:
  - LIVE data
  - a mainnet-verified decoder for the venue
  - venue state at most 2 s old at fill
  - fee rates observed on-chain

  A position is validated only if both of its fills are. Unverified fills never count toward validated P&L.
- **Profitability evidence.**
  - Validated vs unverified P&L.
  - A fixed chronological split: even UTC days are calibration, odd days are holdout. Only **validated holdout** figures are presented as evidence. `calibrate` reads the calibration split only and never changes config.
  - Results by strategy × trade size.
  - Execution failure rate and unresolved positions.
  - Quote freshness at decision and at fill.
  - Fee / slippage / priority-fee sensitivity.
  - The expected-move model stays labelled **UNCALIBRATED** until at least 1,000 live calibration-split outcomes exist.

## Going live (data only, PAPER trading)

See **[docs/LIVE.md](docs/LIVE.md)** for the exact Mac Terminal steps. In short: put provider URLs in `.env` (`SOLANA_WS_URLS`, optionally `SOLANA_RPC_URLS` and `JUPITER_QUOTE_URL`), then run `./scripts/sniper verify-live 120`, `./scripts/sniper capture 25`, and `./scripts/sniper start live`.

What V1.1 adds for live operation:

- **Multiple providers at once**, deduplicated, with failover. The feed state machine is HEALTHY / DEGRADED / DOWN, driven by provider connection state, event silence, chain-tip freshness (`slotSubscribe`) and slot lag. Entries are blocked unless the feed is HEALTHY, and every unhealthy interval is persisted as an observation gap.
- **PumpSwap events with a logs-only provider.** Logs that invoke PumpSwap are routed to a bounded-concurrency `getTransaction` fetcher (fetch latency is measured). Alternatively, `SOLANA_STREAM_KIND=transactions` uses `transactionSubscribe`.
- **Startup backfill.** Recent history comes from `getSignaturesForAddress` + `getTransaction`. It updates state only and never triggers a trade, so tokens launched before startup can still be evaluated.
- **Pool discovery.** Pool accounts are fetched via `getAccountInfo` and decoded against the IDL. **Mint checks** cover freeze authority and risky Token-2022 extensions (transfer hook, permanent delegate, non-transferable, transfer fee, pausable, default-frozen).
- **Live executable quote observations.** Jupiter quotes are attached to pending orders. The fill basis is `conservative`, i.e. min(venue simulation, fresh aggregator quote). There are no fills on stale state.
- **Execution evidence.** Each simulated fill is compared with the next real same-side market trade.
- **Forward evaluation.** Accepted and rejected candidates are re-quoted at 30 s / 120 s / 300 s and written as immutable records. Decisions never read them; a test proves this.

## Data modes

| Mode | Source | Shown as |
|---|---|---|
| `FIXTURE` | `fixtures/synthetic/hybrid_session.jsonl`, generated deterministically by `sniper gen-fixtures`. Synthetic events encoded with the venue byte layouts and a reserve-consistent curve/pool simulation. | Amber `FIXTURE DATA` badge and a "NOT market data" banner |
| `LIVE` | Your provider's WebSocket (`SOLANA_WS_URL`) | Green `LIVE DATA` badge; trades are still PAPER |
| `REPLAY` | A profile's PostgreSQL journal | Blue `REPLAY DATA` badge |

Labels cannot be crossed: the fixture loader relabels everything as FIXTURE, a live engine rejects non-live events, and a non-live engine rejects live-labelled events.

## Decoder verification status

| Venue / item | Status | Evidence |
|---|---|---|
| Pump.fun Create/Trade/Complete/Migration events, BondingCurve account | **IDL-verified** | Official IDL from `@pump-fun/pump-sdk@4.0.0` (`fixtures/idl/`). A generic IDL-driven encoder is checked against our decoders in `tests/idl_verification.rs`. Curve math is ported from the SDK's `bondingCurve.ts` / `fees.ts`. |
| PumpSwap CreatePool/Buy/Sell/Deposit/Withdraw events, Pool account | **IDL-verified** | Official IDL from `@pump-fun/pump-swap-sdk@2.1.0`. AMM math (with `virtual_quote_reserves` and per-component fees) is ported from the SDK's `buy.ts` / `sell.ts`. |
| Raydium AMM v4 `ray_log` | **unverified**, decode-only | Never traded. |
| Captured mainnet transactions | **pending** | No RPC access from the build environment. `sniper capture` and `tests/real_fixtures.rs` are ready; the harness is self-tested. |

The IDL check found and fixed real issues relative to v1.0:

- Pump.fun curves can be quoted in **non-SOL mints**. These are now detected and never traded.
- Fees now include creator / coin-creator components, ceil-rounded per component, taken from each event.
- PumpSwap pricing now uses `vault + virtual_quote_reserves`.
- Sells that exceed the pool's real reserves have no route.

## Tests

`./scripts/sniper test` runs 98 tests, plus the static check and the dashboard typecheck/build. V1.2 adds:

- `v12_evidence` (9): transaction-evidence verification of curve trades and PumpSwap swaps, including that tampered events are caught; the mainnet-verified summary; explicit subscription capability checks (a rejected `transactionSubscribe` never reports "Subscribed"); doctor probes against mock providers with key scrubbing; fixture fills never validating; validated-fill rules; sensitivity monotonicity; the fixed split; and the end-to-end `capture` CLI with provenance and no key leakage.
- Unit tests: secret scrubbing (found and fixed a leak of key-bearing path segments) and the doctor assessment.
- `live_engine`: the execution check proven never to look ahead.

V1.1 suites:

- `idl_verification` (8): decoders vs. the official IDLs, including full new tails, the signed i128 offset, and account discriminators.
- `live_pipeline` (1): full LIVE mode against local mock providers. Covers dead-provider failover, chain tip, logs → `getTransaction` PumpSwap fetch, startup backfill, pool discovery, mint checks, a read-only RPC method audit, and the feed reaching HEALTHY.
- `live_engine` (9): entries blocked while the feed is unhealthy, gap persisted; mint-safety gating; backfill never trades; out-of-order guard; non-SOL quotes rejected; virtual-reserve pricing; conservative fill and stale-state refusal; execution checks; forward evaluation with a no-future-information proof.
- `real_fixtures` (2): real mainnet captures (currently none, reported explicitly), plus a self-test of the invariant harness.
- Feed state machine (4) and mint-safety unit tests (3); SDK-formula math tests.
- The v1.0 suites described below.

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

- **LIVE mainnet streaming has NOT been run.** No provider credentials were available, and the build container's egress policy blocks Solana RPC and Jupiter. Every live adapter is tested end-to-end against local mock providers that use the real RPC JSON shapes. Mainnet validation is the first `verify-live` run on your machine.
- **No decoder has been checked against a captured mainnet transaction yet.** They are checked against the official IDLs and SDK math (strong, but not the same thing). Run `capture` first.
- **Raydium v4 is decode-only.** `ray_log` has no pool or mint ids.
- **Docker Compose was not exercised here** (no Docker daemon in the container). The same scripts were run against a local PostgreSQL 16.
- **Latency figures** come from this machine on fixture data. The provider-lag and queue-wait series only fill in LIVE mode.
