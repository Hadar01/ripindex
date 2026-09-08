//! Unsigned LEB128.

use super::{FormatError, Result};

pub fn put_u64(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    put_u64(out, v as u64)
}

/// Decode at `*pos`, advancing it. At most 10 bytes; rejects overlong or
/// overflowing encodings.
pub fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos).ok_or(FormatError::Truncated { at: *pos, needed: 1 })?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err(FormatError::corrupt("varint overflows u64"));
        }
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(FormatError::corrupt("varint longer than 10 bytes"));
        }
    }
}

pub fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let v = read_u64(buf, pos)?;
    u32::try_from(v).map_err(|_| FormatError::corrupt("varint exceeds u32"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: u64) -> usize {
        let mut buf = Vec::new();
        put_u64(&mut buf, v);
        let mut pos = 0;
        assert_eq!(read_u64(&buf, &mut pos).unwrap(), v);
        assert_eq!(pos, buf.len());
        buf.len()
    }

    #[test]
    fn edges() {
        assert_eq!(roundtrip(0), 1);
        assert_eq!(roundtrip(127), 1);
        assert_eq!(roundtrip(128), 2);
        assert_eq!(roundtrip(16_383), 2);
        assert_eq!(roundtrip(16_384), 3);
        assert_eq!(roundtrip(u32::MAX as u64), 5);
        assert_eq!(roundtrip(u64::MAX), 10);
    }

    #[test]
    fn u32_rejects_larger() {
        let mut buf = Vec::new();
        put_u64(&mut buf, u32::MAX as u64 + 1);
        assert!(matches!(read_u32(&buf, &mut 0), Err(FormatError::Corrupt(_))));
    }

    #[test]
    fn truncated_and_overlong() {
        assert!(matches!(read_u64(&[0x80], &mut 0), Err(FormatError::Truncated { .. })));
        assert!(matches!(read_u64(&[], &mut 0), Err(FormatError::Truncated { .. })));
        // 11 continuation bytes.
        let bad = [0x80u8; 11];
        assert!(matches!(read_u64(&bad, &mut 0), Err(FormatError::Corrupt(_))));
        // 10th byte carries more than one bit.
        let mut over = vec![0xffu8; 9];
        over.push(0x02);
        assert!(matches!(read_u64(&over, &mut 0), Err(FormatError::Corrupt(_))));
    }

    #[test]
    fn sequence() {
        let mut buf = Vec::new();
        for v in [1u64, 300, 70_000, 5] {
            put_u64(&mut buf, v);
        }
        let mut pos = 0;
        let got: Vec<u64> = (0..4).map(|_| read_u64(&buf, &mut pos).unwrap()).collect();
        assert_eq!(got, vec![1, 300, 70_000, 5]);
        assert_eq!(pos, buf.len());
    }
}
