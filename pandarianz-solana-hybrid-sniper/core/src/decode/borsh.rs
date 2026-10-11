//! Minimal little-endian Borsh reader/writer for venue event layouts.

use crate::types::Pubkey;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum DecodeError {
    #[error("truncated: needed {needed} bytes at offset {offset}, have {len}")]
    Truncated { needed: usize, offset: usize, len: usize },
    #[error("invalid utf8 string")]
    Utf8,
    #[error("string too long ({0})")]
    StringTooLong(u32),
    #[error("invalid bool byte {0}")]
    Bool(u8),
    #[error("base64: {0}")]
    Base64(String),
    #[error("unknown layout: {0}")]
    Unknown(String),
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated { needed: n, offset: self.pos, len: self.buf.len() });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(DecodeError::Bool(b)),
        }
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn pubkey(&mut self) -> Result<Pubkey, DecodeError> {
        let mut a = [0u8; 32];
        a.copy_from_slice(self.take(32)?);
        Ok(Pubkey(a))
    }
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let n = self.u32()?;
        if n > 4096 {
            return Err(DecodeError::StringTooLong(n));
        }
        let b = self.take(n as usize)?;
        String::from_utf8(b.to_vec()).map_err(|_| DecodeError::Utf8)
    }
    /// Optional trailing field: newer program versions append fields; older logs end early.
    pub fn opt_u64(&mut self) -> Option<u64> {
        if self.remaining() >= 8 { self.u64().ok() } else { None }
    }
    pub fn opt_i64(&mut self) -> Option<i64> {
        if self.remaining() >= 8 { self.i64().ok() } else { None }
    }
    pub fn opt_pubkey(&mut self) -> Option<Pubkey> {
        if self.remaining() >= 32 { self.pubkey().ok() } else { None }
    }
}

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn pubkey(&mut self, p: &Pubkey) -> &mut Self {
        self.bytes(&p.0)
    }
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.bytes(&(s.len() as u32).to_le_bytes());
        self.bytes(s.as_bytes())
    }
}
