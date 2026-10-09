"""Solana mint verification.

Reads the mint twice (jsonParsed and raw base64) and requires both views to agree on supply,
decimals, authorities AND the Token-2022 extension set. Supply is kept as the exact raw integer
string; it is never converted through a float.

SPL mint layout (82 bytes, identical for Token and Token-2022 base):
  0..4   mint_authority COption tag (u32 LE), 4..36 pubkey
  36..44 supply u64 LE
  44     decimals u8
  45     is_initialized bool
  46..50 freeze_authority COption tag, 50..82 pubkey

Token-2022 mint with extensions (spl-token-2022 layout, restored from the v2.2 safeguards):
  82..165  zero padding (the mint is padded to the token-account size)
  165      AccountType, must be 1 (Mint)
  166..    TLV entries: type u16 LE, length u16 LE, value[length]
           a type-0 (Uninitialized) header or all-zero tail ends the region; any non-zero byte
           after it is malformed.

Policy: METADATA-ONLY. The only extensions this desk understands are metadataPointer (18) and
tokenMetadata (19). Every other raw extension ID, known or unknown, is rejected from the raw
bytes themselves, whatever the parsed view or the settings allowlist say. The settings allowlist
can only NARROW this set, never widen it.
"""

from __future__ import annotations

import base64
import binascii
import struct
from dataclasses import asdict, dataclass, field

from .util import b58decode, b58encode, inum

TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
TOKEN_2022_PROGRAM = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
MAX_DECIMALS = 18
BASE_LEN = 82
ACCOUNT_LEN = 165            # Token-2022 pads mints with extensions to the account size
ACCOUNT_TYPE_MINT = 1

# Hard-coded supported set. Not configurable upwards.
SUPPORTED_EXTENSIONS = {18: "metadataPointer", 19: "tokenMetadata"}
SUPPORTED_NAMES = frozenset(SUPPORTED_EXTENSIONS.values())
METADATA_POINTER_LEN = 64            # authority (32) + metadata address (32), OptionalNonZeroPubkey
TOKEN_METADATA_MIN_LEN = 32 + 32 + 4 + 4 + 4 + 4
MAX_METADATA_STRING = 4096

# Names for diagnostics only (spl-token-2022 ExtensionType). Never used to allow anything.
KNOWN_EXTENSION_NAMES = {
    1: "transferFeeConfig", 2: "transferFeeAmount", 3: "mintCloseAuthority",
    4: "confidentialTransferMint", 5: "confidentialTransferAccount", 6: "defaultAccountState",
    7: "immutableOwner", 8: "memoTransfer", 9: "nonTransferable", 10: "interestBearingConfig",
    11: "cpiGuard", 12: "permanentDelegate", 13: "nonTransferableAccount", 14: "transferHook",
    15: "transferHookAccount", 16: "confidentialTransferFeeConfig", 17: "confidentialTransferFeeAmount",
    18: "metadataPointer", 19: "tokenMetadata", 20: "groupPointer", 21: "tokenGroup",
    22: "groupMemberPointer", 23: "tokenGroupMember",
}


class RawMintError(ValueError):
    """reason is one of: token2022_layout_invalid, extension_truncated, extension_duplicate,
    extension_malformed, unsupported_extension."""

    def __init__(self, reason: str, detail: str):
        super().__init__(detail)
        self.reason, self.detail = reason, detail


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
    raw_extensions: list = field(default_factory=list)
    slot: int | None = None
    parsed_raw_agree: bool | None = None

    def as_dict(self):
        return asdict(self)


def ext_label(kind: int) -> str:
    return f"{KNOWN_EXTENSION_NAMES.get(kind, 'unknown')}({kind})"


def _read_string(v: bytes, pos: int, what: str) -> tuple[str, int]:
    if pos + 4 > len(v):
        raise RawMintError("extension_malformed", f"tokenMetadata {what} length truncated")
    n = int.from_bytes(v[pos:pos + 4], "little")
    pos += 4
    if n > MAX_METADATA_STRING or pos + n > len(v):
        raise RawMintError("extension_malformed", f"tokenMetadata {what} overruns the extension")
    try:
        s = v[pos:pos + n].decode("utf-8")
    except UnicodeDecodeError:
        raise RawMintError("extension_malformed", f"tokenMetadata {what} is not UTF-8") from None
    return s, pos + n


def parse_token_metadata(v: bytes, expected_mint: bytes | None) -> dict:
    """Borsh TokenMetadata: update_authority(32) mint(32) name symbol uri additional_metadata.
    The value must be consumed exactly; its mint must be this mint."""
    if len(v) < TOKEN_METADATA_MIN_LEN:
        raise RawMintError("extension_malformed", f"tokenMetadata length {len(v)} < {TOKEN_METADATA_MIN_LEN}")
    mint = v[32:64]
    if expected_mint is not None and mint != expected_mint:
        raise RawMintError("extension_malformed", "tokenMetadata mint field is a different mint")
    pos = 64
    out = {}
    for what in ("name", "symbol", "uri"):
        out[what], pos = _read_string(v, pos, what)
    if pos + 4 > len(v):
        raise RawMintError("extension_malformed", "tokenMetadata additional_metadata count truncated")
    count = int.from_bytes(v[pos:pos + 4], "little")
    pos += 4
    if count > 256:
        raise RawMintError("extension_malformed", "tokenMetadata additional_metadata count implausible")
    for i in range(count):
        _, pos = _read_string(v, pos, f"additional_metadata[{i}].key")
        _, pos = _read_string(v, pos, f"additional_metadata[{i}].value")
    if pos != len(v):
        raise RawMintError("extension_malformed", f"tokenMetadata has {len(v) - pos} unexplained trailing bytes")
    return out


def parse_raw_extensions(b: bytes, expected_mint: bytes | None) -> list[str]:
    """Validate the complete Token-2022 region after the 82-byte base. Returns extension names
    in raw order. Raises RawMintError on any layout, length, duplicate or policy problem."""
    if len(b) == BASE_LEN:
        return []
    if len(b) <= ACCOUNT_LEN:
        raise RawMintError("token2022_layout_invalid", f"length {len(b)} is between base mint and account size")
    if any(b[BASE_LEN:ACCOUNT_LEN]):
        raise RawMintError("token2022_layout_invalid", "non-zero bytes in mint padding (82..165)")
    if b[ACCOUNT_LEN] != ACCOUNT_TYPE_MINT:
        raise RawMintError("token2022_layout_invalid", f"account type {b[ACCOUNT_LEN]} is not Mint (1)")
    seen: list[int] = []
    pos = ACCOUNT_LEN + 1
    while pos < len(b):
        if len(b) - pos < 4:
            if any(b[pos:]):
                raise RawMintError("extension_truncated", f"{len(b) - pos} trailing bytes cannot hold a TLV header")
            break
        kind, length = struct.unpack_from("<HH", b, pos)
        if kind == 0:                                  # Uninitialized: end of extensions
            if any(b[pos:]):
                raise RawMintError("extension_malformed", "non-zero bytes after the extension terminator")
            break
        pos += 4
        if pos + length > len(b):
            raise RawMintError("extension_truncated", f"{ext_label(kind)} declares {length} bytes, "
                                                      f"{len(b) - pos} remain")
        if kind in seen:
            raise RawMintError("extension_duplicate", f"{ext_label(kind)} appears twice")
        seen.append(kind)
        value = b[pos:pos + length]
        pos += length
        if kind not in SUPPORTED_EXTENSIONS:
            raise RawMintError("unsupported_extension", f"raw {ext_label(kind)}")
        if kind == 18 and length != METADATA_POINTER_LEN:
            raise RawMintError("extension_malformed", f"metadataPointer length {length} != {METADATA_POINTER_LEN}")
        if kind == 19:
            parse_token_metadata(value, expected_mint)
    return [SUPPORTED_EXTENSIONS[k] for k in seen]


def decode_raw_mint(b: bytes) -> dict:
    if len(b) < BASE_LEN:
        raise ValueError("mint account shorter than 82 bytes")
    tag_m = int.from_bytes(b[0:4], "little")
    tag_f = int.from_bytes(b[46:50], "little")
    if tag_m not in (0, 1) or tag_f not in (0, 1):
        raise ValueError("invalid COption tag")
    if (tag_m == 0 and any(b[4:36])) or (tag_f == 0 and any(b[50:82])):
        raise ValueError("authority bytes present with a None tag")
    return {
        "mint_authority": b58encode(b[4:36]) if tag_m == 1 else None,
        "supply": int.from_bytes(b[36:44], "little"),
        "decimals": b[44],
        "is_initialized": b[45] == 1,
        "freeze_authority": b58encode(b[50:82]) if tag_f == 1 else None,
        "account_type": b[ACCOUNT_LEN] if len(b) > ACCOUNT_LEN else None,
    }


def effective_allowlist(configured) -> set[str]:
    """Settings can only narrow the supported metadata-only set."""
    return {x for x in (configured or []) if x in SUPPORTED_NAMES}


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

    # raw view: base layout
    try:
        data = rv.get("data")
        if not (isinstance(data, list) and len(data) == 2 and data[1] == "base64" and isinstance(data[0], str)):
            raise ValueError("raw data is not base64")
        raw_bytes = base64.b64decode(data[0], validate=True)
        raw = decode_raw_mint(raw_bytes)
    except (ValueError, binascii.Error) as e:
        return MintCheck(False, "mint_raw_invalid", str(e)[:120], mint=expected_mint, program=program)
    if program == "spl-token" and len(raw_bytes) != BASE_LEN:
        return MintCheck(False, "mint_raw_invalid", f"classic SPL mint length {len(raw_bytes)} != 82",
                         mint=expected_mint, program=program)

    # raw view: complete Token-2022 TLV region, validated from the bytes themselves
    raw_exts: list[str] = []
    if program == "spl-token-2022":
        try:
            mint_bytes = b58decode(expected_mint)
            if len(mint_bytes) != 32:
                raise ValueError("mint address is not 32 bytes")
        except ValueError:
            return MintCheck(False, "mint_raw_invalid", "mint address is not valid base58",
                             mint=expected_mint, program=program)
        try:
            raw_exts = parse_raw_extensions(raw_bytes, mint_bytes)
        except RawMintError as e:
            return MintCheck(False, e.reason, e.detail[:160], mint=expected_mint, program=program, slot=slot)

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
    if info.get("isInitialized") is not True or not raw["is_initialized"]:
        return MintCheck(False, "mint_uninitialized", None, mint=expected_mint, program=program)
    if "mintAuthority" not in info or "freezeAuthority" not in info:
        return MintCheck(False, "mint_view_mismatch", "parsed view omits authority fields",
                         mint=expected_mint, program=program)

    agree = (int(p_sup) == raw["supply"] and p_dec == raw["decimals"]
             and info.get("mintAuthority") == raw["mint_authority"]
             and info.get("freezeAuthority") == raw["freeze_authority"])
    if not agree:
        return MintCheck(False, "mint_view_mismatch", "parsed and raw mint data disagree",
                         mint=expected_mint, program=program, parsed_raw_agree=False)

    # parsed extension list must be well-formed and agree with the raw set exactly
    p_ext = info.get("extensions", [])
    if p_ext is None:
        p_ext = []
    if not isinstance(p_ext, list) or not all(isinstance(e, dict) and isinstance(e.get("extension"), str)
                                              for e in p_ext):
        return MintCheck(False, "extension_view_mismatch", "unreadable parsed extension entries",
                         mint=expected_mint, program=program)
    parsed_exts = [e["extension"] for e in p_ext]
    if program == "spl-token" and parsed_exts:
        return MintCheck(False, "mint_view_mismatch", "classic mint reports extensions",
                         mint=expected_mint, program=program)
    if len(set(parsed_exts)) != len(parsed_exts) or sorted(parsed_exts) != sorted(raw_exts):
        return MintCheck(False, "extension_view_mismatch",
                         f"parsed {sorted(parsed_exts)} vs raw {sorted(raw_exts)}"[:160],
                         mint=expected_mint, program=program, raw_extensions=raw_exts, extensions=parsed_exts,
                         parsed_raw_agree=False)

    base = dict(mint=expected_mint, program=program, raw_supply=str(raw["supply"]),
                decimals=raw["decimals"], mint_authority=raw["mint_authority"],
                freeze_authority=raw["freeze_authority"], extensions=raw_exts, raw_extensions=raw_exts,
                slot=slot, parsed_raw_agree=True)
    if raw["mint_authority"]:
        return MintCheck(False, "mint_authority_open", "supply can still be minted", **base)
    if raw["freeze_authority"]:
        return MintCheck(False, "freeze_authority_open", "balances can be frozen", **base)
    allowed = effective_allowlist(allowed_extensions)
    bad = [e for e in raw_exts if e not in allowed]
    if bad:
        return MintCheck(False, "unsupported_extension", "not in allowlist: " + ",".join(bad)[:120], **base)
    return MintCheck(True, None, None, **base)
