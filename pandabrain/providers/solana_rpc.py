"""Solana JSON-RPC. Endpoint is configurable via SOLANA_RPC_URL; it is never logged in full
(paid RPC URLs embed keys), only its host.
"""

from __future__ import annotations

import os

from ..util import redact_url
from .http import Budget, Http, ProviderError

DEFAULT_RPC = "https://api.mainnet-beta.solana.com"
NAME = "solana_rpc"


def rpc_url() -> str:
    return os.environ.get("SOLANA_RPC_URL", "").strip() or DEFAULT_RPC


class SolanaRpc:
    def __init__(self, http: Http, url: str | None = None):
        self.http = http
        self.url = url or rpc_url()

    @property
    def display(self) -> str:
        return redact_url(self.url)

    def call(self, method: str, params: list, budget: Budget | None):
        body, _ = self.http.post_json(NAME, self.url, {"jsonrpc": "2.0", "id": 1, "method": method,
                                                       "params": params}, budget=budget)
        if not isinstance(body, dict):
            raise ProviderError(NAME, "bad_response", "expected an object")
        if body.get("error"):
            err = body["error"]
            code = err.get("code") if isinstance(err, dict) else None
            if code in (-32005, 429):
                raise ProviderError(NAME, "rate_limited", f"rpc error {code}")
            raise ProviderError(NAME, "bad_response", f"rpc error {code}")
        if "result" not in body:
            raise ProviderError(NAME, "bad_response", "missing result")
        return body["result"]

    def account_parsed(self, addr: str, budget: Budget | None):
        return self.call("getAccountInfo", [addr, {"encoding": "jsonParsed", "commitment": "confirmed"}], budget)

    def account_raw(self, addr: str, budget: Budget | None):
        return self.call("getAccountInfo", [addr, {"encoding": "base64", "commitment": "confirmed"}], budget)

    def largest_accounts(self, mint: str, budget: Budget | None):
        return self.call("getTokenLargestAccounts", [mint, {"commitment": "confirmed"}], budget)
