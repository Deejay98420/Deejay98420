"""Durable watchlist of discovered (chain, pool, token) identities.

Discovery is not limited to the freshest pools: each identity is remembered and re-quoted as
it matures (too_young entries wake exactly when they reach min_age). No history is invented:
only snapshots actually fetched are stored.
"""

from __future__ import annotations

from . import bench
from .db import Database
from .util import jdump, now

SNAP_COLS = ("chain", "pool_address", "token_address", "source", "fetched_at", "provider_ts",
             "price_usd", "liquidity_usd", "volume_h1", "volume_h6", "volume_h24", "buys_h1",
             "sells_h1", "buys_h24", "sells_h24", "mcap_usd", "mcap_source", "fdv_usd",
             "pool_created_at")


def save_snapshot(db: Database, s: dict) -> int:
    cur = db.x(f"INSERT INTO snapshots({','.join(SNAP_COLS)}, data_json) "
               f"VALUES({','.join('?' * len(SNAP_COLS))}, ?)",
               tuple(s.get(c) for c in SNAP_COLS) + (jdump(s),))
    return cur.lastrowid


def upsert_discovered(db: Database, s: dict, snapshot_id: int) -> bool:
    """Insert or refresh an identity. Returns True when new. Pool identity is immutable:
    a different token under the same pool address is refused."""
    t = now()
    r = db.q1("SELECT token_address FROM watchlist WHERE chain=? AND pool_address=?",
              (s["chain"], s["pool_address"]))
    if r:
        if r["token_address"] != s["token_address"]:
            return False
        db.x("""UPDATE watchlist SET last_seen_at=?, last_snapshot_id=?,
                symbol=COALESCE(symbol, ?), name=COALESCE(name, ?) WHERE chain=? AND pool_address=?""",
             (t, snapshot_id, s.get("symbol"), s.get("name"), s["chain"], s["pool_address"]))
        return False
    db.x("""INSERT INTO watchlist(chain, pool_address, token_address, symbol, name, dex,
            pool_created_at, first_seen_at, last_seen_at, next_check_at, status, last_snapshot_id)
            VALUES(?,?,?,?,?,?,?,?,?,?, 'active', ?)""",
         (s["chain"], s["pool_address"], s["token_address"], s.get("symbol"), s.get("name"),
          s.get("dex"), s.get("pool_created_at"), t, t, t, snapshot_id))
    return True


def due(db: Database, limit: int, settings_version: int) -> list[dict]:
    rows = db.q("""SELECT * FROM watchlist WHERE status='active' AND next_check_at <= ?
                   ORDER BY next_check_at ASC LIMIT ?""", (now(), limit * 4))
    out = []
    for r in rows:
        if bench.active(db, r["chain"], r["pool_address"], settings_version):
            continue
        out.append(dict(r))
        if len(out) >= limit:
            break
    return out


def record_check(db: Database, chain: str, pool: str, stage: str, reason: str | None,
                 st: dict, pool_created_at: float | None, snapshot_id: int | None = None):
    t = now()
    status = "active"
    if reason == "too_young" and pool_created_at:
        nxt = pool_created_at + st["min_age_minutes"] * 60 + 30
    elif reason in ("too_old",):
        status, nxt = "retired", t
    elif reason in ("evm_unverified", "chain_unsupported", "mint_authority_open",
                    "freeze_authority_open", "unsupported_extension", "unsupported_program",
                    "not_a_mint"):
        status, nxt = "rejected", t
    elif reason:
        nxt = t + max(bench.MINUTES.get(reason, bench.DEFAULT_MINUTES), 5) * 60
        nxt = min(nxt, t + st["recheck_interval_minutes"] * 60 * 6)
    elif stage == "judge":
        # survived every gate but was not picked (or the book was busy): look again soon
        nxt = t + min(300, st["recheck_interval_minutes"] * 60)
    else:
        nxt = t + st["recheck_interval_minutes"] * 60
    db.x("""UPDATE watchlist SET last_checked_at=?, next_check_at=?, status=?, last_stage=?,
            last_reason=?, checks=checks+1, last_snapshot_id=COALESCE(?, last_snapshot_id)
            WHERE chain=? AND pool_address=?""",
         (t, nxt, status, stage, reason, snapshot_id, chain, pool))


def enforce_cap(db: Database, cap: int) -> int:
    """Retire the oldest active entries beyond the cap. Never deletes rows."""
    n = db.q1("SELECT count(*) AS n FROM watchlist WHERE status='active'")["n"]
    if n <= cap:
        return 0
    extra = n - cap
    db.x("""UPDATE watchlist SET status='retired', last_reason='watchlist_cap'
            WHERE rowid IN (SELECT rowid FROM watchlist WHERE status='active'
                            ORDER BY first_seen_at ASC LIMIT ?)""", (extra,))
    return extra


def counts(db: Database) -> dict:
    return {r["status"]: r["n"] for r in db.q("SELECT status, count(*) AS n FROM watchlist GROUP BY status")}
