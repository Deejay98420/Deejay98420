# Mainnet evidence V1.2.3 PumpSwap capture (20 real transactions)

Byte-for-byte copies of the 20 Solana Mainnet PumpSwap transactions (10 buys, 10 sells selected)
captured read-only on the user's machine with `capture --venue pumpswap` (V1.2.3), from
`hybrid-sniper-v123-pumpswap-evidence.zip` (sha256
`98b20299b88090baa24bc8c486bb86c4ff101d63409ff491251e15d328d06e64`). `EVIDENCE_MANIFEST.json` is the
archive's own manifest (it also lists the 15 V1.2.1 files, kept in `../v121/`); the capture
manifest `capture-pumpswap-*.json` is included as recorded. `core/tests/v124_pumpswap_replay.rs`
checks every file against the manifest before replaying and never writes here.

Public blockchain data only.
