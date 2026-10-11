//! Exact integer swap math for venue-local simulation. All results are floor-rounded in the
//! trader's disfavour, matching on-chain integer behaviour.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwapResult {
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee: u64,
    /// true if the trade was capped by available real reserves
    pub capped: bool,
}

/// Pump.fun bonding curve buy with total spend `spend` lamports (fee charged on top of net).
pub fn curve_buy(vs: u64, vt: u64, real_token: u64, spend: u64, fee_bps: u64) -> Option<SwapResult> {
    if vs == 0 || vt == 0 || spend == 0 {
        return None;
    }
    let net = (spend as u128 * 10_000 / (10_000 + fee_bps as u128)) as u64;
    let fee = spend - net;
    let k = vs as u128 * vt as u128;
    let new_vs = vs as u128 + net as u128;
    let new_vt = k.div_ceil(new_vs);
    let mut out = (vt as u128).saturating_sub(new_vt) as u64;
    let mut capped = false;
    if out > real_token {
        out = real_token;
        capped = true;
    }
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: spend, amount_out: out, fee, capped })
}

/// Pump.fun bonding curve sell of `tokens`; fee deducted from SOL proceeds.
pub fn curve_sell(vs: u64, vt: u64, real_sol: u64, tokens: u64, fee_bps: u64) -> Option<SwapResult> {
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
    let fee = (gross as u128 * fee_bps as u128).div_ceil(10_000) as u64;
    let out = gross.saturating_sub(fee);
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: tokens, amount_out: out, fee, capped })
}

/// Constant-product buy of base with `spend` quote (fees on top of net input).
pub fn cpmm_buy(base: u64, quote: u64, spend: u64, fee_bps: u64) -> Option<SwapResult> {
    if base == 0 || quote == 0 || spend == 0 {
        return None;
    }
    let net = (spend as u128 * 10_000 / (10_000 + fee_bps as u128)) as u64;
    let out = (base as u128 * net as u128 / (quote as u128 + net as u128)) as u64;
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: spend, amount_out: out, fee: spend - net, capped: false })
}

/// Constant-product sell of `tokens` base for quote (fees deducted from output).
pub fn cpmm_sell(base: u64, quote: u64, tokens: u64, fee_bps: u64) -> Option<SwapResult> {
    if base == 0 || quote == 0 || tokens == 0 {
        return None;
    }
    let gross = (quote as u128 * tokens as u128 / (base as u128 + tokens as u128)) as u64;
    let fee = (gross as u128 * fee_bps as u128).div_ceil(10_000) as u64;
    let out = gross.saturating_sub(fee);
    if out == 0 {
        return None;
    }
    Some(SwapResult { amount_in: tokens, amount_out: out, fee, capped: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn curve_round_trip_loses_fees_and_never_profits() {
        let (vs, vt, rt) = (30_000_000_000u64, 1_073_000_000_000_000u64, 793_100_000_000_000u64);
        let b = curve_buy(vs, vt, rt, 100_000_000, 100).unwrap();
        let net = b.amount_in - b.fee;
        let (vs2, vt2) = (vs + net, vt - b.amount_out);
        let s = curve_sell(vs2, vt2, net, b.amount_out, 100).unwrap();
        assert!(s.amount_out < b.amount_in);
        // ~2% fees round trip, so we should get back at least 97%
        assert!(s.amount_out as f64 > b.amount_in as f64 * 0.97, "{}", s.amount_out);
    }
    #[test]
    fn bigger_size_worse_price() {
        let small = cpmm_buy(1_000_000_000_000, 50_000_000_000, 20_000_000, 30).unwrap();
        let big = cpmm_buy(1_000_000_000_000, 50_000_000_000, 100_000_000, 30).unwrap();
        let ps = small.amount_out as f64 / small.amount_in as f64;
        let pb = big.amount_out as f64 / big.amount_in as f64;
        assert!(pb < ps);
    }
    #[test]
    fn cap_on_real_reserves() {
        let r = curve_buy(30_000_000_000, 1_073_000_000_000_000, 1_000, 1_000_000_000, 100).unwrap();
        assert!(r.capped);
        assert_eq!(r.amount_out, 1_000);
        let s = curve_sell(80_000_000_000, 300_000_000_000_000, 5, 1_000_000_000_000, 100);
        assert!(s.is_some_and(|s| s.capped) || s.is_none());
    }
    #[test]
    fn zero_inputs_give_no_quote() {
        assert!(cpmm_sell(0, 10, 10, 30).is_none());
        assert!(curve_buy(0, 1, 1, 1, 1).is_none());
    }
}
