//! Raydium AMM v4 `ray_log` decoder (program 675kPX…1Mp8).
//! ray_log payloads do not contain pool or mint addresses, so these events are decode-only
//! unless the provider supplies the transaction account keys. The engine does not trade on them.

use super::borsh::{DecodeError, Reader, Writer};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RayLog {
    Init { time: u64, pc_decimals: u8, coin_decimals: u8, pc_lot_size: u64, coin_lot_size: u64, pc_amount: u64, coin_amount: u64 },
    SwapBaseIn { amount_in: u64, minimum_out: u64, direction: u64, user_source: u64, pool_coin: u64, pool_pc: u64, out_amount: u64 },
    SwapBaseOut { max_in: u64, amount_out: u64, direction: u64, user_source: u64, pool_coin: u64, pool_pc: u64, deduct_in: u64 },
    Deposit,
    Withdraw,
}

pub fn decode(data: &[u8]) -> Result<RayLog, DecodeError> {
    let mut r = Reader::new(data);
    match r.u8()? {
        0 => Ok(RayLog::Init {
            time: r.u64()?,
            pc_decimals: r.u8()?,
            coin_decimals: r.u8()?,
            pc_lot_size: r.u64()?,
            coin_lot_size: r.u64()?,
            pc_amount: r.u64()?,
            coin_amount: r.u64()?,
        }),
        1 => Ok(RayLog::Deposit),
        2 => Ok(RayLog::Withdraw),
        3 => Ok(RayLog::SwapBaseIn {
            amount_in: r.u64()?,
            minimum_out: r.u64()?,
            direction: r.u64()?,
            user_source: r.u64()?,
            pool_coin: r.u64()?,
            pool_pc: r.u64()?,
            out_amount: r.u64()?,
        }),
        4 => Ok(RayLog::SwapBaseOut {
            max_in: r.u64()?,
            amount_out: r.u64()?,
            direction: r.u64()?,
            user_source: r.u64()?,
            pool_coin: r.u64()?,
            pool_pc: r.u64()?,
            deduct_in: r.u64()?,
        }),
        t => Err(DecodeError::Unknown(format!("ray_log type {t}"))),
    }
}

pub fn encode(l: &RayLog) -> Vec<u8> {
    let mut w = Writer::default();
    match l {
        RayLog::Init { time, pc_decimals, coin_decimals, pc_lot_size, coin_lot_size, pc_amount, coin_amount } => {
            w.u8(0).u64(*time).u8(*pc_decimals).u8(*coin_decimals).u64(*pc_lot_size).u64(*coin_lot_size);
            w.u64(*pc_amount).u64(*coin_amount);
            w.bytes(&[0u8; 32]); // market
        }
        RayLog::SwapBaseIn { amount_in, minimum_out, direction, user_source, pool_coin, pool_pc, out_amount } => {
            w.u8(3).u64(*amount_in).u64(*minimum_out).u64(*direction).u64(*user_source);
            w.u64(*pool_coin).u64(*pool_pc).u64(*out_amount);
        }
        RayLog::SwapBaseOut { max_in, amount_out, direction, user_source, pool_coin, pool_pc, deduct_in } => {
            w.u8(4).u64(*max_in).u64(*amount_out).u64(*direction).u64(*user_source);
            w.u64(*pool_coin).u64(*pool_pc).u64(*deduct_in);
        }
        RayLog::Deposit => {
            w.u8(1);
        }
        RayLog::Withdraw => {
            w.u8(2);
        }
    }
    w.buf
}
