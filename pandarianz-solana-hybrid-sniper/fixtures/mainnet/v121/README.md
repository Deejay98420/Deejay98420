# Mainnet evidence V1.2.1 (15 real transactions)

Byte-for-byte copies of the 15 Solana Mainnet transactions captured read-only on the user's machine
with V1.2.1 (`getTransaction`, jsonParsed), from `hybrid-sniper-v121-mainnet-evidence.zip`
(sha256 `6673aea03e91ebab166771a7af6e8a7d91990c15d5088c67647a36e590e7cdee`). `EVIDENCE_MANIFEST.json`
is the archive's own manifest; `core/tests/v123_mainnet_replay.rs` checks every file against it
before replaying, and never writes here.

Public blockchain data only (signatures, accounts, balances, logs). These copies are regression
fixtures; the user's originals stay in their untracked `fixtures/real/`.
