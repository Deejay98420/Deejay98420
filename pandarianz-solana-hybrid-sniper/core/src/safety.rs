//! Mint sellability / safety checks from raw mint account data (SPL Token and Token-2022).
//!
//! Layout: SPL Mint (82 bytes): mint_authority COption<Pubkey> (4+32), supply u64, decimals u8,
//! is_initialized bool, freeze_authority COption<Pubkey> (4+32). Token-2022 mints with extensions
//! are padded to 165 bytes, then an AccountType byte (1 = Mint), then TLV entries (u16 type, u16 len).

use crate::types::Pubkey;
use serde::{Deserialize, Serialize};

pub const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MintSafety {
    pub program: String,
    pub decimals: u8,
    pub supply: u64,
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
    pub extensions: Vec<String>,
    /// Reasons this mint may not be freely sellable (blocking; empty = no known risk).
    pub risks: Vec<String>,
    /// Noteworthy but not blocking by default (e.g. mint authority still set).
    pub warnings: Vec<String>,
}

impl MintSafety {
    pub fn is_safe(&self) -> bool {
        self.risks.is_empty()
    }
}

fn coption_pubkey(d: &[u8], off: usize) -> Option<Option<Pubkey>> {
    let tag = u32::from_le_bytes(d.get(off..off + 4)?.try_into().ok()?);
    let mut k = [0u8; 32];
    k.copy_from_slice(d.get(off + 4..off + 36)?);
    Some(if tag == 1 { Some(Pubkey(k)) } else { None })
}

pub fn extension_name(t: u16) -> &'static str {
    match t {
        1 => "TransferFeeConfig",
        3 => "MintCloseAuthority",
        4 => "ConfidentialTransferMint",
        6 => "DefaultAccountState",
        9 => "NonTransferable",
        10 => "InterestBearingConfig",
        12 => "PermanentDelegate",
        14 => "TransferHook",
        16 => "ConfidentialTransferFeeConfig",
        18 => "MetadataPointer",
        19 => "TokenMetadata",
        20 => "GroupPointer",
        21 => "TokenGroup",
        22 => "GroupMemberPointer",
        23 => "TokenGroupMember",
        25 => "ScaledUiAmount",
        26 => "Pausable",
        _ => "Unknown",
    }
}

/// Decode a mint account owned by `owner_program` and classify risks.
pub fn check_mint(data: &[u8], owner_program: &str) -> Result<MintSafety, String> {
    if owner_program != SPL_TOKEN && owner_program != TOKEN_2022 {
        return Err(format!("mint owned by unknown program {owner_program}"));
    }
    if data.len() < 82 {
        return Err(format!("mint account too short ({} bytes)", data.len()));
    }
    let mint_authority = coption_pubkey(data, 0).ok_or("bad mint_authority")?;
    let supply = u64::from_le_bytes(data[36..44].try_into().unwrap());
    let decimals = data[44];
    if data[45] != 1 {
        return Err("mint not initialized".into());
    }
    let freeze_authority = coption_pubkey(data, 46).ok_or("bad freeze_authority")?;
    let mut s = MintSafety {
        program: if owner_program == TOKEN_2022 { "token-2022".into() } else { "spl-token".into() },
        decimals,
        supply,
        mint_authority,
        freeze_authority,
        extensions: vec![],
        risks: vec![],
        warnings: vec![],
    };
    if freeze_authority.is_some() {
        s.risks.push("freeze authority set (holders can be frozen)".into());
    }
    if mint_authority.is_some() {
        s.warnings.push("mint authority not revoked (supply can be inflated)".into());
    }
    if owner_program == TOKEN_2022 && data.len() > 166 {
        if data[165] != 1 {
            return Err(format!("unexpected Token-2022 account type {}", data[165]));
        }
        let mut off = 166;
        while off + 4 <= data.len() {
            let t = u16::from_le_bytes([data[off], data[off + 1]]);
            let len = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
            if t == 0 {
                break;
            }
            let body = data.get(off + 4..off + 4 + len).ok_or("truncated TLV")?;
            let name = extension_name(t);
            s.extensions.push(name.to_string());
            match t {
                // TransferFeeConfig: ..., older/newer TransferFee {epoch u64, max_fee u64, bps u16}
                1 => {
                    // authority(32) + withdraw authority(32) + withheld u64 + older{epoch,max,bps} + newer{epoch,max,bps}
                    let bps_at = |o: usize| body.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
                    let (older, newer) = (bps_at(88), bps_at(106));
                    if older > 0 || newer > 0 {
                        s.risks.push(format!("transfer fee {} bps", older.max(newer)));
                    }
                }
                6 => {
                    if body.first() == Some(&2) {
                        s.risks.push("default account state = frozen".into());
                    }
                }
                9 => s.risks.push("non-transferable".into()),
                12 => {
                    if body.iter().any(|b| *b != 0) {
                        s.risks.push("permanent delegate can move holder balances".into());
                    }
                }
                14 => {
                    // TransferHook { authority: OptionalNonZeroPubkey, program_id: OptionalNonZeroPubkey }
                    if body.get(32..64).is_some_and(|p| p.iter().any(|b| *b != 0)) {
                        s.risks.push("transfer hook program set".into());
                    }
                }
                26 => s.risks.push("pausable mint".into()),
                4 | 16 => s.risks.push("confidential transfers enabled".into()),
                _ => {}
            }
            off += 4 + len;
        }
    }
    Ok(s)
}

/// Build raw mint bytes (tests / fixtures).
pub fn build_mint(mint_authority: Option<Pubkey>, freeze_authority: Option<Pubkey>, supply: u64, decimals: u8, extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut d = Vec::new();
    let opt = |d: &mut Vec<u8>, p: Option<Pubkey>| {
        d.extend((p.is_some() as u32).to_le_bytes());
        d.extend(p.map(|k| k.0).unwrap_or([0u8; 32]));
    };
    opt(&mut d, mint_authority);
    d.extend(supply.to_le_bytes());
    d.push(decimals);
    d.push(1);
    opt(&mut d, freeze_authority);
    if !extensions.is_empty() {
        d.resize(165, 0);
        d.push(1);
        for (t, body) in extensions {
            d.extend(t.to_le_bytes());
            d.extend((body.len() as u16).to_le_bytes());
            d.extend(body);
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_revoked_mint_is_safe() {
        let s = check_mint(&build_mint(None, None, 1_000_000_000_000_000, 6, &[]), SPL_TOKEN).unwrap();
        assert!(s.is_safe(), "{:?}", s.risks);
    }
    #[test]
    fn token_2022_metadata_only_is_safe_but_hooks_and_freeze_are_not() {
        let meta = vec![(18u16, vec![0u8; 64]), (19u16, vec![1u8; 40])];
        let s = check_mint(&build_mint(None, None, 1, 6, &meta), TOKEN_2022).unwrap();
        assert!(s.is_safe(), "{:?}", s.risks);
        assert_eq!(s.extensions, vec!["MetadataPointer", "TokenMetadata"]);
        let mut hook = vec![0u8; 64];
        hook[40] = 7;
        let s = check_mint(&build_mint(None, Some(Pubkey::synthetic("f")), 1, 6, &[(14, hook), (9, vec![])]), TOKEN_2022).unwrap();
        assert_eq!(s.risks.len(), 3, "{:?}", s.risks);
        let mut fee = vec![0u8; 108];
        fee[106] = 100; // newer transfer fee 100 bps
        let s = check_mint(&build_mint(Some(Pubkey::synthetic("m")), None, 1, 6, &[(1, fee)]), TOKEN_2022).unwrap();
        assert_eq!(s.risks, vec!["transfer fee 100 bps".to_string()]);
        assert_eq!(s.warnings.len(), 1);
    }
    #[test]
    fn rejects_garbage() {
        assert!(check_mint(&[0u8; 10], SPL_TOKEN).is_err());
        assert!(check_mint(&build_mint(None, None, 1, 6, &[]), "Other111").is_err());
    }
}
