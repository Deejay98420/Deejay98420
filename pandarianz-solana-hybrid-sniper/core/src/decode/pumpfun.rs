//! Pump.fun bonding-curve program events (program 6EF8…F6P).
//!
//! Layouts are verified against the official IDL shipped in `@pump-fun/pump-sdk@4.0.0`
//! (`fixtures/idl/pump.idl-subset.json`, checked by `tests/idl_verification.rs`). Older program
//! versions emitted shorter events; the required prefix is always decoded and the newer tail
//! (quote mint, fee components, mayhem/cashback flags, ...) is decoded when present.

use super::anchor::event_discriminator;
use super::borsh::{DecodeError, Reader, Writer};
use crate::types::{Pubkey, WSOL_MINT};
use serde::{Deserialize, Serialize};

/// SOL-like quote: legacy curves store the zero key; WSOL is also accepted (pump-sdk `isLegacyQuoteMint`).
pub fn is_sol_quote(q: Option<&Pubkey>) -> bool {
    match q {
        None => true,
        Some(p) => *p == Pubkey::default() || Pubkey::from_b58(WSOL_MINT).map(|w| w == *p).unwrap_or(false),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateTail {
    pub token_program: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_enabled: bool,
    pub quote_mint: Pubkey,
    pub virtual_quote_reserves: u64,
    pub creator_fee_bps: u64,
    pub is_holder_reward: bool,
    pub depth: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PumpCreate {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub user: Pubkey,
    pub creator: Option<Pubkey>,
    pub timestamp: Option<i64>,
    pub virtual_token_reserves: Option<u64>,
    pub virtual_sol_reserves: Option<u64>,
    pub real_token_reserves: Option<u64>,
    pub token_total_supply: Option<u64>,
    #[serde(default)]
    pub tail: Option<CreateTail>,
}

impl PumpCreate {
    pub fn quote_mint(&self) -> Option<Pubkey> {
        self.tail.as_ref().map(|t| t.quote_mint)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TradeTail {
    pub track_volume: bool,
    pub total_unclaimed_tokens: u64,
    pub total_claimed_tokens: u64,
    pub current_sol_volume: u64,
    pub last_update_timestamp: i64,
    pub ix_name: String,
    pub mayhem_mode: bool,
    pub cashback_fee_basis_points: u64,
    pub cashback: u64,
    pub buyback_fee_basis_points: u64,
    pub buyback_fee: u64,
    pub shareholders: Vec<(Pubkey, u16)>,
    pub quote_mint: Pubkey,
    pub quote_amount: u64,
    pub virtual_quote_reserves: u64,
    pub real_quote_reserves: u64,
    pub holder_rewards_bps: u64,
    pub holder_rewards: u64,
    pub creator_fee_unclaimed: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PumpTrade {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: Pubkey,
    pub timestamp: i64,
    /// Reserves AFTER the trade.
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: Option<u64>,
    pub real_token_reserves: Option<u64>,
    pub fee_recipient: Option<Pubkey>,
    pub fee_basis_points: Option<u64>,
    pub fee: Option<u64>,
    pub creator: Option<Pubkey>,
    pub creator_fee_basis_points: Option<u64>,
    pub creator_fee: Option<u64>,
    #[serde(default)]
    pub tail: Option<TradeTail>,
}

impl PumpTrade {
    pub fn quote_mint(&self) -> Option<Pubkey> {
        self.tail.as_ref().map(|t| t.quote_mint)
    }
    pub fn is_sol_quoted(&self) -> bool {
        is_sol_quote(self.quote_mint().as_ref())
    }
    /// Fee bps the trader pays on the curve: protocol + creator (creator only when a creator is set).
    /// Buyback is a slice of the protocol fee; cashback is a rebate and is ignored (conservative).
    pub fn total_fee_bps(&self) -> Option<u64> {
        let p = self.fee_basis_points?;
        let c = match self.creator {
            Some(cr) if cr != Pubkey::default() => self.creator_fee_basis_points.unwrap_or(0),
            _ => 0,
        };
        Some(p + c)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PumpComplete {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub timestamp: i64,
    #[serde(default)]
    pub quote_mint: Option<Pubkey>,
}

/// Emitted when a completed curve is migrated into a PumpSwap pool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PumpMigration {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub mint_amount: u64,
    pub sol_amount: u64,
    pub pool_migration_fee: u64,
    pub bonding_curve: Pubkey,
    pub timestamp: i64,
    pub pool: Pubkey,
    #[serde(default)]
    pub quote_mint: Option<Pubkey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PumpEvent {
    Create(PumpCreate),
    Trade(PumpTrade),
    Complete(PumpComplete),
    Migration(PumpMigration),
}

fn create_tail(r: &mut Reader) -> Result<CreateTail, DecodeError> {
    Ok(CreateTail {
        token_program: r.pubkey()?,
        is_mayhem_mode: r.bool()?,
        is_cashback_enabled: r.bool()?,
        quote_mint: r.pubkey()?,
        virtual_quote_reserves: r.u64()?,
        creator_fee_bps: r.u64()?,
        is_holder_reward: r.bool()?,
        depth: r.u8()?,
    })
}

fn trade_tail(r: &mut Reader) -> Result<TradeTail, DecodeError> {
    let track_volume = r.bool()?;
    let total_unclaimed_tokens = r.u64()?;
    let total_claimed_tokens = r.u64()?;
    let current_sol_volume = r.u64()?;
    let last_update_timestamp = r.i64()?;
    let ix_name = r.string()?;
    let mayhem_mode = r.bool()?;
    let cashback_fee_basis_points = r.u64()?;
    let cashback = r.u64()?;
    let buyback_fee_basis_points = r.u64()?;
    let buyback_fee = r.u64()?;
    let n = r.u32()?;
    if n > 1024 {
        return Err(DecodeError::Unknown(format!("shareholders len {n}")));
    }
    let mut shareholders = Vec::with_capacity(n as usize);
    for _ in 0..n {
        shareholders.push((r.pubkey()?, r.u16()?));
    }
    Ok(TradeTail {
        track_volume,
        total_unclaimed_tokens,
        total_claimed_tokens,
        current_sol_volume,
        last_update_timestamp,
        ix_name,
        mayhem_mode,
        cashback_fee_basis_points,
        cashback,
        buyback_fee_basis_points,
        buyback_fee,
        shareholders,
        quote_mint: r.pubkey()?,
        quote_amount: r.u64()?,
        virtual_quote_reserves: r.u64()?,
        real_quote_reserves: r.u64()?,
        holder_rewards_bps: r.u64()?,
        holder_rewards: r.u64()?,
        creator_fee_unclaimed: r.u64()?,
    })
}

pub fn decode(disc: [u8; 8], body: &[u8]) -> Result<Option<PumpEvent>, DecodeError> {
    let mut r = Reader::new(body);
    if disc == event_discriminator("CreateEvent") {
        let name = r.string()?;
        let symbol = r.string()?;
        let uri = r.string()?;
        let mint = r.pubkey()?;
        let bonding_curve = r.pubkey()?;
        let user = r.pubkey()?;
        let creator = r.opt_pubkey();
        let timestamp = r.opt_i64();
        let virtual_token_reserves = r.opt_u64();
        let virtual_sol_reserves = r.opt_u64();
        let real_token_reserves = r.opt_u64();
        let token_total_supply = r.opt_u64();
        let tail = if r.remaining() > 0 { create_tail(&mut r).ok() } else { None };
        Ok(Some(PumpEvent::Create(PumpCreate {
            name,
            symbol,
            uri,
            mint,
            bonding_curve,
            user,
            creator,
            timestamp,
            virtual_token_reserves,
            virtual_sol_reserves,
            real_token_reserves,
            token_total_supply,
            tail,
        })))
    } else if disc == event_discriminator("TradeEvent") {
        let mint = r.pubkey()?;
        let sol_amount = r.u64()?;
        let token_amount = r.u64()?;
        let is_buy = r.bool()?;
        let user = r.pubkey()?;
        let timestamp = r.i64()?;
        let virtual_sol_reserves = r.u64()?;
        let virtual_token_reserves = r.u64()?;
        let real_sol_reserves = r.opt_u64();
        let real_token_reserves = r.opt_u64();
        let fee_recipient = r.opt_pubkey();
        let fee_basis_points = r.opt_u64();
        let fee = r.opt_u64();
        let creator = r.opt_pubkey();
        let creator_fee_basis_points = r.opt_u64();
        let creator_fee = r.opt_u64();
        let tail = if r.remaining() > 0 { trade_tail(&mut r).ok() } else { None };
        Ok(Some(PumpEvent::Trade(PumpTrade {
            mint,
            sol_amount,
            token_amount,
            is_buy,
            user,
            timestamp,
            virtual_sol_reserves,
            virtual_token_reserves,
            real_sol_reserves,
            real_token_reserves,
            fee_recipient,
            fee_basis_points,
            fee,
            creator,
            creator_fee_basis_points,
            creator_fee,
            tail,
        })))
    } else if disc == event_discriminator("CompleteEvent") {
        Ok(Some(PumpEvent::Complete(PumpComplete {
            user: r.pubkey()?,
            mint: r.pubkey()?,
            bonding_curve: r.pubkey()?,
            timestamp: r.i64()?,
            quote_mint: r.opt_pubkey(),
        })))
    } else if disc == event_discriminator("CompletePumpAmmMigrationEvent") {
        Ok(Some(PumpEvent::Migration(PumpMigration {
            user: r.pubkey()?,
            mint: r.pubkey()?,
            mint_amount: r.u64()?,
            sol_amount: r.u64()?,
            pool_migration_fee: r.u64()?,
            bonding_curve: r.pubkey()?,
            timestamp: r.i64()?,
            pool: r.pubkey()?,
            quote_mint: r.opt_pubkey(),
        })))
    } else {
        Ok(None)
    }
}

/// Encoders mirror the decoders (current IDL layout when the tail is present); used for
/// labelled synthetic fixtures and tests only.
pub fn encode(ev: &PumpEvent) -> Vec<u8> {
    let mut w = Writer::default();
    match ev {
        PumpEvent::Create(c) => {
            w.bytes(&event_discriminator("CreateEvent"));
            w.string(&c.name).string(&c.symbol).string(&c.uri);
            w.pubkey(&c.mint).pubkey(&c.bonding_curve).pubkey(&c.user);
            if let (Some(cr), Some(ts)) = (c.creator, c.timestamp) {
                w.pubkey(&cr).i64(ts);
                if let (Some(a), Some(b), Some(cc), Some(d)) =
                    (c.virtual_token_reserves, c.virtual_sol_reserves, c.real_token_reserves, c.token_total_supply)
                {
                    w.u64(a).u64(b).u64(cc).u64(d);
                    if let Some(t) = &c.tail {
                        w.pubkey(&t.token_program).bool(t.is_mayhem_mode).bool(t.is_cashback_enabled).pubkey(&t.quote_mint);
                        w.u64(t.virtual_quote_reserves).u64(t.creator_fee_bps).bool(t.is_holder_reward).u8(t.depth);
                    }
                }
            }
        }
        PumpEvent::Trade(t) => {
            w.bytes(&event_discriminator("TradeEvent"));
            w.pubkey(&t.mint).u64(t.sol_amount).u64(t.token_amount).bool(t.is_buy).pubkey(&t.user);
            w.i64(t.timestamp).u64(t.virtual_sol_reserves).u64(t.virtual_token_reserves);
            if let (Some(a), Some(b)) = (t.real_sol_reserves, t.real_token_reserves) {
                w.u64(a).u64(b);
                if let (Some(fr), Some(fbp), Some(f)) = (t.fee_recipient, t.fee_basis_points, t.fee) {
                    w.pubkey(&fr).u64(fbp).u64(f);
                    if let (Some(cr), Some(cbp), Some(cf)) = (t.creator, t.creator_fee_basis_points, t.creator_fee) {
                        w.pubkey(&cr).u64(cbp).u64(cf);
                        if let Some(x) = &t.tail {
                            w.bool(x.track_volume).u64(x.total_unclaimed_tokens).u64(x.total_claimed_tokens);
                            w.u64(x.current_sol_volume).i64(x.last_update_timestamp).string(&x.ix_name).bool(x.mayhem_mode);
                            w.u64(x.cashback_fee_basis_points).u64(x.cashback).u64(x.buyback_fee_basis_points).u64(x.buyback_fee);
                            w.u32(x.shareholders.len() as u32);
                            for (p, bps) in &x.shareholders {
                                w.pubkey(p).u16(*bps);
                            }
                            w.pubkey(&x.quote_mint).u64(x.quote_amount).u64(x.virtual_quote_reserves).u64(x.real_quote_reserves);
                            w.u64(x.holder_rewards_bps).u64(x.holder_rewards).u64(x.creator_fee_unclaimed);
                        }
                    }
                }
            }
        }
        PumpEvent::Complete(c) => {
            w.bytes(&event_discriminator("CompleteEvent"));
            w.pubkey(&c.user).pubkey(&c.mint).pubkey(&c.bonding_curve).i64(c.timestamp);
            if let Some(q) = &c.quote_mint {
                w.pubkey(q);
            }
        }
        PumpEvent::Migration(m) => {
            w.bytes(&event_discriminator("CompletePumpAmmMigrationEvent"));
            w.pubkey(&m.user).pubkey(&m.mint).u64(m.mint_amount).u64(m.sol_amount).u64(m.pool_migration_fee);
            w.pubkey(&m.bonding_curve).i64(m.timestamp).pubkey(&m.pool);
            if let Some(q) = &m.quote_mint {
                w.pubkey(q);
            }
        }
    }
    w.buf
}

/// `BondingCurve` account (IDL-verified layout) — used for state seeding during backfill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BondingCurveAccount {
    pub virtual_token_reserves: u64,
    pub virtual_quote_reserves: u64,
    pub real_token_reserves: u64,
    pub real_quote_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey,
    pub is_mayhem_mode: Option<bool>,
    pub quote_mint: Option<Pubkey>,
}

pub const BONDING_CURVE_DISCRIMINATOR: [u8; 8] = [0x17, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60];

pub fn decode_bonding_curve_account(data: &[u8]) -> Result<BondingCurveAccount, DecodeError> {
    if data.len() < 8 || data[..8] != BONDING_CURVE_DISCRIMINATOR {
        return Err(DecodeError::Unknown("not a BondingCurve account".into()));
    }
    let mut r = Reader::new(&data[8..]);
    let virtual_token_reserves = r.u64()?;
    let virtual_quote_reserves = r.u64()?;
    let real_token_reserves = r.u64()?;
    let real_quote_reserves = r.u64()?;
    let token_total_supply = r.u64()?;
    let complete = r.bool()?;
    let creator = r.pubkey()?;
    let is_mayhem_mode = r.bool().ok();
    let quote_mint = if is_mayhem_mode.is_some() && r.bool().is_ok() { r.opt_pubkey() } else { None };
    Ok(BondingCurveAccount {
        virtual_token_reserves,
        virtual_quote_reserves,
        real_token_reserves,
        real_quote_reserves,
        token_total_supply,
        complete,
        creator,
        is_mayhem_mode,
        quote_mint,
    })
}
