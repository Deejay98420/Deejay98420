"""DEX Screener public API (no key). Only exact pair addresses are requested, so a quote can
never be blended from an unrelated pool of the same token.

  GET /latest/dex/pairs/{chainId}/{pairAddresses}   (comma separated, up to 30)
"""

from __future__ import annotations

from urllib.parse import quote

from ..util import dec, fnum, inum
from .http import Budget, Http, ProviderError

BASE = "https://api.dexscreener.com"
NAME = "dexscreener"


def parse_pair(p: dict, chain: str, fetched_at: float, provider_ts: float | None) -> dict | None:
    pool = p.get("pairAddress")
    token = (p.get("baseToken") or {}).get("address")
    if not pool or not token or p.get("chainId") != chain:
        return None
    tx = p.get("txns") or {}
    vol = p.get("volume") or {}
    price = dec(p.get("priceUsd"))
    mcap = dec(p.get("marketCap"))
    fdv = dec(p.get("fdv"))
    created = fnum(p.get("pairCreatedAt"))

    def t(w, s):
        return inum((tx.get(w) or {}).get(s))

    return {
        "chain": chain, "pool_address": pool, "token_address": token,
        "symbol": ((p.get("baseToken") or {}).get("symbol") or None),
        "name": ((p.get("baseToken") or {}).get("name") or None),
        "dex": p.get("dexId"), "source": NAME, "fetched_at": fetched_at, "provider_ts": provider_ts,
        "price_usd": str(price) if price is not None else None,
        "liquidity_usd": fnum((p.get("liquidity") or {}).get("usd")),
        "volume_h1": fnum(vol.get("h1")), "volume_h6": fnum(vol.get("h6")),
        "volume_h24": fnum(vol.get("h24")),
        "buys_h1": t("h1", "buys"), "sells_h1": t("h1", "sells"),
        "buys_h6": t("h6", "buys"), "sells_h6": t("h6", "sells"),
        "buys_h24": t("h24", "buys"), "sells_h24": t("h24", "sells"),
        "mcap_usd": str(mcap) if mcap is not None and mcap > 0 else None,
        "mcap_source": NAME if mcap is not None and mcap > 0 else None,
        "fdv_usd": str(fdv) if fdv is not None else None,
        "pool_created_at": (created / 1000.0) if created and created > 1e11 else created,
    }


class DexScreener:
    def __init__(self, http: Http, base: str = BASE):
        self.http, self.base = http, base

    def pairs(self, chain: str, pools: list[str], budget: Budget | None, fetched_at: float) -> list[dict]:
        if not pools:
            return []
        if len(pools) > 30:
            raise ValueError("at most 30 pairs per call")
        joined = ",".join(quote(p, safe="") for p in pools)
        body, r = self.http.get_json(NAME, f"{self.base}/latest/dex/pairs/{quote(chain)}/{joined}",
                                     budget=budget)
        if not isinstance(body, dict):
            raise ProviderError(NAME, "bad_response", "expected an object")
        raw = body.get("pairs") or ([body["pair"]] if body.get("pair") else [])
        wanted = set(pools)
        out = []
        for p in raw:
            snap = parse_pair(p, chain, fetched_at, r.date_ts())
            if snap and snap["pool_address"] in wanted:
                out.append(snap)
        return out
