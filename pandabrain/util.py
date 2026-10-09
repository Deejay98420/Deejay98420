"""Small shared helpers: time, Decimal parsing, base58, sanitising."""

from __future__ import annotations

import hashlib
import json
import math
import re
import time
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation

_B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
_B58_RE = re.compile(r"^[1-9A-HJ-NP-Za-km-z]{32,44}$")
_EVM_RE = re.compile(r"^0x[0-9a-fA-F]{40}$")


def now() -> float:
    return time.time()


def iso(ts: float | None) -> str | None:
    if ts is None:
        return None
    return datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso(s: str | None) -> float | None:
    if not s:
        return None
    try:
        return datetime.fromisoformat(str(s).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def dec(x) -> Decimal | None:
    """Parse to a finite Decimal or return None. Never invents a value."""
    if x is None or isinstance(x, bool):
        return None
    try:
        d = Decimal(str(x))
    except (InvalidOperation, ValueError):
        return None
    if not d.is_finite():
        return None
    return d


def pos_dec(x) -> Decimal | None:
    d = dec(x)
    return d if d is not None and d > 0 else None


def fnum(x) -> float | None:
    """Finite float or None."""
    if x is None or isinstance(x, bool):
        return None
    try:
        f = float(x)
    except (TypeError, ValueError):
        return None
    return f if math.isfinite(f) else None


def inum(x) -> int | None:
    if x is None or isinstance(x, bool):
        return None
    try:
        f = float(x)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(f) or f != int(f):
        return None
    return int(f)


def b58encode(b: bytes) -> str:
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = _B58[r] + out
    pad = len(b) - len(b.lstrip(b"\0"))
    return "1" * pad + out


def b58decode(s: str) -> bytes:
    if not isinstance(s, str) or not s or any(c not in _B58 for c in s):
        raise ValueError("invalid base58")
    n = 0
    for c in s:
        n = n * 58 + _B58.index(c)
    body = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    pad = len(s) - len(s.lstrip("1"))
    return b"\0" * pad + body


def is_solana_address(s: str) -> bool:
    return isinstance(s, str) and bool(_B58_RE.match(s))


def is_evm_address(s: str) -> bool:
    return isinstance(s, str) and bool(_EVM_RE.match(s))


def stable_candidate_id(chain: str, pool: str) -> str:
    """Stable ID for a candidate. Tickers are not identities."""
    return "c_" + hashlib.sha256(f"{chain}:{pool}".encode()).hexdigest()[:12]


_SECRET_PATTERNS = [
    (re.compile(r"(?i)bearer\s+[A-Za-z0-9._\-]+"), "Bearer [redacted]"),
    (re.compile(r"\bts-[A-Za-z0-9_\-]{6,}"), "ts-[redacted]"),
    (re.compile(r"\bbot\d{5,}:[A-Za-z0-9_\-]{20,}"), "bot[redacted]"),
    (re.compile(r"(?i)(api[-_]?key|token|secret|password)=([^&\s\"']+)"), r"\1=[redacted]"),
]


def sanitize(text) -> str:
    """Remove anything that looks like a credential. Applied to every logged error."""
    s = str(text)
    for pat, rep in _SECRET_PATTERNS:
        s = pat.sub(rep, s)
    return s[:500]


def redact_url(url: str) -> str:
    """Show only scheme and host of a URL (RPC URLs often embed keys in path or query)."""
    m = re.match(r"^(https?://[^/?#]+)", url or "")
    return (m.group(1) + "/…") if m else "[invalid url]"


def jdump(obj) -> str:
    return json.dumps(obj, default=str, separators=(",", ":"), sort_keys=True)


def jload(s, default=None):
    if s is None:
        return default
    try:
        return json.loads(s)
    except (TypeError, ValueError):
        return default
