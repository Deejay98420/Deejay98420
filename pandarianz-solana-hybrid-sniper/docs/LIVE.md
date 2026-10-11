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

## 3. Validate the live connection (no trading state is persisted)

```bash
./scripts/sniper verify-live 120      # 2 minutes; writes reports/live-validation-<ts>.json
```

The verdict must show `"live_validated": true`, decoded venue events > 0, zero decode warnings, and feed state HEALTHY. Any problem is listed explicitly.

## 4. Verify the decoders against real mainnet transactions

```bash
./scripts/sniper capture 25           # saves fixtures/real/*.json, then runs the real-fixture checks
```

Commit `fixtures/real/` afterwards. These files are permanent regression fixtures.

## 5. Run live-data PAPER trading

```bash
./scripts/sniper start live --profile main     # dashboard: http://127.0.0.1:3000
./scripts/sniper status
./scripts/sniper logs
./scripts/sniper stop                          # graceful; restart with the same profile resumes
./scripts/sniper report LIVE:main
```

The dashboard banner must read **LIVE DATA / PAPER TRADING** with feed **HEALTHY**. While the feed is DEGRADED or DOWN, new entries are blocked and the gap is recorded.
