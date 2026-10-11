//! PumpSwap AMM events (program pAMMBay…XEA). Emitted via Anchor `emit_cpi!`, so they are visible
//! only to providers that deliver inner instructions (full-transaction streams), not plain logsSubscribe.
//! Only the leading fields needed for reserve tracking are decoded; trailing account fields are ignored.

use super::anchor::event_discriminator;
use super::borsh::{DecodeError, Reader, Writer};
use crate::types::Pubkey;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreatePool {
    pub timestamp: i64,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_mint_decimals: u8,
    pub quote_mint_decimals: u8,
    pub base_amount_in: u64,
    pub quote_amount_in: u64,
    pub pool_base_amount: u64,
    pub pool_quote_amount: u64,
    pub minimum_liquidity: u64,
    pub initial_liquidity: u64,
    pub lp_token_amount_out: u64,
    pub pool_bump: u8,
    pub pool: Pubkey,
    pub lp_mint: Pubkey,
}

/// Reserves are the pool reserves BEFORE the swap (as in the IDL).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    pub is_buy: bool,
    pub timestamp: i64,
    /// base (token) amount out for buys / in for sells
    pub base_amount: u64,
    /// buys: user_quote_amount_in (incl. fees); sells: user_quote_amount_out (after fees)
    pub quote_amount_user: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    /// quote amount that actually moved into/out of the pool reserves (excl. fees)
    pub quote_amount_pool: u64,
    pub lp_fee: u64,
    pub protocol_fee: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Liquidity {
    pub is_deposit: bool,
    pub timestamp: i64,
    pub lp_token_amount: u64,
    /// reserves before the change
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub lp_mint_supply: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PumpSwapEvent {
    CreatePool(CreatePool),
    Swap(Swap),
    Liquidity(Liquidity),
}

pub fn decode(disc: [u8; 8], body: &[u8]) -> Result<Option<PumpSwapEvent>, DecodeError> {
    let mut r = Reader::new(body);
    if disc == event_discriminator("CreatePoolEvent") {
        Ok(Some(PumpSwapEvent::CreatePool(CreatePool {
            timestamp: r.i64()?,
            index: r.u16()?,
            creator: r.pubkey()?,
            base_mint: r.pubkey()?,
            quote_mint: r.pubkey()?,
            base_mint_decimals: r.u8()?,
            quote_mint_decimals: r.u8()?,
            base_amount_in: r.u64()?,
            quote_amount_in: r.u64()?,
            pool_base_amount: r.u64()?,
            pool_quote_amount: r.u64()?,
            minimum_liquidity: r.u64()?,
            initial_liquidity: r.u64()?,
            lp_token_amount_out: r.u64()?,
            pool_bump: r.u8()?,
            pool: r.pubkey()?,
            lp_mint: r.pubkey()?,
        })))
    } else if disc == event_discriminator("BuyEvent") {
        // timestamp, base_amount_out, max_quote_amount_in, user_base_res, user_quote_res,
        // pool_base_res, pool_quote_res, quote_amount_in, lp_fee_bps, lp_fee, protocol_fee_bps,
        // protocol_fee, quote_amount_in_with_lp_fee, user_quote_amount_in, pool, user
        let timestamp = r.i64()?;
        let base_amount = r.u64()?;
        let _max_quote_in = r.u64()?;
        let _ub = r.u64()?;
        let _uq = r.u64()?;
        let pb = r.u64()?;
        let pq = r.u64()?;
        let quote_amount_in = r.u64()?;
        let _lp_bps = r.u64()?;
        let lp_fee = r.u64()?;
        let _p_bps = r.u64()?;
        let protocol_fee = r.u64()?;
        let _q_with_lp = r.u64()?;
        let user_quote_in = r.u64()?;
        Ok(Some(PumpSwapEvent::Swap(Swap {
            is_buy: true,
            timestamp,
            base_amount,
            quote_amount_user: user_quote_in,
            pool_base_token_reserves: pb,
            pool_quote_token_reserves: pq,
            quote_amount_pool: quote_amount_in + lp_fee,
            lp_fee,
            protocol_fee,
            pool: r.pubkey()?,
            user: r.pubkey()?,
        })))
    } else if disc == event_discriminator("SellEvent") {
        // timestamp, base_amount_in, min_quote_amount_out, user_base_res, user_quote_res,
        // pool_base_res, pool_quote_res, quote_amount_out, lp_fee_bps, lp_fee, protocol_fee_bps,
        // protocol_fee, quote_amount_out_without_lp_fee, user_quote_amount_out, pool, user
        let timestamp = r.i64()?;
        let base_amount = r.u64()?;
        let _min_out = r.u64()?;
        let _ub = r.u64()?;
        let _uq = r.u64()?;
        let pb = r.u64()?;
        let pq = r.u64()?;
        let quote_amount_out = r.u64()?;
        let _lp_bps = r.u64()?;
        let lp_fee = r.u64()?;
        let _p_bps = r.u64()?;
        let protocol_fee = r.u64()?;
        let _q_wo_lp = r.u64()?;
        let user_quote_out = r.u64()?;
        Ok(Some(PumpSwapEvent::Swap(Swap {
            is_buy: false,
            timestamp,
            base_amount,
            quote_amount_user: user_quote_out,
            pool_base_token_reserves: pb,
            pool_quote_token_reserves: pq,
            // LP fee stays in the pool; only the rest leaves the reserves.
            quote_amount_pool: quote_amount_out.saturating_sub(lp_fee),
            lp_fee,
            protocol_fee,
            pool: r.pubkey()?,
            user: r.pubkey()?,
        })))
    } else if disc == event_discriminator("DepositEvent") || disc == event_discriminator("WithdrawEvent") {
        let is_deposit = disc == event_discriminator("DepositEvent");
        // timestamp, lp_token_amount, max/min base, max/min quote, user_base_res, user_quote_res,
        // pool_base_res, pool_quote_res, base_amount, quote_amount, lp_mint_supply, pool, user
        let timestamp = r.i64()?;
        let lp_token_amount = r.u64()?;
        let _b = r.u64()?;
        let _q = r.u64()?;
        let _ub = r.u64()?;
        let _uq = r.u64()?;
        Ok(Some(PumpSwapEvent::Liquidity(Liquidity {
            is_deposit,
            timestamp,
            lp_token_amount,
            pool_base_token_reserves: r.u64()?,
            pool_quote_token_reserves: r.u64()?,
            base_amount: r.u64()?,
            quote_amount: r.u64()?,
            lp_mint_supply: r.u64()?,
            pool: r.pubkey()?,
            user: r.pubkey()?,
        })))
    } else {
        Ok(None)
    }
}

/// Fixture/test encoder (writes the full documented prefix with zeros for unused fields).
pub fn encode(ev: &PumpSwapEvent) -> Vec<u8> {
    let mut w = Writer::default();
    match ev {
        PumpSwapEvent::CreatePool(c) => {
            w.bytes(&event_discriminator("CreatePoolEvent"));
            w.i64(c.timestamp).u16(c.index).pubkey(&c.creator).pubkey(&c.base_mint).pubkey(&c.quote_mint);
            w.u8(c.base_mint_decimals).u8(c.quote_mint_decimals);
            w.u64(c.base_amount_in).u64(c.quote_amount_in).u64(c.pool_base_amount).u64(c.pool_quote_amount);
            w.u64(c.minimum_liquidity).u64(c.initial_liquidity).u64(c.lp_token_amount_out).u8(c.pool_bump);
            w.pubkey(&c.pool).pubkey(&c.lp_mint);
            // trailing account fields (user token accounts) — zero filled
            w.bytes(&[0u8; 64]);
        }
        PumpSwapEvent::Swap(s) => {
            if s.is_buy {
                w.bytes(&event_discriminator("BuyEvent"));
                let quote_in = s.quote_amount_pool - s.lp_fee;
                w.i64(s.timestamp).u64(s.base_amount).u64(s.quote_amount_user).u64(0).u64(0);
                w.u64(s.pool_base_token_reserves).u64(s.pool_quote_token_reserves).u64(quote_in);
                w.u64(20).u64(s.lp_fee).u64(5).u64(s.protocol_fee).u64(s.quote_amount_pool).u64(s.quote_amount_user);
            } else {
                w.bytes(&event_discriminator("SellEvent"));
                let quote_out = s.quote_amount_pool + s.lp_fee;
                w.i64(s.timestamp).u64(s.base_amount).u64(0).u64(0).u64(0);
                w.u64(s.pool_base_token_reserves).u64(s.pool_quote_token_reserves).u64(quote_out);
                w.u64(20).u64(s.lp_fee).u64(5).u64(s.protocol_fee).u64(s.quote_amount_pool).u64(s.quote_amount_user);
            }
            w.pubkey(&s.pool).pubkey(&s.user);
            w.bytes(&[0u8; 128]);
        }
        PumpSwapEvent::Liquidity(l) => {
            w.bytes(&event_discriminator(if l.is_deposit { "DepositEvent" } else { "WithdrawEvent" }));
            w.i64(l.timestamp).u64(l.lp_token_amount).u64(0).u64(0).u64(0).u64(0);
            w.u64(l.pool_base_token_reserves).u64(l.pool_quote_token_reserves);
            w.u64(l.base_amount).u64(l.quote_amount).u64(l.lp_mint_supply);
            w.pubkey(&l.pool).pubkey(&l.user);
            w.bytes(&[0u8; 96]);
        }
    }
    w.buf
}
