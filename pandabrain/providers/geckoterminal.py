"""GeckoTerminal public API (no key). Default budget assumes the free ~10 calls/minute.

Endpoints used (public v2):
  GET /networks/{net}/new_pools?page=N&include=base_token
  GET /networks/{net}/pools/multi/{a,b,...}?include=base_token   (exact pool identities)
  GET /networks/{net}/pools/{pool}?include=base_token
  GET /networks/{net}/tokens/{addr}/info
"""

from __future__ import annotations

from urllib.parse import quote

from ..util import dec, fnum, inum, parse_iso
from .http import Budget, Http, ProviderError

BASE = "https://api.geckoterminal.com/api/v2"
NAME = "geckoterminal"


def _strip_net(gid: str | None, net: str) -> str | None:
    if not gid or not isinstance(gid, str):
        return None
    pre = f"{net}_"
    return gid[len(pre):] if gid.startswith(pre) else None


def _txn(t: dict, w: str, side: str):
    return inum(((t or {}).get(w) or {}).get(side))


def parse_pool(item: dict, included: list, net: str, fetched_at: float, provider_ts: float | None) -> dict | None:
    a = item.get("attributes") or {}
    rel = item.get("relationships") or {}
    token = _strip_net(((rel.get("base_token") or {}).get("data") or {}).get("id"), net)
    pool = a.get("address") or _strip_net(item.get("id"), net)
    if not token or not pool:
        return None
    sym = name = None
    for inc in included or []:
        if inc.get("type") == "token" and _strip_net(inc.get("id"), net) == token:
            ia = inc.get("attributes") or {}
            sym, name = ia.get("symbol"), ia.get("name")
    vol = a.get("volume_usd") or {}
    tx = a.get("transactions") or {}
    price = dec(a.get("base_token_price_usd"))
    mcap = dec(a.get("market_cap_usd"))
    fdv = dec(a.get("fdv_usd"))
    out = {
        "chain": net, "pool_address": pool, "token_address": token,
        "symbol": (str(sym)[:32] if sym else None), "name": (str(name)[:64] if name else None),
        "dex": ((rel.get("dex") or {}).get("data") or {}).get("id"),
        "source": NAME, "fetched_at": fetched_at, "provider_ts": provider_ts,
        "price_usd": str(price) if price is not None else None,
        "liquidity_usd": fnum(a.get("reserve_in_usd")),
        "volume_h1": fnum(vol.get("h1")), "volume_h6": fnum(vol.get("h6")),
        "volume_h24": fnum(vol.get("h24")),
        "buys_h1": _txn(tx, "h1", "buys"), "sells_h1": _txn(tx, "h1", "sells"),
        "buys_h6": _txn(tx, "h6", "buys"), "sells_h6": _txn(tx, "h6", "sells"),
        "buys_h24": _txn(tx, "h24", "buys"), "sells_h24": _txn(tx, "h24", "sells"),
        "mcap_usd": str(mcap) if mcap is not None and mcap > 0 else None,
        "mcap_source": NAME if mcap is not None and mcap > 0 else None,
        "fdv_usd": str(fdv) if fdv is not None else None,
        "pool_created_at": parse_iso(a.get("pool_created_at")),
    }
    return out


class GeckoTerminal:
    def __init__(self, http: Http, base: str = BASE):
        self.http, self.base = http, base

    def _get(self, path: str, budget: Budget | None):
        body, r = self.http.get_json(NAME, f"{self.base}{path}", budget=budget,
                                     headers={"Accept": "application/json;version=20230302"})
        if not isinstance(body, dict):
            raise ProviderError(NAME, "bad_response", "expected an object")
        return body, r

    def _pools(self, body, r, net, fetched_at):
        data = body.get("data")
        if isinstance(data, dict):
            data = [data]
        if not isinstance(data, list):
            raise ProviderError(NAME, "bad_response", "missing data list")
        out = []
        for item in data:
            p = parse_pool(item, body.get("included") or [], net, fetched_at, r.date_ts())
            if p:
                out.append(p)
        return out

    def new_pools(self, net: str, page: int, budget: Budget | None, fetched_at: float) -> list[dict]:
        body, r = self._get(f"/networks/{quote(net)}/new_pools?page={int(page)}&include=base_token", budget)
        return self._pools(body, r, net, fetched_at)

    def pools_multi(self, net: str, pools: list[str], budget: Budget | None, fetched_at: float) -> list[dict]:
        if not pools:
            return []
        if len(pools) > 30:
            raise ValueError("at most 30 pools per call")
        joined = ",".join(quote(p, safe="") for p in pools)
        body, r = self._get(f"/networks/{quote(net)}/pools/multi/{joined}?include=base_token", budget)
        return self._pools(body, r, net, fetched_at)

    def pool(self, net: str, pool: str, budget: Budget | None, fetched_at: float) -> dict | None:
        body, r = self._get(f"/networks/{quote(net)}/pools/{quote(pool, safe='')}?include=base_token", budget)
        ps = self._pools(body, r, net, fetched_at)
        return next((p for p in ps if p["pool_address"] == pool), None)

    def token_info(self, net: str, token: str, budget: Budget | None) -> dict:
        body, r = self._get(f"/networks/{quote(net)}/tokens/{quote(token, safe='')}/info", budget)
        a = ((body.get("data") or {}).get("attributes")) or {}
        if not a:
            raise ProviderError(NAME, "bad_response", "token info missing attributes")
        if a.get("address") and a["address"] != token:
            raise ProviderError(NAME, "bad_response", "token info identity mismatch")
        holders = a.get("holders") or {}
        dist = holders.get("distribution_percentage") or {}
        return {
            "holder_count": inum(holders.get("count")),
            "top_10_percent": fnum(dist.get("top_10")),
            "mint_authority": a.get("mint_authority"),
            "freeze_authority": a.get("freeze_authority"),
            "twitter_handle": a.get("twitter_handle"),
            "description": a.get("description"),
            "gt_score": fnum(a.get("gt_score")),
            "fetched_provider_ts": r.date_ts(),
        }
