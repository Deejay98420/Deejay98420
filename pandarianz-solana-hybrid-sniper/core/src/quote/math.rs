//! Exact integer swap math for venue-local simulation, ported from the official SDKs
//! (`@pump-fun/pump-sdk` bondingCurve.ts / fees.ts and `@pump-fun/pump-swap-sdk` buy.ts / sell.ts).
//! Fees are ceil-rounded per component, as on chain.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwapResult {
    /// Quote actually charged (buys: ≤ budget) or tokens sold (sells).
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee: u64,
    /// true if the trade was capped by available real reserves
    pub capped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct CurveFees {
    pub protocol_bps: u64,
    /// creator rate; pass 0 when the curve has no creator (the program charges none)
    pub creator_bps: u64,
}

impl CurveFees {
    pub fn total(&self) -> u64 {
        self.protocol_bps + self.creator_bps
    }
    fn on(&self, amount: u64) -> u64 {
        fee(amount, self.protocol_bps) + fee(amount, self.creator_bps)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct AmmFees {
    pub lp_bps: u64,
    pub protocol_bps: u64,
    /// coin-creator rate; 0 when the pool has no coin creator
    pub creator_bps: u64,
}

impl AmmFees {
    pub fn total(&self) -> u64 {
        self.lp_bps + self.protocol_bps + self.creator_bps
    }
}

/// ceil(amount * bps / 10_000)
pub fn fee(amount: u64, bps: u64) -> u64 {
    (amount as u128 * bps as u128).div_ceil(10_000) as u64
}

/// Pump.fun bonding-curve buy with a quote budget `spend` (SDK `getBuyTokenAmountFromSolAmount`),
/// then the actual charge for those tokens (SDK `getBuySolAmountFromTokenAmount`).
pub fn curve_buy(vs: u64, vt: u64, real_token: u64, spend: u64, fees: CurveFees) -> Option<SwapResult> {
    if vs == 0 || vt == 0 || spend <= 1 {
        return None;
    }
    let input = ((spend as u128 - 1) * 10_000 / (10_000 + fees.total() as u128)) as u64;
    let mut out = (input as u128 * vt as u128 / (vs as u128 + input as u128)) as u64;
    let mut capped = false;
    if out > real_token {
        out = real_token;
        capped = true;
    }
    if out == 0 || out >= vt {
        return None;
    }
    // cost of exactly `out` tokens: out*vs/(vt-out) + 1, plus fees on that cost
    let cost = (out as u128 * vs as u128 / (vt as u128 - out as u128)) as u64 + 1;
    let f = fees.on(cost);
    Some(SwapResult { amount_in: (cost + f).min(spend), amount_out: out, fee: f, capped })
}

/// Pump.fun bonding-curve sell (SDK `getSellSolAmountFromTokenAmount`).
pub fn curve_sell(vs: u64, vt: u64, real_sol: u64, tokens: u64, fees: CurveFees) -> Option<SwapResult> {
    if vs == 0 || vt == 0 || tokens == 0 {
        return None;
    }
    let gross = (tokens as u128 * vs as u128 / (vt as u128 + tokens as u128)) as u64;
    let mut capped = false;
    let gross = if gross > real_sol {
        capped = true;
        real_sol
    } else {
        gross
    };
    let f = fees.on(gross);
    let out = gross.saturating_sub(f);
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: tokens, amount_out: out, fee: f, capped })
}

/// PumpSwap exact-quote-in buy (SDK `buyQuoteInput`). `quote_eff` = vault quote + virtual quote reserves.
pub fn amm_buy(base: u64, quote_eff: u64, spend: u64, fees: AmmFees) -> Option<SwapResult> {
    if base == 0 || quote_eff == 0 || spend <= 1 {
        return None;
    }
    let mut net = (spend as u128 * 10_000 / (10_000 + fees.total() as u128)) as u64;
    let (lp, pr, cr) = (fee(net, fees.lp_bps), fee(net, fees.protocol_bps), fee(net, fees.creator_bps));
    let total = net + lp + pr + cr;
    if total > spend {
        net = net.checked_sub(total - spend)?;
    }
    if net <= 1 {
        return None;
    }
    let input = net - 1;
    let out = (base as u128 * input as u128 / (quote_eff as u128 + input as u128)) as u64;
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: (net + lp + pr + cr).min(spend), amount_out: out, fee: lp + pr + cr, capped: false })
}

/// PumpSwap sell (SDK `sellBaseInput`). `real_quote` = vault quote available for withdrawal;
/// a sell that would need more than that has no route.
pub fn amm_sell(base: u64, quote_eff: u64, real_quote: u64, tokens: u64, fees: AmmFees) -> Option<SwapResult> {
    if base == 0 || quote_eff == 0 || tokens == 0 {
        return None;
    }
    let gross = (quote_eff as u128 * tokens as u128 / (base as u128 + tokens as u128)) as u64;
    let (lp, pr, cr) = (fee(gross, fees.lp_bps), fee(gross, fees.protocol_bps), fee(gross, fees.creator_bps));
    if real_quote < gross.saturating_sub(lp) {
        return None;
    }
    let out = gross.checked_sub(lp + pr + cr)?;
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: tokens, amount_out: out, fee: lp + pr + cr, capped: false })
}

/// Backwards-compatible single-rate wrappers.
pub fn cpmm_buy(base: u64, quote: u64, spend: u64, fee_bps: u64) -> Option<SwapResult> {
    amm_buy(base, quote, spend, AmmFees { lp_bps: fee_bps, ..Default::default() })
}
pub fn cpmm_sell(base: u64, quote: u64, tokens: u64, fee_bps: u64) -> Option<SwapResult> {
    amm_sell(base, quote, quote, tokens, AmmFees { lp_bps: fee_bps, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    const F: CurveFees = CurveFees { protocol_bps: 95, creator_bps: 5 };
    #[test]
    fn curve_round_trip_loses_fees_and_never_profits() {
        let (vs, vt, rt) = (30_000_000_000u64, 1_073_000_000_000_000u64, 793_100_000_000_000u64);
        let b = curve_buy(vs, vt, rt, 100_000_000, F).unwrap();
        assert!(b.amount_in <= 100_000_000);
        let net = b.amount_in - b.fee;
        let (vs2, vt2) = (vs + net, vt - b.amount_out);
        let s = curve_sell(vs2, vt2, net, b.amount_out, F).unwrap();
        assert!(s.amount_out < b.amount_in);
        assert!(s.amount_out as f64 > b.amount_in as f64 * 0.97, "{}", s.amount_out);
    }
    #[test]
    fn curve_buy_matches_sdk_formula() {
        // SDK: input = (amount-1)*10000/(10000+fee); tokens = input*vt/(vq+input)
        let (vs, vt) = (30_000_000_000u64, 1_073_000_000_000_000u64);
        let spend = 50_000_000u64;
        let input = (spend - 1) * 10_000 / 10_100;
        let tokens = (input as u128 * vt as u128 / (vs as u128 + input as u128)) as u64;
        assert_eq!(curve_buy(vs, vt, u64::MAX, spend, F).unwrap().amount_out, tokens);
    }
    #[test]
    fn bigger_size_worse_price() {
        let small = cpmm_buy(1_000_000_000_000, 50_000_000_000, 20_000_000, 30).unwrap();
        let big = cpmm_buy(1_000_000_000_000, 50_000_000_000, 100_000_000, 30).unwrap();
        assert!((big.amount_out as f64 / big.amount_in as f64) < (small.amount_out as f64 / small.amount_in as f64));
    }
    #[test]
    fn amm_sell_respects_real_reserves_and_virtual_pricing() {
        let fees = AmmFees { lp_bps: 20, protocol_bps: 5, creator_bps: 5 };
        // virtual quote raises the price: more out than without it
        let a = amm_sell(1_000_000_000, 50_000_000_000, 50_000_000_000, 10_000_000, fees).unwrap();
        let b = amm_sell(1_000_000_000, 60_000_000_000, 50_000_000_000, 10_000_000, fees).unwrap();
        assert!(b.amount_out > a.amount_out);
        // not enough real quote in the vault → no route
        assert!(amm_sell(1_000_000_000, 60_000_000_000, 1_000, 10_000_000, fees).is_none());
    }
    #[test]
    fn cap_on_real_reserves() {
        let r = curve_buy(30_000_000_000, 1_073_000_000_000_000, 1_000, 1_000_000_000, F).unwrap();
        assert!(r.capped);
        assert_eq!(r.amount_out, 1_000);
    }
    #[test]
    fn zero_inputs_give_no_quote() {
        assert!(cpmm_sell(0, 10, 10, 30).is_none());
        assert!(curve_buy(0, 1, 1, 1, F).is_none());
    }
}
