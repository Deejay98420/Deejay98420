# Migration notes — v2.2 → v3.0.0

## The v2.2 source was not available

`pandarianz-pandabrain-desk-v2-2.zip` was not attached to the build session, so v3 could not be
built as a patch on that code, and its `desk.db` schema could not be inspected. v3 is a clean
implementation of the v2.2 behaviour as described in the brief.

Consequences:

1. **v3 does not open a v2.2 `desk.db`.** On first start, v3 creates a new book at
   `data/public/desk.db` (and `data/demo/desk.db` for demo). If it is pointed at a SQLite file that
   has tables but no PandaBrain v3 schema, it refuses to touch it (`database has unknown tables`).
   Your v2.2 book is never modified or deleted.
2. **Paper cash, the open paper position and the v2.2 event history are not carried over
   automatically.** If a v2.2 paper position is open, close it in v2.2 first (or note its details).
   v3 starts from `starting_cash_usd` (default $10,000, a setting).
3. An importer for v2.2 data can be written once the v2.2 schema is available: send the v2.2 ZIP
   or `sqlite3 desk.db .schema`. It would run as a v3 migration with a backup, preserving cash,
   positions and events.

## Behaviour changes vs v2.2 (by design)

| Area | v2.2 | v3 |
|---|---|---|
| Market cap | required before the dossier → 18/19 rejected `missing_mcap_usd` | pre-dossier checks no longer read mcap. After VET: provider market cap if valid, else `TOTAL_SUPPLY_VALUATION` (new policy, own limits) |
| `missing_mcap_usd` cooldowns | honoured | **retired reason**: ignored automatically, never deleted |
| Cooldowns | per reason | per reason + stage policy version + settings version (for threshold reasons) |
| Universe | latest fresh pools only | durable watchlist of identities, re-quoted as they mature |
| Judge | rules only | pluggable: RULES (default) or TYPED_AI (optional, no fallback) |
| Dashboard | CLI | local web desk with seats, floor, live stream, settings |

## v3 schema versioning

- Schema version lives in `meta.schema_version`. Book mode (`public`/`demo`) lives in `meta.mode`
  and is enforced on open.
- Migrations are lists of statements applied inside one `BEGIN IMMEDIATE` transaction per version.
- Before upgrading a non-empty book, a copy is written to `data/<mode>/backups/desk-<ts>-pre-vX-to-vY.db`
  using SQLite's online backup API.
- A book newer than the code is refused rather than downgraded.
- Settings are versioned rows (`settings_versions`). New keys introduced by a code upgrade take
  their defaults automatically. Changes apply at the next cycle boundary.

## Restart and interruption safety

- One position max (`position.id CHECK = 1`), idempotency keys in `orders`.
- Open and close are single transactions that also verify the engine lease.
- A close without a real exit price stays `CLOSE_PENDING`. The book is released only in the
  transaction that records the simulated close.
- A killed process leaves its lease to expire (90 s). The next worker takes over, and RISK resumes on
  the same exact pool.
