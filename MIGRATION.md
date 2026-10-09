# Migration notes

## v3.0.0 → v3.0.1

- **No schema change** (book schema v1). Copy your `data/` folder into the new release (see the
  README upgrade steps). `./scripts/stop.sh` now waits for the server to exit, so the copy is safe.
- **Stricter mint verification.** Candidates are now rejected for raw Token-2022 problems the
  v3.0.0 parser missed. A paper position opened under v3.0.0 is not re-vetted. RISK keeps
  monitoring and closes it normally.
- **Settings.** `allowed_token2022_extensions` can only narrow `{metadataPointer, tokenMetadata}`.
  If an older settings row lists other names, they are dropped on load (they never had any effect
  under the metadata-only policy). Saving a new version with them is refused.
- Existing cooldowns stay valid: 3.0.1 only rejects more, never less, so no v3.0.0 rejection can
  hide a token that 3.0.1 would accept.

## v2.2 → v3: what the actual v2.2 source shows

Reviewed against `pandarianz-pandabrain-desk-v2-2.zip` (folder `pandarianz-desk/`). Its own 52
tests pass unchanged.

| v2.2 fact | Source | Consequence for v3 |
|---|---|---|
| Public ledger is **`pandarianz.db`**; demo is **`pandarianz-demo.db`**, created in the folder `main.py` runs from (or `--db PATH`) | `main.py` | v3 never opens these as its own book. `migrate-preview --from` reads them **read-only** |
| `metadata(engine, feed)`; engine `PANDARIANZ_BRAIN_RULES_2.0.0`; feed `PUBLIC_MARKET_DATA` or `DEMO_SYNTHETIC` | `book.bind` | feed decides the target v3 book (public / demo). A ledger with events but no metadata is refused, as v2.2 itself does |
| `account.cash` is a REAL (float) | `book.py` | converted via `Decimal(repr(float))`, no rounding |
| open: cash −= `ticket + entry_fee`; close: cash += proceeds; `net_pnl = proceeds − ticket − entry_fee` | `book.open`, `risk.monitor` | v3 `ticket_usd = ticket + entry_fee`; v3 `pnl = proceeds − ticket_usd` equals v2.2 `net_pnl` exactly |
| one `position(id=1, payload JSON)`; payload has `token.pair` (exact pool), `token.addr`, `units`, `entry_price`, `opened_at`, `missing_checks`, `close_required` | `risk.paper_open` | maps to the v3 position on the **same exact pool**. `close_required` maps to `CLOSE_PENDING` (closed only at a real quote, as in v2.2) |
| no trades table: closes exist only as `paper_close` events, without pool/units/opened_at | `book.close` | trades are rebuilt by pairing each `paper_close` with the preceding `paper_open` of the same `tid`. Unpaired closes stay events only |
| `events(id, at, kind, payload)` with kinds `cycle`, `brain_evaluation`, `brain_selection`, `rejected`, `collector_failure`, `paper_open`, `paper_close`, `paper_mark`, `risk_data_unavailable`, `worker_failure` | `main.py`, `risk.py` | every row is kept verbatim with its original timestamp (kind prefixed `v22_`) |
| `bench(tid, reason, until)` per token, v2.2 reasons | `book.reject` | **not imported**: v3 re-verifies under its pool-level policy |
| `lease(id=1, owner, until)` | `book.acquire` | a live lease is a blocker: stop the v2.2 worker first |
| Solana mint reads at `finalized`, metadata-only Token-2022 with raw TLV/parsed agreement | `collect.py`, `mint_controls.py` | **restored in 3.0.1** (v3.0.0 had dropped the raw TLV check and used `confirmed`) |

### Read-only preview (available now)

```bash
python3 -m pandabrain migrate-preview --from "/path/to/pandarianz.db"          # summary
python3 -m pandabrain migrate-preview --from "/path/to/pandarianz.db" --json   # full report
```

Guarantees, all covered by tests:
- the source `pandarianz.db` (and any `-wal` / `-shm`) is hashed before and after the run. Only a
  temporary copy is opened, with `query_only`. The report includes `source_unchanged: true`.
  Un-checkpointed WAL content is included via the copy;
- no v3 book or data directory is created;
- cash is reconciled against v2.2's own accounting: starting bank (default 10,000, or
  `--starting-cash`) + closed net P&L − open position cost. A mismatch is reported, never "fixed";
- blockers: missing or unknown metadata/feed, schema mismatch, live v2.2 lease, invalid cash,
  unparseable or incomplete open position, or a target v3 book that already has trading history.

The preview was run on ledgers produced by the actual v2.2 code (synthetic demo tokens, public and
demo feeds). Cash reconciled to the cent and the closed trade's P&L matched v2.2's `net_pnl`
exactly. **It has not been run on your real `pandarianz.db`.** Please run it on your Mac and review
the output.

### Importer (not built yet)

After you've reviewed a preview, the importer would write into a **fresh** v3 book only (never
merging), as one transaction with a backup, preserving cash, the open position (same pool), closed
trades and every event. Until then, v2.2 and v3 ledgers stay completely separate.

## v3 schema versioning (unchanged)

- Schema version lives in `meta.schema_version`; book mode (`public`/`demo`) in `meta.mode`, which
  is enforced on open.
- Migrations are applied inside one `BEGIN IMMEDIATE` transaction per version, with a backup of a
  non-empty book first (`data/<mode>/backups/`). A book newer than the code is refused.
- One position max (`position.id CHECK = 1`), idempotency keys in `orders`, and the book is
  released only in the transaction that records the simulated close.
