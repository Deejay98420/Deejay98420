//! Shared primitive types. All SOL amounts are lamports (u64/i64); token amounts are raw base units.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

pub fn sol(lamports: i64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL as f64
}

pub fn lamports(sol: f64) -> u64 {
    (sol * LAMPORTS_PER_SOL as f64).round() as u64
}

/// 32-byte Solana address. Only used as an identifier; there is no key material anywhere in this crate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Pubkey(pub [u8; 32]);

impl Pubkey {
    pub fn from_b58(s: &str) -> anyhow::Result<Self> {
        let v = bs58::decode(s).into_vec()?;
        if v.len() != 32 {
            anyhow::bail!("address must be 32 bytes, got {}", v.len());
        }
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        Ok(Pubkey(a))
    }
    /// Deterministic test/fixture address derived from a label (NOT a real key, no private key exists).
    pub fn synthetic(label: &str) -> Self {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(format!("pandarianz-fixture:{label}").as_bytes());
        let mut a = [0u8; 32];
        a.copy_from_slice(&h);
        Pubkey(a)
    }
    pub fn short(&self) -> String {
        let s = self.to_string();
        format!("{}…{}", &s[..4], &s[s.len() - 4..])
    }
}

impl fmt::Display for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(self.0).into_string())
    }
}
impl fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pubkey({self})")
    }
}
impl Serialize for Pubkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for Pubkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Pubkey::from_b58(&s).map_err(serde::de::Error::custom)
    }
}

/// Where the data came from. Fixture data is never presented as live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DataMode {
    /// Real provider stream (mainnet).
    Live,
    /// Synthetic fixture events (encoded per documented venue layouts). Not market data.
    Fixture,
    /// Re-run of a recorded journal; the original mode is kept on each event.
    Replay,
}

impl DataMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DataMode::Live => "LIVE",
            DataMode::Fixture => "FIXTURE",
            DataMode::Replay => "REPLAY",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    PumpBondingCurve,
    PumpSwap,
    RaydiumAmmV4,
}

impl Venue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Venue::PumpBondingCurve => "pump_bonding_curve",
            Venue::PumpSwap => "pumpswap",
            Venue::RaydiumAmmV4 => "raydium_amm_v4",
        }
    }
}

/// Program ids of supported venues (public mainnet addresses).
pub mod programs {
    pub const PUMP_FUN: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
    pub const PUMP_SWAP: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
    pub const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
}
