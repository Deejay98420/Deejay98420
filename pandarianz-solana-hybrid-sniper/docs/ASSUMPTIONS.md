# Assumptions, cost model and limitations

## Venue layouts (from public IDLs; verify against captured mainnet transactions)

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

## Known limitations

- Paper fills don't change the observed reserves, so other traders never react to our trades. Exit quotes include the impact of our own position. Entry quotes don't model how others would trade around us.
- Holder concentration comes from venue trades only; transfers are invisible. It is flagged `concentration_complete=false` for tokens not seen from creation.
- Mint and Token-2022 extension checks (freeze authority, transfer hooks, etc.) are not implemented. Sellability is inferred from observed non-creator sells and from an executable sell quote existing.
- Jupiter is used for route coverage and exit fallback in LIVE mode only. It is disabled in fixture/replay, and the route stats say so explicitly. The default quote URL must be set in `.env`; check your Jupiter plan and API version.
- With `logsSubscribe`, Pump.fun events are visible but PumpSwap events are not; use `transactionSubscribe`. Unknown PumpSwap pools are not resolved via RPC.
- A Yellowstone/gRPC source is not implemented. The `StreamMsg` interface is the extension point, including `Slot{Dead}` for fork handling. Standard WS subscriptions use `confirmed` commitment, where rollbacks are rare.
