"""Pluggable judge interface.

A judge receives candidates that already survived every factual gate. It returns per-candidate
judgments and then one pick (or none). Judges never fetch data, never size, never execute.
There is no automatic fallback between engines: if the configured engine is unavailable the
cycle ends with a recorded no-trade.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field


class JudgeUnavailable(RuntimeError):
    """Engine unconfigured, unreachable, rate limited or refused auth. Never a pass."""


class JudgeSchemaError(RuntimeError):
    """The request or response violated the typed contract. Never retried."""


@dataclass
class CandidateJudgment:
    candidate_id: str
    passed: bool
    reason: str | None
    engine: str
    score_type: str
    model_id: str | None
    summary: str
    values: dict = field(default_factory=dict)      # rule components or typed answers
    raw: list = field(default_factory=list)          # exact raw provider answers, per call
    size_factor: float = 1.0
    size_notes: list = field(default_factory=list)

    def as_dict(self):
        return asdict(self)


@dataclass
class PickResult:
    candidate_id: str | None
    reason: str | None
    engine: str
    score_type: str
    model_id: str | None
    confidence: float | None = None
    detail: dict = field(default_factory=dict)
    raw: dict | None = None

    def as_dict(self):
        return asdict(self)


class Judge:
    engine = "BASE"
    score_type = ""

    def status(self) -> dict:
        raise NotImplementedError

    def judge(self, candidate: dict, settings: dict) -> CandidateJudgment:
        raise NotImplementedError

    def pick(self, survivors: list[tuple[dict, CandidateJudgment]], settings: dict) -> PickResult:
        raise NotImplementedError


def social_size_factor(candidate: dict, settings: dict) -> tuple[float, list]:
    """Documented size policy: no social evidence -> no_social_cut. Not a pass, not a skip."""
    soc = candidate.get("social") or {}
    if soc.get("status") != "read":
        return float(settings["no_social_cut"]), [f"social evidence missing ({soc.get('status', 'none')}): "
                                                  f"size x{settings['no_social_cut']}"]
    return 1.0, []
