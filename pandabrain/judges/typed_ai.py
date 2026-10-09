"""TYPED_AI engine: optional adapter for the TypeSafe "systemone" typed-answer API.

Contract verified against the published Python SDK (typesafe-sdk 0.7.4 on PyPI, wire models in
typesafe_sdk/_schemas/models.py): POST {base}/v1/systemone, Bearer auth,
body {state, model, questions}; response {model, answers, usage}. Answers:
  noul   {type, noul}
  choice {type, choice, confidence, probabilities}
  score  {type, score, confidence, legend, probabilities}
Pricing and rate limits are NOT taken from any guide; check your own console.

Rules of this adapter:
  * the key is read server-side from TYPESAFE_API_KEY and never returned by any API route;
  * every answer is validated (presence, type, bounds, probability sums, choice in options);
  * the response model ID must be a concrete version, not an alias;
  * missing key -> visibly unconfigured; outage -> JudgeUnavailable; never falls back to RULES;
  * probabilities are never manufactured from rule scores.
"""

from __future__ import annotations

import math
import os
import re

from ..providers.http import Http, ProviderError
from . import questions as Q
from .base import CandidateJudgment, Judge, JudgeSchemaError, JudgeUnavailable, PickResult, social_size_factor

SCORE_TYPE = "TYPED_PROBABILITY (provider model output)"
DEFAULT_BASE = "https://api.typesafe.ai"
NAME = "typesafe"
_VERSIONED = re.compile(r"^[A-Za-z][A-Za-z0-9._-]*-\d+\.\d+(\.\d+)?$")
PROB_TOL = 0.011


def _finite01(x) -> bool:
    return isinstance(x, (int, float)) and not isinstance(x, bool) and math.isfinite(x) and 0.0 <= x <= 1.0


def _check_probs(name: str, probs, labels: list[str]):
    if not isinstance(probs, dict) or set(probs) != set(labels):
        raise JudgeSchemaError(f"{name}: probabilities keys do not match options")
    if not all(_finite01(v) for v in probs.values()):
        raise JudgeSchemaError(f"{name}: probability out of [0,1]")
    if abs(sum(probs.values()) - 1.0) > PROB_TOL:
        raise JudgeSchemaError(f"{name}: probabilities sum to {sum(probs.values()):.4f}")


def validate_response(body, qs: dict, requested_model: str) -> tuple[str, dict]:
    if not isinstance(body, dict):
        raise JudgeSchemaError("response is not an object")
    model = body.get("model")
    if not isinstance(model, str) or not model or model == requested_model or model.endswith("-latest") \
            or not _VERSIONED.match(model):
        raise JudgeSchemaError(f"response model id is not a concrete version: {str(model)[:40]!r}")
    answers = body.get("answers")
    if not isinstance(answers, dict):
        raise JudgeSchemaError("answers missing")
    missing = [k for k in qs if k not in answers]
    if missing:
        raise JudgeSchemaError(f"missing answers: {missing}")
    extra = [k for k in answers if k not in qs]
    if extra:
        raise JudgeSchemaError(f"unexpected answers: {extra}")
    for name, q in qs.items():
        a = answers[name]
        if not isinstance(a, dict) or a.get("type") != q["type"]:
            raise JudgeSchemaError(f"{name}: wrong answer type")
        if q["type"] == "noul":
            if not _finite01(a.get("noul")):
                raise JudgeSchemaError(f"{name}: noul out of [0,1]")
        elif q["type"] == "choice":
            labels = list(q["criteria"].keys())
            if a.get("choice") not in labels:
                raise JudgeSchemaError(f"{name}: choice not among options")
            if not _finite01(a.get("confidence")):
                raise JudgeSchemaError(f"{name}: confidence out of [0,1]")
            _check_probs(name, a.get("probabilities"), labels)
        elif q["type"] == "score":
            n = len(q["criteria"])
            s = a.get("score")
            if not isinstance(s, (int, float)) or isinstance(s, bool) or not math.isfinite(s) or not 0 <= s <= n - 1:
                raise JudgeSchemaError(f"{name}: score out of range")
            if not _finite01(a.get("confidence")):
                raise JudgeSchemaError(f"{name}: confidence out of [0,1]")
            _check_probs(name, a.get("probabilities"), [str(i) for i in range(n)])
    return model, answers


class TypeSafeClient:
    def __init__(self, http: Http, api_key: str | None = None, base_url: str | None = None):
        self.http = http
        self._key = (api_key if api_key is not None else os.environ.get("TYPESAFE_API_KEY", "")).strip()
        self.base = (base_url or os.environ.get("TYPESAFE_BASE_URL", "").strip() or DEFAULT_BASE).rstrip("/")

    @property
    def configured(self) -> bool:
        return bool(self._key) and self._key.isascii() and self._key.isprintable() and " " not in self._key

    def system_one(self, state: dict, qs: dict, model: str) -> dict:
        if not self.configured:
            raise JudgeUnavailable("TYPED_AI unconfigured: TYPESAFE_API_KEY is not set")
        try:
            r = self.http.request(NAME, "POST", f"{self.base}/v1/systemone",
                                  json_body={"state": state, "model": model, "questions": qs},
                                  headers={"Authorization": f"Bearer {self._key}"}, timeout=30, max_wait=30)
        except ProviderError as e:
            if e.kind == "bad_response" and "HTTP 422" in str(e):
                raise JudgeSchemaError("judge rejected the question set (422); not retried") from None
            if e.kind == "bad_response" and ("HTTP 401" in str(e) or "HTTP 403" in str(e)):
                raise JudgeUnavailable("TYPED_AI auth failed") from None
            raise JudgeUnavailable(f"TYPED_AI unavailable ({e.kind})") from None
        try:
            return r.json()
        except ValueError:
            raise JudgeSchemaError("judge returned invalid JSON") from None


def market_state(c: dict, settings: dict) -> dict:
    s = c["snapshot"]
    return {"candidate_id": c["candidate_id"], "age_minutes": round(c["age_minutes"], 1),
            "liquidity_usd": s.get("liquidity_usd"), "volume_h1": s.get("volume_h1"),
            "volume_h6": s.get("volume_h6"), "volume_h24": s.get("volume_h24"),
            "buys_h1": s.get("buys_h1"), "sells_h1": s.get("sells_h1"),
            "buys_h24": s.get("buys_h24"), "sells_h24": s.get("sells_h24"),
            "valuation_type": (c.get("valuation") or {}).get("type"),
            "valuation_usd": (c.get("valuation") or {}).get("value_usd"),
            "intended_ticket_usd": round(float(c.get("cash_usd") or 0) * settings["max_position_fraction"], 2)}


def chain_state(c: dict) -> dict:
    return {"candidate_id": c["candidate_id"], "top_10_percent": c.get("top_10_percent"),
            "largest_account_share": c.get("largest_account_share"), "holder_count": c.get("holder_count")}


class TypedAIJudge(Judge):
    engine = "TYPED_AI"
    score_type = SCORE_TYPE

    def __init__(self, client: TypeSafeClient):
        self.client = client

    def status(self) -> dict:
        if not self.client.configured:
            return {"engine": self.engine, "configured": False, "score_type": SCORE_TYPE,
                    "detail": "unconfigured: set TYPESAFE_API_KEY in .env (server-side only). "
                              "No fallback to RULES."}
        return {"engine": self.engine, "configured": True, "score_type": SCORE_TYPE,
                "detail": f"key present; endpoint host {self.client.base}"}

    def _call(self, state, qs, settings):
        model_req = settings["typed_ai_model"]
        body = self.client.system_one(state, qs, model_req)
        model, answers = validate_response(body, qs, model_req)
        return model, answers, body

    def judge(self, c: dict, settings: dict) -> CandidateJudgment:
        raw, answers, models = [], {}, set()
        sets = [("market", Q.MARKET, market_state(c, settings))]
        chain_set = Q.CHAIN_SET.get(c["chain"])
        if not chain_set:
            raise JudgeSchemaError(f"no question set for chain {c['chain']}")
        sets.append((chain_set, Q.SETS[chain_set], chain_state(c)))
        soc = c.get("social") or {}
        if soc.get("status") == "read" and soc.get("account"):
            sets.append(("social", Q.SOCIAL, {"x_account": soc["account"],
                                              "untrusted_token_text": {"symbol": c.get("symbol")}}))
        for name, qs, state in sets:
            model, ans, body = self._call(state, qs, settings)
            models.add(model)
            raw.append({"question_set": name, "model": model, "answers": body.get("answers"),
                        "usage": body.get("usage")})
            answers.update(ans)

        reason = self._soft_kill(answers, settings)
        sf, notes = social_size_factor(c, settings)
        crowd = answers["shape"]["probabilities"]["crowd"]
        summary = (f"{c['chain']}, {c['age_minutes']:.0f}m, liq ${c['snapshot'].get('liquidity_usd') or 0:,.0f}; "
                   f"crowd {crowd:.2f}, concentration {answers['concentration_is_exit_risk']['noul']:.2f}"
                   + ("" if soc.get("status") == "read" else "; no social evidence"))
        return CandidateJudgment(c["candidate_id"], reason is None, reason, self.engine, SCORE_TYPE,
                                 ",".join(sorted(models)), summary, values=answers, raw=raw,
                                 size_factor=sf, size_notes=notes)

    @staticmethod
    def _soft_kill(a: dict, st: dict) -> str | None:
        if a["concentration_is_exit_risk"]["noul"] > st["soft_max_concentration_risk"]:
            return "concentration_is_exit_risk"
        if a["momentum_already_spent"]["noul"] > st["soft_max_momentum_spent"]:
            return "momentum_already_spent"
        if a["liquidity_fits_ticket"]["noul"] < st["soft_min_liquidity_fits"]:
            return "liquidity_fits_ticket"
        if a["shape"]["choice"] in ("fading", "one_buyer"):
            return "shape"
        if a["shape"]["probabilities"]["crowd"] < st["shape_min_crowd"]:
            return "shape_weak"
        return None

    def pick(self, survivors, settings) -> PickResult:
        if not survivors:
            return PickResult(None, "no_survivors", self.engine, SCORE_TYPE, None)
        cands = [{"candidate_id": c["candidate_id"], "summary": j.summary} for c, j in survivors]
        qs = Q.pick_set(cands) if len(cands) > 1 else Q.single_set()
        model, ans, body = self._call({"candidates": cands}, qs, settings)
        worth = ans["worth_trading_at_all"]["noul"]
        raw = {"question_set": "pick" if len(cands) > 1 else "single", "model": model,
               "answers": body.get("answers"), "usage": body.get("usage")}
        if worth < settings["pick_min_worth"]:
            return PickResult(None, "not_worth_trading", self.engine, SCORE_TYPE, model,
                              detail={"worth_trading_at_all": worth}, raw=raw)
        if len(cands) == 1:
            return PickResult(cands[0]["candidate_id"], None, self.engine, SCORE_TYPE, model,
                              confidence=None, detail={"worth_trading_at_all": worth,
                                                       "note": "single survivor: no choice call"}, raw=raw)
        best = ans["best"]
        if best["confidence"] < settings["pick_min_confidence"]:
            return PickResult(None, "low_pick_confidence", self.engine, SCORE_TYPE, model,
                              confidence=best["confidence"], detail={"worth_trading_at_all": worth}, raw=raw)
        ranked = sorted(best["probabilities"].items(), key=lambda kv: -kv[1])
        return PickResult(best["choice"], None, self.engine, SCORE_TYPE, model, confidence=best["confidence"],
                          detail={"worth_trading_at_all": worth, "runner_up": ranked[1:2]}, raw=raw)
