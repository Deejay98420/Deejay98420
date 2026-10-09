import time
import unittest

from helpers import DeskCase, only

from pandabrain import bench, book, filters, store, watchlist
from pandabrain.config import defaults
from pandabrain.engine import run_once_cli
from pandabrain.store import Lease

ST = defaults()


def snap(**kw):
    t = time.time()
    s = {"chain": "solana", "pool_address": "P", "token_address": "T", "price_usd": "0.001", "fetched_at": t,
         "pool_created_at": t - 3 * 3600, "liquidity_usd": 50_000, "volume_h24": 100_000, "buys_h24": 200,
         "sells_h24": 150, "buys_h1": 30, "sells_h1": 20, "mcap_usd": None, "source": "test"}
    s.update(kw)
    return s


class PreDossier(unittest.TestCase):
    def test_missing_mcap_does_not_block_pre_dossier(self):
        self.assertIsNone(filters.pre_dossier(snap(mcap_usd=None), ST))

    def test_reasons_are_distinct(self):
        t = time.time()
        cases = {"stale_quote": snap(fetched_at=t - 10_000), "too_young": snap(pool_created_at=t - 60),
                 "too_old": snap(pool_created_at=t - 10 * 86400), "age_unknown": snap(pool_created_at=None),
                 "low_liquidity": snap(liquidity_usd=10), "missing_liquidity": snap(liquidity_usd=None),
                 "low_volume": snap(volume_h24=1), "missing_trades": snap(buys_h24=None),
                 "low_trades": snap(buys_h24=1, sells_h24=1), "no_sells": snap(sells_h1=0, buys_h1=50),
                 "price_invalid": snap(price_usd="NaN"), "evm_unverified": snap(chain="base")}
        for reason, s in cases.items():
            with self.subTest(reason=reason):
                self.assertEqual(filters.pre_dossier(s, ST), reason)

    def test_missing_concentration_has_its_own_reason(self):
        self.assertEqual(filters.vet_facts({"top_10_percent": None, "holder_count": 500}, ST), "missing_distribution")
        self.assertEqual(filters.vet_facts({"top_10_percent": 20.0, "holder_count": None}, ST), "missing_holders")
        relaxed = dict(ST, require_distribution=False)
        self.assertIsNone(filters.vet_facts({"top_10_percent": None, "holder_count": None}, relaxed))


class Cycle(DeskCase):
    def test_full_cycle_reasons_and_open(self):
        desk, w, db = self.make(settings={"max_dossiers_per_cycle": 12, "gt_calls_per_cycle": 30,
                                          "rpc_calls_per_cycle": 60})
        r = run_once_cli(desk)
        self.assertEqual(r["outcome"], "opened")
        st = r["stats"]
        self.assertEqual(st["pre"], {"too_young": 1, "low_liquidity": 1, "no_sells": 1, "too_old": 1})
        self.assertEqual(st["vet"].get("mint_authority_open"), 1)
        self.assertEqual(st["vet"].get("freeze_authority_open"), 1)
        self.assertEqual(st["vet"].get("unsupported_extension"), 1)
        self.assertEqual(st["vet"].get("top10_concentration"), 1)
        self.assertEqual(st["vet"].get("missing_distribution"), 1)
        self.assertEqual(st["valuation"].get("supply_valuation_above_max"), 1)
        # JADE has no provider market cap and still reached the judge via TOTAL_SUPPLY_VALUATION
        jade = [w2 for w2 in w.specs if w2["sym"] == "JADE"][0]
        ev = db.q1("SELECT * FROM evaluations WHERE pool_address=? AND stage='judge'", (jade["pool"],))
        self.assertEqual(ev["outcome"], "pass")
        dz = db.q1("SELECT data_json FROM dossiers WHERE pool_address=? ORDER BY id DESC", (jade["pool"],))
        self.assertIn("TOTAL_SUPPLY_VALUATION", dz["data_json"])
        self.assertIsNotNone(book.position(db))

    def test_bounded_dossiers_defer_not_reject(self):
        desk, w, db = self.make(settings={"max_dossiers_per_cycle": 1})
        r = run_once_cli(desk)
        self.assertEqual(r["stats"]["dossiers"], 1)
        self.assertGreater(r["stats"]["deferred"], 0)
        self.assertEqual(db.q1("SELECT count(*) n FROM bench WHERE reason LIKE 'deferred%'")["n"], 0)

    def test_provider_outage_reports_and_keeps_watchlist(self):
        desk, w, db = self.make()
        run_once_cli(desk)
        book_pos = book.position(db)
        # close the position directly so the next cycle scans
        lease = Lease(db)
        lease.acquire()
        from pandabrain.execution import PaperAdapter
        ex = PaperAdapter().exit(book_pos["quote_price"], 50_000, book_pos["qty"], store.active_settings(db)[1])
        book.close_position(db, lease, book_pos["position_uid"], ex, "test")
        lease.release()
        before = db.q1("SELECT count(*) n FROM watchlist")["n"]
        w.outages = {"geckoterminal"}
        r = run_once_cli(desk)
        self.assertEqual(r["outcome"], "no_trade")
        self.assertIn("geckoterminal:unavailable", r["stats"]["outages"])
        self.assertEqual(db.q1("SELECT count(*) n FROM watchlist")["n"], before)
        p = {x["name"]: x for x in store.providers(db)}
        self.assertEqual(p["geckoterminal"]["state"], "unavailable")
        self.assertTrue(db.q1("SELECT 1 FROM events WHERE kind='provider_unavailable'"))

    def test_rpc_unavailable_is_distinct_and_short(self):
        desk, w, db = self.make()
        only(w, "BAMBOO")
        w.outages = {"solana_rpc"}
        r = run_once_cli(desk)
        self.assertEqual(r["stats"]["vet"], {"rpc_unavailable": 1})
        b = db.q1("SELECT * FROM bench")
        self.assertLess(b["until"] - b["created_at"], 11 * 60)

    def test_watchlist_maturation(self):
        desk, w, db = self.make()
        only(w, "CUB")
        r = run_once_cli(desk)
        self.assertEqual(r["stats"]["pre"], {"too_young": 1})
        row = db.q1("SELECT * FROM watchlist")
        self.assertAlmostEqual(row["next_check_at"], row["pool_created_at"] + 15 * 60 + 30, delta=1)
        self.assertIsNone(db.q1("SELECT * FROM bench"))          # too_young is scheduled, not benched
        # time passes: the pool ages; GT stops listing it as new, but the watchlist rechecks it
        cub = w.specs[0]
        cub["created"] -= 30 * 60
        w.specs = []                                              # not "new" any more
        db.x("UPDATE watchlist SET next_check_at=0, pool_created_at=?", (cub["created"],))
        w.specs_hidden = [cub]
        w.by_pool = lambda pool: cub if pool == cub["pool"] else None
        w.by_token = lambda tok: cub if tok == cub["token"] else None
        r = run_once_cli(desk)
        self.assertEqual(r["stats"]["due"], 1)
        self.assertNotIn("too_young", r["stats"]["pre"])
        self.assertEqual(r["outcome"], "opened")

    def test_no_scan_while_held(self):
        desk, w, db = self.make()
        run_once_cli(desk)
        self.assertIsNotNone(book.position(db))
        calls = len(w.calls)
        lease = Lease(db)
        lease.acquire()
        r = desk.run_cycle(lease)
        self.assertEqual((r["outcome"], r["reason"]), ("held", "position_open"))
        self.assertEqual(len(w.calls), calls)                     # not a single provider request

    def test_judge_unconfigured_no_fallback(self):
        desk, w, db = self.make(settings={"judge_engine": "TYPED_AI"})
        r = run_once_cli(desk)
        self.assertEqual((r["outcome"], r["reason"]), ("no_trade", "judge_unconfigured"))
        self.assertIsNone(book.position(db))
        self.assertEqual(db.q1("SELECT count(*) n FROM judgments")["n"], 0)
        self.assertEqual(store.seats(db)["CHIEF"]["state"], "unconfigured")


class Bench(DeskCase):
    def test_policy_aware_invalidation(self):
        _, _, db = self.make()
        bench.sit(db, "solana", "A", "pre", "low_liquidity", 1)
        self.assertTrue(bench.active(db, "solana", "A", 1))
        self.assertIsNone(bench.active(db, "solana", "A", 2))      # settings changed: threshold reason void
        bench.sit(db, "solana", "B", "vet", "mint_authority_open", 1)
        self.assertTrue(bench.active(db, "solana", "B", 2))       # facts survive settings changes
        # a stale v2.2-era rejection never hides a fixed pipeline
        db.x("INSERT INTO bench VALUES('solana','C','missing_mcap_usd','pre@2',1,?,?)", (time.time(), time.time() + 1e6))
        self.assertIsNone(bench.active(db, "solana", "C", 1))
        db.x("INSERT INTO bench VALUES('solana','D','low_volume','pre@2',1,?,?)", (time.time(), time.time() + 1e6))
        self.assertIsNone(bench.active(db, "solana", "D", 1))     # old policy version
        self.assertEqual(db.q1("SELECT count(*) n FROM bench")["n"], 4)   # nothing deleted


class Watchlist(DeskCase):
    def test_pool_identity_is_immutable_and_cap_retires(self):
        _, _, db = self.make()
        s = snap(pool_address="P1", token_address="T1")
        sid = watchlist.save_snapshot(db, s)
        self.assertTrue(watchlist.upsert_discovered(db, s, sid))
        self.assertFalse(watchlist.upsert_discovered(db, dict(s, token_address="EVIL"), sid))
        self.assertEqual(db.q1("SELECT token_address FROM watchlist")["token_address"], "T1")
        for i in range(30):
            x = snap(pool_address=f"Q{i}", token_address=f"U{i}")
            watchlist.upsert_discovered(db, x, watchlist.save_snapshot(db, x))
        self.assertEqual(watchlist.enforce_cap(db, 20), 11)
        self.assertEqual(db.q1("SELECT count(*) n FROM watchlist")["n"], 31)


if __name__ == "__main__":
    unittest.main()
