"""Read models for the dashboard. Every number comes from the book; nothing is synthesised."""

from __future__ import annotations

import csv
import io
from decimal import Decimal

from . import EXECUTION_MODE, __version__, bench, book, config, integrations, store, watchlist
from .util import dec, jload, now

SORTABLE = {"first_seen_at", "last_checked_at", "pool_created_at", "symbol", "liquidity_usd", "volume_h24",
            "price_usd", "last_reason", "status", "checks"}


def _latest_valuations(db, pools: list[tuple]) -> dict:
    out = {}
    for chain, pool in pools:
        r = db.q1("SELECT data_json FROM dossiers WHERE chain=? AND pool_address=? ORDER BY id DESC LIMIT 1",
                  (chain, pool))
        if r:
            d = jload(r["data_json"], {})
            out[(chain, pool)] = d
    return out


def gaps_for(dossier: dict | None) -> list[str]:
    if not dossier:
        return ["not_vetted"]
    g = []
    if dossier.get("top_10_percent") is None:
        g.append("distribution")
    if dossier.get("holder_count") is None:
        g.append("holders")
    if dossier.get("largest_account_share") is None:
        g.append("largest_account")
    soc = (dossier.get("social") or {}).get("status")
    if soc != "read":
        g.append("social")
    if not dossier.get("valuation"):
        g.append("valuation")
    return g


def watchlist_rows(db, sort="last_checked_at", direction="desc", q="", status="", limit=200) -> list[dict]:
    if sort not in SORTABLE:
        sort = "last_checked_at"
    direction = "ASC" if str(direction).lower() == "asc" else "DESC"
    limit = max(1, min(int(limit), 1000))
    where, args = [], []
    if status in ("active", "retired", "rejected"):
        where.append("w.status=?")
        args.append(status)
    if q:
        where.append("(w.symbol LIKE ? OR w.name LIKE ? OR w.token_address LIKE ? OR w.pool_address LIKE ?)")
        args += [f"%{q[:64]}%"] * 4
    col = {"liquidity_usd": "s.liquidity_usd", "volume_h24": "s.volume_h24",
           "price_usd": "CAST(s.price_usd AS REAL)"}.get(sort, f"w.{sort}")
    sql = f"""SELECT w.*, s.price_usd, s.liquidity_usd, s.volume_h24, s.buys_h1, s.sells_h1, s.buys_h24,
              s.sells_h24, s.mcap_usd, s.mcap_source, s.source AS quote_source, s.fetched_at
              FROM watchlist w LEFT JOIN snapshots s ON s.id = w.last_snapshot_id
              {'WHERE ' + ' AND '.join(where) if where else ''}
              ORDER BY {col} {direction} NULLS LAST LIMIT ?"""
    rows = [dict(r) for r in db.q(sql, (*args, limit))]
    dossiers = _latest_valuations(db, [(r["chain"], r["pool_address"]) for r in rows])
    t = now()
    for r in rows:
        d = dossiers.get((r["chain"], r["pool_address"]))
        r["age_minutes"] = round((t - r["pool_created_at"]) / 60, 1) if r["pool_created_at"] else None
        val = (d or {}).get("valuation")
        r["valuation_type"] = val["type"] if val else ("PROVIDER_MARKET_CAP (unvetted)" if r["mcap_usd"] else None)
        r["valuation_usd"] = val["value_usd"] if val else r["mcap_usd"]
        r["evidence_gaps"] = gaps_for(d)
        r["quote_age_s"] = round(t - r["fetched_at"]) if r["fetched_at"] else None
    return rows


def candidate_detail(db, chain: str, pool: str) -> dict | None:
    w = db.q1("SELECT * FROM watchlist WHERE chain=? AND pool_address=?", (chain, pool))
    if not w:
        return None
    st_ver, _st = store.active_settings(db)
    snaps = [dict(r) for r in db.q("""SELECT id, source, fetched_at, provider_ts, price_usd, liquidity_usd, volume_h1,
              volume_h6, volume_h24, buys_h1, sells_h1, buys_h24, sells_h24, mcap_usd, mcap_source, fdv_usd
              FROM snapshots WHERE chain=? AND pool_address=? ORDER BY id DESC LIMIT 20""", (chain, pool))]
    evals = [dict(r) | {"data": jload(r["data_json"])} for r in db.q(
        "SELECT * FROM evaluations WHERE chain=? AND pool_address=? ORDER BY id DESC LIMIT 30", (chain, pool))]
    for e in evals:
        e.pop("data_json", None)
    dz = db.q1("SELECT * FROM dossiers WHERE chain=? AND pool_address=? ORDER BY id DESC LIMIT 1", (chain, pool))
    dossier = jload(dz["data_json"]) if dz else None
    if dossier:
        dossier.pop("snapshot", None)
    cid = evals[0]["candidate_id"] if evals else None
    judg = [dict(r) | {"raw": jload(r["raw_json"]), "result": jload(r["result_json"])} for r in db.q(
        "SELECT * FROM judgments WHERE candidate_id=? ORDER BY id DESC LIMIT 10", (cid,))] if cid else []
    for j in judg:
        j.pop("raw_json", None)
        j.pop("result_json", None)
    b = db.q1("SELECT * FROM bench WHERE chain=? AND pool_address=?", (chain, pool))
    bd = dict(b) if b else None
    if bd:
        bd["valid"] = bench.is_valid(bd, st_ver)
    return {"watch": dict(w), "snapshots": snaps, "evaluations": evals, "dossier": dossier,
            "judgments": judg, "bench": bd, "evidence_gaps": gaps_for(dossier)}


def events(db, q="", seat="", level="", after_id=0, limit=200) -> list[dict]:
    where, args = ["id > ?"], [int(after_id)]
    if q:
        where.append("(message LIKE ? OR kind LIKE ?)")
        args += [f"%{q[:80]}%"] * 2
    if seat in store.SEATS:
        where.append("seat=?")
        args.append(seat)
    if level in ("info", "warn", "error"):
        where.append("level=?")
        args.append(level)
    limit = max(1, min(int(limit), 2000))
    order = "ASC" if after_id else "DESC"
    rows = db.q(f"SELECT * FROM events WHERE {' AND '.join(where)} ORDER BY id {order} LIMIT ?", (*args, limit))
    out = [dict(r) | {"data": jload(r["data_json"])} for r in rows]
    for r in out:
        r.pop("data_json", None)
    return out


def events_csv(rows: list[dict]) -> str:
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(["id", "at", "seat", "level", "kind", "message"])
    for r in rows:
        msg = r["message"]
        if msg and msg[0] in "=+-@":   # neutralise spreadsheet formula injection
            msg = "'" + msg
        w.writerow([r["id"], r["at"], r["seat"], r["level"], r["kind"], msg])
    return buf.getvalue()


def funnel(db) -> dict:
    r = db.q1("SELECT * FROM cycles WHERE outcome IS NOT NULL ORDER BY id DESC LIMIT 1")
    if not r:
        return {"cycle": None}
    s = jload(r["stats_json"], {})
    return {"cycle": {"id": r["id"], "started_at": r["started_at"], "ended_at": r["ended_at"], "kind": r["kind"],
                      "outcome": r["outcome"], "reason": r["reason"], "engine": r["engine"],
                      "settings_version": r["settings_version"]},
            "stats": s}


def cycles(db, limit=60) -> list[dict]:
    out = []
    for r in db.q("SELECT id, started_at, ended_at, kind, outcome, reason, engine, stats_json FROM cycles "
                  "WHERE outcome IS NOT NULL ORDER BY id DESC LIMIT ?", (limit,)):
        s = jload(r["stats_json"], {})
        out.append({"id": r["id"], "at": r["started_at"], "outcome": r["outcome"], "reason": r["reason"],
                    "seen": s.get("seen", 0), "survivors": s.get("survivors", 0), "dossiers": s.get("dossiers", 0)})
    return out[::-1]


def chief(db) -> dict:
    j = db.q1("SELECT * FROM judgments WHERE question_set='pick' ORDER BY id DESC LIMIT 1")
    h = db.q1("SELECT * FROM events WHERE kind IN ('handoff','no_pick','cycle_end') ORDER BY id DESC LIMIT 1")
    return {"last_pick": (dict(j) | {"result": jload(j["result_json"]), "raw": jload(j["raw_json"])}) if j else None,
            "last_event": (dict(h) | {"data": jload(h["data_json"])}) if h else None}


def position_view(db, st) -> dict | None:
    p = book.position(db)
    if not p:
        return None
    t = now()
    mark, entry_q = dec(p["last_mark_price"]), dec(p["quote_price"])
    qty = Decimal(p["qty"])
    unreal = None
    if mark is not None:
        unreal = str((mark * qty - Decimal(p["ticket_usd"])).quantize(Decimal("0.01")))
    marks = [dict(r) for r in db.q("SELECT at, price_usd FROM marks WHERE position_uid=? ORDER BY id DESC LIMIT 400",
                                   (p["position_uid"],))][::-1]
    return {**p, "held_minutes": round((t - p["opened_at"]) / 60, 1), "unrealized_usd_before_exit_costs": unreal,
            "mark_age_s": round(t - p["last_mark_at"]) if p["last_mark_at"] else None,
            "next_risk_in_s": max(0, round((p["last_risk_at"] or t) + st["risk_poll_seconds"] - t)),
            "exits": {"stop_loss_price": str(entry_q * (1 - Decimal(str(st["stop_loss_pct"])))),
                      "take_profit_price": str(entry_q * (1 + Decimal(str(st["take_profit_pct"])))),
                      "max_hold_at": p["opened_at"] + st["max_hold_minutes"] * 60,
                      "volume_ratio_exit": st["volume_ratio_exit"],
                      "quote_fail_close_after": st["quote_fail_close_after"]},
            "marks": marks}


def state(desk, worker) -> dict:
    db = desk.db
    st_ver, st = store.active_settings(db)
    latest = db.q1("SELECT max(version) AS v FROM settings_versions")["v"]
    applied = db.q1("SELECT settings_version FROM cycles ORDER BY id DESC LIMIT 1")
    judge = desk.judge_for(st).status()
    return {
        "version": __version__, "execution_mode": EXECUTION_MODE, "book_mode": db.mode, "demo": desk.demo,
        "server_time": now(), "worker": worker.status(), "seats": store.seats(db),
        "judge": judge, "account": book.account(db), "position": position_view(db, st),
        "stats": book.stats(db), "funnel": funnel(db), "chief": chief(db),
        "watchlist_counts": watchlist.counts(db), "bench": bench.summary(db, st_ver),
        "providers": store.providers(db), "integrations": integrations.inventory(),
        "rpc_endpoint": desk.rpc.display,
        "settings": {"latest_version": latest, "last_applied_version": applied["settings_version"] if applied else None,
                     "values": st},
        "fees": {"fee_rate": st["fee_rate"], "min_fee_usd": st["min_fee_usd"],
                 "base_slippage_bps": st["base_slippage_bps"], "impact_factor": st["impact_factor"],
                 "max_slippage_bps": st["max_slippage_bps"]},
        "thresholds": {k: st[k] for k in ("min_age_minutes", "max_age_hours", "min_liquidity_usd", "min_volume_h24",
                                          "min_trades_h24", "max_quote_age_seconds", "max_top10_share",
                                          "max_largest_account_share", "min_holders", "mcap_min_usd", "mcap_max_usd",
                                          "supply_valuation_min_usd", "supply_valuation_max_usd", "rules_min_score")},
        "threshold_version": config.THRESHOLD_VERSION,
        "focus": focus(db),
    }


def focus(db) -> dict | None:
    """Candidate the thresholds panel shows: open position, else last handoff, else last dossier."""
    p = db.q1("SELECT chain, pool_address FROM position WHERE id=1")
    if p:
        return {"chain": p["chain"], "pool_address": p["pool_address"], "why": "position"}
    e = db.q1("SELECT data_json FROM events WHERE kind='handoff' ORDER BY id DESC LIMIT 1")
    d = db.q1("SELECT chain, pool_address, at FROM dossiers ORDER BY id DESC LIMIT 1")
    if e:
        h = jload(e["data_json"], {})
        if h.get("pool_address"):
            return {"chain": h["chain"], "pool_address": h["pool_address"], "why": "last pick"}
    if d:
        return {"chain": d["chain"], "pool_address": d["pool_address"], "why": "last dossier"}
    return None
