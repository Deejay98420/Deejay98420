"""RULES engine: deterministic, transparent, no key required.

The rule score is a 0-100 sum of five capped components. It is a ranking device, NOT a
probability, and is never presented or stored as one.
"""

from __future__ import annotations

from .base import CandidateJudgment, Judge, PickResult, social_size_factor

VERSION = "rules-v3"
SCORE_TYPE = "RULE_SCORE_0_100 (not a probability)"


def _clamp(x: float, lo: float = 0.0, hi: float = 1.0) -> float:
    return max(lo, min(hi, x))


def components(c: dict, settings: dict) -> tuple[dict, list]:
    """Each component is 0..20. Missing inputs score 0 and are listed as gaps."""
    s = c["snapshot"]
    comp, gaps = {}, []
    ticket_cap = float(settings["max_position_fraction"]) * float(c.get("cash_usd") or 0)
    liq = s.get("liquidity_usd")
    if liq is None or ticket_cap <= 0:
        comp["liquidity_depth"] = 0.0
        gaps.append("liquidity_depth")
    else:  # 20 points when the max ticket is <= 2% of the pool
        comp["liquidity_depth"] = round(20 * _clamp(liq * 0.02 / ticket_cap), 2)

    b, se = s.get("buys_h1"), s.get("sells_h1")
    if b is None or se is None or b + se == 0:
        comp["flow_balance"] = 0.0
        gaps.append("flow_balance")
    else:  # 20 points when buys are 50-75% of h1 trades; falls off outside
        r = b / (b + se)
        dist = 0.5 - r if r < 0.5 else (r - 0.75 if r > 0.75 else 0.0)
        comp["flow_balance"] = round(20 * _clamp(1 - dist * 4), 2)

    v6, v24 = s.get("volume_h6"), s.get("volume_h24")
    if v6 is None or not v24:
        comp["volume_alive"] = 0.0
        gaps.append("volume_alive")
    else:  # ratio of last-6h volume to the 24h average 6h block; 1.0+ is full marks
        comp["volume_alive"] = round(20 * _clamp(v6 / (v24 / 4)), 2)

    top10 = c.get("top_10_percent")
    if top10 is None:
        comp["spread_float"] = 0.0
        gaps.append("spread_float")
    else:
        comp["spread_float"] = round(20 * _clamp(1 - top10 / 100.0), 2)

    h = c.get("holder_count")
    if h is None:
        comp["holder_base"] = 0.0
        gaps.append("holder_base")
    else:
        comp["holder_base"] = round(20 * _clamp(h / 500.0), 2)
    return comp, gaps


class RulesJudge(Judge):
    engine = "RULES"
    score_type = SCORE_TYPE

    def status(self) -> dict:
        return {"engine": self.engine, "configured": True, "version": VERSION,
                "score_type": SCORE_TYPE, "detail": "deterministic backend; no key required"}

    def judge(self, candidate: dict, settings: dict) -> CandidateJudgment:
        comp, gaps = components(candidate, settings)
        score = round(sum(comp.values()), 2)
        passed = score >= float(settings["rules_min_score"])
        sf, notes = social_size_factor(candidate, settings)
        return CandidateJudgment(
            candidate_id=candidate["candidate_id"], passed=passed,
            reason=None if passed else "judge_reject", engine=self.engine,
            score_type=SCORE_TYPE, model_id=VERSION,
            summary=f"rule score {score:.1f}/100 (min {settings['rules_min_score']})",
            values={"rule_score": score, "components": comp, "gaps": gaps,
                    "min_score": settings["rules_min_score"]},
            raw=[], size_factor=sf, size_notes=notes)

    def pick(self, survivors, settings) -> PickResult:
        if not survivors:
            return PickResult(None, "no_survivors", self.engine, SCORE_TYPE, VERSION)
        ranked = sorted(survivors, key=lambda x: (-x[1].values["rule_score"], x[0]["candidate_id"]))
        best_c, best_j = ranked[0]
        # absolute gate: applies even to a single survivor
        if best_j.values["rule_score"] < float(settings["rules_min_score"]):
            return PickResult(None, "below_absolute_gate", self.engine, SCORE_TYPE, VERSION)
        return PickResult(best_c["candidate_id"], None, self.engine, SCORE_TYPE, VERSION,
                          confidence=None,
                          detail={"rule_score": best_j.values["rule_score"],
                                  "ranked": [(c["candidate_id"], j.values["rule_score"]) for c, j in ranked],
                                  "note": "highest rule score; not a probability"})
