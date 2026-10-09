# Changelog

## 3.0.1

Reviewed against the actual v2.2 source. Its mint safeguards (`mint_controls.py`) are the reference.

### Fixed
- **Raw Token-2022 extensions were not validated** (`pandabrain/mint.py`). v3.0.0 read only the
  AccountType byte, so a raw mint containing extension ID 1 (transferFeeConfig) was accepted when
  the parsed view reported only metadata extensions. Reproduced, then fixed:
  - the full TLV region is parsed: zero padding at 82–164, AccountType 1 (Mint), complete headers
    and values, type-0 terminator or zero tail, no data after the terminator;
  - the raw and parsed extension multisets must match exactly (`extension_view_mismatch`);
  - unsupported, unknown, duplicate, truncated or malformed extensions are rejected with distinct
    reasons;
  - `metadataPointer` must be exactly 64 bytes, and `tokenMetadata` is fully decoded (its mint
    field must equal this mint; every byte must be consumed);
  - the supported set is hard-coded metadata-only. `allowed_token2022_extensions` can only narrow
    it: saving wider values is refused, and old rows are sanitised on load;
  - classic SPL mints must be exactly 82 bytes; a `None` authority tag must have zero key bytes.
- Mint evidence is read at `finalized` commitment again (v2.2 behaviour; v3.0.0 used `confirmed`).
- An early-rejected POST (bad token/origin) left its unread body on the keep-alive connection,
  where it was parsed as a bogus next request. Such responses now close the connection.
- `scripts/stop.sh` now waits for the server to exit, so `data/` can be copied safely.

### Added
- `python3 -m pandabrain migrate-preview --from pandarianz.db [--json]`: a read-only v2.2 ledger
  preview. It works on a temporary copy, proves the source unchanged, maps cash, the open position,
  rebuilt closed trades and every event, reconciles cash, and lists blockers. **No importer yet.**
- 42 tests: 33 raw-mint regressions (including the exact reported disagreement, every known
  unsupported ID, allowlist bypass attempts and the engine-level rejection) and 9 preview tests
  built from rows produced by the actual v2.2 code. Total: 107.

### Corrected documentation
- The v2.2 public ledger is `pandarianz.db` (demo `pandarianz-demo.db`), not `desk.db`.
- v2.2's 52 tests were run and pass.

## 3.0.0
- First v3 release (built without access to the v2.2 source). See README.
