"""Paper book: cash, the one position, idempotent open/close, trades, marks, equity, stats.

One position at a time is enforced by the schema (position.id CHECK = 1). Opening and closing
are idempotent on their keys. The book is only released inside the same transaction that
records the simulated close. A close without a real exit price stays CLOSE_PENDING.
"""

from __future__ import annotations

import uuid
from decimal import Decimal

from .db import Database
from .store import Lease, LeaseLost
from .util import dec, jdump, jload, now


class PositionHeld(RuntimeError):
    pass


class InsufficientCash(RuntimeError):
    pass


def ensure_account(db: Database, starting_cash: float):
    t = now()
    db.x("INSERT OR IGNORE INTO account VALUES(1, ?, ?, ?, ?)",
         (str(Decimal(str(starting_cash))), str(Decimal(str(starting_cash))), t, t))


def account(db: Database) -> dict | None:
    r = db.q1("SELECT * FROM account WHERE id=1")
    return dict(r) if r else None


def position(db: Database) -> dict | None:
    r = db.q1("SELECT * FROM position WHERE id=1")
    if not r:
        return None
    d = dict(r)
    d["decision"] = jload(d.pop("decision_json"))
    return d


def open_position(db: Database, lease: Lease | None, key: str, cand: dict, fill: dict, decision: dict) -> dict:
    with db.tx() as c:
        if lease is not None and not lease.held_in(c):
            raise LeaseLost("lease lost before opening")
        o = c.execute("SELECT * FROM orders WHERE idempotency_key=?", (key,)).fetchone()
        if o:
            return {"duplicate": True, "position_uid": o["position_uid"]}
        if c.execute("SELECT 1 FROM position WHERE id=1").fetchone():
            raise PositionHeld("a position is already open")
        acct = c.execute("SELECT cash_usd FROM account WHERE id=1").fetchone()
        cash, ticket = Decimal(acct["cash_usd"]), Decimal(fill["ticket_usd"])
        if ticket > cash:
            raise InsufficientCash("ticket exceeds free cash")
        uid = "pos_" + uuid.uuid4().hex[:12]
        t = now()
        c.execute("""INSERT INTO position(id, position_uid, chain, pool_address, token_address, symbol,
                     qty, entry_price, quote_price, ticket_usd, entry_fee_usd, entry_slippage_bps,
                     opened_at, status, last_mark_price, last_mark_at, last_risk_at, decision_json)
                     VALUES(1,?,?,?,?,?,?,?,?,?,?,?,?, 'OPEN', ?, ?, ?, ?)""",
                  (uid, cand["chain"], cand["pool_address"], cand["token_address"], cand.get("symbol"),
                   fill["qty"], fill["fill_price"], fill["quote_price"], fill["ticket_usd"], fill["fee_usd"],
                   fill["slippage_bps"], t, fill["quote_price"], t, t, jdump(decision)))
        c.execute("UPDATE account SET cash_usd=?, updated_at=? WHERE id=1", (str(cash - ticket), t))
        c.execute("INSERT INTO orders VALUES(?, 'open', ?, ?, ?)", (key, uid, t, jdump(fill)))
    return {"duplicate": False, "position_uid": uid}


def request_close(db: Database, uid: str, reason: str):
    db.x("""UPDATE position SET status='CLOSE_PENDING', close_reason=COALESCE(close_reason, ?),
            close_requested_at=COALESCE(close_requested_at, ?) WHERE position_uid=?""", (reason, now(), uid))


def close_position(db: Database, lease: Lease | None, uid: str, exit_fill: dict, reason: str) -> dict:
    key = f"close:{uid}"
    with db.tx() as c:
        if lease is not None and not lease.held_in(c):
            raise LeaseLost("lease lost before closing")
        if c.execute("SELECT 1 FROM orders WHERE idempotency_key=?", (key,)).fetchone():
            return {"duplicate": True}
        p = c.execute("SELECT * FROM position WHERE id=1 AND position_uid=?", (uid,)).fetchone()
        if not p:
            raise RuntimeError("no such open position")
        t = now()
        proceeds = Decimal(exit_fill["proceeds_usd"])
        pnl = proceeds - Decimal(p["ticket_usd"])
        c.execute("""INSERT INTO trades(position_uid, chain, pool_address, token_address, symbol, opened_at,
                     closed_at, entry_price, exit_price, qty, ticket_usd, proceeds_usd, entry_fee_usd,
                     exit_fee_usd, entry_slippage_bps, exit_slippage_bps, pnl_usd, close_reason, decision_json)
                     VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                  (uid, p["chain"], p["pool_address"], p["token_address"], p["symbol"], p["opened_at"], t,
                   p["entry_price"], exit_fill["fill_price"], p["qty"], p["ticket_usd"], str(proceeds),
                   p["entry_fee_usd"], exit_fill["fee_usd"], p["entry_slippage_bps"], exit_fill["slippage_bps"],
                   str(pnl), p["close_reason"] or reason, p["decision_json"]))
        cash = Decimal(c.execute("SELECT cash_usd FROM account WHERE id=1").fetchone()["cash_usd"])
        c.execute("UPDATE account SET cash_usd=?, updated_at=? WHERE id=1", (str(cash + proceeds), t))
        c.execute("INSERT INTO orders VALUES(?, 'close', ?, ?, ?)", (key, uid, t, jdump(exit_fill)))
        c.execute("DELETE FROM position WHERE id=1 AND position_uid=?", (uid,))   # release, same tx
    return {"duplicate": False, "pnl_usd": str(pnl)}


def record_mark(db: Database, uid: str, snap: dict):
    db.x("INSERT INTO marks(position_uid, at, price_usd, volume_h6, volume_h24, source) VALUES(?,?,?,?,?,?)",
         (uid, snap.get("fetched_at") or now(), snap.get("price_usd"), snap.get("volume_h6"),
          snap.get("volume_h24"), snap.get("source")))
    db.x("UPDATE position SET last_mark_price=?, last_mark_at=?, mark_fail_count=0 WHERE position_uid=?",
         (snap.get("price_usd"), snap.get("fetched_at") or now(), uid))


def record_equity(db: Database):
    a = account(db)
    if not a:
        return
    cash = Decimal(a["cash_usd"])
    p = position(db)
    pv, basis = None, "cash"
    if p:
        mark = dec(p["last_mark_price"])
        if mark is None:
            return            # no honest value available; skip rather than invent
        pv = mark * Decimal(p["qty"])
        basis = "cash + position at last mark (before exit fees/slippage)"
    eq = cash + (pv or 0)
    db.x("INSERT INTO equity(at, equity_usd, cash_usd, position_value_usd, basis) VALUES(?,?,?,?,?)",
         (now(), str(eq.quantize(Decimal('0.01'))), str(cash.quantize(Decimal('0.01'))),
          str(pv.quantize(Decimal('0.01'))) if pv is not None else None, basis))


def trades(db: Database, limit: int = 200) -> list[dict]:
    out = []
    for r in db.q("SELECT * FROM trades ORDER BY closed_at DESC LIMIT ?", (limit,)):
        d = dict(r)
        d["decision"] = jload(d.pop("decision_json"))
        out.append(d)
    return out


def stats(db: Database) -> dict:
    rows = db.q("SELECT pnl_usd, entry_fee_usd, exit_fee_usd, opened_at, closed_at FROM trades")
    a = account(db)
    base = {"trades": len(rows), "wins": None, "losses": None, "win_rate": None, "total_pnl_usd": None,
            "avg_pnl_usd": None, "best_pnl_usd": None, "worst_pnl_usd": None, "total_fees_usd": None,
            "avg_hold_minutes": None,
            "basis": "net of simulated fees and modelled slippage; paper only; no guarantee",
            "cash_usd": a["cash_usd"] if a else None,
            "starting_cash_usd": a["starting_cash_usd"] if a else None}
    if not rows:
        return base
    pnl = [Decimal(r["pnl_usd"]) for r in rows]
    fees = sum(Decimal(r["entry_fee_usd"]) + Decimal(r["exit_fee_usd"]) for r in rows)
    wins = sum(1 for p in pnl if p > 0)
    base.update({
        "wins": wins, "losses": sum(1 for p in pnl if p < 0), "win_rate": round(wins / len(pnl), 4),
        "total_pnl_usd": str(sum(pnl)), "avg_pnl_usd": str((sum(pnl) / len(pnl)).quantize(Decimal("0.01"))),
        "best_pnl_usd": str(max(pnl)), "worst_pnl_usd": str(min(pnl)), "total_fees_usd": str(fees),
        "avg_hold_minutes": round(sum((r["closed_at"] - r["opened_at"]) for r in rows) / len(rows) / 60, 1),
    })
    return base
