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

/// Verification table including real-transaction results when a summary is available.
pub fn verification_with(summary: Option<&crate::verify::VerificationSummary>) -> Vec<VerificationRow> {
    let mut rows: Vec<VerificationRow> = verification()
        .into_iter()
        .filter(|v| v.item != "captured mainnet transactions")
        .map(|v| {
            let key = format!("{}/{}", v.venue, v.item);
            let real = summary.and_then(|s| s.items.get(&key));
            VerificationRow {
                venue: v.venue.into(),
                item: v.item.into(),
                status: match real {
                    Some(r) if r.mainnet_verified => "mainnet_verified".into(),
                    Some(r) if r.hard_checks_failed > 0 => "MAINNET_MISMATCH".into(),
                    _ => v.status.into(),
                },
                evidence: match real {
                    Some(r) => format!(
                        "{} real txs / {} events ({} fully verified); hard checks {} passed, {} failed, {} inconclusive. {}",
                        r.transactions, r.events, r.events_fully_verified, r.hard_checks_passed, r.hard_checks_failed, r.hard_checks_inconclusive, v.evidence
                    ),
                    None => v.evidence.into(),
                },
            }
        })
        .collect();
    rows.push(VerificationRow {
        venue: "all".into(),
        item: "captured mainnet transactions".into(),
        status: match summary {
            Some(s) if s.files > 0 => format!("{} captured", s.files),
            _ => "pending".into(),
        },
        evidence: match summary {
            Some(s) => s
                .venues
                .iter()
                .map(|(v, r)| format!("{v}: {}", r.status))
                .collect::<Vec<_>>()
                .join("; "),
            None => "`./scripts/sniper check --capture 25` (reports/verification/verification.json)".into(),
        },
    });
    rows
}

#[derive(Clone, Debug, Serialize)]
pub struct VerificationRow {
    pub venue: String,
    pub item: String,
    pub status: String,
    pub evidence: String,
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

/// Where a decoded event came from inside its transaction.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// "log" (`Program data:`), "cpi" (Anchor self-CPI inner instruction) or "log+cpi" when the
    /// same event was emitted through both channels and was merged into one.
    pub channel: String,
    /// Top-level instruction index the event was emitted under.
    pub outer_ix: Option<u16>,
    /// Inner-instruction position of the self-CPI carrying the event (cpi channel).
    pub inner_pos: Option<u16>,
    /// Call depth of the emitting program (log channel) or of the self-CPI (cpi channel).
    pub stack_height: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decoded {
    /// Ordinal within the transaction (log order; CPI-only events 1000+) — with the signature a unique id.
    pub index: u16,
    pub event: VenueEvent,
    #[serde(default)]
    pub provenance: Provenance,
}

impl Decoded {
    /// Stable event identifier: signature, instruction provenance and ordinal.
    pub fn event_id(&self, signature: &str) -> String {
        let o = |x: Option<u16>| x.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
        format!("{signature}:{}:{}:{}", o(self.provenance.outer_ix), o(self.provenance.inner_pos), self.index)
    }
}

#[derive(Debug, Default)]
pub struct DecodeOutcome {
    pub events: Vec<Decoded>,
    pub warnings: Vec<String>,
    /// Transaction failed on chain: no state changes are applied.
    pub tx_failed: bool,
    /// Events seen through both the log and the self-CPI channel and merged (counted once).
    pub duplicates_merged: u32,
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
///
/// Anchor programs can emit the same event twice: as a `Program data:` log (`emit!`) and as a
/// self-CPI inner instruction (`emit_cpi!`). A full transaction then carries both copies of ONE
/// trade. Copies are merged when venue, payload bytes and top-level instruction match (pairing is
/// one-to-one, so genuinely repeated trades in one transaction are all kept).
pub fn decode_raw(raw: &RawEvent) -> DecodeOutcome {
    let mut out = DecodeOutcome::default();
    if raw.err.is_some() {
        out.tx_failed = true;
        return out;
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut stack: Vec<String> = Vec::new();
    let mut idx: u16 = 0;
    let mut outer: Option<u16> = None;
    // (venue, payload) of each log-channel event, aligned with out.events; None for ray_log
    let mut log_keys: Vec<Option<(Venue, Vec<u8>)>> = Vec::new();
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
                                    let provenance = Provenance { channel: "log".into(), outer_ix: outer, inner_pos: None, stack_height: Some(stack.len().min(255) as u8) };
                                    out.events.push(Decoded { index: idx, event: ev, provenance });
                                    log_keys.push(Some((venue, bytes.clone())));
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
                                Ok(l) => {
                                    let provenance = Provenance { channel: "log".into(), outer_ix: outer, inner_pos: None, stack_height: Some(stack.len().min(255) as u8) };
                                    out.events.push(Decoded { index: idx, event: VenueEvent::Raydium(l), provenance });
                                    log_keys.push(None);
                                }
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
                if tail == "invoke [1]" {
                    outer = Some(outer.map(|o| o + 1).unwrap_or(0));
                }
                stack.push(id.to_string());
            } else if tail == "success" || tail.starts_with("failed") {
                stack.pop();
            }
        } else if line.starts_with("Log truncated") {
            out.warnings.push("log truncated by validator; events may be missing".into());
        }
    }
    // emit_cpi events delivered as inner instructions (full-transaction providers only)
    let mut merged = vec![false; out.events.len()];
    for ix in &raw.inner_ix {
        let Some(venue) = program_venue(&ix.program_id) else { continue };
        let Ok(bytes) = b64.decode(&ix.data_b64) else {
            out.warnings.push("inner ix base64".into());
            continue;
        };
        let Some((disc, body)) = anchor::split_cpi_event(&bytes) else { continue };
        let payload = &bytes[anchor::EVENT_IX_TAG.len()..];
        // same event already decoded from the log channel → merge, never count twice
        let twin = (0..log_keys.len()).find(|&i| {
            !merged[i]
                && log_keys[i].as_ref().is_some_and(|(v, p)| *v == venue && p.as_slice() == payload)
                && match (out.events[i].provenance.outer_ix, ix.outer_index) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                }
        });
        if let Some(i) = twin {
            merged[i] = true;
            let p = &mut out.events[i].provenance;
            p.channel = "log+cpi".into();
            p.inner_pos = ix.position;
            out.duplicates_merged += 1;
            continue;
        }
        match decode_anchor(venue, disc, body) {
            Ok(Some(ev)) => {
                let provenance = Provenance { channel: "cpi".into(), outer_ix: ix.outer_index, inner_pos: ix.position, stack_height: ix.stack_height };
                out.events.push(Decoded { index: 1000 + idx, event: ev, provenance });
            }
            Ok(None) => {}
            Err(e) => out.warnings.push(format!("{} cpi decode: {e}", venue.as_str())),
        }
        idx += 1;
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
                InnerIx { program_id: programs::PUMP_SWAP.into(), data_b64: b64.encode(data), outer_index: None, position: None, stack_height: None }
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
