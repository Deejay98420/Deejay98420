"""Desk settings: schema, defaults, validation.

Settings are versioned in the database (settings_versions). The engine reads the newest
version only at the start of a cycle, so a change never lands half way through one.
Every value here is a policy choice, not a recommendation. Copy the shape, tune the numbers.
"""

from __future__ import annotations

import copy
import math

# key -> (type, min, max, default, group, label)
#   type: "int" | "float" | "bool" | "str" | "list"
SCHEMA: dict[str, tuple] = {
    # --- discovery / watchlist ---------------------------------------------------------
    "chains": ("list", None, None, ["solana"], "discovery",
               "GeckoTerminal networks to discover on. Only 'solana' can pass VET today; "
               "EVM networks are discovered and then rejected as evm_unverified."),
    "new_pool_pages": ("int", 1, 5, 2, "discovery", "new_pools pages per chain per cycle"),
    "watchlist_max": ("int", 20, 5000, 400, "discovery", "Max active watchlist entries"),
    "recheck_batch": ("int", 1, 120, 30, "discovery", "Watchlist entries re-quoted per cycle"),
    "recheck_interval_minutes": ("int", 5, 1440, 20, "discovery",
                                 "Default wait before re-quoting a watchlist entry"),
    # --- SCAN: pre-dossier arithmetic (no per-token network) ----------------------------
    "min_age_minutes": ("float", 0, 10_000, 15, "pre_filter", "Minimum pool age (minutes)"),
    "max_age_hours": ("float", 1, 10_000, 72, "pre_filter", "Maximum pool age (hours)"),
    "min_liquidity_usd": ("float", 0, 1e9, 12_000, "pre_filter", "Minimum pool liquidity (USD)"),
    "min_volume_h24": ("float", 0, 1e10, 40_000, "pre_filter", "Minimum 24h volume (USD)"),
    "min_trades_h24": ("int", 0, 1_000_000, 150, "pre_filter", "Minimum real trades in 24h"),
    "max_quote_age_seconds": ("int", 10, 3600, 300, "pre_filter",
                              "Quotes older than this are stale and rejected"),
    "no_sells_buy_floor": ("int", 0, 100_000, 20, "pre_filter",
                           "Reject when sells_h1 == 0 and buys_h1 exceeds this"),
    # --- VET: post-dossier facts ----------------------------------------------------------
    "max_top10_share": ("float", 0, 1, 0.60, "vet", "Max top-10 holder share (0-1)"),
    "max_largest_account_share": ("float", 0, 1, 0.25, "vet",
                                  "Max share of the largest token account (RPC). Note: this "
                                  "account is often the pool vault."),
    "min_holders": ("int", 0, 10_000_000, 80, "vet", "Minimum holder count"),
    "require_distribution": ("bool", None, None, True, "vet",
                             "Reject when holder distribution is unavailable"),
    "allowed_token2022_extensions": ("list", None, None, ["metadataPointer", "tokenMetadata"],
                                     "vet", "Token-2022 extensions to accept. Can only NARROW the "
                                     "supported metadata-only set (metadataPointer, tokenMetadata); "
                                     "raw bytes are always validated regardless of this list."),
    # --- valuation (post-dossier) -----------------------------------------------------------
    "mcap_min_usd": ("float", 0, 1e12, 60_000, "valuation",
                     "Provider-reported market cap minimum (USD)"),
    "mcap_max_usd": ("float", 0, 1e12, 8_000_000, "valuation",
                     "Provider-reported market cap maximum (USD)"),
    "supply_valuation_min_usd": ("float", 0, 1e12, 60_000, "valuation",
                                 "TOTAL_SUPPLY_VALUATION minimum (USD). NEW POLICY: price x "
                                 "verified total supply. Not circulating market cap."),
    "supply_valuation_max_usd": ("float", 0, 1e12, 8_000_000, "valuation",
                                 "TOTAL_SUPPLY_VALUATION maximum (USD)."),
    # --- judge ------------------------------------------------------------------------------
    "judge_engine": ("str", None, None, "RULES", "judge", "RULES or TYPED_AI"),
    "rules_min_score": ("float", 0, 100, 60, "judge",
                        "RULES: minimum rule score (0-100). Not a probability."),
    "typed_ai_model": ("str", None, None, "jev-latest", "judge",
                       "TYPED_AI request model alias. The response must carry a versioned ID."),
    "pick_min_worth": ("float", 0, 1, 0.60, "judge", "TYPED_AI: worth_trading_at_all minimum"),
    "pick_min_confidence": ("float", 0, 1, 0.55, "judge", "TYPED_AI: pick confidence minimum"),
    "shape_min_crowd": ("float", 0, 1, 0.55, "judge", "TYPED_AI: shape probabilities.crowd min"),
    "soft_max_concentration_risk": ("float", 0, 1, 0.55, "judge", "TYPED_AI noul max"),
    "soft_max_momentum_spent": ("float", 0, 1, 0.60, "judge", "TYPED_AI noul max"),
    "soft_min_liquidity_fits": ("float", 0, 1, 0.60, "judge", "TYPED_AI noul min"),
    "soft_max_dev_loaded": ("float", 0, 1, 0.55, "judge", "TYPED_AI noul max"),
    # --- SIZE --------------------------------------------------------------------------------
    "starting_cash_usd": ("float", 100, 1e9, 10_000, "sizing",
                          "Paper bank used only when the account is first created"),
    "max_position_fraction": ("float", 0.001, 0.25, 0.06, "sizing", "Max ticket as share of cash"),
    "max_pool_fraction": ("float", 0.0001, 0.2, 0.02, "sizing", "Max ticket as share of pool liquidity"),
    "no_social_cut": ("float", 0, 1, 0.60, "sizing", "Size factor when social evidence is missing"),
    "min_ticket_usd": ("float", 1, 1e7, 25, "sizing", "Smallest ticket worth sending"),
    # --- FILLS (simulated) ---------------------------------------------------------------------
    "fee_rate": ("float", 0, 0.1, 0.0045, "fills", "Simulated venue fee rate per side"),
    "min_fee_usd": ("float", 0, 100, 0.95, "fills", "Simulated minimum fee per side (USD)"),
    "max_fee_fraction": ("float", 0, 0.5, 0.02, "fills", "Reject tickets whose fee share exceeds this"),
    "base_slippage_bps": ("float", 0, 2000, 50, "fills", "Simulated base slippage (bps)"),
    "impact_factor": ("float", 0, 10, 1.0, "fills", "Simulated price impact multiplier"),
    "max_slippage_bps": ("float", 1, 5000, 300, "fills", "Refuse simulated fills above this slippage"),
    # --- RISK ----------------------------------------------------------------------------------
    "risk_poll_seconds": ("int", 10, 3600, 300, "risk", "Seconds between position checks"),
    "volume_ratio_exit": ("float", 0, 5, 0.20, "risk", "Close when vol_h6 / (vol_h24/4) is below"),
    "stop_loss_pct": ("float", 0.01, 1, 0.25, "risk", "Close when mark is this far below entry"),
    "take_profit_pct": ("float", 0.01, 100, 0.60, "risk", "Close when mark is this far above entry"),
    "max_hold_minutes": ("float", 5, 100_000, 720, "risk", "Close after this long"),
    "quote_fail_close_after": ("int", 1, 50, 3, "risk",
                               "Request close after this many failed marks (close stays "
                               "pending until a real exit price exists)"),
    # --- budgets / cycle -------------------------------------------------------------------------
    "gt_per_minute": ("int", 1, 30, 10, "budgets", "GeckoTerminal requests per minute"),
    "gt_calls_per_cycle": ("int", 1, 60, 8, "budgets", "GeckoTerminal requests per cycle"),
    "dex_calls_per_cycle": ("int", 0, 60, 6, "budgets", "DEX Screener requests per cycle"),
    "rpc_calls_per_cycle": ("int", 0, 200, 12, "budgets", "Solana RPC requests per cycle"),
    "max_dossiers_per_cycle": ("int", 0, 20, 3, "budgets", "Candidates that may reach VET per cycle"),
    "cycle_seconds": ("int", 30, 86_400, 900, "budgets", "Seconds between scan cycles"),
}

KNOWN_CHAINS = {"solana", "bsc", "base", "eth", "robinhood"}
JUDGE_ENGINES = {"RULES", "TYPED_AI"}

DEFAULTS: dict = {k: v[3] for k, v in SCHEMA.items()}

# Thresholds whose change should invalidate cooldowns that they produced.
THRESHOLD_VERSION = "thresholds-v3"


class SettingsError(ValueError):
    def __init__(self, errors: dict[str, str]):
        super().__init__("; ".join(f"{k}: {v}" for k, v in errors.items()))
        self.errors = errors


def defaults() -> dict:
    return copy.deepcopy(DEFAULTS)


def validate(patch: dict, base: dict | None = None) -> dict:
    """Merge `patch` over `base` (or defaults) and validate. Raises SettingsError."""
    if not isinstance(patch, dict):
        raise SettingsError({"_": "settings must be an object"})
    out = copy.deepcopy(base if base is not None else DEFAULTS)
    errors: dict[str, str] = {}
    for key, value in patch.items():
        if key not in SCHEMA:
            errors[key] = "unknown setting"
            continue
        typ, lo, hi = SCHEMA[key][0], SCHEMA[key][1], SCHEMA[key][2]
        if typ == "bool":
            if not isinstance(value, bool):
                errors[key] = "must be true or false"
                continue
        elif typ in ("int", "float"):
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                errors[key] = "must be a number"
                continue
            if not math.isfinite(float(value)):
                errors[key] = "must be finite"
                continue
            if typ == "int":
                if float(value) != int(value):
                    errors[key] = "must be a whole number"
                    continue
                value = int(value)
            else:
                value = float(value)
            if (lo is not None and value < lo) or (hi is not None and value > hi):
                errors[key] = f"must be between {lo} and {hi}"
                continue
        elif typ == "str":
            if not isinstance(value, str) or not (1 <= len(value) <= 64):
                errors[key] = "must be a short string"
                continue
        elif typ == "list":
            if not isinstance(value, list) or not all(isinstance(x, str) and 0 < len(x) <= 64
                                                      for x in value) or len(value) > 20:
                errors[key] = "must be a list of short strings"
                continue
        out[key] = value

    if not errors:
        if out["judge_engine"] not in JUDGE_ENGINES:
            errors["judge_engine"] = "must be RULES or TYPED_AI"
        from .mint import SUPPORTED_NAMES
        ext_bad = [x for x in out["allowed_token2022_extensions"] if x not in SUPPORTED_NAMES]
        if ext_bad:
            errors["allowed_token2022_extensions"] = (f"unsupported {ext_bad}: only "
                                                      f"{sorted(SUPPORTED_NAMES)} can be allowed")
        bad = [c for c in out["chains"] if c not in KNOWN_CHAINS]
        if bad or not out["chains"]:
            errors["chains"] = f"unknown chains {bad}" if bad else "at least one chain"
        for lo_k, hi_k in (("mcap_min_usd", "mcap_max_usd"),
                           ("supply_valuation_min_usd", "supply_valuation_max_usd")):
            if out[lo_k] > out[hi_k]:
                errors[hi_k] = f"must be >= {lo_k}"
        if out["min_age_minutes"] > out["max_age_hours"] * 60:
            errors["max_age_hours"] = "must be after min_age_minutes"
    if errors:
        raise SettingsError(errors)
    return out


def schema_for_ui() -> list[dict]:
    return [{"key": k, "type": v[0], "min": v[1], "max": v[2], "default": v[3],
             "group": v[4], "label": v[5]} for k, v in SCHEMA.items()]
