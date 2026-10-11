# Assumptions, cost model and limitations

## Venue layouts

Pump.fun and PumpSwap event layouts are verified against the official IDLs (`fixtures/idl/`, with provenance and sha256) in `tests/idl_verification.rs`. Instruction account layouts (names and order) come from the same IDLs (`fixtures/idl/*.ix-accounts.json`) and are used only to locate the accounts an observed instruction touched. **Neither venue is mainnet-verified yet**: that requires real transactions to pass `verify-fixtures` (V1.2.1 rules below).

### Mainnet findings (first real run, 15 transactions) and how they are handled

- **Events are emitted twice.** Pump.fun instructions carry `event_authority` + `program` (Anchor `emit_cpi!`) and the program also logs the event (`Program data:`). Decoding both copies doubled trade amounts. Copies are merged by (venue, payload bytes, top-level instruction), one-to-one.
- **Version-1 transactions** need `maxSupportedTransactionVersion: 1`; with 0 the RPC answers `-32015`.
- **Token creation** mints the full supply (10^15 raw units for the standard 1B × 6 decimals) into the curve vault inside `create`. Conservation is now Σ balance changes = parsed minted − burned.
- **Wallet-wide deltas are not trade deltas.** Checks now use the accounts of the emitting instruction (vault, user token account) and the token transfer executed inside it.

### Accounting rules used by verification (each is a hard check unless noted)

- **Token amount**: the SPL transfer inside the trade instruction between the curve vault and the user token account equals `token_amount`, in the direction of the instruction (`buy*` / `sell*`).
- **Curve SOL**: on buys the bonding curve receives exactly `sol_amount` (net of fees); on sells it pays out exactly `sol_amount` (gross; the user, fee recipient and creator vault are paid from it). Rent for a curve created in the same transaction is taken from the parsed `createAccount`. If mainnet disagrees, the check fails and the report shows observed vs decoded — it is never relaxed.
- **Protocol fee**: the recipients bound to the trade instruction — its `fee_recipient` account, plus a buyback recipient when one is among its accounts (IDL `buyback_fee_recipient`, or a remaining account listed in the official `Global.buybackFeeRecipients`, `fixtures/idl/pump.fee-recipients.json`) — must be credited exactly the decoded `fee` in total. Evidence: parsed System transfers inside that instruction, or balance deltas of recipients no other instruction names (sells pay from the program-owned curve without System instructions). Through a wrapper, balance-only evidence is inconclusive: the wrapper could also have moved lamports. Observed on mainnet: a ~50/50 split between the two (e.g. 4,692 + 4,691 of 9,383) — not assumed by the check.
- **Creator fee**: the creator-fee transfer inside the trade instruction equals `creator_fee` (0 without a creator). Other credits to the vault in the same instruction (observed: 650,240 lamports into a new vault) are reported as notes, purpose unknown, never counted as fee. Balance-only evidence follows the same exclusivity rule. For cashback coins a mismatch is *inconclusive* (routing undocumented), never a pass.
- **Fee formula**: `fee = ceil(sol_amount × fee_bps / 10 000)` and the same for the creator fee (SDK `fees.ts`); PumpSwap LP / protocol / creator fees are `ceil(quote × bps / 10 000)` on the pre-fee quote (SDK `util.ts`). Mismatches report the implied rate.
- **PumpSwap**: pool base / quote vault and user base account deltas; v1 protocol and coin-creator fee token accounts; constant-product output within 1 + 1 bp. v2 instructions have no per-fee recipient accounts, so v2 fee transfers are *inconclusive*; v2 vaults may retain protocol/creator fees (accepted only as that named routing).
- **v3 trade instructions** (official SDK `state.ts`: protocol fees net of the buyback slice and creator fees accrue on the curve; the buyback slice is paid out by the trade): buys — parsed payment into the curve = base + protocol fee + creator fee (minus a buyback slice the payer sends directly, as observed on token-quoted curves); curve net balance = payment − buyback slice; the buyback slice = event `buyback_fee`, credited to the instruction's `buyback_fee_recipient`, which must be an official wallet (or its quote token account). Sells — curve net = −base + retained fees − buyback slice. The protocol/creator split of the retained part is proven only jointly (plus the SDK fee formulas).
- **Attribution scope of balance-only evidence**: exclusive (only the trade instruction and its CPIs name the account) → PASS/FAIL; also named by a calling wrapper → exact reconciliation reported, INCONCLUSIVE in strict mode (default), PASS with `--accept-ancestor-scoped`; named by unrelated instructions → INCONCLUSIVE.
- **Token-account closes**: SPL Token closes only empty accounts (native WSOL excepted). The balance at the `closeAccount` is the pre balance plus parsed movements before it; if it is 0 the close is explained (no burn), otherwise the account is unexplained.
- **Instruction matching**: an event's emitting instruction is the parent (one call level up) of its self-CPI, or the n-th invocation of the program within the top-level instruction per the log invoke records.
- **Inconclusive**, never pass: no inner-instruction record or call depth; unrecognized instruction discriminator; several events attributed to one instruction (e.g. `multi_hop_curve_swap`); an account changed by `syncNative`, a non-empty close or an unrecognised token instruction; balance-only fee evidence for accounts other instructions also name; non-SOL quote curves.
- The user's wallet-wide SOL change is reported as information only (it also includes rent, tips and other instructions).

Earlier assumptions still used by the paper engine: **buyback and holder-reward fees** are slices of the protocol/creator fee, not extra charges; **cashback** is treated as zero (conservative); PumpSwap swap events carry the absolute pre-trade reserves, so any drift self-corrects on the next event.

## Earlier layout notes

- **Pump.fun (`6EF8…F6P`)**: Anchor `Program data:` logs (and, on current program versions, the same event again as a self-CPI — merged, see above). Events:
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
