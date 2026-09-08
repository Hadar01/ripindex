//! The 16-byte header and 16-byte footer shared by every file (FORMAT.md §2).

use std::io::{self, Write};
use std::ops::Range;

use super::{read_u32_le, read_u64_le, FormatError, Result};

pub const HEADER_LEN: usize = 16;
pub const FOOTER_LEN: usize = 16;
/// v2 (M3): 48-byte doc records with a content hash and Binary/TooLarge
/// statuses, the `state-GGGGG.ovl` overlay file, and matching MANIFEST
/// fields. A v1 index is `Error::Incompatible`... no — older, so a v2 reader
/// opening a v1 file gets `FormatError::Version(1)`, which `store::reader`
/// maps to `Error::Incompatible` for a *newer* version only; an *older*
/// version the same way (never silently reinterpreted): the caller rebuilds.
pub const FORMAT_VERSION: u32 = 2;
pub const FOOTER_MAGIC: &[u8; 4] = b"FSRF";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Manifest,
    Idx,
    Doc,
    Del,
    Overlay,
}

impl Kind {
    pub fn magic(self) -> &'static [u8; 8] {
        match self {
            Kind::Manifest => b"FSRCHMAN",
            Kind::Idx => b"FSRCHIDX",
            Kind::Doc => b"FSRCHDOC",
            Kind::Del => b"FSRCHDEL",
            Kind::Overlay => b"FSRCHOVL",
        }
    }
}

pub fn header(kind: Kind) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..8].copy_from_slice(kind.magic());
    h[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    // flags = 0
    h
}

pub fn footer(body_len: u64, crc: u32) -> [u8; FOOTER_LEN] {
    let mut f = [0u8; FOOTER_LEN];
    f[..8].copy_from_slice(&body_len.to_le_bytes());
    f[8..12].copy_from_slice(&crc.to_le_bytes());
    f[12..].copy_from_slice(FOOTER_MAGIC);
    f
}

pub fn crc(body: &[u8]) -> u32 {
    crc32fast::hash(body)
}

/// Header + body + footer for a body that is already in memory.
pub fn seal(kind: Kind, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + FOOTER_LEN);
    out.extend_from_slice(&header(kind));
    out.extend_from_slice(body);
    out.extend_from_slice(&footer(body.len() as u64, crc(body)));
    out
}

/// Location of the body and the stored crc, after the structural checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub body: Range<usize>,
    pub crc: u32,
}

/// Check magic, version, flags, footer magic and `body_len` against the
/// slice length. Does **not** verify the crc — see [`verify_crc`].
pub fn parse(kind: Kind, bytes: &[u8]) -> Result<Envelope> {
    if bytes.len() < HEADER_LEN + FOOTER_LEN {
        return Err(FormatError::Truncated { at: 0, needed: HEADER_LEN + FOOTER_LEN });
    }
    if &bytes[..8] != kind.magic() {
        return Err(FormatError::BadMagic);
    }
    let version = read_u32_le(bytes, 8)?;
    if version != FORMAT_VERSION {
        return Err(FormatError::Version(version));
    }
    if read_u32_le(bytes, 12)? != 0 {
        return Err(FormatError::corrupt("non-zero header flags"));
    }
    let footer_at = bytes.len() - FOOTER_LEN;
    if &bytes[footer_at + 12..] != FOOTER_MAGIC {
        return Err(FormatError::corrupt("bad footer magic"));
    }
    let body_len = read_u64_le(bytes, footer_at)?;
    if body_len != (bytes.len() - HEADER_LEN - FOOTER_LEN) as u64 {
        return Err(FormatError::corrupt(format!(
            "body_len {body_len} does not match file length {}",
            bytes.len()
        )));
    }
    Ok(Envelope { body: HEADER_LEN..footer_at, crc: read_u32_le(bytes, footer_at + 8)? })
}

pub fn verify_crc(bytes: &[u8], env: &Envelope) -> Result<()> {
    let computed = crc(&bytes[env.body.clone()]);
    if computed != env.crc {
        return Err(FormatError::Checksum { stored: env.crc, computed });
    }
    Ok(())
}

/// [`parse`] then [`verify_crc`].
pub fn parse_verified(kind: Kind, bytes: &[u8]) -> Result<Envelope> {
    let env = parse(kind, bytes)?;
    verify_crc(bytes, &env)?;
    Ok(env)
}

/// Streams a body to `W`, tracking its length and crc for the footer.
pub struct BodyWriter<W: Write> {
    inner: W,
    hasher: crc32fast::Hasher,
    len: u64,
}

impl<W: Write> BodyWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, hasher: crc32fast::Hasher::new(), len: 0 }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `(writer, body_len, crc)`.
    pub fn finish(self) -> (W, u64, u32) {
        (self.inner, self.len, self.hasher.finalize())
    }
}

impl<W: Write> Write for BodyWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write_all(buf)?;
        self.hasher.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_parse() {
        let file = seal(Kind::Doc, b"body bytes");
        assert_eq!(file.len(), 32 + 10);
        let env = parse_verified(Kind::Doc, &file).unwrap();
        assert_eq!(&file[env.body.clone()], b"body bytes");
        assert_eq!(env.crc, crc(b"body bytes"));
        // Empty body is legal.
        let empty = seal(Kind::Del, b"");
        assert_eq!(parse_verified(Kind::Del, &empty).unwrap().body, 16..16);
    }

    #[test]
    fn streaming_matches_seal() {
        let mut out = Vec::new();
        out.extend_from_slice(&header(Kind::Idx));
        let mut w = BodyWriter::new(out);
        w.write_all(b"body ").unwrap();
        w.write_all(b"bytes").unwrap();
        let (mut out, len, c) = w.finish();
        out.extend_from_slice(&footer(len, c));
        assert_eq!(out, seal(Kind::Idx, b"body bytes"));
    }

    #[test]
    fn rejections() {
        let good = seal(Kind::Doc, b"xyz");
        assert!(matches!(parse(Kind::Idx, &good), Err(FormatError::BadMagic)));
        assert!(matches!(parse(Kind::Doc, &good[..20]), Err(FormatError::Truncated { .. })));

        let mut v = good.clone();
        v[8] = 99;
        assert!(matches!(parse(Kind::Doc, &v), Err(FormatError::Version(99))));

        let mut f = good.clone();
        f[12] = 1;
        assert!(matches!(parse(Kind::Doc, &f), Err(FormatError::Corrupt(_))));

        let mut short = good.clone();
        short.truncate(good.len() - 1); // footer magic damaged
        assert!(matches!(parse(Kind::Doc, &short), Err(FormatError::Corrupt(_))));

        let mut trunc = good.clone();
        trunc.remove(17); // body shorter than body_len claims
        assert!(matches!(parse(Kind::Doc, &trunc), Err(FormatError::Corrupt(_))));

        let mut flipped = good.clone();
        flipped[17] ^= 0xff;
        assert!(parse(Kind::Doc, &flipped).is_ok());
        assert!(matches!(parse_verified(Kind::Doc, &flipped), Err(FormatError::Checksum { .. })));
    }
}
