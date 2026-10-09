"""Local dashboard server (stdlib only).

Security model for a local control surface:
  * binds 127.0.0.1 by default;
  * every request must carry an allowed Host header (blocks DNS-rebinding);
  * mutations are POST-only, need JSON, the per-run X-Desk-Token (injected into the page),
    a same-origin Origin when present, and Sec-Fetch-Site same-origin/none when present;
  * request bodies are size-limited and validated; errors are sanitised; no route executes
    commands or exposes secrets; live execution is always refused.
"""

from __future__ import annotations

import json
import mimetypes
import os
import secrets
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from . import api, store
from .config import SettingsError, schema_for_ui
from .execution import LiveExecutionBlocked, request_live_execution
from .util import jdump, sanitize
from .worker import Busy, Worker

STATIC = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "static")
MAX_BODY = 64 * 1024
MAX_STREAMS = 8


class DeskServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, desk, worker: Worker):
        super().__init__(addr, Handler)
        self.desk, self.worker = desk, worker
        self.token = secrets.token_urlsafe(24)
        host, port = self.server_address[0], self.server_address[1]
        self.allowed_hosts = {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}
        if host not in ("127.0.0.1", "localhost", "::1"):
            self.allowed_hosts.add(f"{host}:{port}")
        self.allowed_origins = {f"http://{h}" for h in self.allowed_hosts}
        self.streams = 0
        self.streams_lock = threading.Lock()
        self.shutting_down = threading.Event()
        self.action_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    server: DeskServer
    protocol_version = "HTTP/1.1"
    server_version = "PandaBrainDesk"
    sys_version = ""

    def log_message(self, fmt, *args):  # quiet, and never log query strings
        pass

    def finish(self):
        try:
            super().finish()
        finally:
            self.server.desk.db.close()      # request threads are short-lived; close their connection

    # ------------------------------------------------------------------ helpers
    def _headers(self, code: int, ctype: str, length: int | None, extra: dict | None = None):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        if length is not None:
            self.send_header("Content-Length", str(length))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy",
                         "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; "
                         "connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()

    def _json(self, code: int, obj):
        body = jdump(obj).encode()
        self._headers(code, "application/json; charset=utf-8", len(body))
        self.wfile.write(body)

    def _err(self, code: int, msg: str):
        self._json(code, {"error": sanitize(msg)})

    def _host_ok(self) -> bool:
        return self.headers.get("Host", "") in self.server.allowed_hosts

    def _mutation_ok(self) -> str | None:
        if self.headers.get("X-Desk-Token", "") != self.server.token:
            return "missing or bad desk token"
        origin = self.headers.get("Origin")
        if origin is not None and origin not in self.server.allowed_origins:
            return "cross-origin request refused"
        sfs = self.headers.get("Sec-Fetch-Site")
        if sfs is not None and sfs not in ("same-origin", "none"):
            return "cross-site request refused"
        if not (self.headers.get("Content-Type", "").split(";")[0].strip() == "application/json"):
            return "content-type must be application/json"
        return None

    def _body(self) -> dict:
        try:
            n = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            raise ValueError("bad content-length") from None
        if n > MAX_BODY:
            raise ValueError("body too large")
        raw = self.rfile.read(n) if n else b"{}"
        obj = json.loads(raw.decode("utf-8") or "{}")
        if not isinstance(obj, dict):
            raise ValueError("body must be an object")
        return obj

    # ------------------------------------------------------------------ GET
    def do_GET(self):
        if not self._host_ok():
            return self._err(421, "host not allowed")
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        desk, db = self.server.desk, self.server.desk.db
        try:
            if u.path == "/" or u.path == "/index.html":
                return self._index()
            if u.path.startswith("/static/"):
                return self._static(u.path[len("/static/"):])
            if u.path == "/api/state":
                return self._json(200, api.state(desk, self.server.worker))
            if u.path == "/api/health":
                return self._json(200, {"ok": True, "worker": self.server.worker.status()["state"],
                                        "providers": store.providers(db), "book_mode": db.mode})
            if u.path == "/api/watchlist":
                return self._json(200, api.watchlist_rows(db, q.get("sort", "last_checked_at"), q.get("dir", "desc"),
                                                          q.get("q", ""), q.get("status", ""),
                                                          int(q.get("limit", "200") or 200)))
            if u.path == "/api/candidate":
                d = api.candidate_detail(db, q.get("chain", ""), q.get("pool", ""))
                return self._json(200, d) if d else self._err(404, "unknown candidate")
            if u.path == "/api/events":
                return self._json(200, api.events(db, q.get("q", ""), q.get("seat", ""), q.get("level", ""),
                                                  int(q.get("after_id", "0") or 0), int(q.get("limit", "200") or 200)))
            if u.path == "/api/events/export":
                rows = api.events(db, q.get("q", ""), q.get("seat", ""), q.get("level", ""), 0, 2000)
                if q.get("format") == "csv":
                    body = api.events_csv(rows).encode()
                    self._headers(200, "text/csv; charset=utf-8", len(body),
                                  {"Content-Disposition": 'attachment; filename="pandabrain-events.csv"'})
                    return self.wfile.write(body)
                body = jdump(rows).encode()
                self._headers(200, "application/json", len(body),
                              {"Content-Disposition": 'attachment; filename="pandabrain-events.json"'})
                return self.wfile.write(body)
            if u.path == "/api/trades":
                return self._json(200, api.book.trades(db))
            if u.path == "/api/equity":
                return self._json(200, [dict(r) for r in db.q(
                    "SELECT at, equity_usd, cash_usd, position_value_usd, basis FROM equity ORDER BY id DESC LIMIT 500")][::-1])
            if u.path == "/api/cycles":
                return self._json(200, api.cycles(db))
            if u.path == "/api/settings":
                v, st = store.active_settings(db)
                hist = [dict(r) for r in db.q("SELECT version, created_at, note FROM settings_versions "
                                              "ORDER BY version DESC LIMIT 30")]
                return self._json(200, {"version": v, "values": st, "schema": schema_for_ui(), "history": hist})
            if u.path == "/api/stream":
                return self._stream(int(q.get("after_id", "0") or 0))
            return self._err(404, "not found")
        except (ValueError, KeyError) as e:
            return self._err(400, f"bad request: {e}")
        except (BrokenPipeError, ConnectionResetError):
            return None
        except Exception as e:  # noqa: BLE001 - sanitised
            return self._err(500, f"server error: {type(e).__name__}")

    def _index(self):
        with open(os.path.join(STATIC, "index.html"), "rb") as f:
            html = f.read().replace(b"__DESK_TOKEN__", self.server.token.encode())
        self._headers(200, "text/html; charset=utf-8", len(html))
        self.wfile.write(html)

    def _static(self, rel: str):
        root = os.path.realpath(STATIC)
        p = os.path.realpath(os.path.join(root, rel))
        if not p.startswith(root + os.sep) or not os.path.isfile(p):
            return self._err(404, "not found")
        ctype = mimetypes.guess_type(p)[0] or "application/octet-stream"
        with open(p, "rb") as f:
            body = f.read()
        self._headers(200, ctype, len(body))
        self.wfile.write(body)

    def _stream(self, after_id: int):
        with self.server.streams_lock:
            if self.server.streams >= MAX_STREAMS:
                return self._err(429, "too many streams")
            self.server.streams += 1
        try:
            self.close_connection = True
            self._headers(200, "text/event-stream", None, {"Connection": "close"})
            db = self.server.desk.db
            last = after_id or (db.q1("SELECT max(id) AS m FROM events")["m"] or 0)
            last_state = 0.0
            while not self.server.shutting_down.is_set():
                for e in api.events(db, after_id=last, limit=200):
                    last = e["id"]
                    self.wfile.write(f"id: {e['id']}\nevent: ev\ndata: {jdump(e)}\n\n".encode())
                if time.time() - last_state > 2:
                    last_state = time.time()
                    s = api.state(self.server.desk, self.server.worker)
                    self.wfile.write(f"event: state\ndata: {jdump(s)}\n\n".encode())
                self.wfile.flush()
                time.sleep(0.5)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            with self.server.streams_lock:
                self.server.streams -= 1

    # ------------------------------------------------------------------ POST
    def do_POST(self):
        if not self._host_ok():
            return self._err(421, "host not allowed")
        bad = self._mutation_ok()
        if bad:
            return self._err(403, bad)
        u = urlparse(self.path)
        w, db = self.server.worker, self.server.desk.db
        try:
            body = self._body()
        except (ValueError, UnicodeDecodeError) as e:
            return self._err(400, f"bad request: {e}")
        try:
            if u.path == "/api/execution/live":
                request_live_execution(body)
            if not self.server.action_lock.acquire(blocking=False):
                return self._err(409, "another control action is in progress")
            try:
                return self._mutate(u.path, body, w, db)
            finally:
                self.server.action_lock.release()
        except LiveExecutionBlocked as e:
            store.emit(db, "FILLS", "live_refused", f"live execution request refused: {e}", level="warn")
            return self._err(403, str(e))
        except Exception as e:  # noqa: BLE001
            return self._err(500, f"server error: {type(e).__name__}")

    def _mutate(self, path, body, w: Worker, db):
        if path == "/api/control/scan":
            on = body.get("enabled")
            if not isinstance(on, bool):
                return self._err(400, "enabled must be true or false")
            w.set_scanning(on)
            return self._json(200, {"ok": True, "scanning": on, "worker_running": w.alive})
        if path == "/api/control/run-once":
            try:
                return self._json(202, w.request_run_once())
            except Busy as e:
                return self._err(409, str(e))
        if path == "/api/worker/start":
            r = w.start()
            return self._json(200 if r["ok"] else 409, r)
        if path == "/api/worker/stop":
            from . import book
            if book.position(db) and body.get("confirm_unmonitored") is not True:
                return self._err(409, "a paper position is open; stopping the worker stops RISK monitoring. "
                                      "Resend with confirm_unmonitored=true to proceed.")
            r = w.stop()
            return self._json(200 if r["ok"] else 409, r)
        if path == "/api/settings":
            patch = body.get("values")
            exp = body.get("expected_version")
            if not isinstance(patch, dict) or (exp is not None and not isinstance(exp, int)):
                return self._err(400, "values must be an object; expected_version an integer")
            note = body.get("note") if isinstance(body.get("note"), str) else None
            try:
                v = store.save_settings(db, patch, note, exp)
            except SettingsError as e:
                return self._json(422, {"error": "invalid settings", "fields": e.errors})
            store.emit(db, "CHIEF", "settings_saved", f"settings v{v} saved; applies at the next cycle boundary",
                       data={"changed": sorted(patch)})
            return self._json(200, {"ok": True, "version": v, "applies": "next cycle boundary"})
        return self._err(404, "not found")


def serve(desk, host: str = "127.0.0.1", port: int = 8765, start_worker: bool = True) -> DeskServer:
    worker = Worker(desk)
    srv = DeskServer((host, port), desk, worker)
    if start_worker:
        worker.start()
    return srv
