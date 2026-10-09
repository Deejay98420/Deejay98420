"""SIZE: documented arithmetic, Decimal throughout.

  1. ticket = cash x max_position_fraction            (free cash only; never the open bag)
  2. ticket x= size_factor                              (no social evidence -> no_social_cut)
  3. ticket = min(ticket, liquidity x max_pool_fraction)
  4. fee = max(fee_rate x ticket, min_fee); fee/ticket > max_fee_fraction -> 0 (fee_floor)
  5. ticket < min_ticket -> 0 (below_min_ticket)

No Kelly step: RULES produces no edge probability, and none is manufactured. A TYPED_AI
probability is a judgement about a question, not a calibrated trading edge.
"""

from __future__ import annotations

from decimal import ROUND_DOWN, Decimal

from .util import dec

CENT = Decimal("0.01")


def size(cash, liquidity_usd, size_factor: float, st: dict) -> dict:
    steps = []
    c = dec(cash) or Decimal(0)
    t = c * Decimal(str(st["max_position_fraction"]))
    steps.append(f"cash {c:.2f} x {st['max_position_fraction']} = {t:.2f}")
    sf = Decimal(str(size_factor))
    t = t * sf
    steps.append(f"x size_factor {sf} = {t:.2f}")
    liq = dec(liquidity_usd)
    if liq is None or liq <= 0:
        return {"ticket_usd": "0", "reason": "missing_liquidity", "steps": steps}
    cap = liq * Decimal(str(st["max_pool_fraction"]))
    if t > cap:
        t = cap
        steps.append(f"capped at {st['max_pool_fraction']} of pool = {t:.2f}")
    t = t.quantize(CENT, rounding=ROUND_DOWN)
    if t <= 0:
        return {"ticket_usd": "0", "reason": "zero_ticket", "steps": steps}
    fee = max(t * Decimal(str(st["fee_rate"])), Decimal(str(st["min_fee_usd"])))
    frac = fee / t
    steps.append(f"fee {fee:.4f} = {frac:.4%} of ticket")
    if frac > Decimal(str(st["max_fee_fraction"])):
        return {"ticket_usd": "0", "reason": "fee_floor", "steps": steps}
    if t < Decimal(str(st["min_ticket_usd"])):
        return {"ticket_usd": "0", "reason": "below_min_ticket", "steps": steps}
    return {"ticket_usd": str(t), "reason": None, "steps": steps}
