"""Solana mint verification.

Reads the mint twice (jsonParsed and raw base64) and requires both views to agree on
supply, decimals and authorities. Supply is kept as the exact raw integer string; it is never
converted through a float.

SPL mint layout (82 bytes, identical for Token and Token-2022 base):
  0..4   mint_authority COption tag (u32 LE), 4..36 pubkey
  36..44 supply u64 LE
  44     decimals u8
  45     is_initialized bool
  46..50 freeze_authority COption tag, 50..82 pubkey
Token-2022 mints with extensions are padded to 165 bytes, then byte 165 = AccountType (1=Mint).
"""

from __future__ import annotations

import base64
import binascii
from dataclasses import asdict, dataclass, field

from .util import b58encode, inum

TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
TOKEN_2022_PROGRAM = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
MAX_DECIMALS = 18


@dataclass
class MintCheck:
    ok: bool
    reason: str | None = None
    detail: str | None = None
    mint: str | None = None
    program: str | None = None
    raw_supply: str | None = None      # exact integer as a string
    decimals: int | None = None
    mint_authority: str | None = None
    freeze_authority: str | None = None
    extensions: list = field(default_factory=list)
    slot: int | None = None
    parsed_raw_agree: bool | None = None

    def as_dict(self):
        return asdict(self)


def decode_raw_mint(b: bytes) -> dict:
    if len(b) < 82:
        raise ValueError("mint account shorter than 82 bytes")
    tag_m = int.from_bytes(b[0:4], "little")
    tag_f = int.from_bytes(b[46:50], "little")
    if tag_m not in (0, 1) or tag_f not in (0, 1):
        raise ValueError("invalid COption tag")
    out = {
        "mint_authority": b58encode(b[4:36]) if tag_m == 1 else None,
        "supply": int.from_bytes(b[36:44], "little"),
        "decimals": b[44],
        "is_initialized": b[45] == 1,
        "freeze_authority": b58encode(b[50:82]) if tag_f == 1 else None,
        "account_type": None,
    }
    if len(b) > 82:
        if len(b) <= 165:
            raise ValueError("Token-2022 mint padding is malformed")
        out["account_type"] = b[165]
    return out


def verify_mint(expected_mint: str, parsed_res, raw_res, allowed_extensions) -> MintCheck:
    if not isinstance(parsed_res, dict) or not isinstance(raw_res, dict):
        return MintCheck(False, "rpc_bad_response", "missing RPC result", mint=expected_mint)
    pv, rv = parsed_res.get("value"), raw_res.get("value")
    slot = inum((parsed_res.get("context") or {}).get("slot"))
    if pv is None or rv is None:
        return MintCheck(False, "mint_missing", "no account at this address", mint=expected_mint, slot=slot)
    owner = pv.get("owner")
    if owner != rv.get("owner"):
        return MintCheck(False, "mint_view_mismatch", "parsed and raw owners differ", mint=expected_mint)
    if owner not in (TOKEN_PROGRAM, TOKEN_2022_PROGRAM):
        return MintCheck(False, "unsupported_program", f"owner {str(owner)[:44]}", mint=expected_mint)
    program = "spl-token" if owner == TOKEN_PROGRAM else "spl-token-2022"

    parsed = (pv.get("data") or {}).get("parsed") if isinstance(pv.get("data"), dict) else None
    if not isinstance(parsed, dict) or parsed.get("type") != "mint":
        return MintCheck(False, "not_a_mint", "account is not a mint", mint=expected_mint, program=program)
    info = parsed.get("info") or {}

    # raw view
    try:
        data = rv.get("data")
        if not (isinstance(data, list) and len(data) == 2 and data[1] == "base64"):
            raise ValueError("raw data is not base64")
        raw = decode_raw_mint(base64.b64decode(data[0], validate=True))
    except (ValueError, binascii.Error) as e:
        return MintCheck(False, "mint_raw_invalid", str(e)[:120], mint=expected_mint, program=program)

    # decimals and supply validation
    p_dec = info.get("decimals")
    if isinstance(p_dec, bool) or not isinstance(p_dec, int) or not 0 <= p_dec <= MAX_DECIMALS:
        return MintCheck(False, "invalid_decimals", f"decimals={p_dec!r}", mint=expected_mint, program=program)
    p_sup = info.get("supply")
    if not isinstance(p_sup, str) or not p_sup.isdigit():
        return MintCheck(False, "invalid_supply", "supply is not an integer string", mint=expected_mint,
                         program=program)
    if int(p_sup) <= 0:
        return MintCheck(False, "invalid_supply", "supply is zero", mint=expected_mint, program=program)
    if not info.get("isInitialized", False) or not raw["is_initialized"]:
        return MintCheck(False, "mint_uninitialized", None, mint=expected_mint, program=program)

    agree = (int(p_sup) == raw["supply"] and p_dec == raw["decimals"]
             and info.get("mintAuthority") == raw["mint_authority"]
             and info.get("freezeAuthority") == raw["freeze_authority"])
    if program == "spl-token-2022" and raw["account_type"] not in (None, 1):
        agree = False
    if not agree:
        return MintCheck(False, "mint_view_mismatch", "parsed and raw mint data disagree",
                         mint=expected_mint, program=program, parsed_raw_agree=False)

    exts = []
    for e in info.get("extensions") or []:
        if isinstance(e, dict) and isinstance(e.get("extension"), str):
            exts.append(e["extension"])
        else:
            return MintCheck(False, "unsupported_extension", "unreadable extension entry",
                             mint=expected_mint, program=program)
    if program == "spl-token" and exts:
        return MintCheck(False, "mint_view_mismatch", "classic mint reports extensions",
                         mint=expected_mint, program=program)

    base = dict(mint=expected_mint, program=program, raw_supply=str(raw["supply"]),
                decimals=raw["decimals"], mint_authority=raw["mint_authority"],
                freeze_authority=raw["freeze_authority"], extensions=exts, slot=slot,
                parsed_raw_agree=True)
    if raw["mint_authority"]:
        return MintCheck(False, "mint_authority_open", "supply can still be minted", **base)
    if raw["freeze_authority"]:
        return MintCheck(False, "freeze_authority_open", "balances can be frozen", **base)
    bad = [e for e in exts if e not in set(allowed_extensions)]
    if bad:
        return MintCheck(False, "unsupported_extension", ",".join(bad)[:120], **base)
    return MintCheck(True, None, None, **base)
