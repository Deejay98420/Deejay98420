import base64
import unittest
from decimal import Decimal

from pandabrain import config
from pandabrain.mint import TOKEN_2022_PROGRAM, TOKEN_PROGRAM, MintCheck, decode_raw_mint, verify_mint
from pandabrain.providers.fixtures import FixtureWorld, default_specs
from pandabrain.valuation import (PROVIDER_MARKET_CAP, TOTAL_SUPPLY_VALUATION, ValuationError, choose,
                                  total_supply_valuation)

ST = config.defaults()


def mc(raw="1000000000000000", dec=6, mint="M"):
    return MintCheck(True, mint=mint, raw_supply=raw, decimals=dec, program="spl-token", slot=1)


class TotalSupplyValuation(unittest.TestCase):
    def test_raw_supply_decimal_scaling_exact(self):
        # 1e9 tokens with 6 decimals at $0.00042 = $420,000 exactly
        self.assertEqual(total_supply_valuation("0.00042", "1000000000000000", 6), Decimal("420000.00000"))
        # 9 decimals: same human supply needs 3 more zeros in the raw integer
        self.assertEqual(total_supply_valuation("0.00042", "1000000000000000000", 9), Decimal("420000"))
        # 0 decimals
        self.assertEqual(total_supply_valuation("2", "5", 0), Decimal("10"))

    def test_huge_raw_supply_not_lost_to_float(self):
        raw = "18446744073709551615"          # u64 max
        v = total_supply_valuation("1", raw, 18)
        self.assertEqual(v, Decimal("18.446744073709551615"))

    def test_invalid_inputs_rejected(self):
        for price, raw, d in [("nan", "1", 0), ("inf", "1", 0), ("-1", "1", 0), ("0", "1", 0), (None, "1", 0),
                              ("1", "0", 0), ("1", "-5", 0), ("1", "1.5", 0), ("1", "abc", 0),
                              ("1", "1", -1), ("1", "1", 19), ("1", "1", True), ("1", 100, 6)]:
            with self.subTest(price=price, raw=raw, d=d):
                with self.assertRaises(ValuationError):
                    total_supply_valuation(price, raw, d)


class ChooseValuation(unittest.TestCase):
    def snap(self, **kw):
        s = {"token_address": "M", "price_usd": "0.00042", "source": "geckoterminal", "fetched_at": 1.0,
             "provider_ts": 1.0, "mcap_usd": None, "mcap_source": None, "fdv_usd": "999999999"}
        s.update(kw)
        return s

    def test_provider_market_cap_preferred(self):
        r = choose(self.snap(mcap_usd="300000", mcap_source="dexscreener"), mc(), ST)
        self.assertTrue(r["ok"])
        self.assertEqual(r["valuation"]["type"], PROVIDER_MARKET_CAP)
        self.assertEqual(r["valuation"]["source"], "dexscreener")

    def test_total_supply_used_when_mcap_missing_and_labelled(self):
        r = choose(self.snap(), mc(), ST)
        self.assertTrue(r["ok"])
        v = r["valuation"]
        self.assertEqual(v["type"], TOTAL_SUPPLY_VALUATION)
        self.assertEqual(v["value_usd"], "420000.00")
        self.assertEqual(v["raw_supply"], "1000000000000000")
        self.assertEqual(v["decimals"], 6)
        self.assertIn("not circulating market cap", v["note"])
        self.assertIn("10**6", v["derivation"])

    def test_fdv_never_used_as_market_cap(self):
        r = choose(self.snap(), None, ST)
        self.assertFalse(r["ok"])
        self.assertEqual(r["reason"], "valuation_unavailable")

    def test_separate_limits(self):
        st = dict(ST, mcap_max_usd=100_000, supply_valuation_max_usd=1_000_000)
        self.assertEqual(choose(self.snap(mcap_usd="420000"), mc(), st)["reason"], "mcap_above_max")
        self.assertTrue(choose(self.snap(), mc(), st)["ok"])          # supply limit is the one applied
        st2 = dict(ST, supply_valuation_max_usd=100_000)
        self.assertEqual(choose(self.snap(), mc(), st2)["reason"], "supply_valuation_above_max")

    def test_identity_mismatch(self):
        self.assertEqual(choose(self.snap(), mc(mint="OTHER"), ST)["reason"], "valuation_identity_mismatch")

    def test_nonfinite_price(self):
        self.assertEqual(choose(self.snap(price_usd="NaN"), mc(), ST)["reason"], "valuation_invalid")


class MintVerification(unittest.TestCase):
    def setUp(self):
        self.w = FixtureWorld()
        self.w.specs = default_specs(self.w.t0)

    def views(self, sym):
        s = next(x for x in self.w.specs if x["sym"] == sym)
        parsed = {"context": {"slot": 5}, "value": {"owner": self.w.owner(s), "data": {
            "parsed": {"type": "mint", "info": self.w.mint_parsed(s)}}}}
        raw = {"context": {"slot": 5}, "value": {"owner": self.w.owner(s), "data": [
            base64.b64encode(self.w.mint_bytes(s)).decode(), "base64"]}}
        return s, parsed, raw

    def test_clean_mint(self):
        s, p, r = self.views("BAMBOO")
        m = verify_mint(s["token"], p, r, ST["allowed_token2022_extensions"])
        self.assertTrue(m.ok, m.reason)
        self.assertEqual(m.raw_supply, str(s["supply"]))
        self.assertEqual(m.decimals, 6)
        self.assertTrue(m.parsed_raw_agree)

    def test_token2022_metadata_only_allowed(self):
        s, p, r = self.views("JADE")
        m = verify_mint(s["token"], p, r, ST["allowed_token2022_extensions"])
        self.assertTrue(m.ok, m.reason)
        self.assertEqual(m.program, "spl-token-2022")
        self.assertEqual(p["value"]["owner"], TOKEN_2022_PROGRAM)

    def test_unsupported_extension(self):
        s, p, r = self.views("FEEZ")
        m = verify_mint(s["token"], p, r, ST["allowed_token2022_extensions"])
        self.assertFalse(m.ok)
        self.assertEqual(m.reason, "unsupported_extension")
        self.assertIn("transferFeeConfig", m.detail)

    def test_authorities(self):
        for sym, reason in (("MINTY", "mint_authority_open"), ("FROST", "freeze_authority_open")):
            s, p, r = self.views(sym)
            self.assertEqual(verify_mint(s["token"], p, r, []).reason, reason)

    def test_parsed_raw_disagreement(self):
        s, p, r = self.views("BAMBOO")
        p["value"]["data"]["parsed"]["info"]["supply"] = str(s["supply"] + 1)
        self.assertEqual(verify_mint(s["token"], p, r, []).reason, "mint_view_mismatch")
        s, p, r = self.views("JADE")
        p["value"]["data"]["parsed"]["info"]["decimals"] = 9
        self.assertEqual(verify_mint(s["token"], p, r, ST["allowed_token2022_extensions"]).reason, "mint_view_mismatch")

    def test_invalid_decimals_and_supply(self):
        s, p, r = self.views("BAMBOO")
        p["value"]["data"]["parsed"]["info"]["decimals"] = 40
        self.assertEqual(verify_mint(s["token"], p, r, []).reason, "invalid_decimals")
        s, p, r = self.views("BAMBOO")
        p["value"]["data"]["parsed"]["info"]["supply"] = "1e9"
        self.assertEqual(verify_mint(s["token"], p, r, []).reason, "invalid_supply")

    def test_missing_and_foreign_program(self):
        s, p, r = self.views("BAMBOO")
        self.assertEqual(verify_mint(s["token"], {"value": None}, {"value": None}, []).reason, "mint_missing")
        p["value"]["owner"] = r["value"]["owner"] = "11111111111111111111111111111111"
        self.assertEqual(verify_mint(s["token"], p, r, []).reason, "unsupported_program")

    def test_raw_layout_decoder(self):
        b = bytearray(82)
        b[36:44] = (123456789).to_bytes(8, "little")
        b[44] = 9
        b[45] = 1
        d = decode_raw_mint(bytes(b))
        self.assertEqual((d["supply"], d["decimals"], d["mint_authority"]), (123456789, 9, None))
        with self.assertRaises(ValueError):
            decode_raw_mint(bytes(40))
        self.assertEqual(TOKEN_PROGRAM[:5], "Token")


if __name__ == "__main__":
    unittest.main()
