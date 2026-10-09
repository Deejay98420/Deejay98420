"""Read-only v2.2 migration preview. Fixture rows were produced by the actual v2.2 code."""

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from decimal import Decimal

from pandabrain.db import open_book
from pandabrain.legacy_v22 import PreviewError, preview

FIX = json.load(open(os.path.join(os.path.dirname(__file__), "fixtures", "v22_ledger.json")))
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def build(path, *, feed="PUBLIC_MARKET_DATA", metadata=True, wal=False, mutate=None):
    c = sqlite3.connect(path)
    if wal:
        c.execute("PRAGMA journal_mode=WAL")
    for ddl in FIX["schema"]:
        c.execute(ddl)
    if metadata:
        for k, v in FIX["metadata"]:
            c.execute("INSERT INTO metadata VALUES(?,?)", (k, feed if k == "feed" else v))
    c.executemany("INSERT INTO account VALUES(?,?)", FIX["account"])
    c.executemany("INSERT INTO position VALUES(?,?)", FIX["position"])
    c.executemany("INSERT INTO events VALUES(?,?,?,?)", FIX["events"])
    c.executemany("INSERT INTO bench VALUES(?,?,?)", FIX["bench"])
    if mutate:
        mutate(c)
    c.commit()
    return c


def tree(d):
    out = {}
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name)
        with open(p, "rb") as f:
            out[name] = (os.stat(p).st_mtime_ns, f.read())
    return out


class Preview(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp(prefix="pb-v22-")
        self.addCleanup(shutil.rmtree, self.d, True)
        self.db = os.path.join(self.d, "pandarianz.db")

    def test_maps_cash_position_trades_events_and_reconciles(self):
        build(self.db).close()
        r = preview(self.db)
        self.assertEqual(r["blockers"], [])
        self.assertTrue(r["ready_for_import"])
        self.assertEqual(r["target_book"], "public")
        self.assertTrue(r["reconciliation"]["balanced"])
        self.assertEqual(Decimal(r["reconciliation"]["ledger_cash"]), Decimal(repr(FIX["account"][0][1])).quantize(Decimal("0.000001")))
        # open position: exact pool kept, v3 ticket = v2.2 ticket + entry_fee
        pos = json.loads(FIX["position"][0][1])
        p = r["open_position"]
        self.assertEqual(p["pool_address"], pos["token"]["pair"])
        self.assertEqual(p["token_address"], pos["token"]["addr"])
        self.assertEqual(Decimal(p["ticket_usd"]), (Decimal(repr(pos["ticket"])) + Decimal(repr(pos["entry_fee"]))).quantize(Decimal("0.000001")))
        self.assertEqual(p["status"], "OPEN")
        # closed trade rebuilt from the open/close pair, pnl equals v2.2 net_pnl
        close = next(json.loads(e[3]) for e in FIX["events"] if e[2] == "paper_close")
        t = r["closed_trades"]["rows"][0]
        self.assertEqual(r["closed_trades"]["count"], 1)
        self.assertEqual(Decimal(t["pnl_usd"]), Decimal(repr(close["net_pnl"])).quantize(Decimal("0.000001")))
        self.assertEqual(t["close_reason"], close["reason"])
        self.assertEqual(r["events"]["total"], len(FIX["events"]))
        self.assertEqual(sum(r["events"]["by_kind"].values()), len(FIX["events"]))

    def test_source_is_never_modified_even_in_wal_mode(self):
        c = build(self.db, wal=True)          # connection held open: un-checkpointed WAL content
        c.execute("INSERT INTO events(at,kind,payload) VALUES(?,?,?)", (time.time(), "cycle", "{}"))
        c.commit()
        before = tree(self.d)
        r = preview(self.db)
        self.assertEqual(tree(self.d), before)                       # bytes, mtimes and file set
        self.assertTrue(r["integrity"]["source_unchanged"])
        self.assertIn("pandarianz.db-wal", r["integrity"]["before"])
        self.assertEqual(r["events"]["total"], len(FIX["events"]) + 1)   # WAL rows were seen via the copy
        c.close()

    def test_close_required_maps_to_close_pending(self):
        def m(c):
            p = json.loads(FIX["position"][0][1])
            p["close_required"], p["missing_checks"] = True, 3
            c.execute("UPDATE position SET payload=?", (json.dumps(p),))
        build(self.db, mutate=m).close()
        p = preview(self.db)["open_position"]
        self.assertEqual((p["status"], p["close_reason"], p["mark_fail_count"]), ("CLOSE_PENDING", "unmeasurable", 3))

    def test_demo_feed_targets_demo_book(self):
        build(self.db, feed="DEMO_SYNTHETIC").close()
        self.assertEqual(preview(self.db)["target_book"], "demo")

    def test_blockers(self):
        cases = {
            "no metadata": dict(metadata=False),
            "live lease": dict(mutate=lambda c: c.execute("INSERT INTO lease VALUES(1,'w',?)", (time.time() + 300,))),
            "bad cash": dict(mutate=lambda c: c.execute("UPDATE account SET cash=-5")),
            "corrupt position": dict(mutate=lambda c: c.execute("UPDATE position SET payload='{nope'")),
            "position without pool": dict(mutate=lambda c: c.execute(
                "UPDATE position SET payload=?", (json.dumps({**json.loads(FIX["position"][0][1]),
                                                             "token": {"chain": "solana", "addr": "A"}}),))),
            "unknown feed": dict(feed="SOMETHING"),
        }
        for name, kw in cases.items():
            with self.subTest(name):
                if os.path.exists(self.db):
                    os.remove(self.db)
                build(self.db, **kw).close()
                r = preview(self.db)
                self.assertFalse(r["ready_for_import"], r["blockers"])
                self.assertTrue(r["blockers"])

    def test_missing_table_and_wrong_files(self):
        c = sqlite3.connect(self.db)
        c.execute("CREATE TABLE events(id INTEGER PRIMARY KEY, at REAL, kind TEXT, payload TEXT)")
        c.commit()
        c.close()
        r = preview(self.db)
        self.assertFalse(r["ready_for_import"])
        self.assertFalse(r["schema"]["matches_v22"])
        v3 = open_book(os.path.join(self.d, "v3"), "public")
        v3.close()
        with self.assertRaises(PreviewError):
            preview(os.path.join(self.d, "v3", "desk.db"))
        with self.assertRaises(PreviewError):
            preview(os.path.join(self.d, "nope.db"))
        junk = os.path.join(self.d, "junk.db")
        with open(junk, "wb") as f:
            f.write(b"not sqlite" * 50)
        with self.assertRaises(PreviewError):
            preview(junk)

    def test_unreconciled_cash_is_a_warning_not_silently_fixed(self):
        build(self.db, mutate=lambda c: c.execute("UPDATE account SET cash=cash+1")).close()
        r = preview(self.db)
        self.assertFalse(r["reconciliation"]["balanced"])
        self.assertEqual(Decimal(r["reconciliation"]["difference"]), Decimal("1.000000"))
        self.assertTrue(any("reconcile" in w for w in r["warnings"]))
        self.assertTrue(preview(self.db, Decimal("10001"))["reconciliation"]["balanced"])

    def test_target_v3_book_with_history_blocks(self):
        build(self.db).close()
        target = os.path.join(self.d, "data")
        r = preview(self.db, target_dir=target)
        self.assertEqual(r["target_check"]["exists"], False)
        self.assertFalse(os.path.exists(target))                      # preview never creates it
        b = open_book(os.path.join(target, "public"), "public")
        b.x("INSERT INTO orders VALUES('k','open','u',1,'{}')")
        b.close()
        r = preview(self.db, target_dir=target)
        self.assertFalse(r["ready_for_import"])
        self.assertTrue(any("already has trading history" in x for x in r["blockers"]))

    def test_cli_is_read_only_and_creates_no_v3_book(self):
        build(self.db).close()
        data = os.path.join(self.d, "v3data")
        before = tree(self.d)
        env = dict(os.environ, PANDABRAIN_DATA_DIR=data)
        out = subprocess.run([sys.executable, "-m", "pandabrain", "migrate-preview", "--from", self.db, "--json"],
                             cwd=ROOT, env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode, 0, out.stderr)
        r = json.loads(out.stdout)
        self.assertTrue(r["read_only"])
        self.assertFalse(os.path.exists(data))
        self.assertEqual(tree(self.d), before)


if __name__ == "__main__":
    unittest.main()
