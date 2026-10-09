"""v3.0.1 regression: raw Token-2022 TLV bytes are validated on their own, not taken from the
parsed view or the settings allowlist."""

import base64
import hashlib
import struct
import unittest

from helpers import DeskCase, only

from pandabrain import config, store
from pandabrain.engine import run_once_cli
from pandabrain.mint import (KNOWN_EXTENSION_NAMES, SUPPORTED_NAMES, TOKEN_2022_PROGRAM, TOKEN_PROGRAM,
                             effective_allowlist, verify_mint)
from pandabrain.providers.fixtures import tlv_value
from pandabrain.util import b58encode

MINT_B = hashlib.sha256(b"regression-mint").digest()
MINT = b58encode(MINT_B)
OTHER_B = hashlib.sha256(b"other-mint").digest()
ALLOW = ["metadataPointer", "tokenMetadata"]
SUPPLY = 10 ** 15


def base(mint_auth=False, freeze_auth=False, garbage_in_none=False) -> bytes:
    b = bytearray(82)
    if mint_auth:
        b[0:4] = (1).to_bytes(4, "little")
        b[4:36] = b"\x07" * 32
    elif garbage_in_none:
        b[10] = 5
    b[36:44] = SUPPLY.to_bytes(8, "little")
    b[44] = 6
    b[45] = 1
    if freeze_auth:
        b[46:50] = (1).to_bytes(4, "little")
        b[50:82] = b"\x09" * 32
    return bytes(b)


def tlv(kind: int, value: bytes, length: int | None = None) -> bytes:
    return struct.pack("<HH", kind, len(value) if length is None else length) + value


def mp(mint=MINT_B):
    return tlv(18, tlv_value("metadataPointer", mint))


def tm(mint=MINT_B):
    return tlv(19, tlv_value("tokenMetadata", mint, "REG"))


def t22(*entries, account_type=1, padding=b"") -> bytes:
    pad = bytearray(165 - 82)
    pad[:len(padding)] = padding
    return base() + bytes(pad) + bytes([account_type]) + b"".join(entries)


def views(raw: bytes, parsed_exts, program=TOKEN_2022_PROGRAM, mint_auth=None, freeze_auth=None):
    info = {"decimals": 6, "supply": str(SUPPLY), "isInitialized": True,
            "mintAuthority": mint_auth, "freezeAuthority": freeze_auth}
    if parsed_exts is not None:
        info["extensions"] = [{"extension": e, "state": {}} for e in parsed_exts]
    parsed = {"context": {"slot": 7}, "value": {"owner": program, "data": {"parsed": {"type": "mint", "info": info}}}}
    rawr = {"context": {"slot": 7}, "value": {"owner": program, "data": [base64.b64encode(raw).decode(), "base64"]}}
    return parsed, rawr


def check(raw, parsed_exts, allow=ALLOW, **kw):
    p, r = views(raw, parsed_exts, **kw)
    return verify_mint(MINT, p, r, allow)


class ReportedBug(unittest.TestCase):
    def test_exact_disagreement_raw_transfer_fee_parsed_metadata_only(self):
        """Raw holds extension ID 1 (transferFeeConfig); parsed reports only metadataPointer."""
        raw = t22(mp(), tlv(1, bytes(108)))
        m = check(raw, ["metadataPointer"])
        self.assertFalse(m.ok)
        self.assertEqual(m.reason, "unsupported_extension")
        self.assertIn("transferFeeConfig(1)", m.detail)

    def test_same_bug_with_both_allowed_metadata_extensions_reported(self):
        raw = t22(mp(), tm(), tlv(1, bytes(108)))
        m = check(raw, ["metadataPointer", "tokenMetadata"])
        self.assertEqual(m.reason, "unsupported_extension")

    def test_transfer_fee_first_in_raw_order(self):
        self.assertEqual(check(t22(tlv(1, bytes(108)), mp()), ["metadataPointer"]).reason, "unsupported_extension")

    def test_raw_unsupported_rejected_even_when_parsed_view_also_reports_it(self):
        self.assertEqual(check(t22(tlv(1, bytes(108))), ["transferFeeConfig"]).reason, "unsupported_extension")


class SetAgreement(unittest.TestCase):
    def test_metadata_only_accepted(self):
        m = check(t22(mp(), tm()), ["tokenMetadata", "metadataPointer"])     # order-insensitive
        self.assertTrue(m.ok, (m.reason, m.detail))
        self.assertEqual(m.raw_extensions, ["metadataPointer", "tokenMetadata"])
        self.assertTrue(m.parsed_raw_agree)

    def test_parsed_omits_a_raw_extension(self):
        self.assertEqual(check(t22(mp(), tm()), ["metadataPointer"]).reason, "extension_view_mismatch")

    def test_parsed_reports_extension_raw_lacks(self):
        self.assertEqual(check(t22(mp()), ["metadataPointer", "tokenMetadata"]).reason, "extension_view_mismatch")
        self.assertEqual(check(base(), ["metadataPointer"]).reason, "extension_view_mismatch")   # 82-byte T22

    def test_parsed_duplicates_or_unreadable(self):
        self.assertEqual(check(t22(mp()), ["metadataPointer", "metadataPointer"]).reason, "extension_view_mismatch")
        p, r = views(t22(mp()), None)
        p["value"]["data"]["parsed"]["info"]["extensions"] = ["metadataPointer"]
        self.assertEqual(verify_mint(MINT, p, r, ALLOW).reason, "extension_view_mismatch")

    def test_parsed_omits_extension_field_entirely(self):
        self.assertEqual(check(t22(mp()), None).reason, "extension_view_mismatch")

    def test_plain_token2022_without_extensions(self):
        self.assertTrue(check(base(), []).ok)
        self.assertTrue(check(base(), None).ok)


class RawLayout(unittest.TestCase):
    def test_duplicate_raw_extension(self):
        self.assertEqual(check(t22(mp(), mp()), ["metadataPointer"]).reason, "extension_duplicate")

    def test_truncated_value(self):
        self.assertEqual(check(t22(tlv(18, bytes(20), length=64)), ["metadataPointer"]).reason, "extension_truncated")

    def test_truncated_header(self):
        self.assertEqual(check(t22(mp()) + b"\x12\x00", ["metadataPointer"]).reason, "extension_truncated")

    def test_zero_tail_after_entries_is_padding(self):
        self.assertTrue(check(t22(mp()) + bytes(3), ["metadataPointer"]).ok)
        self.assertTrue(check(t22(mp()) + bytes(40), ["metadataPointer"]).ok)

    def test_data_after_terminator(self):
        self.assertEqual(check(t22(mp()) + bytes(4) + b"\x01", ["metadataPointer"]).reason, "extension_malformed")
        # a smuggled transfer fee hidden behind an Uninitialized header
        hidden = t22(mp()) + tlv(0, b"") + tlv(1, bytes(108))
        self.assertEqual(check(hidden, ["metadataPointer"]).reason, "extension_malformed")

    def test_nonzero_padding(self):
        self.assertEqual(check(t22(mp(), padding=b"\x01"), ["metadataPointer"]).reason, "token2022_layout_invalid")

    def test_wrong_account_type(self):
        for at in (0, 2, 255):
            with self.subTest(account_type=at):
                self.assertEqual(check(t22(mp(), account_type=at), ["metadataPointer"]).reason,
                                 "token2022_layout_invalid")

    def test_lengths_between_base_and_account(self):
        for n in (83, 100, 165):
            with self.subTest(length=n):
                self.assertEqual(check(base() + bytes(n - 82), []).reason, "token2022_layout_invalid")

    def test_unknown_and_every_known_unsupported_id(self):
        for kind in [k for k in KNOWN_EXTENSION_NAMES if k not in (18, 19)] + [24, 77, 999, 65535]:
            with self.subTest(kind=kind):
                self.assertEqual(check(t22(tlv(kind, bytes(8))), []).reason, "unsupported_extension")

    def test_classic_spl_must_be_exactly_82_bytes(self):
        self.assertEqual(check(t22(mp()), [], program=TOKEN_PROGRAM).reason, "mint_raw_invalid")
        self.assertTrue(check(base(), [], program=TOKEN_PROGRAM).ok)

    def test_none_tag_with_authority_bytes(self):
        self.assertEqual(check(base(garbage_in_none=True), []).reason, "mint_raw_invalid")


class MetadataLengths(unittest.TestCase):
    def test_metadata_pointer_exact_length(self):
        for n in (0, 32, 63, 65, 128):
            with self.subTest(length=n):
                self.assertEqual(check(t22(tlv(18, bytes(n))), ["metadataPointer"]).reason, "extension_malformed")

    def test_token_metadata_structure(self):
        good = tlv_value("tokenMetadata", MINT_B, "REG")
        cases = {
            "too short": good[:70],
            "trailing bytes": good + b"\x00",
            "string overrun": good[:64] + (10_000).to_bytes(4, "little") + b"x",
            "bad utf8": good[:64] + (2).to_bytes(4, "little") + b"\xff\xfe" + good[64 + 4 + len("SYNTHETIC REG"):],
            "other mint": tlv_value("tokenMetadata", OTHER_B, "REG"),
            "count overrun": good[:-4] + (3).to_bytes(4, "little"),
        }
        for name, value in cases.items():
            with self.subTest(name):
                self.assertEqual(check(t22(tlv(19, value)), ["tokenMetadata"]).reason, "extension_malformed")

    def test_token_metadata_with_additional_fields(self):
        v = bytes(32) + MINT_B
        for s in ("Name", "SYM", "uri"):
            v += len(s).to_bytes(4, "little") + s.encode()
        v += (1).to_bytes(4, "little") + (3).to_bytes(4, "little") + b"key" + (5).to_bytes(4, "little") + b"value"
        self.assertTrue(check(t22(tlv(19, v)), ["tokenMetadata"]).ok)


class AllowlistCannotBypass(unittest.TestCase):
    def test_configured_extra_names_do_not_allow_raw_extensions(self):
        wide = ALLOW + ["transferFeeConfig", "transferHook", "permanentDelegate"]
        self.assertEqual(check(t22(tlv(1, bytes(108))), ["transferFeeConfig"], allow=wide).reason,
                         "unsupported_extension")
        self.assertEqual(check(t22(tlv(14, bytes(64))), ["transferHook"], allow=wide).reason,
                         "unsupported_extension")
        self.assertEqual(effective_allowlist(wide), set(SUPPORTED_NAMES))

    def test_allowlist_can_narrow(self):
        m = check(t22(mp(), tm()), ALLOW, allow=["metadataPointer"])
        self.assertEqual(m.reason, "unsupported_extension")
        self.assertIn("tokenMetadata", m.detail)
        self.assertEqual(check(t22(mp()), ["metadataPointer"], allow=[]).reason, "unsupported_extension")

    def test_settings_refuse_unsupported_names(self):
        with self.assertRaises(config.SettingsError) as cm:
            config.validate({"allowed_token2022_extensions": ["metadataPointer", "transferFeeConfig"]})
        self.assertIn("allowed_token2022_extensions", cm.exception.errors)
        config.validate({"allowed_token2022_extensions": ["metadataPointer"]})       # narrowing is fine

    def test_authorities_checked_alongside_extensions(self):
        self.assertEqual(check(t22(mp()), ["metadataPointer"], mint_auth=b58encode(b"\x07" * 32)).reason,
                         "mint_view_mismatch")


class StoredSettings(DeskCase):
    def test_old_settings_rows_with_wider_allowlist_load_safely(self):
        _, _, db = self.make()
        import json
        import time
        bad = dict(config.defaults(), allowed_token2022_extensions=["metadataPointer", "transferFeeConfig"])
        db.x("INSERT INTO settings_versions VALUES(99, ?, ?, 'v3.0.0 row')", (time.time(), json.dumps(bad)))
        v, st = store.active_settings(db)
        self.assertEqual(v, 99)
        self.assertEqual(st["allowed_token2022_extensions"], ["metadataPointer"])


class EngineLevel(DeskCase):
    def test_cycle_rejects_hidden_transfer_fee_before_the_judge(self):
        desk, w, db = self.make()
        only(w, "JADE")
        w.specs[0]["raw_extensions"] = ["metadataPointer", "tokenMetadata", "transferFeeConfig"]
        r = run_once_cli(desk)
        self.assertEqual(r["outcome"], "no_trade")
        self.assertEqual(r["stats"]["vet"], {"unsupported_extension": 1})
        self.assertEqual(db.q1("SELECT count(*) n FROM judgments")["n"], 0)
        ev = db.q1("SELECT reason FROM evaluations WHERE stage='vet'")
        self.assertEqual(ev["reason"], "unsupported_extension")
        self.assertEqual(db.q1("SELECT status FROM watchlist")["status"], "rejected")

    def test_cycle_rejects_raw_parsed_disagreement(self):
        desk, w, db = self.make()
        only(w, "JADE")
        w.specs[0]["raw_extensions"] = ["metadataPointer"]       # parsed still says both
        r = run_once_cli(desk)
        self.assertEqual(r["stats"]["vet"], {"extension_view_mismatch": 1})

    def test_clean_metadata_only_token2022_still_trades(self):
        desk, w, db = self.make()
        only(w, "JADE")
        r = run_once_cli(desk)
        self.assertEqual(r["outcome"], "opened")


if __name__ == "__main__":
    unittest.main()


class Commitment(unittest.TestCase):
    def test_mint_reads_use_finalized_commitment(self):
        import json as _j
        from pandabrain.providers import Http, Response
        from pandabrain.providers.solana_rpc import SolanaRpc
        seen = []

        def tr(method, url, headers, body, timeout):
            seen.append(_j.loads(body))
            return Response(200, b'{"jsonrpc":"2.0","id":1,"result":{"value":null}}', {})
        rpc = SolanaRpc(Http(transport=tr, sleep=lambda s: None), "https://rpc.local")
        rpc.account_parsed(MINT, None)
        rpc.account_raw(MINT, None)
        rpc.largest_accounts(MINT, None)
        self.assertEqual([r["params"][1]["commitment"] for r in seen], ["finalized"] * 3)
