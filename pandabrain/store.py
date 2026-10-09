"""Desk state stored in the book: settings versions, controls, leases, events, seats, providers."""

from __future__ import annotations

import os
import uuid

from . import config
from .db import Database
from .util import jdump, jload, now, sanitize

SEATS = ("CHIEF", "SCAN", "VET", "SOCIAL", "SIZE", "FILLS", "RISK", "WATCH")
SEAT_STATES = {"idle", "running", "blocked", "error", "holding", "unconfigured", "stopped"}


# ------------------------------------------------------------------- settings
def active_settings(db: Database) -> tuple[int, dict]:
    r = db.q1("SELECT version, settings_json FROM settings_versions ORDER BY version DESC LIMIT 1")
    if not r:
        s = config.defaults()
        db.x("INSERT INTO settings_versions VALUES(1, ?, ?, ?)", (now(), jdump(s), "defaults"))
        return 1, s
    # merge over defaults so new keys added by a code upgrade get their default
    return r["version"], config.validate(jload(r["settings_json"], {}), config.defaults())


def save_settings(db: Database, patch: dict, note: str | None, expected_version: int | None) -> int:
    with db.tx() as t:
        r = t.execute("SELECT version, settings_json FROM settings_versions "
                      "ORDER BY version DESC LIMIT 1").fetchone()
        cur_v = r["version"] if r else 0
        if expected_version is not None and expected_version != cur_v:
            raise config.SettingsError({"_": f"settings changed (now v{cur_v}); reload and retry"})
        base = config.validate(jload(r["settings_json"], {}), config.defaults()) if r else config.defaults()
        merged = config.validate(patch, base)
        v = cur_v + 1
        t.execute("INSERT INTO settings_versions VALUES(?, ?, ?, ?)",
                  (v, now(), jdump(merged), (note or "")[:200]))
    return v


# ------------------------------------------------------------------- controls
def get_control(db: Database, key: str, default: str | None = None) -> str | None:
    r = db.q1("SELECT value FROM control WHERE key=?", (key,))
    return r["value"] if r else default


def set_control(db: Database, key: str, value: str):
    db.x("INSERT OR REPLACE INTO control VALUES(?, ?, ?)", (key, value, now()))


# ------------------------------------------------------------------- leases
class LeaseLost(RuntimeError):
    pass


class Lease:
    """A database lease so two workers (or a CLI run and the server) never overlap.

    renew() is called between every expensive step; losing the lease aborts the cycle before
    any book mutation.
    """

    def __init__(self, db: Database, name: str = "engine", ttl: float = 90.0, owner: str | None = None):
        self.db, self.name, self.ttl = db, name, ttl
        self.owner = owner or f"{os.getpid()}:{uuid.uuid4().hex[:8]}"
        self._renewed = 0.0

    def acquire(self) -> bool:
        t = now()
        with self.db.tx() as c:
            r = c.execute("SELECT owner, expires_at FROM lease WHERE name=?", (self.name,)).fetchone()
            if r and r["owner"] != self.owner and r["expires_at"] > t:
                return False
            c.execute("INSERT OR REPLACE INTO lease VALUES(?, ?, ?, ?)",
                      (self.name, self.owner, t, t + self.ttl))
        self._renewed = t
        return True

    def renew(self, force: bool = False):
        t = now()
        if not force and t - self._renewed < self.ttl / 6:
            return
        with self.db.tx() as c:
            r = c.execute("SELECT owner FROM lease WHERE name=?", (self.name,)).fetchone()
            if not r or r["owner"] != self.owner:
                raise LeaseLost(f"lease {self.name} is held by another worker")
            c.execute("UPDATE lease SET expires_at=? WHERE name=?", (t + self.ttl, self.name))
        self._renewed = t

    def held_in(self, c) -> bool:
        r = c.execute("SELECT owner, expires_at FROM lease WHERE name=?", (self.name,)).fetchone()
        return bool(r and r["owner"] == self.owner and r["expires_at"] > now())

    def release(self):
        self.db.x("DELETE FROM lease WHERE name=? AND owner=?", (self.name, self.owner))


def lease_info(db: Database, name: str = "engine") -> dict | None:
    r = db.q1("SELECT * FROM lease WHERE name=?", (name,))
    if not r:
        return None
    return {"owner": r["owner"], "acquired_at": r["acquired_at"], "expires_at": r["expires_at"],
            "live": r["expires_at"] > now()}


# ------------------------------------------------------------------- events & seats
def emit(db: Database, seat: str, kind: str, message: str, level: str = "info", data=None) -> int:
    cur = db.x("INSERT INTO events(at, seat, level, kind, message, data_json) VALUES(?,?,?,?,?,?)",
               (now(), seat, level, kind, sanitize(message), jdump(data) if data is not None else None))
    return cur.lastrowid


def set_seat(db: Database, seat: str, state: str, action: str | None = None,
             count: int | None = None, metric: float | None = None, detail=None):
    assert seat in SEATS and state in SEAT_STATES, (seat, state)
    db.x("INSERT OR REPLACE INTO seats VALUES(?,?,?,?,?,?,?)",
         (seat, state, sanitize(action) if action else None, count, metric, now(),
          jdump(detail) if detail is not None else None))


def seats(db: Database) -> dict:
    out = {s: {"seat": s, "state": "idle", "action": None, "count": None, "metric": None,
               "last_at": None, "detail": None} for s in SEATS}
    for r in db.q("SELECT * FROM seats"):
        out[r["seat"]] = {"seat": r["seat"], "state": r["state"], "action": r["action"],
                          "count": r["count"], "metric": r["metric"], "last_at": r["last_at"],
                          "detail": jload(r["detail_json"])}
    return out


# ------------------------------------------------------------------- providers
def provider_ok(db: Database, name: str, detail: str | None = None):
    db.x("""INSERT INTO provider_status(name, state, last_ok_at, detail) VALUES(?, 'ok', ?, ?)
            ON CONFLICT(name) DO UPDATE SET state='ok', last_ok_at=excluded.last_ok_at,
            detail=excluded.detail""", (name, now(), detail))


def provider_err(db: Database, name: str, kind: str, err):
    db.x("""INSERT INTO provider_status(name, state, last_error_at, last_error) VALUES(?, ?, ?, ?)
            ON CONFLICT(name) DO UPDATE SET state=excluded.state,
            last_error_at=excluded.last_error_at, last_error=excluded.last_error""",
         (name, kind, now(), sanitize(err)))


def providers(db: Database) -> list[dict]:
    return [dict(r) for r in db.q("SELECT * FROM provider_status ORDER BY name")]
