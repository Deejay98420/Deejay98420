# Assumptions, cost model and limitations

## Venue layouts

Pump.fun and PumpSwap layouts are verified against the official IDLs (`fixtures/idl/`, with provenance and sha256) in `tests/idl_verification.rs`. They have not yet been verified against captured mainnet transactions (pending RPC access; see `fixtures/real/README.md`).

Open semantic assumptions that only real captures can settle (`tests/real_fixtures.rs` checks them):

- **Curve `TradeEvent.sol_amount` is net of fees.** For observed execution checks, buyers pay `sol_amount + fee + creator_fee` and sellers receive `sol_amount − fees`.
- **PumpSwap pool vault updates.** v2 trades may leave protocol/creator fees inside the vault and offset them in `virtual_quote_reserves`. Each swap event carries the absolute pre-trade reserves, so any drift self-corrects on the next event.
- **Buyback and holder-reward fees** are taken to be slices of the protocol/creator fee, not extra charges. **Cashback** is treated as zero (conservative).

## Earlier layout notes

- **Pump.fun (`6EF8…F6P`)**: Anchor `emit!` → `Program data:` logs. Events:
  - `CreateEvent` (name, symbol, uri, mint, bonding_curve, user; optional creator, timestamp, reserves, supply);
  - `TradeEvent` (mint, sol_amount, token_amount, is_buy, user, timestamp, virtual reserves; optional real reserves, fee recipient/bps/fee, creator fee fields);
  - `CompleteEvent`;
  - `CompletePumpAmmMigrationEvent` (provides the PumpSwap pool).

  Optional trailing fields are decoded only when present.
- **PumpSwap (`pAMMBay…XEA`)**: Anchor `emit_cpi!` → inner-instruction data `e445a52e51cb9a1d ++ disc ++ borsh`. Decoded: `CreatePoolEvent`, `BuyEvent`, `SellEvent`, `DepositEvent`, `WithdrawEvent`. Pool reserves in swap events are taken as the values **before** the swap.
- **Raydium AMM v4**: `ray_log` (init, deposit, withdraw, swap_base_in, swap_base_out). Decode-only.
- Discriminators are `sha256("event:<Name>")[..8]`. A unit test checks the published Pump.fun `TradeEvent` value `bddb7fd34ee661ee`.

## Cost model (per scenario, configurable)

| Item | Default | Notes |
|---|---|---|
| Base fee | 5,000 lamports/tx | Charged on fills and on landed-but-failed txs. |
| Priority fee | 0.00005 / 0.0002 / 0.001 SOL | optimistic / base / pessimistic |
| Tip | 0 / 0.0001 / 0.001 SOL | Charged only when the tx lands successfully. |
| ATA rent | 0.00203928 SOL | Deposited on entry, refunded on exit (`close_ata_on_exit`). Locked rent is counted in equity. |
| Curve fee | From the trade event (`fee_bps + creator_fee_bps`), else 125 bps | Conservative fallback. |
| PumpSwap fee | 30 bps | Configurable. |
| Latency | 0.4–0.6 s / 1.2–1.8 s / 3.0–4.5 s | Decision → landing; the fill uses state at that time. |
| Adverse slippage | 0 / 75 / 250 bps | Unseen competing flow / MEV. |
| Drop probability | 2% / 8% / 20% | Not landed, no fee. Deterministic per order. |

Realized P&L is computed as proceeds − exit tx costs − failed-exit fees − cost basis, where cost basis is size + entry tx costs. Tests check that it equals the sum of the position's ledger entries, both in memory and in SQL.

## Measurement caveats

- **Execution checks** compare our simulated fill with the latest same-side market trade at or before the fill (within 5 s), recording trade age and size ratio. v1.1 used the next trade, which looked ahead and biased trending markets; that is removed. Sizes still differ, so this is a consistency bound, not slippage. Fills are never priced from market trades.
- **Fill evidence.** Fills are simulated from venue reserves at fill time, or conservatively capped by a fresh live Jupiter quote. Landing latency is a configured per-scenario estimate added after the measured pipeline delay; real landing latency cannot be measured without sending transactions. Fills are **validated** only with LIVE data, a mainnet-verified venue decoder, state at most 2 s old, and on-chain-observed fee rates. Everything else is **unverified** and excluded from validated metrics.
- **Calibration discipline.** A fixed chronological split is used: even UTC days are calibration, odd days are holdout. Thresholds may only be tuned on calibration data; `calibrate` enforces this and never writes config. Reported evidence is validated holdout only.
- **Sensitivity** re-costs closed trades under harsher venue fees (×1.5, ×2), extra slippage per side (+50/100/200 bps) and priority fees (×2, ×5).
- **Forward evaluation** uses a 0.05 SOL probe quoted at decision time, with no latency or failures, and fixed costs of two base-scenario transactions. It is an opportunity measure, not a P&L.
- **The expected-move model** (`expected_move_scale_bps`) is an uncalibrated heuristic prior. Calibrate it against forward outcomes before trusting `min_net_edge_bps`.

## Known limitations

- Paper fills don't change the observed reserves, so other traders never react to our trades. Exit quotes include the impact of our own position. Entry quotes don't model how others would trade around us.
- Holder concentration comes from venue trades only; transfers are invisible. It is flagged `concentration_complete=false` for tokens not seen from creation.
- Mint checks run only in LIVE mode with RPC. Fixture mode has no mint accounts; the check is skipped there and the docs and status say so. Mint authority is reported as a warning, not a blocker, because whether pump.fun revokes it on every create path has not been verified.
- Jupiter is used for route coverage and exit fallback in LIVE mode only. It is disabled in fixture/replay, and the route stats say so explicitly. The default quote URL must be set in `.env`; check your Jupiter plan and API version.
- With `logsSubscribe`, PumpSwap transactions are fetched with `getTransaction`. That adds measured latency (`tx_fetch`) and RPC load. `transactionSubscribe` avoids it where the provider offers it.
- Raydium v4 remains decode-only: `ray_log` has no pool/mint identifiers, and its layout is unverified.
- A Yellowstone/gRPC source is not implemented (its proto crates depend on the Solana SDK, which the safety policy forbids). The `StreamMsg` interface is the extension point, including `Slot{Dead}` for fork handling. Standard WS subscriptions use `confirmed` commitment, where rollbacks are rare.
