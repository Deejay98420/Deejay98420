"""SQLite book: connection, transactional migrations with backups, mode isolation.

Each data directory holds exactly one mode ("public" or "demo"). Opening a public book in
demo mode, or the reverse, is refused so synthetic data can never land in the public ledger.
"""

from __future__ import annotations

import os
import shutil
import sqlite3
import threading
from contextlib import contextmanager

from .util import iso, now

SCHEMA_VERSION = 1

# Each migration is a list of single statements, applied inside one transaction.
MIGRATIONS: dict[int, list[str]] = {
    1: [
        """CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)""",
        """CREATE TABLE settings_versions(
             version INTEGER PRIMARY KEY, created_at REAL NOT NULL,
             settings_json TEXT NOT NULL, note TEXT)""",
        """CREATE TABLE control(key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL)""",
        """CREATE TABLE lease(name TEXT PRIMARY KEY, owner TEXT NOT NULL,
             acquired_at REAL NOT NULL, expires_at REAL NOT NULL)""",
        """CREATE TABLE provider_status(name TEXT PRIMARY KEY, state TEXT NOT NULL,
             last_ok_at REAL, last_error_at REAL, last_error TEXT, detail TEXT)""",
        """CREATE TABLE watchlist(
             chain TEXT NOT NULL, pool_address TEXT NOT NULL, token_address TEXT NOT NULL,
             symbol TEXT, name TEXT, dex TEXT, pool_created_at REAL,
             first_seen_at REAL NOT NULL, last_seen_at REAL NOT NULL,
             last_checked_at REAL, next_check_at REAL NOT NULL,
             status TEXT NOT NULL DEFAULT 'active', last_stage TEXT, last_reason TEXT,
             checks INTEGER NOT NULL DEFAULT 0, last_snapshot_id INTEGER,
             PRIMARY KEY(chain, pool_address))""",
        """CREATE INDEX watchlist_due ON watchlist(status, next_check_at)""",
        """CREATE TABLE snapshots(
             id INTEGER PRIMARY KEY AUTOINCREMENT, chain TEXT NOT NULL,
             pool_address TEXT NOT NULL, token_address TEXT NOT NULL, source TEXT NOT NULL,
             fetched_at REAL NOT NULL, provider_ts REAL, price_usd TEXT,
             liquidity_usd REAL, volume_h1 REAL, volume_h6 REAL, volume_h24 REAL,
             buys_h1 INTEGER, sells_h1 INTEGER, buys_h24 INTEGER, sells_h24 INTEGER,
             mcap_usd TEXT, mcap_source TEXT, fdv_usd TEXT, pool_created_at REAL,
             data_json TEXT)""",
        """CREATE INDEX snapshots_pool ON snapshots(chain, pool_address, fetched_at)""",
        """CREATE TABLE cycles(
             id INTEGER PRIMARY KEY AUTOINCREMENT, started_at REAL NOT NULL, ended_at REAL,
             kind TEXT NOT NULL, outcome TEXT, reason TEXT, settings_version INTEGER,
             engine TEXT, stats_json TEXT)""",
        """CREATE TABLE evaluations(
             id INTEGER PRIMARY KEY AUTOINCREMENT, cycle_id INTEGER, at REAL NOT NULL,
             chain TEXT NOT NULL, pool_address TEXT NOT NULL, token_address TEXT,
             candidate_id TEXT NOT NULL, stage TEXT NOT NULL, outcome TEXT NOT NULL,
             reason TEXT, policy TEXT, settings_version INTEGER, data_json TEXT)""",
        """CREATE INDEX evaluations_pool ON evaluations(chain, pool_address, at)""",
        """CREATE TABLE dossiers(
             id INTEGER PRIMARY KEY AUTOINCREMENT, cycle_id INTEGER, at REAL NOT NULL,
             chain TEXT NOT NULL, pool_address TEXT NOT NULL, candidate_id TEXT NOT NULL,
             data_json TEXT NOT NULL)""",
        """CREATE TABLE judgments(
             id INTEGER PRIMARY KEY AUTOINCREMENT, cycle_id INTEGER, at REAL NOT NULL,
             candidate_id TEXT, engine TEXT NOT NULL, model_id TEXT, question_set TEXT,
             score_type TEXT NOT NULL, threshold_version TEXT, settings_version INTEGER,
             raw_json TEXT, result_json TEXT)""",
        """CREATE TABLE bench(
             chain TEXT NOT NULL, pool_address TEXT NOT NULL, reason TEXT NOT NULL,
             policy TEXT NOT NULL, settings_version INTEGER, created_at REAL NOT NULL,
             until REAL NOT NULL, PRIMARY KEY(chain, pool_address))""",
        """CREATE TABLE account(id INTEGER PRIMARY KEY CHECK(id = 1), cash_usd TEXT NOT NULL,
             starting_cash_usd TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL)""",
        """CREATE TABLE position(
             id INTEGER PRIMARY KEY CHECK(id = 1), position_uid TEXT NOT NULL UNIQUE,
             chain TEXT NOT NULL, pool_address TEXT NOT NULL, token_address TEXT NOT NULL,
             symbol TEXT, qty TEXT NOT NULL, entry_price TEXT NOT NULL, quote_price TEXT NOT NULL,
             ticket_usd TEXT NOT NULL, entry_fee_usd TEXT NOT NULL, entry_slippage_bps REAL,
             opened_at REAL NOT NULL, status TEXT NOT NULL, close_reason TEXT,
             close_requested_at REAL, last_mark_price TEXT, last_mark_at REAL,
             mark_fail_count INTEGER NOT NULL DEFAULT 0, last_risk_at REAL,
             decision_json TEXT)""",
        """CREATE TABLE orders(
             idempotency_key TEXT PRIMARY KEY, kind TEXT NOT NULL, position_uid TEXT NOT NULL,
             created_at REAL NOT NULL, result_json TEXT)""",
        """CREATE TABLE trades(
             id INTEGER PRIMARY KEY AUTOINCREMENT, position_uid TEXT NOT NULL UNIQUE,
             chain TEXT, pool_address TEXT, token_address TEXT, symbol TEXT,
             opened_at REAL, closed_at REAL, entry_price TEXT, exit_price TEXT,
             qty TEXT, ticket_usd TEXT, proceeds_usd TEXT, entry_fee_usd TEXT,
             exit_fee_usd TEXT, entry_slippage_bps REAL, exit_slippage_bps REAL,
             pnl_usd TEXT, close_reason TEXT, decision_json TEXT)""",
        """CREATE TABLE marks(
             id INTEGER PRIMARY KEY AUTOINCREMENT, position_uid TEXT NOT NULL, at REAL NOT NULL,
             price_usd TEXT, volume_h6 REAL, volume_h24 REAL, source TEXT)""",
        """CREATE TABLE equity(
             id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL, equity_usd TEXT NOT NULL,
             cash_usd TEXT NOT NULL, position_value_usd TEXT, basis TEXT NOT NULL)""",
        """CREATE TABLE events(
             id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL, seat TEXT NOT NULL,
             level TEXT NOT NULL, kind TEXT NOT NULL, message TEXT NOT NULL, data_json TEXT)""",
        """CREATE INDEX events_at ON events(at)""",
        """CREATE TABLE seats(
             seat TEXT PRIMARY KEY, state TEXT NOT NULL, action TEXT, count INTEGER,
             metric REAL, last_at REAL, detail_json TEXT)""",
    ],
}


class ModeMismatch(RuntimeError):
    pass


class Database:
    """Thread-local connections onto one SQLite file."""

    def __init__(self, path: str, mode: str):
        if mode not in ("public", "demo"):
            raise ValueError("mode must be public or demo")
        self.path = path
        self.mode = mode
        self._local = threading.local()
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)

    def conn(self) -> sqlite3.Connection:
        c = getattr(self._local, "c", None)
        if c is None:
            c = sqlite3.connect(self.path, timeout=10, isolation_level=None,
                                check_same_thread=False)
            c.row_factory = sqlite3.Row
            c.execute("PRAGMA journal_mode=WAL")
            c.execute("PRAGMA busy_timeout=10000")
            c.execute("PRAGMA foreign_keys=ON")
            self._local.c = c
        return c

    def close(self):
        c = getattr(self._local, "c", None)
        if c is not None:
            c.close()
            self._local.c = None

    @contextmanager
    def tx(self):
        """BEGIN IMMEDIATE ... COMMIT, rolled back on any exception."""
        c = self.conn()
        c.execute("BEGIN IMMEDIATE")
        try:
            yield c
        except BaseException:
            c.execute("ROLLBACK")
            raise
        else:
            c.execute("COMMIT")

    def q(self, sql, args=()):
        return self.conn().execute(sql, args).fetchall()

    def q1(self, sql, args=()):
        return self.conn().execute(sql, args).fetchone()

    def x(self, sql, args=()):
        return self.conn().execute(sql, args)

    # ------------------------------------------------------------------ migration
    def current_version(self) -> int:
        c = self.conn()
        has = c.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='meta'").fetchone()
        if not has:
            return 0
        r = c.execute("SELECT value FROM meta WHERE key='schema_version'").fetchone()
        return int(r[0]) if r else 0

    def migrate(self, backup_dir: str | None = None) -> dict:
        """Apply pending migrations transactionally. Back up a non-empty book first."""
        c = self.conn()
        start = self.current_version()
        info = {"from": start, "to": start, "backup": None}
        if start > SCHEMA_VERSION:
            raise RuntimeError(f"book schema v{start} is newer than this code (v{SCHEMA_VERSION})")
        if start == 0:
            other = c.execute("SELECT count(*) FROM sqlite_master WHERE type='table'").fetchone()[0]
            if other:
                raise RuntimeError("database has unknown tables and no PandaBrain schema; "
                                   "refusing to touch it")
        if start and start < SCHEMA_VERSION:
            info["backup"] = self.backup(backup_dir, f"pre-v{start}-to-v{SCHEMA_VERSION}")
        for v in range(start + 1, SCHEMA_VERSION + 1):
            with self.tx() as t:
                for stmt in MIGRATIONS[v]:
                    t.execute(stmt)
                t.execute("INSERT OR REPLACE INTO meta VALUES('schema_version', ?)", (str(v),))
                if v == 1:
                    t.execute("INSERT INTO meta VALUES('mode', ?)", (self.mode,))
                    t.execute("INSERT INTO meta VALUES('created_at', ?)", (iso(now()),))
            info["to"] = v
        r = c.execute("SELECT value FROM meta WHERE key='mode'").fetchone()
        if not r or r[0] != self.mode:
            raise ModeMismatch(f"book at {self.path} is mode={r[0] if r else '?'}; "
                               f"refusing to open it as mode={self.mode}")
        return info

    def backup(self, backup_dir: str | None, label: str) -> str:
        d = backup_dir or os.path.join(os.path.dirname(os.path.abspath(self.path)), "backups")
        os.makedirs(d, exist_ok=True)
        dest = os.path.join(d, f"desk-{int(now())}-{label}.db")
        src = self.conn()
        out = sqlite3.connect(dest)
        try:
            src.backup(out)
        finally:
            out.close()
        return dest


def open_book(data_dir: str, mode: str) -> Database:
    db = Database(os.path.join(data_dir, "desk.db"), mode)
    db.migrate()
    return db


def remove_book(data_dir: str):  # only used by tests on temporary dirs
    shutil.rmtree(data_dir, ignore_errors=True)
