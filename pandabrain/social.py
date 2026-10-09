"""SOCIAL seat: reads only the exact handle the project published (GeckoTerminal token info).

No adapter that can actually read X is implemented, so evidence is recorded as missing with the
reason, and SIZE applies the documented no_social_cut. Nothing is searched for, nothing is
substituted, no follower/engagement numbers are ever invented.
"""

from __future__ import annotations

import re

_HANDLE = re.compile(r"^[A-Za-z0-9_]{1,15}$")


def clean_handle(h) -> str | None:
    """'LuffyX100X/status/123' -> 'LuffyX100X'. URL forms are reduced to the first path segment."""
    if not h or not isinstance(h, str):
        return None
    s = h.strip()
    s = re.sub(r"^(https?://)?(www\.)?(x|twitter)\.com/", "", s, flags=re.I)
    s = s.lstrip("@").split("?")[0].split("/")[0]
    return s if _HANDLE.match(s) else None


class SocialAdapter:
    name = "none"
    connected = False

    def status(self) -> dict:
        return {"name": self.name, "connected": self.connected}

    def read(self, handle: str) -> dict | None:
        raise NotImplementedError


class DisconnectedXAdapter(SocialAdapter):
    name = "x"
    connected = False

    def status(self) -> dict:
        return {"name": "X / Grok plugin", "connected": False,
                "detail": "disconnected: no implemented, documented X read adapter is configured. "
                          "Social evidence is recorded as missing; size is cut per policy."}

    def read(self, handle):
        return None


def social_evidence(raw_handle, adapter: SocialAdapter) -> dict:
    handle = clean_handle(raw_handle)
    base = {"handle": handle, "handle_source": "geckoterminal token info (project-published field)",
            "raw_handle_present": bool(raw_handle), "adapter": adapter.name}
    if not handle:
        return {**base, "status": "no_handle", "account": None}
    if not adapter.connected:
        return {**base, "status": "adapter_disconnected", "account": None}
    acct = adapter.read(handle)
    if not acct:
        return {**base, "status": "not_found", "account": None}
    if str(acct.get("handle", "")).lower() != handle.lower():
        return {**base, "status": "identity_mismatch", "account": None}
    return {**base, "status": "read", "account": acct}
