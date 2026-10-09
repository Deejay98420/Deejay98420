"""READ-ONLY migration preview for a v2.2 ledger (pandarianz.db / pandarianz-demo.db).

Nothing here writes anywhere except a private temporary directory:
  1. the source file and its -wal / -shm siblings are hashed (sha256, size, mtime);
  2. they are copied into a TemporaryDirectory and ONLY the copy is opened (with query_only), so
     SQLite's WAL/shm housekeeping can never touch your real ledger;
  3. the source is hashed again and the report states whether it changed during the preview.

The preview maps what an importer WOULD create in the v3 book (cash, the open paper position,
closed trades rebuilt from paper_open/paper_close event pairs, every event verbatim) and reports
blockers. There is no importer yet: it will be written only after this preview is reviewed.

v2.2 ledger facts (from the v2.2 source, book.py / risk.py / main.py):
  tables   metadata(key,value) events(id,at,kind,payload) bench(tid,reason,until)
           position(id=1,payload) account(id=1,cash REAL) lease(id=1,owner,until)
  metadata engine=PANDARIANZ_BRAIN_RULES_2.0.0, feed=PUBLIC_MARKET_DATA | DEMO_SYNTHETIC
  cash     debited by ticket + entry_fee on open, credited by proceeds on close
  closes   only recorded as paper_close events; net_pnl = proceeds - ticket - entry_fee
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import shutil
import sqlite3
import tempfile
import time
from decimal import Decimal, InvalidOperation

V22_TABLES = {
    "metadata": {"key", "value"},
    "events": {"id", "at", "kind", "payload"},
    "bench": {"tid", "reason", "until"},
    "position": {"id", "payload"},
    "account": {"id", "cash"},
    "lease": {"id", "owner", "until"},
}
KNOWN_ENGINES = {"PANDARIANZ_BRAIN_RULES_2.0.0"}
FEEDS = {"PUBLIC_MARKET_DATA": "public", "DEMO_SYNTHETIC": "demo"}
V22_DEFAULT_BANK = Decimal("10000")
EVENT_SEAT = {"cycle": "CHIEF", "brain_evaluation": "CHIEF", "brain_selection": "CHIEF", "rejected": "VET",
              "collector_failure": "SCAN", "paper_open": "FILLS", "paper_close": "RISK", "paper_mark": "RISK",
              "risk_data_unavailable": "RISK", "worker_failure": "CHIEF"}


class PreviewError(RuntimeError):
    pass


def _fingerprint(path: str) -> dict:
    out = {}
    for suffix in ("", "-wal", "-shm", "-journal"):
        p = path + suffix
        if os.path.isfile(p):
            h = hashlib.sha256()
            with open(p, "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
            st = os.stat(p)
            out[os.path.basename(p)] = {"sha256": h.hexdigest(), "bytes": st.st_size, "mtime": st.st_mtime}
    return out


def _dec(x) -> Decimal | None:
    if x is None or isinstance(x, bool):
        return None
    try:
        if isinstance(x, float) and not math.isfinite(x):
            return None
        d = Decimal(repr(x)) if isinstance(x, float) else Decimal(str(x))
    except (InvalidOperation, ValueError):
        return None
    return d if d.is_finite() else None


def _money(d: Decimal | None) -> str | None:
    return None if d is None else str(d.quantize(Decimal("0.000001")))


def _slip_bps(fill: Decimal | None, quote: Decimal | None, side: str) -> float | None:
    if fill is None or quote is None or quote <= 0:
        return None
    r = (fill / quote - 1) if side == "buy" else (1 - fill / quote)
    return round(float(r * 10_000), 2)


def _snapshot_copy(src: str, tmp: str) -> str:
    dst = os.path.join(tmp, "v22-copy.db")
    shutil.copy2(src, dst)
    for suffix in ("-wal", "-shm"):
        if os.path.isfile(src + suffix):
            shutil.copy2(src + suffix, dst + suffix)
    return dst


def _map_position(p: dict, issues: list) -> dict | None:
    t = p.get("token") if isinstance(p.get("token"), dict) else {}
    need = {"chain": t.get("chain"), "addr": t.get("addr"), "pair": t.get("pair")}
    missing = [k for k, v in need.items() if not v]
    units, entry, quote = _dec(p.get("units")), _dec(p.get("entry_price")), _dec(t.get("price_usd"))
    ticket, fee, opened = _dec(p.get("ticket")), _dec(p.get("entry_fee")), p.get("opened_at")
    for k, v in (("units", units), ("entry_price", entry), ("ticket", ticket), ("entry_fee", fee)):
        if v is None or v <= 0:
            missing.append(k)
    if not isinstance(opened, (int, float)) or isinstance(opened, bool) or not math.isfinite(opened):
        missing.append("opened_at")
    if missing:
        issues.append(f"open position is missing or has invalid fields: {sorted(set(missing))}")
        return None
    close_required = bool(p.get("close_required"))
    return {
        "chain": t["chain"], "pool_address": t["pair"], "token_address": t["addr"], "symbol": t.get("ticker"),
        "qty": str(units), "entry_price": str(entry), "quote_price": str(quote) if quote else str(entry),
        "ticket_usd": _money(ticket + fee), "entry_fee_usd": _money(fee),
        "entry_slippage_bps": _slip_bps(entry, quote, "buy"), "opened_at": opened,
        "status": "CLOSE_PENDING" if close_required else "OPEN",
        "close_reason": "unmeasurable" if close_required else None,
        "mark_fail_count": int(p.get("missing_checks") or 0),
        "quote_source_v22": t.get("source"),
        "monitoring": ("exact pool continues: v3 RISK re-quotes this pair address via DEX Screener, "
                       "then GeckoTerminal pool"),
        "v22_cash_debited": _money(ticket + fee),
        "note": ("v3 ticket_usd = v2.2 ticket + entry_fee (v3 debits cash once by ticket_usd and "
                 "computes pnl = proceeds - ticket_usd, which equals v2.2 net_pnl)"),
    }


def preview(path: str, starting_cash: Decimal = V22_DEFAULT_BANK, target_dir: str | None = None) -> dict:
    path = os.path.abspath(os.path.expanduser(path))
    if not os.path.isfile(path):
        raise PreviewError(f"no such file: {path}")
    blockers, warnings = [], []
    before = _fingerprint(path)
    report: dict = {"tool": "pandabrain migrate-preview", "read_only": True, "source": path,
                    "generated_at": time.time(), "blockers": blockers, "warnings": warnings}

    with tempfile.TemporaryDirectory(prefix="pb-v22-preview-") as tmp:
        copy = _snapshot_copy(path, tmp)
        try:
            c = sqlite3.connect(copy)
            c.execute("PRAGMA query_only=ON")
            tables = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        except sqlite3.DatabaseError as e:
            raise PreviewError(f"not a readable SQLite database: {e}") from None
        try:
            if "meta" in tables and "settings_versions" in tables:
                raise PreviewError("this is a v3 PandaBrain book, not a v2.2 ledger")
            cols = {t: {r[1] for r in c.execute(f"PRAGMA table_info({t})")} for t in V22_TABLES if t in tables}
            schema_ok = True
            for t, want in V22_TABLES.items():
                if t not in tables:
                    if t in ("lease", "bench"):
                        warnings.append(f"table {t} absent")
                    else:
                        blockers.append(f"required v2.2 table '{t}' is missing")
                        schema_ok = False
                elif not want <= cols[t]:
                    blockers.append(f"table {t} lacks columns {sorted(want - cols[t])}")
                    schema_ok = False
            extra = sorted(tables - set(V22_TABLES) - {"sqlite_sequence"})
            if extra:
                warnings.append(f"unrecognised tables (ignored by an importer): {extra}")
            report["schema"] = {"tables": sorted(tables), "matches_v22": schema_ok}
            if not schema_ok:
                return _finish(report, path, before, None)

            meta = dict(c.execute("SELECT key, value FROM metadata").fetchall())
            n_events = c.execute("SELECT count(*) FROM events").fetchone()[0]
            engine, feed = meta.get("engine"), meta.get("feed")
            report["ledger"] = {"engine": engine, "feed": feed, "events": n_events}
            if not meta:
                blockers.append("ledger has no engine/feed metadata (a pre-v2 or never-bound database); "
                                "v2.2 itself refuses to treat such files as its ledger")
            if engine and engine not in KNOWN_ENGINES:
                warnings.append(f"unexpected engine identity {engine!r}")
            if feed and feed not in FEEDS:
                blockers.append(f"unknown feed {feed!r}")
            elif meta and not feed:
                blockers.append("ledger feed is not recorded; cannot tell public from demo")
            target_mode = FEEDS.get(feed)
            report["target_book"] = target_mode
            if os.path.basename(path) == "pandarianz-demo.db" and target_mode == "public":
                warnings.append("file is named pandarianz-demo.db but is bound to PUBLIC_MARKET_DATA")

            # lease: a live v2.2 worker would keep writing
            if "lease" in tables:
                lr = c.execute("SELECT owner, until FROM lease WHERE id=1").fetchone()
                if lr and isinstance(lr[1], (int, float)) and lr[1] > time.time():
                    blockers.append("a v2.2 worker lease is live: stop `python3 main.py run` (Ctrl-C) first")
                report["v22_lease"] = {"held": bool(lr), "until": lr[1] if lr else None}

            # cash
            ar = c.execute("SELECT cash FROM account WHERE id=1").fetchone()
            cash = _dec(ar[0]) if ar else None
            if cash is None or cash < 0:
                blockers.append(f"account cash missing or invalid: {ar[0] if ar else None!r}")

            # events (verbatim)
            kinds: dict[str, int] = {}
            bad_json, opens, closes = 0, {}, []
            first_at = last_at = None
            for eid, at, kind, payload in c.execute("SELECT id, at, kind, payload FROM events ORDER BY id"):
                kinds[kind] = kinds.get(kind, 0) + 1
                first_at = at if first_at is None else first_at
                last_at = at
                try:
                    pl = json.loads(payload)
                except (TypeError, ValueError):
                    bad_json += 1
                    continue
                if kind == "paper_open" and isinstance(pl, dict):
                    tid = (pl.get("token") or {}).get("tid")
                    opens.setdefault(tid, []).append((eid, pl))
                elif kind == "paper_close" and isinstance(pl, dict):
                    closes.append((eid, at, pl))
            if bad_json:
                warnings.append(f"{bad_json} event payloads are not valid JSON; they would be imported as raw text")
            report["events"] = {
                "total": n_events, "by_kind": dict(sorted(kinds.items())), "first_at": first_at, "last_at": last_at,
                "import_plan": ("every row copied into the v3 events table with its original timestamp; kind "
                                "prefixed 'v22_'; original id and payload kept verbatim in data; seat mapped "
                                f"by kind {EVENT_SEAT}")}

            # closed trades rebuilt from open/close pairs
            trades, net, unpaired = [], Decimal(0), 0
            for eid, at, pl in closes:
                cand = opens.get(pl.get("tid")) or []
                prior = [o for o in cand if o[0] < eid]
                if not prior:
                    unpaired += 1
                    continue
                oid, op = prior[-1]
                cand.remove((oid, op))
                tok = op.get("token") or {}
                tk, ef, xf = _dec(pl.get("ticket")), _dec(pl.get("entry_fee")), _dec(pl.get("exit_fee"))
                pnl = _dec(pl.get("net_pnl"))
                if None in (tk, ef, xf, pnl):
                    unpaired += 1
                    continue
                net += pnl
                trades.append({
                    "position_uid": f"v22_open_{oid}", "v22_open_event": oid, "v22_close_event": eid,
                    "chain": tok.get("chain"), "pool_address": tok.get("pair"), "token_address": tok.get("addr"),
                    "symbol": tok.get("ticker"), "opened_at": op.get("opened_at"), "closed_at": pl.get("closed_at", at),
                    "entry_price": str(_dec(pl.get("entry_price"))), "exit_price": str(_dec(pl.get("exit_price"))),
                    "qty": str(_dec(op.get("units"))), "ticket_usd": _money(tk + ef),
                    "proceeds_usd": _money(pnl + tk + ef), "entry_fee_usd": _money(ef), "exit_fee_usd": _money(xf),
                    "pnl_usd": _money(pnl), "close_reason": pl.get("reason"),
                })
            if unpaired:
                warnings.append(f"{unpaired} paper_close events could not be paired with a paper_open; they stay "
                                "as events only and are not counted as trades")
            report["closed_trades"] = {"count": len(trades), "net_pnl_usd": _money(net), "rows": trades}

            # open position
            pr = c.execute("SELECT payload FROM position WHERE id=1").fetchone()
            pos, open_cost = None, Decimal(0)
            if pr:
                try:
                    pdata = json.loads(pr[0])
                except (TypeError, ValueError):
                    pdata = None
                    blockers.append("open position payload is not valid JSON")
                if isinstance(pdata, dict):
                    pos = _map_position(pdata, blockers)
                    if pos:
                        open_cost = Decimal(pos["v22_cash_debited"])
                        if pos["chain"] != "solana":
                            warnings.append(f"open position is on {pos['chain']}: v3 can monitor its exact pool "
                                            "but will not open new EVM positions")
                        if pos["status"] == "CLOSE_PENDING":
                            warnings.append("open position is close-pending in v2.2 (unpriced); v3 keeps it "
                                            "CLOSE_PENDING and closes only at a real quote")
            report["open_position"] = pos

            # reconciliation against v2.2's own accounting
            if cash is not None:
                expected = starting_cash + net - open_cost
                diff = cash - expected
                report["reconciliation"] = {
                    "assumed_v22_starting_bank": str(starting_cash), "closed_net_pnl": _money(net),
                    "open_position_cost": _money(open_cost), "expected_cash": _money(expected),
                    "ledger_cash": _money(cash), "difference": _money(diff),
                    "balanced": abs(diff) < Decimal("0.0001")}
                if abs(diff) >= Decimal("0.0001"):
                    warnings.append(f"cash does not reconcile by {_money(diff)} (wrong starting bank? pass "
                                    "--starting-cash, or events were lost); an importer would keep the ledger "
                                    "cash as-is and record the difference")

            # bench
            bn = c.execute("SELECT reason, count(*) FROM bench WHERE until > ? GROUP BY reason",
                           (time.time(),)).fetchall() if "bench" in tables else []
            report["bench"] = {"active_by_reason": dict(bn),
                               "plan": "not imported: v2.2 cooldowns are per token under v2.2 policy; v3 "
                                       "re-verifies every candidate under its own pool-level policy"}

            report["v3_after_import"] = {
                "book": target_mode, "cash_usd": _money(cash),
                "starting_cash_usd": str(starting_cash),
                "position": "1 (" + pos["status"] + ")" if pos else "none",
                "trades": len(trades), "events": n_events,
                "win_rate": round(sum(1 for t in trades if Decimal(t["pnl_usd"]) > 0) / len(trades), 4) if trades else None,
            }
        finally:
            c.close()
        report["target_check"] = _target_check(target_dir, report.get("target_book"), blockers)
        return _finish(report, path, before, None)


def _target_check(target_dir, mode, blockers) -> dict:
    """Read-only look at the v3 book an importer would write into. Never creates it."""
    if not target_dir or not mode:
        return {"checked": False}
    p = os.path.join(target_dir, mode, "desk.db")
    if not os.path.isfile(p):
        return {"checked": True, "path": p, "exists": False, "empty": True}
    with tempfile.TemporaryDirectory(prefix="pb-v3-target-") as tmp:
        cp = _snapshot_copy(p, tmp)
        c = sqlite3.connect(cp)
        try:
            q = lambda sql: c.execute(sql).fetchone()[0]  # noqa: E731
            counts = {"trades": q("SELECT count(*) FROM trades"), "position": q("SELECT count(*) FROM position"),
                      "orders": q("SELECT count(*) FROM orders")}
        except sqlite3.DatabaseError:
            counts = None
        finally:
            c.close()
    empty = bool(counts) and not any(counts.values())
    if counts is None:
        blockers.append(f"target {p} is not a readable v3 book")
    elif not empty:
        blockers.append(f"target v3 {mode} book already has trading history {counts}; an importer must not "
                        "merge ledgers — use a fresh PANDABRAIN_DATA_DIR")
    return {"checked": True, "path": p, "exists": True, "counts": counts, "empty": empty}


def _finish(report, path, before, _):
    after = _fingerprint(path)
    report["integrity"] = {"before": before, "after": after, "source_unchanged": before == after}
    if before != after:
        report["warnings"].append("the source changed while the preview ran: something else is writing to it")
    report["ready_for_import"] = not report["blockers"]
    report["importer"] = "not implemented yet: review this preview first"
    return report


def summary_text(r: dict) -> str:
    L = [f"PandaBrain v2.2 -> v3 MIGRATION PREVIEW (read-only)", f"source: {r['source']}"]
    integ = r.get("integrity", {})
    L.append(f"source unchanged by preview: {integ.get('source_unchanged')}  files: {sorted(integ.get('before', {}))}")
    led = r.get("ledger") or {}
    L.append(f"engine: {led.get('engine')}  feed: {led.get('feed')}  -> v3 {r.get('target_book')} book")
    rc = r.get("reconciliation") or {}
    if rc:
        L.append(f"cash: {rc['ledger_cash']}  (expected {rc['expected_cash']} from bank {rc['assumed_v22_starting_bank']} "
                 f"+ closed pnl {rc['closed_net_pnl']} - open cost {rc['open_position_cost']}; balanced={rc['balanced']})")
    p = r.get("open_position")
    L.append("open position: " + (f"{p['symbol']} {p['chain']} pool {p['pool_address']} qty {p['qty']} "
                                  f"ticket {p['ticket_usd']} status {p['status']}" if p else "none"))
    ct = r.get("closed_trades") or {}
    L.append(f"closed trades: {ct.get('count', 0)}  net pnl {ct.get('net_pnl_usd')}")
    for t in ct.get("rows", [])[:20]:
        L.append(f"  - {t['symbol']} {t['close_reason']} pnl {t['pnl_usd']}")
    ev = r.get("events") or {}
    L.append(f"events: {ev.get('total')} {ev.get('by_kind')}")
    L.append(f"bench (not imported): {(r.get('bench') or {}).get('active_by_reason')}")
    tc = r.get("target_check") or {}
    if tc.get("checked"):
        L.append(f"target: {tc['path']} exists={tc['exists']} empty={tc.get('empty')}")
    for b in r["blockers"]:
        L.append(f"BLOCKER: {b}")
    for w in r["warnings"]:
        L.append(f"warning: {w}")
    L.append(f"ready for import: {r['ready_for_import']}  ({r['importer']})")
    return "\n".join(L)
