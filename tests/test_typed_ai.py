import json
import os
import unittest

from helpers import DeskCase, only

from pandabrain import book
from pandabrain.config import defaults
from pandabrain.engine import run_once_cli
from pandabrain.judges import JudgeSchemaError, JudgeUnavailable, TypedAIJudge, TypeSafeClient
from pandabrain.judges import questions as Q
from pandabrain.judges.typed_ai import validate_response
from pandabrain.providers import Http, Response
from pandabrain.util import sanitize

ST = defaults()


def answer_for(q, choice=None, name=""):
    if q["type"] == "noul":
        return {"type": "noul", "noul": 0.9 if name == "liquidity_fits_ticket" else 0.2}
    if q["type"] == "choice":
        labels = list(q["criteria"])
        c = choice if choice in labels else labels[0]
        probs = {k: (0.7 if k == c else 0.3 / max(1, len(labels) - 1)) for k in labels}
        if len(labels) == 1:
            probs = {c: 1.0}
        return {"type": "choice", "choice": c, "confidence": 0.8, "probabilities": probs}
    n = len(q["criteria"])
    return {"type": "score", "score": n - 1, "confidence": 0.7, "legend": {}, "probabilities": {str(i): 1.0 / n for i in range(n)}}


class FakeTypeSafe:
    """Answers the wire contract; records requests. Lets tests break individual fields."""

    def __init__(self, model="jev-1.13.0", mutate=None, status=200):
        self.model, self.mutate, self.status, self.requests = model, mutate, status, []

    def __call__(self, method, url, headers, body, timeout):
        req = json.loads(body)
        self.requests.append({"url": url, "auth": headers.get("Authorization"), "body": req})
        if self.status != 200:
            return Response(self.status, b'{"detail":"x"}', {})
        qs = req["questions"]
        ans = {k: answer_for(q, "crowd", k) for k, q in qs.items()}
        out = {"model": self.model, "answers": ans, "usage": {"input_tokens": 100, "output_tokens": 5}}
        if self.mutate:
            self.mutate(out)
        return Response(200, json.dumps(out).encode(), {})


def judge(fake, key="ts-testkey"):
    return TypedAIJudge(TypeSafeClient(Http(transport=fake, sleep=lambda s: None), api_key=key, base_url="https://ts.local"))


def cand(cid="c_1"):
    return {"candidate_id": cid, "chain": "solana", "age_minutes": 90.0, "cash_usd": "10000", "symbol": "X",
            "top_10_percent": 20.0, "largest_account_share": 0.1, "holder_count": 500,
            "snapshot": {"liquidity_usd": 50_000, "volume_h1": 1, "volume_h6": 2, "volume_h24": 3, "buys_h1": 5,
                         "sells_h1": 4, "buys_h24": 50, "sells_h24": 40},
            "valuation": {"type": "TOTAL_SUPPLY_VALUATION", "value_usd": "1"}, "social": {"status": "adapter_disconnected"}}


class Validation(unittest.TestCase):
    def test_valid_response_passes(self):
        model, ans = validate_response({"model": "jev-1.13.0", "answers": {k: answer_for(q) for k, q in Q.MARKET.items()}},
                                       Q.MARKET, "jev-latest")
        self.assertEqual(model, "jev-1.13.0")

    def test_rejections(self):
        good = {k: answer_for(q, None, k) for k, q in Q.MARKET.items()}
        bad_cases = {
            "alias model": {"model": "jev-latest", "answers": good},
            "no version": {"model": "jev", "answers": good},
            "missing answer": {"model": "jev-1.0.0", "answers": {k: v for k, v in good.items() if k != "shape"}},
            "noul out of range": {"model": "jev-1.0.0", "answers": {**good, "liquidity_fits_ticket": {"type": "noul", "noul": 1.2}}},
            "noul nan": {"model": "jev-1.0.0", "answers": {**good, "liquidity_fits_ticket": {"type": "noul", "noul": float("nan")}}},
            "probs sum": {"model": "jev-1.0.0", "answers": {**good, "shape": {**good["shape"], "probabilities": {k: 0.5 for k in Q.MARKET["shape"]["criteria"]}}}},
            "choice not option": {"model": "jev-1.0.0", "answers": {**good, "shape": {**good["shape"], "choice": "moon"}}},
            "wrong type": {"model": "jev-1.0.0", "answers": {**good, "shape": {"type": "noul", "noul": 0.5}}},
            "extra answer": {"model": "jev-1.0.0", "answers": {**good, "injected": {"type": "noul", "noul": 1}}},
        }
        for name, body in bad_cases.items():
            with self.subTest(name):
                with self.assertRaises(JudgeSchemaError):
                    validate_response(body, Q.MARKET, "jev-latest")


class Adapter(unittest.TestCase):
    def test_unconfigured_without_key(self):
        j = judge(FakeTypeSafe(), key="")
        self.assertFalse(j.status()["configured"])
        with self.assertRaises(JudgeUnavailable):
            j.judge(cand(), ST)

    def test_grouped_calls_raw_answers_and_key_server_side(self):
        fake = FakeTypeSafe()
        j = judge(fake)
        r = j.judge(cand(), ST)
        self.assertEqual([x["question_set"] for x in r.raw], ["market", "solana"])   # social skipped: no evidence
        self.assertEqual(len(fake.requests), 2)                                       # one call per set
        self.assertEqual(fake.requests[0]["url"], "https://ts.local/v1/systemone")
        self.assertEqual(fake.requests[0]["auth"], "Bearer ts-testkey")
        self.assertEqual(r.model_id, "jev-1.13.0")
        self.assertEqual(r.size_factor, ST["no_social_cut"])
        self.assertTrue(r.passed)
        self.assertNotIn("ts-testkey", json.dumps(r.as_dict()))

    def test_pick_uses_stable_ids_and_absolute_gate(self):
        fake = FakeTypeSafe()
        j = judge(fake)
        a, b = cand("c_aaa"), cand("c_bbb")
        sv = [(a, j.judge(a, ST)), (b, j.judge(b, ST))]
        p = j.pick(sv, ST)
        crit = fake.requests[-1]["body"]["questions"]["best"]["criteria"]
        self.assertEqual(set(crit), {"c_aaa", "c_bbb"})
        self.assertIsNone(p.candidate_id)                     # worth noul 0.2 < 0.60
        self.assertEqual(p.reason, "not_worth_trading")

    def test_single_survivor_gets_only_the_absolute_gate(self):
        def worth_high(out):
            out["answers"]["worth_trading_at_all"]["noul"] = 0.9
        fake = FakeTypeSafe()
        j = judge(fake)
        a = cand()
        sv = [(a, j.judge(a, ST))]
        fake.mutate = worth_high
        p = j.pick(sv, ST)
        self.assertEqual(list(fake.requests[-1]["body"]["questions"]), ["worth_trading_at_all"])
        self.assertEqual(p.candidate_id, "c_1")
        self.assertIsNone(p.confidence)

    def test_errors_map_to_unavailable_or_schema(self):
        for status, exc in ((401, JudgeUnavailable), (429, JudgeUnavailable), (529, JudgeUnavailable), (422, JudgeSchemaError)):
            with self.subTest(status=status):
                with self.assertRaises(exc):
                    judge(FakeTypeSafe(status=status)).judge(cand(), ST)


class EngineTypedAI(DeskCase):
    def test_outage_records_no_trade_and_never_falls_back(self):
        client = TypeSafeClient(Http(transport=FakeTypeSafe(status=503), sleep=lambda s: None), api_key="ts-x",
                                base_url="https://ts.local")
        desk, w, db = self.make(settings={"judge_engine": "TYPED_AI"}, typesafe_client=client)
        only(w, "BAMBOO", "JADE")
        r = run_once_cli(desk)
        self.assertEqual((r["outcome"], r["reason"]), ("no_trade", "judge_unavailable"))
        self.assertIsNone(book.position(db))
        self.assertEqual(db.q1("SELECT count(*) n FROM judgments WHERE engine='RULES'")["n"], 0)

    def test_typed_ai_cycle_records_provenance(self):
        def worth_high(out):
            if "worth_trading_at_all" in out["answers"]:
                out["answers"]["worth_trading_at_all"]["noul"] = 0.9
        fake = FakeTypeSafe(mutate=worth_high)
        client = TypeSafeClient(Http(transport=fake, sleep=lambda s: None), api_key="ts-x", base_url="https://ts.local")
        desk, w, db = self.make(settings={"judge_engine": "TYPED_AI"}, typesafe_client=client)
        only(w, "BAMBOO", "JADE")
        r = run_once_cli(desk)
        self.assertEqual(r["outcome"], "opened")
        j = db.q1("SELECT * FROM judgments WHERE question_set='pick'")
        self.assertEqual(j["model_id"], "jev-1.13.0")
        self.assertEqual(j["threshold_version"], "thresholds-v3")
        self.assertIn("worth_trading_at_all", j["raw_json"])
        # untrusted token text never reaches the market/chain states
        for req in fake.requests:
            self.assertNotIn("Ignore previous instructions", json.dumps(req["body"]["state"]))


class Sanitize(unittest.TestCase):
    def test_secrets_redacted(self):
        s = sanitize("Authorization: Bearer ts-abcdef123456 url?api-key=SECRET bot123456789:AAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        for secret in ("ts-abcdef123456", "SECRET", "AAAAAAAAAAAAAAAAAAAA"):
            self.assertNotIn(secret, s)

    def test_rpc_url_display_hides_key(self):
        from pandabrain.providers.solana_rpc import SolanaRpc
        r = SolanaRpc(Http(), "https://mainnet.helius-rpc.com/?api-key=SUPERSECRET")
        self.assertNotIn("SUPERSECRET", r.display)
        self.assertEqual(os.environ.get("PANDABRAIN_NO_OUTBOUND"), "1")


if __name__ == "__main__":
    unittest.main()
