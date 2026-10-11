//! Pump.fun bonding-curve program events (program 6EF8…F6P).
//! Layouts follow the public Pump.fun IDL. Newer program versions append trailing fields;
//! those are decoded when present and left `None` otherwise.

use super::anchor::event_discriminator;
use super::borsh::{DecodeError, Reader, Writer};
use crate::types::Pubkey;
use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PumpComplete {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub timestamp: i64,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PumpEvent {
    Create(PumpCreate),
    Trade(PumpTrade),
    Complete(PumpComplete),
    Migration(PumpMigration),
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
        Ok(Some(PumpEvent::Create(PumpCreate {
            name,
            symbol,
            uri,
            mint,
            bonding_curve,
            user,
            creator,
            timestamp,
            virtual_token_reserves: r.opt_u64(),
            virtual_sol_reserves: r.opt_u64(),
            real_token_reserves: r.opt_u64(),
            token_total_supply: r.opt_u64(),
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
        Ok(Some(PumpEvent::Trade(PumpTrade {
            mint,
            sol_amount,
            token_amount,
            is_buy,
            user,
            timestamp,
            virtual_sol_reserves,
            virtual_token_reserves,
            real_sol_reserves: r.opt_u64(),
            real_token_reserves: r.opt_u64(),
            fee_recipient: r.opt_pubkey(),
            fee_basis_points: r.opt_u64(),
            fee: r.opt_u64(),
            creator: r.opt_pubkey(),
            creator_fee_basis_points: r.opt_u64(),
            creator_fee: r.opt_u64(),
        })))
    } else if disc == event_discriminator("CompleteEvent") {
        Ok(Some(PumpEvent::Complete(PumpComplete {
            user: r.pubkey()?,
            mint: r.pubkey()?,
            bonding_curve: r.pubkey()?,
            timestamp: r.i64()?,
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
        })))
    } else {
        Ok(None)
    }
}

/// Encoders mirror the decoders; used only to build labelled synthetic fixtures and tests.
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
                    }
                }
            }
        }
        PumpEvent::Complete(c) => {
            w.bytes(&event_discriminator("CompleteEvent"));
            w.pubkey(&c.user).pubkey(&c.mint).pubkey(&c.bonding_curve).i64(c.timestamp);
        }
        PumpEvent::Migration(m) => {
            w.bytes(&event_discriminator("CompletePumpAmmMigrationEvent"));
            w.pubkey(&m.user).pubkey(&m.mint).u64(m.mint_amount).u64(m.sol_amount).u64(m.pool_migration_fee);
            w.pubkey(&m.bonding_curve).i64(m.timestamp).pubkey(&m.pool);
        }
    }
    w.buf
}
