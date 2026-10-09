import http.client
import json
import re
import threading
import time

from helpers import DeskCase, only

from pandabrain import book
from pandabrain.server import DeskServer
from pandabrain.worker import Worker


class Api(DeskCase):
    def setUp(self):
        super().setUp()
        self.desk, self.world, self.db = self.make(settings={"cycle_seconds": 30})
        only(self.world, "BAMBOO", "JADE", "MINTY", "CUB")
        self.worker = Worker(self.desk, tick=0.1)
        self.srv = DeskServer(("127.0.0.1", 0), self.desk, self.worker)
        self.port = self.srv.server_address[1]
        self.t = threading.Thread(target=self.srv.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
        self.t.start()
        self.addCleanup(self._down)
        self.host = f"127.0.0.1:{self.port}"

    def _down(self):
        self.srv.shutting_down.set()
        if self.worker.alive:
            self.worker.stop()
        self.srv.shutdown()
        self.srv.server_close()

    def req(self, method, path, body=None, headers=None, raw=False):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        h = {"Host": self.host}
        h.update(headers or {})
        data = json.dumps(body).encode() if body is not None else None
        c.request(method, path, body=data, headers=h)
        r = c.getresponse()
        out = r.read()
        c.close()
        return r.status, (out if raw else json.loads(out or b"{}")), dict(r.getheaders())

    def post(self, path, body=None, **extra):
        h = {"Content-Type": "application/json", "X-Desk-Token": self.srv.token}
        h.update(extra)
        return self.req("POST", path, body or {}, h)

    def wait_for(self, cond, timeout=10):
        end = time.time() + timeout
        while time.time() < end:
            if cond():
                return True
            time.sleep(0.05)
        return False

    # ------------------------------------------------------------------ security
    def test_index_injects_token_and_sets_csp(self):
        st, body, hdr = self.req("GET", "/", raw=True)
        self.assertEqual(st, 200)
        self.assertIn(self.srv.token.encode(), body)
        self.assertIn("default-src 'self'", hdr["Content-Security-Policy"])

    def test_host_header_checked(self):
        st, _, _ = self.req("GET", "/api/state", headers={"Host": "evil.example:80"})
        self.assertEqual(st, 421)

    def test_mutations_need_token_origin_and_json(self):
        self.assertEqual(self.req("POST", "/api/control/scan", {"enabled": True},
                                  {"Content-Type": "application/json"})[0], 403)
        self.assertEqual(self.post("/api/control/scan", {"enabled": True}, Origin="https://evil.example")[0], 403)
        self.assertEqual(self.post("/api/control/scan", {"enabled": True}, **{"Sec-Fetch-Site": "cross-site"})[0], 403)
        self.assertEqual(self.post("/api/control/scan", {"enabled": True}, **{"Content-Type": "text/plain"})[0], 403)
        self.assertEqual(self.post("/api/control/scan", {"enabled": "yes"})[0], 400)
        st, b, _ = self.post("/api/control/scan", {"enabled": True}, Origin=f"http://{self.host}")
        self.assertEqual((st, b["scanning"]), (200, True))

    def test_static_path_traversal_refused(self):
        self.assertEqual(self.req("GET", "/static/../pandabrain/config.py", raw=True)[0], 404)
        self.assertEqual(self.req("GET", "/static/%2e%2e/README.md", raw=True)[0], 404)

    def test_live_execution_refused_and_logged(self):
        st, b, _ = self.post("/api/execution/live", {"confirm": True})
        self.assertEqual(st, 403)
        self.assertIn("SHADOW_ONLY", b["error"])
        self.assertTrue(self.db.q1("SELECT 1 FROM events WHERE kind='live_refused'"))

    # ------------------------------------------------------------------ settings
    def test_versioned_settings(self):
        st, cfg, _ = self.req("GET", "/api/settings")
        v = cfg["version"]
        st, b, _ = self.post("/api/settings", {"values": {"min_liquidity_usd": -5, "nope": 1}, "expected_version": v})
        self.assertEqual(st, 422)
        self.assertIn("min_liquidity_usd", b["fields"])
        self.assertIn("nope", b["fields"])
        st, b, _ = self.post("/api/settings", {"values": {"supply_valuation_min_usd": 9e9, "supply_valuation_max_usd": 1}})
        self.assertEqual(st, 422)
        st, b, _ = self.post("/api/settings", {"values": {"min_liquidity_usd": 20000}, "expected_version": v, "note": "t"})
        self.assertEqual((st, b["version"], b["applies"]), (200, v + 1, "next cycle boundary"))
        st, b, _ = self.post("/api/settings", {"values": {"min_liquidity_usd": 1}, "expected_version": v})
        self.assertEqual(st, 422)                         # stale version refused
        self.assertEqual(self.req("GET", "/api/settings")[1]["values"]["min_liquidity_usd"], 20000)

    # ------------------------------------------------------------------ actions
    def test_run_once_updates_backend_and_stream(self):
        st, b, _ = self.post("/api/control/run-once")
        self.assertEqual(st, 202)
        self.assertTrue(self.wait_for(lambda: book.position(self.db) is not None))
        st, state, _ = self.req("GET", "/api/state")
        self.assertEqual(state["seats"]["SCAN"]["state"], "blocked")
        self.assertEqual(state["seats"]["CHIEF"]["state"], "holding")
        self.assertEqual(state["execution_mode"], "SHADOW_ONLY")
        self.assertTrue(state["demo"])
        self.assertIsNone(state["stats"]["win_rate"])
        rows = self.req("GET", "/api/watchlist?sort=liquidity_usd&dir=asc")[1]
        self.assertEqual(len(rows), 4)
        cand = self.req("GET", f"/api/candidate?chain=solana&pool={rows[0]['pool_address']}")[1]
        self.assertIn("evaluations", cand)
        ev = self.req("GET", "/api/events?q=SIMULATED")[1]
        self.assertTrue(any(e["kind"] == "paper_fill" for e in ev))
        # stream delivers state + events
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        c.request("GET", "/api/stream?after_id=1", headers={"Host": self.host})
        r = c.getresponse()
        buf = b""
        while b"event: state" not in buf or b"event: ev" not in buf:
            buf += r.read1(4096)
        c.close()
        self.assertIn(b"paper_fill", buf)

    def test_duplicate_actions_and_stop_guard(self):
        self.assertEqual(self.post("/api/worker/start")[0], 200)
        self.assertEqual(self.post("/api/worker/start")[0], 409)          # no duplicate workers
        codes = [self.post("/api/control/run-once")[0] for _ in range(3)]
        self.assertEqual(codes[0], 202)
        self.assertIn(409, codes[1:])                                      # double-click refused
        self.assertTrue(self.wait_for(lambda: book.position(self.db) is not None))
        # pause discovery: RISK keeps marking the open position
        self.post("/api/control/scan", {"enabled": False})
        self.db.x("UPDATE position SET last_risk_at=0")
        n0 = self.db.q1("SELECT count(*) n FROM marks")["n"]
        self.assertTrue(self.wait_for(lambda: self.db.q1("SELECT count(*) n FROM marks")["n"] > n0))
        st, b, _ = self.post("/api/worker/stop")
        self.assertEqual(st, 409)                                          # open position: confirm required
        self.assertIn("RISK", b["error"])
        st, b, _ = self.post("/api/worker/stop", {"confirm_unmonitored": True})
        self.assertEqual(st, 200)
        self.assertTrue(self.wait_for(lambda: self.req("GET", "/api/state")[1]["seats"]["RISK"]["state"] == "stopped"))

    def test_export_csv_neutralises_formulas(self):
        from pandabrain.store import emit
        emit(self.db, "SCAN", "x", "=HYPERLINK(\"http://evil\")")
        st, body, hdr = self.req("GET", "/api/events/export?format=csv", raw=True)
        self.assertEqual(st, 200)
        self.assertIn("attachment", hdr["Content-Disposition"])
        self.assertTrue(re.search(rb"'=HYPERLINK", body))
