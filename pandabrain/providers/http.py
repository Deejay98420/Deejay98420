"""HTTP transport with per-host rate limits and per-cycle request budgets.

The transport is injectable. Demo mode and tests swap in a fixture transport so the exact same
provider code paths run against synthetic data, never against the public book.
"""

from __future__ import annotations

import json
import threading
import time
import urllib.error
import urllib.request
from collections import deque
from dataclasses import dataclass, field
from email.utils import parsedate_to_datetime

from ..util import sanitize

USER_AGENT = "PandaBrainDesk/3.0.1 (+local paper desk)"


class ProviderError(Exception):
    """kind: rate_limited | unavailable | bad_response | not_found | budget"""

    def __init__(self, provider: str, kind: str, message: str):
        super().__init__(sanitize(f"{provider}: {kind}: {message}"))
        self.provider, self.kind = provider, kind


@dataclass
class Response:
    status: int
    body: bytes
    headers: dict = field(default_factory=dict)

    def json(self):
        return json.loads(self.body.decode("utf-8"))

    def date_ts(self) -> float | None:
        d = self.headers.get("date") or self.headers.get("Date")
        if not d:
            return None
        try:
            return parsedate_to_datetime(d).timestamp()
        except (TypeError, ValueError):
            return None


class UrllibTransport:
    def __call__(self, method: str, url: str, headers: dict, body: bytes | None, timeout: float) -> Response:
        req = urllib.request.Request(url, data=body, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return Response(r.status, r.read(2_000_000), {k.lower(): v for k, v in r.headers.items()})
        except urllib.error.HTTPError as e:
            return Response(e.code, e.read(20_000) if e.fp else b"", {k.lower(): v for k, v in (e.headers or {}).items()})


class RateLimiter:
    """Sliding one-minute window per host. Waits up to max_wait, then raises rate_limited."""

    def __init__(self):
        self._lock = threading.Lock()
        self._hits: dict[str, deque] = {}
        self._limits: dict[str, int] = {}
        self._cooldown: dict[str, float] = {}

    def set_limit(self, host: str, per_minute: int):
        self._limits[host] = per_minute

    def cooldown(self, host: str, seconds: float):
        with self._lock:
            self._cooldown[host] = time.monotonic() + seconds

    def acquire(self, host: str, max_wait: float = 15.0, sleep=time.sleep):
        deadline = time.monotonic() + max_wait
        while True:
            with self._lock:
                t = time.monotonic()
                if self._cooldown.get(host, 0) > t:
                    wait = self._cooldown[host] - t
                else:
                    q = self._hits.setdefault(host, deque())
                    while q and t - q[0] >= 60:
                        q.popleft()
                    limit = self._limits.get(host, 60)
                    if len(q) < limit:
                        q.append(t)
                        return
                    wait = 60 - (t - q[0])
            if time.monotonic() + wait > deadline:
                raise ProviderError(host, "rate_limited", "local rate limit window full")
            sleep(min(wait, 1.0))


class Budget:
    """Per-cycle request budget per provider. Exhausting it ends work, it is not an error."""

    def __init__(self, limits: dict[str, int]):
        self.limits = dict(limits)
        self.spent = {k: 0 for k in limits}

    def left(self, name: str) -> int:
        return self.limits.get(name, 0) - self.spent.get(name, 0)

    def spend(self, name: str):
        if self.left(name) <= 0:
            raise ProviderError(name, "budget", "request budget for this cycle is spent")
        self.spent[name] = self.spent.get(name, 0) + 1


class Http:
    def __init__(self, transport=None, limiter: RateLimiter | None = None, sleep=time.sleep):
        self.transport = transport or UrllibTransport()
        self.limiter = limiter or RateLimiter()
        self.sleep = sleep

    def request(self, provider: str, method: str, url: str, *, budget: Budget | None = None,
                json_body=None, headers: dict | None = None, timeout: float = 20.0,
                max_wait: float = 15.0) -> Response:
        if budget is not None:
            budget.spend(provider)       # a failed call still costs its slot
        self.limiter.acquire(provider, max_wait=max_wait, sleep=self.sleep)
        h = {"User-Agent": USER_AGENT, "Accept": "application/json"}
        if headers:
            h.update(headers)
        body = None
        if json_body is not None:
            body = json.dumps(json_body).encode()
            h["Content-Type"] = "application/json"
        try:
            r = self.transport(method, url, h, body, timeout)
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            raise ProviderError(provider, "unavailable", f"{type(e).__name__}: {e}") from None
        if r.status == 429:
            ra = r.headers.get("retry-after")
            try:
                secs = min(300.0, float(ra)) if ra else 60.0
            except ValueError:
                secs = 60.0
            self.limiter.cooldown(provider, secs)
            raise ProviderError(provider, "rate_limited", f"HTTP 429, backing off {secs:.0f}s")
        if r.status == 404:
            raise ProviderError(provider, "not_found", "HTTP 404")
        if r.status >= 500 or r.status in (403, 408):
            raise ProviderError(provider, "unavailable", f"HTTP {r.status}")
        if r.status >= 400:
            raise ProviderError(provider, "bad_response", f"HTTP {r.status}")
        return r

    def get_json(self, provider: str, url: str, **kw):
        r = self.request(provider, "GET", url, **kw)
        try:
            return r.json(), r
        except (ValueError, UnicodeDecodeError):
            raise ProviderError(provider, "bad_response", "invalid JSON") from None

    def post_json(self, provider: str, url: str, body, **kw):
        r = self.request(provider, "POST", url, json_body=body, **kw)
        try:
            return r.json(), r
        except (ValueError, UnicodeDecodeError):
            raise ProviderError(provider, "bad_response", "invalid JSON") from None
