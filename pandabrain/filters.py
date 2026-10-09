"""Facts kill before judgements do.

pre_dossier()  arithmetic on the quote alone. No per-token network. Market cap is NOT checked
               here any more: a missing provider market cap must not stop a Solana candidate
               from reaching bounded mint verification (v3 policy change).
vet_facts()    after the dossier: mint controls, distribution, holders. Facts, not judgements.
"""

from __future__ import annotations

from .util import now, pos_dec

EVM_CHAINS = {"bsc", "base", "eth", "robinhood"}


def pre_dossier(s: dict, st: dict, t: float | None = None) -> str | None:
    t = now() if t is None else t
    if s.get("chain") in EVM_CHAINS:
        return "evm_unverified"            # no verified controls/valuation path for EVM yet
    if s.get("chain") != "solana":
        return "chain_unsupported"
    if pos_dec(s.get("price_usd")) is None:
        return "price_invalid"
    fetched = s.get("fetched_at")
    if fetched is None or t - fetched > st["max_quote_age_seconds"]:
        return "stale_quote"
    created = s.get("pool_created_at")
    if created is None:
        return "age_unknown"
    age_min = (t - created) / 60
    if age_min < st["min_age_minutes"]:
        return "too_young"
    if age_min > st["max_age_hours"] * 60:
        return "too_old"
    if s.get("liquidity_usd") is None:
        return "missing_liquidity"
    if s["liquidity_usd"] < st["min_liquidity_usd"]:
        return "low_liquidity"
    if s.get("volume_h24") is None:
        return "missing_volume"
    if s["volume_h24"] < st["min_volume_h24"]:
        return "low_volume"
    b24, s24 = s.get("buys_h24"), s.get("sells_h24")
    if b24 is None or s24 is None:
        return "missing_trades"
    if b24 + s24 < st["min_trades_h24"]:
        return "low_trades"
    if s.get("sells_h1") == 0 and (s.get("buys_h1") or 0) > st["no_sells_buy_floor"]:
        return "no_sells"
    return None


def vet_facts(d: dict, st: dict) -> str | None:
    """d holds token_info fields plus largest_account_share. Missing evidence has its own reason."""
    top10 = d.get("top_10_percent")
    if top10 is None:
        if st["require_distribution"]:
            return "missing_distribution"
    elif top10 / 100.0 > st["max_top10_share"]:
        return "top10_concentration"
    holders = d.get("holder_count")
    if holders is None:
        if st["require_distribution"]:
            return "missing_holders"
    elif holders < st["min_holders"]:
        return "low_holders"
    share = d.get("largest_account_share")
    if share is not None and share > st["max_largest_account_share"]:
        return "largest_account_concentration"
    return None


def age_minutes(s: dict, t: float | None = None) -> float | None:
    c = s.get("pool_created_at")
    return None if c is None else ((now() if t is None else t) - c) / 60
