//! PumpSwap AMM events (program pAMMBay…XEA). Emitted via Anchor `emit_cpi!`, so they are visible
//! only in inner instructions (full-transaction streams or `getTransaction`), not in plain logs.
//!
//! Layouts verified against the official IDL in `@pump-fun/pump-swap-sdk@2.1.0`
//! (`fixtures/idl/pump_amm.idl-subset.json`, `tests/idl_verification.rs`). Pricing follows the SDK:
//! the effective quote reserve is `vault quote + virtual_quote_reserves` (i128, may be negative).

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
    #[serde(default)]
    pub coin_creator: Option<Pubkey>,
    #[serde(default)]
    pub creator_fee_bps: Option<u64>,
}

/// Reserves are the pool reserves BEFORE the swap (as in the IDL).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    pub is_buy: bool,
    pub timestamp: i64,
    /// base (token) amount out for buys / in for sells
    pub base_amount: u64,
    /// buys: user_quote_amount_in (incl. all fees); sells: user_quote_amount_out (after all fees)
    pub quote_amount_user: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    /// quote that moves into/out of the pool vault for the swap (buys: in incl. LP fee; sells: out excl. LP fee)
    pub quote_amount_pool: u64,
    pub lp_fee: u64,
    pub protocol_fee: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    #[serde(default)]
    pub lp_fee_bps: u64,
    #[serde(default)]
    pub protocol_fee_bps: u64,
    #[serde(default)]
    pub coin_creator: Option<Pubkey>,
    #[serde(default)]
    pub coin_creator_fee_bps: Option<u64>,
    #[serde(default)]
    pub coin_creator_fee: Option<u64>,
    /// signed virtual quote reserves (current IDL); None on older layouts
    #[serde(default)]
    pub virtual_quote_reserves: Option<i128>,
}

impl Swap {
    /// Total fee bps paid by the trader (LP + protocol + coin creator when set). Mirrors the SDK.
    pub fn total_fee_bps(&self) -> u64 {
        let c = match self.coin_creator {
            Some(cc) if cc != Pubkey::default() => self.coin_creator_fee_bps.unwrap_or(0),
            _ => 0,
        };
        self.lp_fee_bps + self.protocol_fee_bps + c
    }
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

/// Fields after `user` common to the current Buy/Sell layouts up to the coin-creator fee.
struct SwapAccountsTail {
    coin_creator: Pubkey,
    coin_creator_fee_bps: u64,
    coin_creator_fee: u64,
}

fn swap_accounts_tail(r: &mut Reader) -> Result<SwapAccountsTail, DecodeError> {
    r.skip(32 * 4)?; // user base/quote token accounts, protocol fee recipient + its token account
    Ok(SwapAccountsTail { coin_creator: r.pubkey()?, coin_creator_fee_bps: r.u64()?, coin_creator_fee: r.u64()? })
}

fn buy_virtual_tail(r: &mut Reader) -> Result<i128, DecodeError> {
    // track_volume, total_unclaimed_tokens, total_claimed_tokens, current_sol_volume,
    // last_update_timestamp, min_base_amount_out, ix_name, cashback bps/amt, buyback bps/amt
    r.bool()?;
    r.skip(8 * 5)?;
    r.string()?;
    r.skip(8 * 4)?;
    r.i128()
}

fn sell_virtual_tail(r: &mut Reader) -> Result<i128, DecodeError> {
    // cashback bps/amt, buyback bps/amt
    r.skip(8 * 4)?;
    r.i128()
}

pub fn decode(disc: [u8; 8], body: &[u8]) -> Result<Option<PumpSwapEvent>, DecodeError> {
    let mut r = Reader::new(body);
    if disc == event_discriminator("CreatePoolEvent") {
        let mut c = CreatePool {
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
            coin_creator: None,
            creator_fee_bps: None,
        };
        // user_base_token_account, user_quote_token_account, coin_creator, is_mayhem_mode, creator_fee_bps
        if r.skip(64).is_ok() {
            c.coin_creator = r.opt_pubkey();
            if c.coin_creator.is_some() && r.bool().is_ok() {
                c.creator_fee_bps = r.opt_u64();
            }
        }
        Ok(Some(PumpSwapEvent::CreatePool(c)))
    } else if disc == event_discriminator("BuyEvent") || disc == event_discriminator("SellEvent") {
        let is_buy = disc == event_discriminator("BuyEvent");
        // Buy:  timestamp, base_amount_out, max_quote_amount_in, user_base_res, user_quote_res,
        //       pool_base_res, pool_quote_res, quote_amount_in, lp_fee_bps, lp_fee, protocol_fee_bps,
        //       protocol_fee, quote_amount_in_with_lp_fee, user_quote_amount_in, pool, user, ...
        // Sell: timestamp, base_amount_in, min_quote_amount_out, user_base_res, user_quote_res,
        //       pool_base_res, pool_quote_res, quote_amount_out, lp_fee_bps, lp_fee, protocol_fee_bps,
        //       protocol_fee, quote_amount_out_without_lp_fee, user_quote_amount_out, pool, user, ...
        let timestamp = r.i64()?;
        let base_amount = r.u64()?;
        let _limit = r.u64()?;
        let _ub = r.u64()?;
        let _uq = r.u64()?;
        let pb = r.u64()?;
        let pq = r.u64()?;
        let quote_amount = r.u64()?;
        let lp_fee_bps = r.u64()?;
        let lp_fee = r.u64()?;
        let protocol_fee_bps = r.u64()?;
        let protocol_fee = r.u64()?;
        let quote_with_lp = r.u64()?;
        let user_quote = r.u64()?;
        let pool = r.pubkey()?;
        let user = r.pubkey()?;
        let mut s = Swap {
            is_buy,
            timestamp,
            base_amount,
            quote_amount_user: user_quote,
            pool_base_token_reserves: pb,
            pool_quote_token_reserves: pq,
            // buys: quote_amount_in_with_lp_fee enters the vault; sells: quote_amount_out_without_lp_fee leaves it
            quote_amount_pool: quote_with_lp,
            lp_fee,
            protocol_fee,
            pool,
            user,
            lp_fee_bps,
            protocol_fee_bps,
            coin_creator: None,
            coin_creator_fee_bps: None,
            coin_creator_fee: None,
            virtual_quote_reserves: None,
        };
        let _ = quote_amount;
        if let Ok(t) = swap_accounts_tail(&mut r) {
            s.coin_creator = Some(t.coin_creator);
            s.coin_creator_fee_bps = Some(t.coin_creator_fee_bps);
            s.coin_creator_fee = Some(t.coin_creator_fee);
            s.virtual_quote_reserves = if is_buy { buy_virtual_tail(&mut r).ok() } else { sell_virtual_tail(&mut r).ok() };
        }
        Ok(Some(PumpSwapEvent::Swap(s)))
    } else if disc == event_discriminator("DepositEvent") || disc == event_discriminator("WithdrawEvent") {
        let is_deposit = disc == event_discriminator("DepositEvent");
        // timestamp, lp_token_amount, max/min base, max/min quote, user_base_res, user_quote_res,
        // pool_base_res, pool_quote_res, base_amount, quote_amount, lp_mint_supply, pool, user
        let timestamp = r.i64()?;
        let lp_token_amount = r.u64()?;
        r.skip(8 * 4)?;
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

/// Fixture/test encoder: writes the full current IDL layout (zeros for fields we do not model).
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
            w.bytes(&[0u8; 64]); // user token accounts
            w.pubkey(&c.coin_creator.unwrap_or_default()).bool(false).u64(c.creator_fee_bps.unwrap_or(0)).bool(false).bool(false);
        }
        PumpSwapEvent::Swap(s) => {
            let (lp, proto) = (s.lp_fee_bps, s.protocol_fee_bps);
            if s.is_buy {
                w.bytes(&event_discriminator("BuyEvent"));
                let quote_in = s.quote_amount_pool - s.lp_fee;
                w.i64(s.timestamp).u64(s.base_amount).u64(s.quote_amount_user).u64(0).u64(0);
                w.u64(s.pool_base_token_reserves).u64(s.pool_quote_token_reserves).u64(quote_in);
                w.u64(lp).u64(s.lp_fee).u64(proto).u64(s.protocol_fee).u64(s.quote_amount_pool).u64(s.quote_amount_user);
            } else {
                w.bytes(&event_discriminator("SellEvent"));
                let quote_out = s.quote_amount_pool + s.lp_fee;
                w.i64(s.timestamp).u64(s.base_amount).u64(0).u64(0).u64(0);
                w.u64(s.pool_base_token_reserves).u64(s.pool_quote_token_reserves).u64(quote_out);
                w.u64(lp).u64(s.lp_fee).u64(proto).u64(s.protocol_fee).u64(s.quote_amount_pool).u64(s.quote_amount_user);
            }
            w.pubkey(&s.pool).pubkey(&s.user);
            w.bytes(&[0u8; 128]); // user token accounts + protocol fee recipient (+ token account)
            w.pubkey(&s.coin_creator.unwrap_or_default()).u64(s.coin_creator_fee_bps.unwrap_or(0)).u64(s.coin_creator_fee.unwrap_or(0));
            if s.is_buy {
                w.bool(false).u64(0).u64(0).u64(0).i64(0).u64(0).string("buy");
            }
            w.u64(0).u64(0).u64(0).u64(0); // cashback bps/amt, buyback bps/amt
            w.i128(s.virtual_quote_reserves.unwrap_or(0)).bool(false).u64(0).u64(0).u64(0).u64(0);
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

/// `Pool` account (IDL-verified) — used to resolve pools whose creation was not observed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PoolAccount {
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub lp_supply: u64,
    pub coin_creator: Option<Pubkey>,
    pub virtual_quote_reserves: Option<i128>,
}

pub const POOL_DISCRIMINATOR: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];

pub fn decode_pool_account(data: &[u8]) -> Result<PoolAccount, DecodeError> {
    if data.len() < 8 || data[..8] != POOL_DISCRIMINATOR {
        return Err(DecodeError::Unknown("not a PumpSwap Pool account".into()));
    }
    let mut r = Reader::new(&data[8..]);
    let _bump = r.u8()?;
    let mut p = PoolAccount {
        index: r.u16()?,
        creator: r.pubkey()?,
        base_mint: r.pubkey()?,
        quote_mint: r.pubkey()?,
        lp_mint: r.pubkey()?,
        pool_base_token_account: r.pubkey()?,
        pool_quote_token_account: r.pubkey()?,
        lp_supply: r.u64()?,
        coin_creator: None,
        virtual_quote_reserves: None,
    };
    p.coin_creator = r.opt_pubkey();
    if p.coin_creator.is_some() && r.bool().is_ok() && r.bool().is_ok() {
        p.virtual_quote_reserves = r.i128().ok();
    }
    Ok(p)
}
