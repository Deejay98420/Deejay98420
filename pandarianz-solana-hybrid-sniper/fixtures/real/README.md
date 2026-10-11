# Real mainnet transaction fixtures

This directory is for **real** mainnet transactions, captured read-only with:

```bash
./scripts/sniper capture 25 150 300 2     # N new transactions, ≤150 requests, ≤300 s, 2 req/s
```

Each file holds the verbatim `getTransaction` (jsonParsed, `maxSupportedTransactionVersion: 1`)
result plus provenance (request parameters, transaction version, slot, block time, classification).
Files here are **never modified or replaced**: capture writes new files with `create_new` and skips
signatures already present; verification only reads them (`core/tests/real_fixtures.rs` hashes
every file before and after). Reports are written to `reports/verification/`, not here.

**Status:** empty. The build environment had no Solana RPC access (egress blocked), so no mainnet
transaction has been captured yet. Synthetic fixtures live in `../synthetic/` and are never mixed in here.
