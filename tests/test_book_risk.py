import os
import threading
import time
import unittest

from helpers import DeskCase, only

from pandabrain import book, store
from pandabrain.db import ModeMismatch, open_book
from pandabrain.engine import Desk, run_once_cli
from pandabrain.execution import LiveExecutionBlocked, PaperAdapter, get_adapter, request_live_execution
from pandabrain.providers import Http
from pandabrain.providers.fixtures import FixtureWorld
from pandabrain.sizing import size
from pandabrain.store import Lease, LeaseLost

CAND = {"chain": "solana", "pool_address": "P", "token_address": "T", "symbol": "X"}


def fill(ticket="100"):
    st = store_defaults()
    return PaperAdapter().entry("0.01", 100_000, ticket, st)


def store_defaults():
    from pandabrain.config import defaults
    return defaults()


class Book(DeskCase):
    def setUp(self):
        super().setUp()
        _, _, self.db = self.make()
        book.ensure_account(self.db, 10_000)
        self.lease = Lease(self.db)
        self.assertTrue(self.lease.acquire())

    def test_empty_stats_are_null(self):
        s = book.stats(self.db)
        self.assertEqual(s["trades"], 0)
        for k in ("wins", "losses", "win_rate", "total_pnl_usd", "avg_pnl_usd", "best_pnl_usd", "total_fees_usd"):
            self.assertIsNone(s[k], k)

    def test_one_position_and_idempotent_open(self):
        r1 = book.open_position(self.db, self.lease, "k1", CAND, fill(), {})
        r2 = book.open_position(self.db, self.lease, "k1", CAND, fill(), {})
        self.assertTrue(r2["duplicate"])
        self.assertEqual(r1["position_uid"], r2["position_uid"])
        with self.assertRaises(book.PositionHeld):
            book.open_position(self.db, self.lease, "k2", CAND, fill(), {})
        self.assertEqual(book.account(self.db)["cash_usd"], "9900")      # charged once

    def test_idempotent_close_releases_book_once(self):
        uid = book.open_position(self.db, self.lease, "k1", CAND, fill(), {})["position_uid"]
        ex = PaperAdapter().exit("0.012", 100_000, book.position(self.db)["qty"], store_defaults())
        a = book.close_position(self.db, self.lease, uid, ex, "take_profit")
        b = book.close_position(self.db, self.lease, uid, ex, "take_profit")
        self.assertFalse(a["duplicate"])
        self.assertTrue(b["duplicate"])
        self.assertIsNone(book.position(self.db))
        self.assertEqual(self.db.q1("SELECT count(*) n FROM trades")["n"], 1)
        s = book.stats(self.db)
        self.assertEqual(s["trades"], 1)
        self.assertEqual(s["win_rate"], 1.0)

    def test_lost_lease_blocks_book_mutation(self):
        other = Lease(self.db, owner="other")
        self.db.x("UPDATE lease SET expires_at=0")
        self.assertTrue(other.acquire())
        with self.assertRaises(LeaseLost):
            book.open_position(self.db, self.lease, "k1", CAND, fill(), {})
        self.assertIsNone(book.position(self.db))

    def test_fee_and_slippage(self):
        f = fill("100")
        self.assertEqual(f["fee_usd"], "0.95")              # min fee floor
        self.assertGreater(f["slippage_bps"], 50)
        self.assertTrue(f["simulated"])
        st = dict(store_defaults(), max_slippage_bps=10)
        from pandabrain.execution import FillRefused
        with self.assertRaises(FillRefused):
            PaperAdapter().entry("0.01", 1_000, "100", st)


class Sizing(unittest.TestCase):
    def test_arithmetic_and_cuts(self):
        st = store_defaults()
        r = size("10000", 100_000, 1.0, st)
        self.assertEqual(r["ticket_usd"], "600.00")
        self.assertEqual(size("10000", 100_000, 0.6, st)["ticket_usd"], "360.00")
        self.assertEqual(size("10000", 5_000, 1.0, st)["ticket_usd"], "100.00")     # 2% of pool
        self.assertEqual(size("10000", 1_000, 1.0, st)["reason"], "fee_floor")      # $20 ticket vs $0.95 fee
        self.assertEqual(size("10000", None, 1.0, st)["reason"], "missing_liquidity")


class Leases(DeskCase):
    def test_lease_collision(self):
        _, _, db = self.make()
        a, b = Lease(db, owner="a"), Lease(db, owner="b")
        self.assertTrue(a.acquire())
        self.assertFalse(b.acquire())
        with self.assertRaises(LeaseLost):
            b.renew(force=True)
        a.release()
        self.assertTrue(b.acquire())

    def test_concurrent_run_once_only_one_runs(self):
        desk, w, db = self.make()
        results = []
        ts = [threading.Thread(target=lambda: results.append(run_once_cli(desk))) for _ in range(4)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        self.assertEqual(db.q1("SELECT count(*) n FROM orders")["n"], 1)
        self.assertLessEqual(sum(1 for r in results if r.get("outcome") == "opened"), 1)


class Risk(DeskCase):
    def test_exact_pool_continuity_and_restart(self):
        desk, w, db = self.make(sub="b")
        only(w, "BAMBOO")
        run_once_cli(desk)
        p = book.position(db)
        self.assertEqual(p["pool_address"], w.specs[0]["pool"])
        # add a second, deeper pool for the SAME token: monitoring must ignore it
        twin = dict(w.specs[0], pool="TwinPoolAddr1111111111111111111111", liq=9_000_000, price=99.0)
        w.specs.append(twin)
        # restart: a brand new Desk on the same book file
        db.close()
        db2 = open_book(os.path.join(self.tmp, "b"), "demo")
        self.addCleanup(db2.close)
        desk2 = Desk(db2, Http(transport=w, sleep=lambda s: None), demo=True)
        p2 = book.position(db2)
        self.assertEqual(p2["position_uid"], p["position_uid"])
        lease = Lease(db2)
        lease.acquire()
        desk2.risk_tick(lease, force=True)
        mark = db2.q1("SELECT * FROM marks ORDER BY id DESC")
        self.assertLess(float(mark["price_usd"]), 1.0)            # not the twin's $99
        self.assertEqual(book.position(db2)["pool_address"], p["pool_address"])

    def test_missing_exit_price_stays_close_pending(self):
        desk, w, db = self.make(settings={"quote_fail_close_after": 2})
        only(w, "BAMBOO")
        run_once_cli(desk)
        w.outages = {"dexscreener", "geckoterminal"}
        lease = Lease(db)
        lease.acquire()
        for _ in range(3):
            desk.risk_tick(lease, force=True)
        p = book.position(db)
        self.assertEqual(p["status"], "CLOSE_PENDING")
        self.assertEqual(p["close_reason"], "unmeasurable")
        self.assertEqual(db.q1("SELECT count(*) n FROM trades")["n"], 0)    # never closed without a price
        r = desk.run_cycle(lease)
        self.assertEqual(r["outcome"], "held")                     # book not released
        w.outages = set()
        desk.risk_tick(lease, force=True)
        self.assertIsNone(book.position(db))
        t = db.q1("SELECT * FROM trades")
        self.assertEqual(t["close_reason"], "unmeasurable")

    def test_stop_loss_and_cooldown_after_close(self):
        desk, w, db = self.make()
        only(w, "BAMBOO")
        run_once_cli(desk)
        w.specs[0]["price"] *= 0.5
        lease = Lease(db)
        lease.acquire()
        r = desk.risk_tick(lease, force=True)
        self.assertEqual(r["reason"], "stop_loss")
        self.assertLess(float(db.q1("SELECT pnl_usd FROM trades")["pnl_usd"]), 0)
        self.assertEqual(db.q1("SELECT reason FROM bench")["reason"], "recently_closed")


class Isolation(DeskCase):
    def test_demo_and_public_books_cannot_mix(self):
        open_book(os.path.join(self.tmp, "x"), "demo").close()
        with self.assertRaises(ModeMismatch):
            open_book(os.path.join(self.tmp, "x"), "public")
        db = open_book(os.path.join(self.tmp, "y"), "public")
        self.addCleanup(db.close)
        self.assertEqual(db.q1("SELECT value FROM meta WHERE key='mode'")["value"], "public")

    def test_migration_backup_and_refuse_foreign_db(self):
        import sqlite3
        p = os.path.join(self.tmp, "z")
        os.makedirs(p)
        c = sqlite3.connect(os.path.join(p, "desk.db"))
        c.execute("CREATE TABLE position(id INT)")
        c.commit()
        c.close()
        with self.assertRaises(RuntimeError):
            open_book(p, "public")


class LiveBlocked(unittest.TestCase):
    def test_live_execution_always_refused(self):
        with self.assertRaises(LiveExecutionBlocked):
            get_adapter("live")
        with self.assertRaises(LiveExecutionBlocked):
            request_live_execution({"force": True})
        from pandabrain import EXECUTION_MODE
        self.assertEqual(EXECUTION_MODE, "SHADOW_ONLY")


if __name__ == "__main__":
    unittest.main()


class RiskPacing(DeskCase):
    def test_close_pending_retries_are_rate_limited(self):
        desk, w, db = self.make(settings={"quote_fail_close_after": 1, "risk_poll_seconds": 300})
        only(w, "BAMBOO")
        run_once_cli(desk)
        w.outages = {"dexscreener", "geckoterminal"}
        lease = Lease(db)
        lease.acquire()
        desk.risk_tick(lease, force=True)
        self.assertEqual(book.position(db)["status"], "CLOSE_PENDING")
        n = len(w.calls)
        for _ in range(5):
            self.assertEqual(desk.risk_tick(lease), {"skipped": True})
        self.assertEqual(len(w.calls), n)
