"""Command line: serve, run-once, status, migrate, export-events.

  python3 -m pandabrain serve [--demo] [--port 8765]
  python3 -m pandabrain run-once [--demo]
  python3 -m pandabrain status [--demo]
  python3 -m pandabrain migrate [--demo]
  python3 -m pandabrain export-events [--demo] [--format csv|json]
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import signal
import sys

from . import EXECUTION_MODE, __version__, api, store
from .db import ModeMismatch, open_book
from .engine import Desk, run_once_cli
from .providers import Http

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load_env(path: str):
    """Minimal .env reader (KEY=VALUE lines). Existing environment variables win."""
    if not os.path.isfile(path):
        return
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            k, v = k.strip(), v.strip().strip('"').strip("'")
            if k and k not in os.environ:
                os.environ[k] = v


def data_dir(demo: bool) -> str:
    base = os.environ.get("PANDABRAIN_DATA_DIR") or os.path.join(ROOT, "data")
    return os.path.join(base, "demo" if demo else "public")


def make_desk(demo: bool) -> Desk:
    db = open_book(data_dir(demo), "demo" if demo else "public")
    if demo:
        from .providers.fixtures import FixtureWorld
        world = FixtureWorld()
        desk = Desk(db, Http(transport=world), demo=True)
        if store.get_control(db, "demo_settings") != "1":
            store.save_settings(db, {"cycle_seconds": 45, "risk_poll_seconds": 15, "max_hold_minutes": 6,
                                     "max_quote_age_seconds": 120}, "demo pacing", None)
            store.set_control(db, "demo_settings", "1")
        return desk
    return Desk(db)


def main(argv=None):
    load_env(os.path.join(ROOT, ".env"))
    ap = argparse.ArgumentParser(prog="pandabrain", description=f"PandaBrain Desk {__version__} ({EXECUTION_MODE})")
    ap.add_argument("command", choices=["serve", "run-once", "status", "migrate", "export-events"])
    ap.add_argument("--demo", action="store_true", help="synthetic fixture world in its own book")
    ap.add_argument("--host", default=os.environ.get("PANDABRAIN_HOST", "127.0.0.1"))
    ap.add_argument("--port", type=int, default=int(os.environ.get("PANDABRAIN_PORT", "8765")))
    ap.add_argument("--no-worker", action="store_true")
    ap.add_argument("--format", choices=["json", "csv"], default="json")
    a = ap.parse_args(argv)

    if a.host not in ("127.0.0.1", "localhost", "::1") and os.environ.get("PANDABRAIN_ALLOW_REMOTE") != "1":
        print("refusing to bind a non-local address (set PANDABRAIN_ALLOW_REMOTE=1 to override)", file=sys.stderr)
        return 2
    try:
        desk = make_desk(a.demo)
    except ModeMismatch as e:
        print(str(e), file=sys.stderr)
        return 2

    if a.command == "migrate":
        print(json.dumps({"schema": desk.db.current_version(), "mode": desk.db.mode, "path": desk.db.path}))
        return 0
    if a.command == "status":
        print(json.dumps({k: v for k, v in api.state(desk, _NoWorker()).items()
                          if k in ("version", "execution_mode", "book_mode", "judge", "account", "stats",
                                   "watchlist_counts", "providers", "funnel")}, indent=2, default=str))
        return 0
    if a.command == "run-once":
        print(json.dumps(run_once_cli(desk), indent=2, default=str))
        return 0
    if a.command == "export-events":
        rows = api.events(desk.db, limit=2000)
        print(api.events_csv(rows) if a.format == "csv" else json.dumps(rows, indent=1, default=str))
        return 0

    # serve: one server per book (file lock), localhost by default
    lock_path = os.path.join(data_dir(a.demo), "server.lock")
    lock = open(lock_path, "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        print("another PandaBrain server is already running on this book", file=sys.stderr)
        return 3
    lock.write(str(os.getpid()))
    lock.flush()
    from .server import serve
    srv = serve(desk, a.host, a.port, start_worker=not a.no_worker)
    pid_path = os.path.join(data_dir(a.demo), "server.pid")
    with open(pid_path, "w") as f:
        f.write(str(os.getpid()))

    def shutdown(*_):
        srv.shutting_down.set()
        srv.worker.stop(timeout=60) if srv.worker.alive else None
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, shutdown)
    mode = "DEMO (synthetic)" if a.demo else "PUBLIC data"
    print(f"PandaBrain Desk {__version__} | {EXECUTION_MODE} | {mode}\n  http://{a.host}:{srv.server_address[1]}/",
          flush=True)
    try:
        srv.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    finally:
        srv.shutting_down.set()
        if srv.worker.alive:
            srv.worker.stop(timeout=60)
        srv.server_close()
        try:
            os.remove(pid_path)
        except OSError:
            pass
    return 0


class _NoWorker:
    def status(self):
        return {"alive": False, "state": "cli"}


if __name__ == "__main__":
    sys.exit(main())
