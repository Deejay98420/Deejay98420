# Live-data PAPER trading on a Mac

The system only **observes** mainnet and simulates trades. It has no wallet, no keys, no signing and no transaction sending.

## 1. One-time setup

```bash
# prerequisites: Xcode CLT, Rust, Node 20+, Docker Desktop (running)
xcode-select --install                                   # if not installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
brew install node                                        # or use nvm

git clone <your repo url> && cd Deejay98420/pandarianz-solana-hybrid-sniper
git checkout claude/wizardly-cray-5bimf7
./scripts/sniper setup          # creates .env, builds, installs dashboard deps, starts PostgreSQL, migrates
```

## 2. Configure providers (`.env`)

Edit `.env` (never commit it). Replace the placeholders:

```
SOLANA_WS_URLS=wss://<provider-1-host>/?api-key=<key>,wss://<provider-2-host>/?api-key=<key>
SOLANA_RPC_URLS=                 # empty = derive https:// from the WS URLs
SOLANA_STREAM_KIND=logs          # or "transactions" if your provider supports transactionSubscribe
JUPITER_QUOTE_URL=<https://.../swap/v1/quote>   # optional, read-only quotes
JUPITER_API_KEY=                 # if your Jupiter plan needs one
```

A free public endpoint will rate-limit `getTransaction` quickly. A provider plan with WebSocket and enough RPC credits is recommended; choosing and paying for one is your decision.

## 2b. The one command

```bash
./scripts/sniper check
```

`check` runs everything below in order and ends with **NEXT REQUIRED ACTION**. The individual steps follow.

## 3. Provider diagnostics

```bash
./scripts/sniper doctor 20
```

For each provider, `doctor` shows which subscriptions are accepted or REJECTED, notification counts, RPC latency, and whether real PumpSwap transactions decode via `getTransaction`. It ends with `READY FOR LIVE` and the recommended `SOLANA_STREAM_KIND`. Keys are never printed.

## 3b. Validate the live connection (no trading state is persisted)

```bash
./scripts/sniper verify-live 120      # 2 minutes; writes reports/live-validation-<ts>.json
```

The verdict must show `"live_validated": true`, decoded venue events > 0, zero decode warnings, and feed state HEALTHY. Any problem is listed explicitly.

## 4. Verify the decoders against real mainnet transactions

```bash
./scripts/sniper capture 25           # diverse sample → fixtures/real/*.json (signature + slot provenance)
./scripts/sniper verify-decoders      # transaction-evidence checks → fixtures/real/verification.json
```

Every `MISMATCH` line must be understood before live results are trusted. Fills on venues without `MAINNET VERIFIED` are marked unverified and never count as validated P&L.

Commit `fixtures/real/` afterwards. These files are permanent regression fixtures.

## 5. Run live-data PAPER trading

```bash
./scripts/sniper start live --profile main     # dashboard: http://127.0.0.1:3000
./scripts/sniper status
./scripts/sniper logs
./scripts/sniper restart                       # safe restart: same profile, positions recovered
./scripts/sniper stop                          # graceful; the store is flushed
./scripts/sniper report LIVE:main              # VALIDATED HOLDOUT figures first
./scripts/sniper calibrate LIVE:main           # once ≥1000 calibration-split outcomes exist (never auto-applied)
```

The dashboard banner must read **LIVE DATA / PAPER TRADING** with feed **HEALTHY**. While the feed is DEGRADED or DOWN, new entries are blocked and the gap is recorded.
