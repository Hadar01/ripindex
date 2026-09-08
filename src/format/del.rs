//! `seg-NNNNN.GGGGG.del` (FORMAT.md §6): a deletion bitmap over local doc ids.

use super::envelope::{self, Kind};
use super::{read_u32_le, FormatError, Result};

pub const BODY_HEADER_LEN: usize = 8;

/// Mutable bitmap used to build the next generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    num_docs: u32,
    bits: Vec<u8>,
}

impl Bitmap {
    pub fn new(num_docs: u32) -> Self {
        Self { num_docs, bits: vec![0; num_docs.div_ceil(8) as usize] }
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Returns whether the bit was newly set. Out-of-range ids are ignored.
    pub fn set(&mut self, i: u32) -> bool {
        if i >= self.num_docs {
            return false;
        }
        let byte = &mut self.bits[(i >> 3) as usize];
        let mask = 1u8 << (i & 7);
        let was = *byte & mask != 0;
        *byte |= mask;
        !was
    }

    pub fn get(&self, i: u32) -> bool {
        i < self.num_docs && self.bits[(i >> 3) as usize] & (1 << (i & 7)) != 0
    }

    pub fn count(&self) -> u32 {
        self.bits.iter().map(|b| b.count_ones()).sum()
    }

    /// Encode a complete `.del` file (envelope included).
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(BODY_HEADER_LEN + self.bits.len());
        body.extend_from_slice(&self.num_docs.to_le_bytes());
        body.extend_from_slice(&self.count().to_le_bytes());
        body.extend_from_slice(&self.bits);
        envelope::seal(Kind::Del, &body)
    }
}

/// Zero-copy view over a verified `.del` file.
#[derive(Debug, Clone)]
pub struct DelView<'a> {
    num_docs: u32,
    num_deleted: u32,
    bits: &'a [u8],
    crc: u32,
}

impl<'a> DelView<'a> {
    /// Deletion files are small and always crc-verified on open.
    pub fn parse_verified(bytes: &'a [u8]) -> Result<Self> {
        let env = envelope::parse_verified(Kind::Del, bytes)?;
        let body = &bytes[env.body.clone()];
        if body.len() < BODY_HEADER_LEN {
            return Err(FormatError::Truncated { at: body.len(), needed: BODY_HEADER_LEN });
        }
        let num_docs = read_u32_le(body, 0)?;
        let num_deleted = read_u32_le(body, 4)?;
        let bits = &body[BODY_HEADER_LEN..];
        if bits.len() != num_docs.div_ceil(8) as usize {
            return Err(FormatError::corrupt("deletion bitmap length does not match num_docs"));
        }
        let popcount: u32 = bits.iter().map(|b| b.count_ones()).sum();
        if popcount != num_deleted {
            return Err(FormatError::corrupt("num_deleted does not match bitmap popcount"));
        }
        // Bits past num_docs in the last byte must be clear.
        if num_docs % 8 != 0 && bits.last().is_some_and(|&b| b >> (num_docs % 8) != 0) {
            return Err(FormatError::corrupt("deletion bitmap has bits past num_docs"));
        }
        Ok(Self { num_docs, num_deleted, bits, crc: env.crc })
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    pub fn num_deleted(&self) -> u32 {
        self.num_deleted
    }

    pub fn crc(&self) -> u32 {
        self.crc
    }

    #[inline]
    pub fn is_deleted(&self, i: u32) -> bool {
        i < self.num_docs && self.bits[(i >> 3) as usize] & (1 << (i & 7)) != 0
    }

    pub fn to_bitmap(&self) -> Bitmap {
        Bitmap { num_docs: self.num_docs, bits: self.bits.to_vec() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut b = Bitmap::new(13);
        assert!(b.set(0));
        assert!(b.set(12));
        assert!(!b.set(12));
        assert!(!b.set(13)); // out of range, ignored
        assert_eq!(b.count(), 2);
        let bytes = b.encode();
        assert_eq!(bytes.len(), 32 + 8 + 2);
        let v = DelView::parse_verified(&bytes).unwrap();
        assert_eq!((v.num_docs(), v.num_deleted()), (13, 2));
        assert!(v.is_deleted(0) && v.is_deleted(12));
        assert!(!v.is_deleted(1) && !v.is_deleted(13) && !v.is_deleted(1000));
        assert_eq!(v.to_bitmap(), b);
    }

    #[test]
    fn empty_and_zero_docs() {
        let bytes = Bitmap::new(0).encode();
        let v = DelView::parse_verified(&bytes).unwrap();
        assert_eq!(v.num_deleted(), 0);
        assert!(!v.is_deleted(0));
    }

    #[test]
    fn corruption() {
        let mut b = Bitmap::new(9);
        b.set(3);
        let bytes = b.encode();
        let mut flipped = bytes.clone();
        flipped[16 + 8] ^= 0x10;
        assert!(matches!(DelView::parse_verified(&flipped), Err(FormatError::Checksum { .. })));

        // Popcount mismatch with a "repaired" crc.
        let body = &flipped[16..flipped.len() - 16];
        let fixed = envelope::seal(Kind::Del, body);
        assert!(matches!(DelView::parse_verified(&fixed), Err(FormatError::Corrupt(_))));

        // Stray bit past num_docs.
        let mut stray = bytes[16..bytes.len() - 16].to_vec();
        stray[9] |= 0x80; // bit 15, num_docs = 9
        stray[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(DelView::parse_verified(&envelope::seal(Kind::Del, &stray)), Err(FormatError::Corrupt(_))));
    }
}
