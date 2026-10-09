"""Execution boundary. The only adapter is PAPER. Live execution is refused, always.

There is no toggle that enables live execution. Real trading would need a separate, authorised
implementation with verified venue APIs, idempotency, reconciliation, fee evidence and signing,
none of which exists here.
"""

from __future__ import annotations

from decimal import Decimal

from . import EXECUTION_MODE
from .util import dec, pos_dec


class LiveExecutionBlocked(PermissionError):
    pass


class FillRefused(ValueError):
    def __init__(self, reason: str):
        super().__init__(reason)
        self.reason = reason


class ExecutionAdapter:
    name = "base"

    def entry(self, price, liquidity, ticket, st) -> dict:
        raise NotImplementedError

    def exit(self, price, liquidity, qty, st) -> dict:
        raise NotImplementedError


def _slip_bps(notional: Decimal, liquidity, st) -> float:
    liq = dec(liquidity)
    impact = 0.0 if liq is None or liq <= 0 else float(notional / liq) * 10_000 * float(st["impact_factor"])
    return round(float(st["base_slippage_bps"]) + impact, 2)


def _fee(notional: Decimal, st) -> Decimal:
    return max(notional * Decimal(str(st["fee_rate"])), Decimal(str(st["min_fee_usd"])))


class PaperAdapter(ExecutionAdapter):
    """Simulated fills: quote price moved against us by modelled slippage, fee deducted."""

    name = "paper"

    def entry(self, price, liquidity, ticket, st) -> dict:
        p, t = pos_dec(price), pos_dec(ticket)
        if p is None or t is None:
            raise FillRefused("invalid_price_or_ticket")
        bps = _slip_bps(t, liquidity, st)
        if bps > float(st["max_slippage_bps"]):
            raise FillRefused("slippage_over_max")
        fee = _fee(t, st)
        fill = p * (Decimal(1) + Decimal(str(bps)) / Decimal(10_000))
        qty = (t - fee) / fill
        if qty <= 0:
            raise FillRefused("fee_exceeds_ticket")
        return {"adapter": self.name, "quote_price": str(p), "fill_price": str(fill), "qty": str(qty),
                "ticket_usd": str(t), "fee_usd": str(fee), "slippage_bps": bps, "simulated": True}

    def exit(self, price, liquidity, qty, st) -> dict:
        p, q = pos_dec(price), pos_dec(qty)
        if p is None or q is None:
            raise FillRefused("invalid_exit_price")
        notional = p * q
        bps = _slip_bps(notional, liquidity, st)   # exits are never refused for slippage
        fill = p * (Decimal(1) - Decimal(str(min(bps, 9_999))) / Decimal(10_000))
        gross = fill * q
        fee = min(_fee(gross, st), gross)
        return {"adapter": self.name, "quote_price": str(p), "fill_price": str(fill),
                "proceeds_usd": str(gross - fee), "gross_usd": str(gross), "fee_usd": str(fee),
                "slippage_bps": bps, "simulated": True}


def get_adapter(name: str) -> ExecutionAdapter:
    if name == "paper":
        return PaperAdapter()
    raise LiveExecutionBlocked(f"execution adapter '{name}' is not available: this build is {EXECUTION_MODE}")


def request_live_execution(*_a, **_k):
    raise LiveExecutionBlocked(f"live execution is disabled in this build ({EXECUTION_MODE})")
