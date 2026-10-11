# Architecture

## Layout

```
core/                       Rust crate `sniper-core` + `sniper` CLI
  src/types.rs              Pubkey (identifier only), DataMode, Commitment, Venue, program ids
  src/time.rs               wall vs event clock, structured pipeline Stamps
  src/stream/               RawEvent/StreamMsg, Ingestor (dedup + bounded queue), backoff, fixture source, WS source
  src/decode/               borsh reader/writer, Anchor framing, pumpfun / pumpswap / raydium_v4, log invoke-stack parser
  src/state/                TokenState + Universe (pool→mint, slot index, rollback), analytics windows/features
  src/quote/                exact swap math, venue-local quotes (own-impact aware), Jupiter client, token bucket
  src/strategy/             entry evaluation, sizing, rejection reasons; dynamic exit signals
  src/paper/                Book, Order, Position, ledger, fill simulation, cost breakdowns
  src/engine.rs             single-owner event loop; fills, exits, rollback, aggregator results, snapshot
  src/store/                StoreCmd channel → writer task; PostgreSQL + in-memory backends; recovery
  src/api.rs                read-only GET API for the dashboard
  src/fixtures.rs           deterministic synthetic scenario generator
migrations/001_init.sql     schema + append-only triggers
dashboard/                  Next.js 14 app (proxy /engine/* → engine API)
scripts/sniper              start/stop/status/test/report/replay workflow
scripts/check_no_live.sh    static PAPER-safety check
config/strategy.toml        optional overrides (changes strategy_version)
fixtures/                   generated synthetic JSONL
```

## Event flow and ordering guarantees

1. Sources push `StreamMsg` into the `Ingestor`. The first copy of a signature wins. A redelivery at a higher commitment is forwarded as an upgrade but not re-applied. The queue is bounded: producers wait (counted as `backpressure_waits`) and nothing is dropped.
2. The `Engine` is the only owner of mutable state. For each event:
   - **settle due orders first**, against the state *before* this event, i.e. the state at their fill time;
   - journal the raw event;
   - decode and apply it to the token state;
   - re-check open positions for that mint (exits take priority);
   - run a throttled entry evaluation;
   - run time-based exit checks for every live position.
3. Async work (Jupiter quotes) runs in spawned tasks. Results come back as `Control::Jupiter` messages. Stale results are rejected by a request-id plus attempt-count race guard.
4. Store writes go through a bounded channel to a single writer, so their order is kept. All commands for one fill (ledger entries, position snapshot, position event, order completion) go in one `StoreCmd::Batch`, which is a single DB transaction.

## Restart recovery

On start with an existing profile:

- Books are rebuilt from `start_balance + Σ ledger`, with the last sequence number.
- Live positions are restored. An `Exiting` position retries promptly because its pending exit order was lost.
- Order submissions without a completion are recorded as `abandoned_restart` and are never filled. Balance was never debited for them, because debits happen at fill time.
- Idempotency keys are reloaded. Journaled signatures seed both dedup layers.
- Token state is warmed up by replaying the journal (all of it for fixture/replay profiles, the last 30 minutes for live), including dead-slot markers.

## Clocks

- **LIVE:** wall clock. Simulated landing latency is added to real processing time, and a 250 ms tick drives fills and exits between events.
- **FIXTURE / REPLAY:** event clock (provider receive stamps). Runs are deterministic, and the simulation seed does not depend on the profile name.
