from .base import CandidateJudgment, Judge, JudgeSchemaError, JudgeUnavailable, PickResult  # noqa: F401
from .rules import RulesJudge  # noqa: F401
from .typed_ai import TypedAIJudge, TypeSafeClient  # noqa: F401


def make_judge(engine: str, http, typesafe_client=None):
    if engine == "RULES":
        return RulesJudge()
    if engine == "TYPED_AI":
        return TypedAIJudge(typesafe_client or TypeSafeClient(http))
    raise ValueError(f"unknown judge engine {engine}")
