"""Inventory of integrations: what is callable and documented vs disconnected.

Nothing here extracts browser sessions or private credentials. Messaging is opt-in only and is
hard-disabled while PANDABRAIN_NO_OUTBOUND=1 (set by the test suite and validation runs).
"""

from __future__ import annotations

import os

from .providers.http import Http, ProviderError
from .util import sanitize


def inventory() -> list[dict]:
    tg = TelegramNotifier.from_env(None)
    return [
        {"name": "GeckoTerminal", "role": "discovery, exact pools, token info", "status": "connected",
         "detail": "public API, no key"},
        {"name": "DEX Screener", "role": "exact-pair quotes", "status": "connected",
         "detail": "public API, no key; exact pair addresses only"},
        {"name": "Solana RPC", "role": "mint verification, largest accounts", "status": "connected",
         "detail": "SOLANA_RPC_URL or public mainnet endpoint"},
        {"name": "TypeSafe (TYPED_AI judge)", "role": "typed judgments",
         "status": "configured" if os.environ.get("TYPESAFE_API_KEY") else "unconfigured",
         "detail": "optional; key stays server-side"},
        {"name": "FOMO", "role": "venue / token metrics", "status": "disconnected",
         "detail": "no public documented API; browser-session extraction is not done"},
        {"name": "Grok bots / X", "role": "SOCIAL reads", "status": "disconnected",
         "detail": "no implemented, documented read adapter; cloud bot skills are not assumed"},
        {"name": "Telegram", "role": "optional notifications", "status": tg.status_label(),
         "detail": "Bot API sendMessage; needs TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID and "
                   "PANDABRAIN_TELEGRAM_OPT_IN=1"},
        {"name": "Live execution", "role": "real orders", "status": "blocked",
         "detail": "SHADOW_ONLY build; refused server-side"},
    ]


class TelegramNotifier:
    def __init__(self, http: Http | None, token: str, chat_id: str, opted_in: bool):
        self.http, self._token, self.chat_id, self.opted_in = http, token, chat_id, opted_in

    @classmethod
    def from_env(cls, http: Http | None):
        return cls(http, os.environ.get("TELEGRAM_BOT_TOKEN", "").strip(),
                   os.environ.get("TELEGRAM_CHAT_ID", "").strip(),
                   os.environ.get("PANDABRAIN_TELEGRAM_OPT_IN", "") == "1")

    @property
    def enabled(self) -> bool:
        return (bool(self._token and self.chat_id and self.opted_in)
                and os.environ.get("PANDABRAIN_NO_OUTBOUND", "") != "1")

    def status_label(self) -> str:
        if os.environ.get("PANDABRAIN_NO_OUTBOUND", "") == "1":
            return "suppressed"
        if not (self._token and self.chat_id):
            return "unconfigured"
        return "enabled" if self.opted_in else "configured, not opted in"

    def send(self, text: str) -> bool:
        if not self.enabled or self.http is None:
            return False
        try:
            self.http.post_json("telegram", f"https://api.telegram.org/bot{self._token}/sendMessage",
                                {"chat_id": self.chat_id, "text": sanitize(text)[:3500],
                                 "disable_web_page_preview": True}, timeout=10, max_wait=5)
            return True
        except ProviderError:
            return False
