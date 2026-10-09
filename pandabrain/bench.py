"""Rejection cooldowns ("the bench"), keyed by exact pool, timed by the check that fired.

Cooldowns are policy-aware so a stale rejection can never hide a fixed pipeline:
  * every row records the policy tag of the stage that produced it (e.g. "pre@3");
    when a stage's policy version changes, its old rows stop counting.
  * threshold-driven reasons also record the settings version; a settings change voids them.
  * RETIRED reasons are never honoured (e.g. v2.2 "missing_mcap_usd", which v3 no longer
    produces because missing market cap now routes to mint verification).
Rows are never bulk-deleted. Fact reasons (open authorities) persist for their full term.
"""

from __future__ import annotations

from .db import Database
from .util import now

POLICY = {"pre": "pre@3", "vet": "vet@3", "valuation": "valuation@3", "judge": "judge@3",
          "provider": "provider@3", "book": "book@3"}

RETIRED = {"missing_mcap_usd", "mcap"}   # v2.2-era reasons voided by the v3 valuation split

MINUTES = {
    # facts that do not change while this token exists
    "mint_authority_open": 100_000, "freeze_authority_open": 100_000,
    "unsupported_extension": 100_000, "unsupported_program": 100_000, "not_a_mint": 100_000,
    "token2022_layout_invalid": 1_440, "extension_truncated": 1_440, "extension_duplicate": 1_440,
    "extension_malformed": 1_440, "mint_raw_invalid": 1_440,
    "evm_unverified": 1_440, "chain_unsupported": 1_440, "too_old": 100_000,
    # slow to change
    "largest_account_concentration": 90, "top10_concentration": 90, "low_holders": 90,
    "missing_distribution": 60, "missing_holders": 60,
    # quick to change: keep short or the token matures unseen
    "low_liquidity": 25, "low_volume": 25, "low_trades": 25, "no_sells": 25,
    "mcap_below_min": 25, "mcap_above_max": 60, "supply_valuation_below_min": 25,
    "supply_valuation_above_max": 60, "judge_reject": 30, "stale_quote": 5,
    # provider faults: the token is not at fault, so only a short pause
    "rpc_unavailable": 10, "token_info_unavailable": 10, "mint_missing": 30,
    "mint_view_mismatch": 30, "extension_view_mismatch": 30, "recently_closed": 60, "valuation_unavailable": 30, "valuation_invalid": 30,
}
DEFAULT_MINUTES = 45
THRESHOLD_REASONS = {"too_young", "low_liquidity", "low_volume", "low_trades", "no_sells",
                     "mcap_below_min", "mcap_above_max", "supply_valuation_below_min",
                     "supply_valuation_above_max", "top10_concentration", "low_holders",
                     "largest_account_concentration", "missing_distribution", "missing_holders",
                     "judge_reject", "stale_quote"}


def sit(db: Database, chain: str, pool: str, stage: str, reason: str, settings_version: int,
        minutes: float | None = None):
    t = now()
    m = minutes if minutes is not None else MINUTES.get(reason, DEFAULT_MINUTES)
    db.x("INSERT OR REPLACE INTO bench VALUES(?,?,?,?,?,?,?)",
         (chain, pool, reason, POLICY.get(stage, stage), settings_version, t, t + m * 60))


def active(db: Database, chain: str, pool: str, settings_version: int) -> dict | None:
    r = db.q1("SELECT * FROM bench WHERE chain=? AND pool_address=?", (chain, pool))
    if not r or not is_valid(dict(r), settings_version):
        return None
    return dict(r)


def is_valid(row: dict, settings_version: int, t: float | None = None) -> bool:
    t = now() if t is None else t
    if row["until"] <= t:
        return False
    if row["reason"] in RETIRED:
        return False
    if row["policy"] not in POLICY.values():
        return False
    if row["reason"] in THRESHOLD_REASONS and row["settings_version"] != settings_version:
        return False
    return True


def summary(db: Database, settings_version: int) -> dict:
    out: dict[str, int] = {}
    for r in db.q("SELECT * FROM bench WHERE until > ?", (now(),)):
        if is_valid(dict(r), settings_version):
            out[r["reason"]] = out.get(r["reason"], 0) + 1
    return out
