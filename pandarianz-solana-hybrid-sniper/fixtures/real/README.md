# Real mainnet transaction fixtures

This directory is for **real** mainnet transactions, captured read-only with:

```bash
cd core && ../core/target/release/sniper capture --from-live 25      # or --signatures <sig1,sig2,...>
```

Each file holds the verbatim `getTransaction` (jsonParsed) result plus provenance.
`core/tests/real_fixtures.rs` decodes every file and checks layout and reserve invariants against
the venue math. The tests skip when no files are present, and the report says so.

**Status:** empty. The build environment had no Solana RPC access (egress blocked), so no mainnet
transaction has been captured yet. Synthetic fixtures live in `../synthetic/` and are never mixed in here.
