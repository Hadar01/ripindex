//! `seg-NNNNN.doc` (FORMAT.md §5): fixed 48-byte records plus a path heap.
//!
//! v2 adds `content_hash` (xxh3-64 of the raw file bytes; 0 for records never
//! read — `Binary`/`TooLarge`) and two more status values, so the reconciler
//! can stat non-text files instead of re-sniffing them.

use super::envelope::{self, Envelope, Kind};
use super::{read_i64_le, read_u32_le, read_u64_le, slice, to_usize, FormatError, Result};
use crate::index::doc_table::DocStatus;

pub const BODY_HEADER_LEN: usize = 32;
pub const RECORD_LEN: usize = 48;
pub const STATUS_SKIPPED: u8 = 0;
pub const STATUS_INDEXED: u8 = 1;
pub const STATUS_BINARY: u8 = 2;
pub const STATUS_TOO_LARGE: u8 = 3;
pub const MAX_STATUS: u8 = STATUS_TOO_LARGE;

/// The record's status byte, decoupled from [`DocStatus`] so this module
/// doesn't need `index` in its public signatures beyond this one conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Skipped,
    Indexed,
    Binary,
    TooLarge,
}

impl DocKind {
    fn byte(self) -> u8 {
        match self {
            DocKind::Skipped => STATUS_SKIPPED,
            DocKind::Indexed => STATUS_INDEXED,
            DocKind::Binary => STATUS_BINARY,
            DocKind::TooLarge => STATUS_TOO_LARGE,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            STATUS_SKIPPED => Some(DocKind::Skipped),
            STATUS_INDEXED => Some(DocKind::Indexed),
            STATUS_BINARY => Some(DocKind::Binary),
            STATUS_TOO_LARGE => Some(DocKind::TooLarge),
            _ => None,
        }
    }
}

impl From<DocStatus> for DocKind {
    fn from(s: DocStatus) -> Self {
        match s {
            DocStatus::Skipped => DocKind::Skipped,
            DocStatus::Indexed => DocKind::Indexed,
            DocStatus::Binary => DocKind::Binary,
            DocStatus::TooLarge => DocKind::TooLarge,
        }
    }
}

impl From<DocKind> for DocStatus {
    fn from(k: DocKind) -> Self {
        match k {
            DocKind::Skipped => DocStatus::Skipped,
            DocKind::Indexed => DocStatus::Indexed,
            DocKind::Binary => DocStatus::Binary,
            DocKind::TooLarge => DocStatus::TooLarge,
        }
    }
}

/// One document as handed to the encoder. `path` is root-relative with `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocIn<'a> {
    pub path: &'a str,
    pub inode: u64,
    pub mtime_nanos: i64,
    pub size: u64,
    pub len: u32,
    pub kind: DocKind,
    /// xxh3-64 of the raw file bytes. 0 for `Binary`/`TooLarge` (never read).
    pub content_hash: u64,
}

/// Encode a complete `.doc` file (envelope included).
pub fn encode<'a>(docs: impl ExactSizeIterator<Item = DocIn<'a>>) -> Result<Vec<u8>> {
    let n = docs.len();
    let mut body = Vec::with_capacity(BODY_HEADER_LEN + n * RECORD_LEN + n * 32);
    body.resize(BODY_HEADER_LEN, 0);
    let mut paths: Vec<u8> = Vec::new();
    let mut num_indexed = 0u32;
    let mut total_len = 0u64;
    for d in docs {
        if d.path.len() > u32::MAX as usize || paths.len() + d.path.len() > u32::MAX as usize {
            return Err(FormatError::Invariant("path heap exceeds 4 GiB"));
        }
        if d.kind != DocKind::Indexed && d.len != 0 {
            return Err(FormatError::Invariant("non-indexed doc with non-zero length"));
        }
        body.extend_from_slice(&d.inode.to_le_bytes());
        body.extend_from_slice(&d.mtime_nanos.to_le_bytes());
        body.extend_from_slice(&d.size.to_le_bytes());
        body.extend_from_slice(&d.len.to_le_bytes());
        body.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        body.extend_from_slice(&(d.path.len() as u32).to_le_bytes());
        body.push(d.kind.byte());
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(&d.content_hash.to_le_bytes());
        paths.extend_from_slice(d.path.as_bytes());
        if d.kind == DocKind::Indexed {
            num_indexed += 1;
            total_len += d.len as u64;
        }
    }
    let paths_off = (envelope::HEADER_LEN + body.len()) as u64;
    body[0..4].copy_from_slice(&(n as u32).to_le_bytes());
    body[4..8].copy_from_slice(&num_indexed.to_le_bytes());
    body[8..16].copy_from_slice(&total_len.to_le_bytes());
    body[16..24].copy_from_slice(&paths_off.to_le_bytes());
    body[24..32].copy_from_slice(&(paths.len() as u64).to_le_bytes());
    body.extend_from_slice(&paths);
    Ok(envelope::seal(Kind::Doc, &body))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRecord {
    pub inode: u64,
    pub mtime_nanos: i64,
    pub size: u64,
    pub len: u32,
    pub status: u8,
    pub content_hash: u64,
    path_off: u32,
    path_len: u32,
}

impl DocRecord {
    pub fn indexed(&self) -> bool {
        self.status == STATUS_INDEXED
    }

    pub fn kind(&self) -> Option<DocKind> {
        DocKind::from_byte(self.status)
    }
}

/// Zero-copy view over a complete `.doc` file.
#[derive(Debug, Clone)]
pub struct DocTableView<'a> {
    bytes: &'a [u8],
    env: Envelope,
    num_docs: u32,
    num_indexed: u32,
    total_len: u64,
    paths: &'a [u8],
}

impl<'a> DocTableView<'a> {
    /// Envelope and structural checks only (no crc).
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let env = envelope::parse(Kind::Doc, bytes)?;
        let body = &bytes[env.body.clone()];
        if body.len() < BODY_HEADER_LEN {
            return Err(FormatError::Truncated { at: body.len(), needed: BODY_HEADER_LEN });
        }
        let num_docs = read_u32_le(body, 0)?;
        let num_indexed = read_u32_le(body, 4)?;
        let total_len = read_u64_le(body, 8)?;
        let paths_off = to_usize(read_u64_le(body, 16)?)?;
        let paths_len = to_usize(read_u64_le(body, 24)?)?;
        let records_end = envelope::HEADER_LEN + BODY_HEADER_LEN + num_docs as usize * RECORD_LEN;
        if paths_off != records_end || paths_off + paths_len != env.body.end {
            return Err(FormatError::corrupt("doc table regions do not tile the body"));
        }
        if num_indexed > num_docs {
            return Err(FormatError::corrupt("num_indexed exceeds num_docs"));
        }
        let paths = slice(bytes, paths_off, paths_len)?;
        Ok(Self { bytes, env, num_docs, num_indexed, total_len, paths })
    }

    /// Rebuild a view from parts validated by an earlier `parse`.
    pub(crate) fn from_parts(
        bytes: &'a [u8],
        env: Envelope,
        num_docs: u32,
        num_indexed: u32,
        total_len: u64,
        paths: std::ops::Range<usize>,
    ) -> Self {
        Self { bytes, env, num_docs, num_indexed, total_len, paths: &bytes[paths] }
    }

    pub fn envelope(&self) -> &Envelope {
        &self.env
    }

    /// Absolute byte range of the path heap.
    pub fn paths_range(&self) -> std::ops::Range<usize> {
        let start = self.env.body.end - self.paths.len();
        start..self.env.body.end
    }

    pub fn crc(&self) -> u32 {
        self.env.crc
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    pub fn num_indexed(&self) -> u32 {
        self.num_indexed
    }

    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    fn record_at(&self, i: u32) -> usize {
        envelope::HEADER_LEN + BODY_HEADER_LEN + i as usize * RECORD_LEN
    }

    pub fn record(&self, i: u32) -> Option<DocRecord> {
        if i >= self.num_docs {
            return None;
        }
        let b = self.bytes;
        let at = self.record_at(i);
        Some(DocRecord {
            inode: read_u64_le(b, at).ok()?,
            mtime_nanos: read_i64_le(b, at + 8).ok()?,
            size: read_u64_le(b, at + 16).ok()?,
            len: read_u32_le(b, at + 24).ok()?,
            path_off: read_u32_le(b, at + 28).ok()?,
            path_len: read_u32_le(b, at + 32).ok()?,
            status: b[at + 36],
            content_hash: read_u64_le(b, at + 40).ok()?,
        })
    }

    /// Root-relative path with `/` separators.
    pub fn path(&self, i: u32) -> Option<&'a str> {
        let r = self.record(i)?;
        let s = self.paths.get(r.path_off as usize..(r.path_off as usize).checked_add(r.path_len as usize)?)?;
        std::str::from_utf8(s).ok()
    }

    /// The one hot read for BM25: `len` when the record is indexed.
    pub fn doc_len_if_indexed(&self, i: u32) -> Option<u32> {
        if i >= self.num_docs {
            return None;
        }
        let at = self.record_at(i);
        if self.bytes[at + 36] != STATUS_INDEXED {
            return None;
        }
        read_u32_le(self.bytes, at + 24).ok()
    }

    /// Full check: crc, every record's status/path/len, counters.
    pub fn verify(&self) -> Result<()> {
        envelope::verify_crc(self.bytes, &self.env)?;
        let mut indexed = 0u32;
        let mut total = 0u64;
        for i in 0..self.num_docs {
            let r = self.record(i).ok_or_else(|| FormatError::corrupt("record out of range"))?;
            if r.status > MAX_STATUS {
                return Err(FormatError::corrupt(format!("doc {i}: bad status {}", r.status)));
            }
            if !r.indexed() && r.len != 0 {
                return Err(FormatError::corrupt(format!("doc {i}: non-indexed but len {}", r.len)));
            }
            if self.bytes[self.record_at(i) + 37..self.record_at(i) + 40] != [0, 0, 0] {
                return Err(FormatError::corrupt(format!("doc {i}: reserved bytes set")));
            }
            self.path(i).ok_or_else(|| FormatError::corrupt(format!("doc {i}: bad path")))?;
            if r.indexed() {
                indexed += 1;
                total += r.len as u64;
            }
        }
        if indexed != self.num_indexed || total != self.total_len {
            return Err(FormatError::corrupt("doc table counters disagree with records"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<DocIn<'static>> {
        vec![
            DocIn { path: "src/main.rs", inode: 42, mtime_nanos: 1_700_000_000_000_000_000, size: 10, len: 7, kind: DocKind::Indexed, content_hash: 0xdead_beef },
            DocIn { path: "bad.bin", inode: 0, mtime_nanos: -5, size: 3, len: 0, kind: DocKind::Skipped, content_hash: 0x1234 },
            DocIn { path: "日本/ünï.txt", inode: u64::MAX, mtime_nanos: i64::MIN, size: u64::MAX, len: u32::MAX, kind: DocKind::Indexed, content_hash: u64::MAX },
            DocIn { path: "", inode: 1, mtime_nanos: 0, size: 0, len: 0, kind: DocKind::Indexed, content_hash: 0 },
            DocIn { path: "img.png", inode: 2, mtime_nanos: 0, size: 5_000_000, len: 0, kind: DocKind::Binary, content_hash: 0 },
            DocIn { path: "huge.log", inode: 3, mtime_nanos: 0, size: 50_000_000, len: 0, kind: DocKind::TooLarge, content_hash: 0 },
        ]
    }

    #[test]
    fn roundtrip() {
        let docs = sample();
        let bytes = encode(docs.clone().into_iter()).unwrap();
        let v = DocTableView::parse(&bytes).unwrap();
        v.verify().unwrap();
        assert_eq!(v.num_docs(), 6);
        assert_eq!(v.num_indexed(), 3);
        assert_eq!(v.total_len(), 7 + u32::MAX as u64);
        for (i, d) in docs.iter().enumerate() {
            let r = v.record(i as u32).unwrap();
            assert_eq!(
                (r.inode, r.mtime_nanos, r.size, r.len, r.kind(), r.content_hash),
                (d.inode, d.mtime_nanos, d.size, d.len, Some(d.kind), d.content_hash)
            );
            assert_eq!(v.path(i as u32), Some(d.path));
        }
        assert_eq!(v.doc_len_if_indexed(0), Some(7));
        assert_eq!(v.doc_len_if_indexed(1), None); // skipped
        assert_eq!(v.doc_len_if_indexed(3), Some(0));
        assert_eq!(v.doc_len_if_indexed(4), None); // binary
        assert_eq!(v.doc_len_if_indexed(5), None); // too large
        assert_eq!(v.doc_len_if_indexed(6), None);
        assert!(v.record(6).is_none());
    }

    #[test]
    fn empty_table() {
        let bytes = encode(std::iter::empty()).unwrap();
        let v = DocTableView::parse(&bytes).unwrap();
        v.verify().unwrap();
        assert_eq!(v.num_docs(), 0);
        assert!(v.record(0).is_none());
    }

    #[test]
    fn encoder_refuses_non_indexed_with_length() {
        for kind in [DocKind::Skipped, DocKind::Binary, DocKind::TooLarge] {
            let bad = DocIn { path: "x", inode: 0, mtime_nanos: 0, size: 0, len: 3, kind, content_hash: 0 };
            assert!(matches!(encode(vec![bad].into_iter()), Err(FormatError::Invariant(_))));
        }
    }

    #[test]
    fn corruption() {
        let bytes = encode(sample().into_iter()).unwrap();
        let mut bad = bytes.clone();
        bad[16 + 32 + 36] = 7; // status byte of record 0 -> invalid
        let v = DocTableView::parse(&bad).unwrap();
        assert!(matches!(v.verify(), Err(FormatError::Checksum { .. })));

        // Same damage but with the crc "fixed" is caught structurally.
        let body = &bad[16..bad.len() - 16];
        let fixed = envelope::seal(Kind::Doc, body);
        assert!(matches!(DocTableView::parse(&fixed).unwrap().verify(), Err(FormatError::Corrupt(_))));

        let mut short = bytes.clone();
        short[16 + 16] = 1; // paths_off wrong
        assert!(matches!(DocTableView::parse(&short), Err(FormatError::Corrupt(_))));
    }

    #[test]
    fn kind_status_byte_roundtrip() {
        for k in [DocKind::Skipped, DocKind::Indexed, DocKind::Binary, DocKind::TooLarge] {
            assert_eq!(DocKind::from_byte(k.byte()), Some(k));
        }
        assert_eq!(DocKind::from_byte(4), None);
        for (s, k) in [
            (DocStatus::Skipped, DocKind::Skipped),
            (DocStatus::Indexed, DocKind::Indexed),
            (DocStatus::Binary, DocKind::Binary),
            (DocStatus::TooLarge, DocKind::TooLarge),
        ] {
            assert_eq!(DocKind::from(s), k);
            assert_eq!(DocStatus::from(k), s);
        }
    }
}
