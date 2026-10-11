//! Venue decoders. Input is a provider-neutral `RawEvent` (logs + optional inner instructions).

pub mod anchor;
pub mod borsh;
pub mod pumpfun;
pub mod pumpswap;
pub mod raydium_v4;

use crate::stream::RawEvent;
use crate::types::{programs, Venue};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "venue", rename_all = "snake_case")]
pub enum VenueEvent {
    Pump(pumpfun::PumpEvent),
    PumpSwap(pumpswap::PumpSwapEvent),
    Raydium(raydium_v4::RayLog),
}

impl VenueEvent {
    pub fn venue(&self) -> Venue {
        match self {
            VenueEvent::Pump(_) => Venue::PumpBondingCurve,
            VenueEvent::PumpSwap(_) => Venue::PumpSwap,
            VenueEvent::Raydium(_) => Venue::RaydiumAmmV4,
        }
    }
    /// Unix seconds timestamp carried in the event payload, if any.
    pub fn timestamp(&self) -> Option<i64> {
        use pumpfun::PumpEvent as P;
        use pumpswap::PumpSwapEvent as S;
        match self {
            VenueEvent::Pump(P::Create(c)) => c.timestamp,
            VenueEvent::Pump(P::Trade(t)) => Some(t.timestamp),
            VenueEvent::Pump(P::Complete(c)) => Some(c.timestamp),
            VenueEvent::Pump(P::Migration(m)) => Some(m.timestamp),
            VenueEvent::PumpSwap(S::CreatePool(c)) => Some(c.timestamp),
            VenueEvent::PumpSwap(S::Swap(s)) => Some(s.timestamp),
            VenueEvent::PumpSwap(S::Liquidity(l)) => Some(l.timestamp),
            VenueEvent::Raydium(_) => None,
        }
    }
}

/// Verification status of each decoded layout (shown on the dashboard and in docs).
#[derive(Clone, Debug, Serialize)]
pub struct Verification {
    pub venue: &'static str,
    pub item: &'static str,
    pub status: &'static str,
    pub evidence: &'static str,
}

pub fn verification() -> Vec<Verification> {
    const PUMP: &str = "official IDL @pump-fun/pump-sdk@4.0.0 (tests/idl_verification.rs); math vs SDK bondingCurve.ts";
    const AMM: &str = "official IDL @pump-fun/pump-swap-sdk@2.1.0 (tests/idl_verification.rs); math vs SDK buy.ts/sell.ts";
    let mut v = Vec::new();
    for e in ["CreateEvent", "TradeEvent", "CompleteEvent", "CompletePumpAmmMigrationEvent", "BondingCurve account"] {
        v.push(Verification { venue: "pump_bonding_curve", item: e, status: "idl_verified", evidence: PUMP });
    }
    for e in ["CreatePoolEvent", "BuyEvent", "SellEvent", "DepositEvent", "WithdrawEvent", "Pool account"] {
        v.push(Verification { venue: "pumpswap", item: e, status: "idl_verified", evidence: AMM });
    }
    v.push(Verification { venue: "raydium_amm_v4", item: "ray_log", status: "unverified", evidence: "layout from public program source knowledge; decode-only, never traded" });
    v.push(Verification {
        venue: "all",
        item: "captured mainnet transactions",
        status: "pending",
        evidence: "run `sniper capture` with RPC access; fixtures/real/ is checked by tests/real_fixtures.rs",
    });
    v
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decoded {
    /// Index within the transaction (log/inner-ix order) — together with the signature this is a unique id.
    pub index: u16,
    pub event: VenueEvent,
}

#[derive(Debug, Default)]
pub struct DecodeOutcome {
    pub events: Vec<Decoded>,
    pub warnings: Vec<String>,
    /// Transaction failed on chain: no state changes are applied.
    pub tx_failed: bool,
}

fn program_venue(id: &str) -> Option<Venue> {
    match id {
        programs::PUMP_FUN => Some(Venue::PumpBondingCurve),
        programs::PUMP_SWAP => Some(Venue::PumpSwap),
        programs::RAYDIUM_AMM_V4 => Some(Venue::RaydiumAmmV4),
        _ => None,
    }
}

fn decode_anchor(venue: Venue, disc: [u8; 8], body: &[u8]) -> Result<Option<VenueEvent>, borsh::DecodeError> {
    match venue {
        Venue::PumpBondingCurve => Ok(pumpfun::decode(disc, body)?.map(VenueEvent::Pump)),
        Venue::PumpSwap => Ok(pumpswap::decode(disc, body)?.map(VenueEvent::PumpSwap)),
        Venue::RaydiumAmmV4 => Ok(None),
    }
}

/// Decode all supported venue events in a transaction. Unknown programs / discriminators are skipped.
pub fn decode_raw(raw: &RawEvent) -> DecodeOutcome {
    let mut out = DecodeOutcome::default();
    if raw.err.is_some() {
        out.tx_failed = true;
        return out;
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut stack: Vec<String> = Vec::new();
    let mut idx: u16 = 0;
    for line in &raw.logs {
        if let Some(rest) = line.strip_prefix("Program ") {
            if let Some(data) = rest.strip_prefix("data: ") {
                let Some(top) = stack.last() else { continue };
                let Some(venue) = program_venue(top) else { continue };
                match b64.decode(data.trim()) {
                    Ok(bytes) => {
                        if let Some((disc, body)) = anchor::split_log_event(&bytes) {
                            match decode_anchor(venue, disc, body) {
                                Ok(Some(ev)) => {
                                    out.events.push(Decoded { index: idx, event: ev });
                                }
                                Ok(None) => {}
                                Err(e) => out.warnings.push(format!("{} data decode: {e}", venue.as_str())),
                            }
                        }
                    }
                    Err(e) => out.warnings.push(format!("base64: {e}")),
                }
                idx += 1;
                continue;
            }
            if let Some(log) = rest.strip_prefix("log: ") {
                if let Some(payload) = log.strip_prefix("ray_log: ") {
                    if stack.last().map(|s| s.as_str()) == Some(programs::RAYDIUM_AMM_V4) {
                        match b64.decode(payload.trim()) {
                            Ok(bytes) => match raydium_v4::decode(&bytes) {
                                Ok(l) => out.events.push(Decoded { index: idx, event: VenueEvent::Raydium(l) }),
                                Err(e) => out.warnings.push(format!("ray_log: {e}")),
                            },
                            Err(e) => out.warnings.push(format!("ray_log base64: {e}")),
                        }
                        idx += 1;
                    }
                }
                continue;
            }
            // "Program <id> invoke [n]" / "Program <id> success" / "Program <id> failed: ..."
            let mut parts = rest.splitn(2, ' ');
            let id = parts.next().unwrap_or("");
            let tail = parts.next().unwrap_or("");
            if tail.starts_with("invoke [") {
                stack.push(id.to_string());
            } else if tail == "success" || tail.starts_with("failed") {
                stack.pop();
            }
        } else if line.starts_with("Log truncated") {
            out.warnings.push("log truncated by validator; events may be missing".into());
        }
    }
    // emit_cpi events delivered as inner instructions (full-transaction providers only)
    for ix in &raw.inner_ix {
        let Some(venue) = program_venue(&ix.program_id) else { continue };
        let Ok(bytes) = b64.decode(&ix.data_b64) else {
            out.warnings.push("inner ix base64".into());
            continue;
        };
        if let Some((disc, body)) = anchor::split_cpi_event(&bytes) {
            match decode_anchor(venue, disc, body) {
                Ok(Some(ev)) => out.events.push(Decoded { index: 1000 + idx, event: ev }),
                Ok(None) => {}
                Err(e) => out.warnings.push(format!("{} cpi decode: {e}", venue.as_str())),
            }
            idx += 1;
        }
    }
    out
}

/// Helpers for building fixture transactions (synthetic, labelled FIXTURE).
pub mod build {
    use super::*;
    use crate::stream::InnerIx;

    pub fn pump_logs(events: &[pumpfun::PumpEvent]) -> Vec<String> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut logs = vec![format!("Program {} invoke [1]", programs::PUMP_FUN)];
        for e in events {
            logs.push("Program log: Instruction: (fixture)".into());
            logs.push(format!("Program data: {}", b64.encode(pumpfun::encode(e))));
        }
        logs.push(format!("Program {} consumed 30000 of 200000 compute units", programs::PUMP_FUN));
        logs.push(format!("Program {} success", programs::PUMP_FUN));
        logs
    }

    pub fn pumpswap_inner(events: &[pumpswap::PumpSwapEvent]) -> (Vec<String>, Vec<InnerIx>) {
        let b64 = base64::engine::general_purpose::STANDARD;
        let logs = vec![
            format!("Program {} invoke [1]", programs::PUMP_SWAP),
            format!("Program {} invoke [2]", programs::PUMP_SWAP),
            format!("Program {} success", programs::PUMP_SWAP),
            format!("Program {} success", programs::PUMP_SWAP),
        ];
        let inner = events
            .iter()
            .map(|e| {
                let mut data = anchor::EVENT_IX_TAG.to_vec();
                data.extend(pumpswap::encode(e));
                InnerIx { program_id: programs::PUMP_SWAP.into(), data_b64: b64.encode(data) }
            })
            .collect();
        (logs, inner)
    }

    pub fn raydium_logs(l: &raydium_v4::RayLog) -> Vec<String> {
        let b64 = base64::engine::general_purpose::STANDARD;
        vec![
            format!("Program {} invoke [1]", programs::RAYDIUM_AMM_V4),
            format!("Program log: ray_log: {}", b64.encode(raydium_v4::encode(l))),
            format!("Program {} success", programs::RAYDIUM_AMM_V4),
        ]
    }
}
