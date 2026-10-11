//! Anchor event framing: `emit!` writes `Program data: base64(disc ++ borsh)` logs;
//! `emit_cpi!` writes a self-CPI whose instruction data is `EVENT_IX_TAG ++ disc ++ borsh`.

use sha2::{Digest, Sha256};

pub fn event_discriminator(name: &str) -> [u8; 8] {
    let h = Sha256::digest(format!("event:{name}").as_bytes());
    let mut d = [0u8; 8];
    d.copy_from_slice(&h[..8]);
    d
}

/// Anchor `EVENT_IX_TAG_LE`: u64 0x1d9acb512ea545e4 in little-endian byte order.
pub const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

/// Split an emit_cpi instruction payload into (discriminator, body).
pub fn split_cpi_event(data: &[u8]) -> Option<([u8; 8], &[u8])> {
    if data.len() < 16 || data[..8] != EVENT_IX_TAG {
        return None;
    }
    let mut d = [0u8; 8];
    d.copy_from_slice(&data[8..16]);
    Some((d, &data[16..]))
}

pub fn split_log_event(data: &[u8]) -> Option<([u8; 8], &[u8])> {
    if data.len() < 8 {
        return None;
    }
    let mut d = [0u8; 8];
    d.copy_from_slice(&data[..8]);
    Some((d, &data[8..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_ix_tag_matches_sha256() {
        // anchor-lang: EVENT_IX_TAG: u64 = 0x1d9acb512ea545e4 (= sha256 prefix read big-endian);
        // instruction data carries EVENT_IX_TAG.to_le_bytes() = e445a52e51cb9a1d.
        let h = Sha256::digest(b"anchor:event");
        let mut rev = EVENT_IX_TAG;
        rev.reverse();
        assert_eq!(&h[..8], &rev);
        assert_eq!(u64::from_le_bytes(EVENT_IX_TAG), 0x1d9a_cb51_2ea5_45e4);
    }
    #[test]
    fn known_pump_trade_discriminator() {
        // Widely published Pump.fun TradeEvent discriminator: bddb7fd34ee661ee
        assert_eq!(event_discriminator("TradeEvent"), [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee]);
    }
}
