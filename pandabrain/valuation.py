"""Valuation gate (post-dossier).

Two distinct, separately labelled valuation types:

  PROVIDER_MARKET_CAP     a valid market cap reported by the quote provider (preferred).
  TOTAL_SUPPLY_VALUATION  price x (verified raw supply / 10**decimals), computed with Decimal.
                          This is NOT circulating market cap. It is closer to FDV and is gated by
                          its own limits (supply_valuation_min_usd / max_usd), a new eligibility
                          policy introduced in v3. mcap_usd stays null in this case.

FDV reported by a provider is never used as market cap.
"""

from __future__ import annotations

from decimal import Decimal, localcontext

from .mint import MAX_DECIMALS, MintCheck
from .util import dec, now, pos_dec

PROVIDER_MARKET_CAP = "PROVIDER_MARKET_CAP"
TOTAL_SUPPLY_VALUATION = "TOTAL_SUPPLY_VALUATION"


class ValuationError(ValueError):
    pass


def total_supply_valuation(price, raw_supply: str, decimals: int) -> Decimal:
    p = pos_dec(price)
    if p is None:
        raise ValuationError("price is missing, non-finite or not positive")
    if not isinstance(raw_supply, str) or not raw_supply.isdigit():
        raise ValuationError("raw supply must be an integer string")
    if isinstance(decimals, bool) or not isinstance(decimals, int) or not 0 <= decimals <= MAX_DECIMALS:
        raise ValuationError("decimals out of range")
    raw = int(raw_supply)
    if raw <= 0:
        raise ValuationError("supply must be positive")
    with localcontext() as ctx:
        ctx.prec = 60
        v = p * (Decimal(raw) / (Decimal(10) ** decimals))
    if not v.is_finite() or v <= 0:
        raise ValuationError("valuation is not finite")
    return v


def choose(snapshot: dict, mint: MintCheck | None, settings: dict) -> dict:
    """Returns {"ok", "reason", "valuation": {...} | None}."""
    t = now()
    mcap = pos_dec(snapshot.get("mcap_usd"))
    if mcap is not None:
        val = {"type": PROVIDER_MARKET_CAP, "value_usd": str(mcap),
               "source": snapshot.get("mcap_source") or snapshot.get("source"),
               "provider_ts": snapshot.get("provider_ts"), "fetched_at": snapshot.get("fetched_at"),
               "computed_at": t, "derivation": "reported by provider"}
        lo, hi = Decimal(str(settings["mcap_min_usd"])), Decimal(str(settings["mcap_max_usd"]))
        if mcap < lo:
            return {"ok": False, "reason": "mcap_below_min", "valuation": val}
        if mcap > hi:
            return {"ok": False, "reason": "mcap_above_max", "valuation": val}
        return {"ok": True, "reason": None, "valuation": val}

    if mint is None or not mint.ok:
        return {"ok": False, "reason": "valuation_unavailable", "valuation": None}
    if mint.mint != snapshot.get("token_address"):
        return {"ok": False, "reason": "valuation_identity_mismatch", "valuation": None}
    try:
        v = total_supply_valuation(snapshot.get("price_usd"), mint.raw_supply, mint.decimals)
    except ValuationError as e:
        return {"ok": False, "reason": "valuation_invalid", "valuation": None, "detail": str(e)}
    val = {"type": TOTAL_SUPPLY_VALUATION, "value_usd": str(v.quantize(Decimal("0.01"))),
           "source": f"{snapshot.get('source')} price x solana_rpc verified supply",
           "price_usd": snapshot.get("price_usd"), "price_source": snapshot.get("source"),
           "price_fetched_at": snapshot.get("fetched_at"), "provider_ts": snapshot.get("provider_ts"),
           "raw_supply": mint.raw_supply, "decimals": mint.decimals, "mint_slot": mint.slot,
           "mint_program": mint.program, "computed_at": t,
           "derivation": f"{snapshot.get('price_usd')} x ({mint.raw_supply} / 10**{mint.decimals})",
           "note": "total supply valuation; not circulating market cap"}
    lo = Decimal(str(settings["supply_valuation_min_usd"]))
    hi = Decimal(str(settings["supply_valuation_max_usd"]))
    if v < lo:
        return {"ok": False, "reason": "supply_valuation_below_min", "valuation": val}
    if v > hi:
        return {"ok": False, "reason": "supply_valuation_above_max", "valuation": val}
    return {"ok": True, "reason": None, "valuation": val}


def fmt_dec(x) -> str | None:
    d = dec(x)
    return None if d is None else str(d)
