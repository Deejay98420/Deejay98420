"""Background worker: one per book, guarded by the database lease and an in-process lock.

Pausing discovery never pauses RISK: while the worker runs, an open paper position is
monitored on its own schedule whatever the scanning switch says.
"""

from __future__ import annotations

import threading
import traceback

from . import book, store
from .engine import Desk
from .store import Lease, LeaseLost, emit, set_seat
from .util import now, sanitize


class Busy(RuntimeError):
    pass


class Worker:
    def __init__(self, desk: Desk, tick: float = 1.0):
        self.desk = desk
        self.db = desk.db
        self.tick = tick
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._run_once = threading.Event()
        self._busy = threading.Lock()
        self._ctl = threading.Lock()
        self.lease = Lease(self.db)
        self.state = "stopped"
        self.activity = None
        self.last_error = None
        self.last_cycle = None
        self.next_cycle_at = None

    # ------------------------------------------------------------------ controls
    @property
    def alive(self) -> bool:
        return bool(self._thread and self._thread.is_alive())

    def start(self) -> dict:
        with self._ctl:
            if self.alive:
                return {"ok": False, "error": "worker already running"}
            if not self.lease.acquire():
                return {"ok": False, "error": "another process holds the engine lease"}
            self._stop.clear()
            self.state = "running"
            self._thread = threading.Thread(target=self._loop, name="pandabrain-worker", daemon=True)
            self._thread.start()
            emit(self.db, "CHIEF", "worker_start", "worker started")
            return {"ok": True}

    def stop(self, timeout: float = 30.0) -> dict:
        with self._ctl:
            if not self.alive:
                return {"ok": False, "error": "worker is not running"}
            self._stop.set()
            self._thread.join(timeout)
            if self._thread.is_alive():
                return {"ok": False, "error": "worker is finishing a step; try again shortly"}
            self.state = "stopped"
            return {"ok": True}

    def scanning(self) -> bool:
        return store.get_control(self.db, "scanning", "0") == "1"

    def set_scanning(self, on: bool):
        store.set_control(self.db, "scanning", "1" if on else "0")
        emit(self.db, "CHIEF", "scan_" + ("resumed" if on else "paused"),
             "discovery " + ("resumed" if on else "paused") + ("" if on else "; RISK monitoring continues"))
        if on and self.next_cycle_at is None:
            self.next_cycle_at = now()
        if not on and not book.position(self.db):
            set_seat(self.db, "SCAN", "idle", "paused by operator")

    def request_run_once(self) -> dict:
        if self.activity is not None or self._run_once.is_set():
            raise Busy("a cycle is already running or queued")
        if self.alive:
            self._run_once.set()
            return {"ok": True, "queued": True}
        # no background worker: run inline in a short-lived thread with its own lease
        if self._busy.locked():
            raise Busy("a cycle is already running")

        def job():
            lease = Lease(self.db)
            if not lease.acquire():
                emit(self.db, "CHIEF", "run_once_refused", "another process holds the engine lease", "warn")
                return
            try:
                self._step(lease, force=True)
            finally:
                lease.release()
                self.db.close()
        threading.Thread(target=job, name="pandabrain-run-once", daemon=True).start()
        return {"ok": True, "queued": False}

    def status(self) -> dict:
        return {"alive": self.alive, "state": self.state if self.alive else "stopped",
                "busy": self._busy.locked(), "activity": self.activity, "scanning": self.scanning(),
                "run_once_queued": self._run_once.is_set(), "last_error": self.last_error,
                "last_cycle": self.last_cycle, "next_cycle_at": self.next_cycle_at,
                "lease": store.lease_info(self.db)}

    # ------------------------------------------------------------------ loop
    def _step(self, lease: Lease, force: bool = False):
        if not self._busy.acquire(blocking=False):
            return
        try:
            _, st = store.active_settings(self.db)
            if book.position(self.db):
                self.activity = "risk"
                self.desk.risk_tick(lease, force=force)
            elif force or (self.scanning() and (self.next_cycle_at is None or now() >= self.next_cycle_at)):
                self.activity = "cycle"
                r = self.desk.run_cycle(lease, kind="manual" if force else "scheduled")
                self.last_cycle = {"id": r.get("cycle_id"), "outcome": r.get("outcome"), "reason": r.get("reason"),
                                   "at": now()}
                self.next_cycle_at = now() + st["cycle_seconds"]
            self.last_error = None
        except LeaseLost:
            raise
        except Exception as e:  # never kill the worker on one bad cycle; record it
            self.last_error = sanitize(f"{type(e).__name__}: {e}")
            emit(self.db, "CHIEF", "cycle_error", self.last_error, level="error",
                 data={"trace": sanitize(traceback.format_exc()[-400:])})
            set_seat(self.db, "CHIEF", "error", self.last_error)
        finally:
            self.activity = None
            self._busy.release()

    def _loop(self):
        try:
            while not self._stop.is_set():
                try:
                    self.lease.renew(force=True)
                except LeaseLost:
                    self.state = "standby"
                    if not self.lease.acquire():
                        self._stop.wait(5)
                        continue
                    self.state = "running"
                force = self._run_once.is_set()
                try:
                    self._step(self.lease, force=force)
                except LeaseLost:
                    self.state = "standby"
                finally:
                    if force:
                        self._run_once.clear()
                self._stop.wait(self.tick)
        finally:
            self.lease.release()
            self.state = "stopped"
            emit(self.db, "CHIEF", "worker_stop", "worker stopped"
                 + ("; WARNING: open paper position is no longer monitored" if book.position(self.db) else ""),
                 level="warn" if book.position(self.db) else "info")
            if book.position(self.db):
                set_seat(self.db, "RISK", "stopped", "worker stopped: position NOT monitored")
            self.db.close()
