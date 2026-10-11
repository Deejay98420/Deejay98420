# pandarianz-solana-hybrid-sniper (v1.2.3)

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
./scripts/sniper check                                                   # no network capture
./scripts/sniper check --capture 25 --max-requests 150 --max-seconds 300 # + bounded mainnet capture
```

`check` runs these steps in order and never trades, signs or sends anything:

1. Verifies tools and `.env` (values are never printed; empty or placeholder endpoints are reported as NOT CONFIGURED).
2. PAPER-only static safety check.
3. Builds and runs every automated regression test.
4. Re-verifies the transactions already captured in `fixtures/real/` (files are only read).
5. Provider diagnostics (`endpoints` + `doctor`).
6. Optional bounded mainnet capture (`--capture N`): stops at the target, the request budget or the time limit.
7. Final verification report per venue (`reports/verification/`), then the **single next required action**.

## V1.2.3: final Pump.fun accounting repair (replayed on the 15 real transactions)

Offline replay of the 15 real mainnet transactions (verbatim copies in `fixtures/mainnet/v121/`, checked against the evidence manifest's SHA-256): **145 pass / 1 fail / 7 inconclusive → 164 pass / 0 fail / 5 inconclusive**. 10 SOL-quoted trades pass every hard check (7 buys, 3 sells); Pump.fun status `mainnet_verified` under the unchanged rule (≥5 incl. buys and sells, zero failures). PumpSwap: 0 samples, unverified. Raydium: disabled.

- **v3 instructions (`buy_exact_quote_in_v3`, `buy_v3`, `sell_v3`).** The official SDK (`state.ts`) documents that v3 trades leave protocol fees *net of the buyback slice* and creator fees **on the curve**, paying only the buyback slice out. The curve therefore receives the **gross** payment: in `3As2Jg52…` the parsed System transfer inside the Pump.fun call is 6,286,873 = base 6,209,257 + protocol fee 58,988 + creator fee 18,628 (`trade.sol_amount` FAIL → PASS). The curve's transaction-wide balance (6,257,379 = gross − buyback slice 29,494) is a separate check, `trade.curve_net_balance`. The buyback slice must go to the instruction's `buyback_fee_recipient`, which must be an official wallet, and equal the event's own `buyback_fee`. No signature-specific logic, no 48,122 constant.
- **Token-quoted v3 curves** (quoted in another pump coin) are verified from parsed token transfers (`trade.quote_amount`; curve quote vault + direct buyback slice = base + fees). They are reported separately and never counted toward the venue status (never traded).
- **Attribution scope.** Balance-only evidence counts only for accounts named exclusively by the trade instruction and its own CPIs. If a calling (wrapper) instruction also names them, an exact reconciliation is reported but stays **INCONCLUSIVE** (strict default; `verify-fixtures --accept-ancestor-scoped` counts it: 169/0/0, 12 trades). Accounts named by unrelated instructions (e.g. other swaps in a routed transaction) are never attributable. Parsed transfers inside the instruction remain the primary evidence; a contradicting parsed transfer is a FAIL.
- **`sniper explain FILE…`** prints a transaction's events, emitting instruction, every account by IDL role with balance deltas and which instructions name it, and the movements inside the instruction.
- **Selective PumpSwap capture** (`capture --venue pumpswap`, `./scripts/sniper capture-pumpswap N R S RPS`): `logsSubscribe` on PumpSwap (works on free plans), keeps only notifications whose **PumpSwap** instruction log says Buy/Sell (routers' own logs, deposits and failed transactions are ignored), deduplicates against notifications and existing files, bounded per-side queues that always fetch the side with fewer samples next, `getTransaction` through the shared scheduler (rate, budget, time, 429 handling), saves only transactions where a PumpSwap swap decodes, never overwrites, and writes a separate `capture-pumpswap-<ts>.json` evidence manifest. Never run by `check --offline`.

## V1.2.2: evidence-based repair (from the forensic audit of 15 real captures)

- **Split protocol fees.** The six fee "failures" were a second recipient sharing the decoded fee. All four observed second accounts are in the official `Global.buybackFeeRecipients` list (`@pump-fun/pump-sdk@4.0.0` `bondingCurve.ts`, recorded with sha256 in `fixtures/idl/pump.fee-recipients.json`), and the SDK passes that account as a remaining account of legacy `buy`/`sell`. The check now sums the credits of every recipient bound to the trade instruction (its `fee_recipient` plus an official buyback recipient among its accounts) and requires the exact decoded total — no 50/50 split is assumed; odd lamports need no special case. Evidence must be parsed System transfers inside that instruction, or balance deltas of accounts no other instruction names; balance-only evidence through a wrapper (`5guPtRHy…`) stays **INCONCLUSIVE**. Parsed credits must also agree with the balance of any account only that instruction touches.
- **Creator fee.** The creator-fee transfer inside the trade instruction is compared with the decoded creator fee; other transfers into the vault (e.g. 650,240 lamports in `3R1MvPxv…`) are recorded as notes ("purpose not established, not counted as fee"). If no transfer equals the decoded fee it is a FAIL.
- **Instruction matching.** Events are tied to their emitting instruction exactly: the self-CPI's parent one call level up, or the n-th invocation of the program in the log invoke records — so Pump.fun trades called through another program's CPI are matched. `buy_exact_quote_in_v3` was added from the IDL. An unrecognized discriminator stays INCONCLUSIVE and its hex is reported.
- **Token-account lifecycle.** A `closeAccount` is checked against the account's running balance at that moment (pre balance + parsed movements before it); closing never counts as a burn. Accounts missing from post balances are treated as closed only with that evidence; otherwise INCONCLUSIVE.
- **Every check names its instruction** (`instruction: "ix 3.0 buy (6EF8rr)"`). `verify-fixtures` copies the previous report into `reports/verification/history/<ts>/` and prints **BEFORE → AFTER** totals and the new outcome of every previously failing or inconclusive check (`comparison-<ts>.json`).
- `setup` no longer rewrites the tracked synthetic fixture; `gen-fixtures` writes `hybrid_session.generated.jsonl` instead of overwriting a differing file (use `--force` to replace).

## V1.2.1: mainnet repair

Fixes for the defects found on the first real mainnet run:

- **Version-1 transactions.** `getTransaction` / `transactionSubscribe` send `maxSupportedTransactionVersion: 1` (env `SOLANA_MAX_TX_VERSION`, default 1); v0/v1 `loadedAddresses` are part of the account list; responses are structurally validated and kept verbatim, with the transaction version recorded.
- **One quota-aware request scheduler** (`rpc.rs`) shared by live fetches, backfill, mint/pool checks, capture and verification: callers wait for rate-limit capacity (bounded by `max_wait`) instead of failing; HTTP 429 honours `Retry-After`, else exponential backoff with jitter, and cools down the whole client; bounded in-flight requests and a bounded queue (excess calls are rejected, never queued without limit); optional request budget; identical concurrent requests share one HTTP call. Sent / ok / 429 / retried / deferred / expired / rejected / deduplicated are counted separately.
- **Bounded capture.** `capture --from-live N` saves N *new, unique, successfully fetched* transactions with venue events, newest first, balanced across Pump.fun and PumpSwap, within `--max-requests` / `--max-seconds`, and explains why it stopped. Existing fixture files are never modified (`create_new`); reports go to `reports/verification/`.
- **Doubled Pump.fun amounts.** Root cause: Pump.fun emits each `TradeEvent` both as a `Program data:` log and as an Anchor self-CPI inner instruction; both copies were decoded, so one trade counted twice. Copies are now merged when venue, payload bytes and top-level instruction match (one-to-one, so genuinely repeated trades survive). Every event carries provenance (channel, top-level instruction, inner position, call depth) and a stable id `signature:outer:inner:ordinal`.
- **Instruction-level accounting** (`verify.rs`, `txmodel.rs`, `ixlayout.rs`). Each event is tied to the instruction that emitted it; its accounts come from the official IDL instruction layouts (`fixtures/idl/*.ix-accounts.json`). Checks use the token transfer executed inside that instruction, the specific curve vault / user token account / pool vaults, lamports of the curve, fee recipient and creator vault, and per-mint conservation **Σ balance changes = minted − burned** from parsed `mintTo`/`burn` (token creation is minted supply, not a violation). Movements by other instructions (account creation rent, unrelated transfers) are accounted for explicitly. Fee routing is tested against named hypotheses and the one that matched is reported.
- **Pass / Fail / Inconclusive.** Missing or ambiguous evidence (no inner-instruction record, unknown layout, several trades in one multi-hop instruction, an account changed without an exact parsed amount) is *inconclusive* and never counts as a pass. A venue is **mainnet-verified** only when no hard check fails and at least 5 trade events (≥1 buy and ≥1 sell) pass every hard check.
- **Per-venue report**: unique signatures, events decoded, duplicate emissions merged, checks passed / failed / inconclusive, failures by check with examples, fee hypotheses, transaction versions, provenance. Raydium stays disabled.

## V1.2: real-data evidence

- **`doctor`** gives an explicit capability check for every provider: `logsSubscribe` per program, `transactionSubscribe` (and whether its notifications carry inner instructions), `slotSubscribe`, `slotsUpdatesSubscribe`. It also checks RPC `getTransaction` decode of real Pump.fun and PumpSwap transactions and Jupiter quote availability, then recommends `SOLANA_STREAM_KIND`. Reports go to `reports/doctor-*.json` with every URL, path segment and key scrubbed. The live engine records the same per-provider subscription results and never reports a provider as subscribed unless its primary subscription was accepted.
- **Decoder verification against transaction evidence** (`capture` → `verify-fixtures`). Every decoded event is cross-checked against independent data in the same transaction:
  - per-owner token-balance deltas (amount and direction)
  - token conservation
  - pool vault deltas
  - per-component fee arithmetic
  - the curve invariant
  - SOL deltas (a soft check)

  Captures keep their signature, slot and block time, and are classified as launch, trade, curve completion, migration, pool creation, swap or liquidity. (V1.2.1 replaced these checks with instruction-level ones; see above. The engine reads `reports/verification/verification.json`.)
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
| Captured mainnet transactions | **not verified** | The first real run (15 transactions, on the user's machine) exposed the defects fixed in V1.2.1. Neither venue is mainnet-verified until `./scripts/sniper check --capture 25` passes on real data. |

The IDL check found and fixed real issues relative to v1.0:

- Pump.fun curves can be quoted in **non-SOL mints**. These are now detected and never traded.
- Fees now include creator / coin-creator components, ceil-rounded per component, taken from each event.
- PumpSwap pricing now uses `vault + virtual_quote_reserves`.
- Sells that exceed the pool's real reserves have no route.

## Tests

`./scripts/sniper test` runs 137 tests, plus the static check and the dashboard typecheck/build. V1.2.3 adds `v123_mainnet_replay` (8: all 15 real transactions with exact outcomes and unchanged bytes; gross v3 payment vs base vs net curve balance; a contradicting parsed transfer FAILs; an unrelated instruction naming the curve makes balance evidence INCONCLUSIVE; missing inner-instruction records are gaps, never failures; the real odd-lamport split and extra vault funding; token-quoted v3 legs; strict vs `--accept-ancestor-scoped` through the CLI) and `v123_pumpswap_capture` (4: PumpSwap-own log classification, bounded dedup selector, CLI against a mock logsSubscribe + RPC with 429s — balanced unique swaps, no overwrite, budget stop). V1.2.2 adds `v122_audit_replay` (9): reconstructions of the audited structures using the audit's amounts and accounts — the five parsed fee splits (incl. 4,692 + 4,691), a non-official second account (FAIL), balance-only split through a wrapper (INCONCLUSIVE) vs. top-level (PASS), creator vault with 650,240 extra funding, nested CPI matching via self-CPI and via log order, unrecognized discriminator, sells closing the token account (incl. closing a non-empty account → INCONCLUSIVE), the venue rule, and the before/after report. These are synthetic reconstructions; mainnet status comes only from `fixtures/real`. V1.2.1 added:

- `v121_mainnet_repair` (18): IDL instruction-layout provenance; version 0/1/legacy transactions and lookup-table accounts; structural validation; log+CPI duplicate merge (the doubled-amount regression); repeated genuine trades in one transaction; token creation in conservation; fee reconciliation (observed transfers and SDK formula); missing evidence → inconclusive, never verified; wrong direction; HTTP 429 with `Retry-After`; bounded retries; `-32015` classification; bounded queue; request budget and wait limit; in-flight dedupe; placeholder endpoints; `capture` under 429 pressure stopping cleanly within its budget.
- `v12_evidence` and `real_fixtures` rewritten on realistic synthetic transactions (`txbuild.rs`: instruction tree, parsed transfers, double emission); `real_fixtures` re-verifies `fixtures/real/` read-only (hashes before/after).

V1.2 added:

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
- **No venue is mainnet-verified.** The V1.2.1 checks were validated on realistic synthetic transactions built to mirror the jsonParsed format, not on your 15 captured mainnet files, which are not in this repository. Run `./scripts/sniper check --capture 25` to re-verify them and capture more.
- **Raydium v4 is decode-only.** `ray_log` has no pool or mint ids.
- **Docker Compose was not exercised here** (no Docker daemon in the container). The same scripts were run against a local PostgreSQL 16.
- **Latency figures** come from this machine on fixture data. The provider-lag and queue-wait series only fill in LIVE mode.
