//! On-disk encodings — pure functions over byte slices, no I/O.
//! The layout is specified field-by-field in `docs/FORMAT.md`.

pub mod del;
pub mod doc;
pub mod envelope;
pub mod idx;
pub mod manifest;
pub mod overlay;
pub mod varint;

use thiserror::Error;

/// Anything wrong with bytes we are decoding, or with data we refuse to encode.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FormatError {
    #[error("truncated: needed {needed} bytes at offset {at}")]
    Truncated { at: usize, needed: usize },
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported format version {0}")]
    Version(u32),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("checksum mismatch: stored {stored:08x}, computed {computed:08x}")]
    Checksum { stored: u32, computed: u32 },
    /// The encoder was handed data that violates a documented invariant
    /// (e.g. a non-increasing position). Never written silently.
    #[error("encoder invariant violated: {0}")]
    Invariant(&'static str),
}

impl FormatError {
    pub(crate) fn corrupt(msg: impl Into<String>) -> Self {
        FormatError::Corrupt(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, FormatError>;

/// Bounds-checked fixed-width reads.
pub(crate) fn slice(buf: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    buf.get(at..at.checked_add(len).ok_or(FormatError::Truncated { at, needed: len })?)
        .ok_or(FormatError::Truncated { at, needed: len })
}

pub(crate) fn read_u32_le(buf: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(slice(buf, at, 4)?.try_into().unwrap()))
}

pub(crate) fn read_u64_le(buf: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(slice(buf, at, 8)?.try_into().unwrap()))
}

pub(crate) fn read_i64_le(buf: &[u8], at: usize) -> Result<i64> {
    Ok(i64::from_le_bytes(slice(buf, at, 8)?.try_into().unwrap()))
}

pub(crate) fn to_usize(v: u64) -> Result<usize> {
    usize::try_from(v).map_err(|_| FormatError::corrupt("offset exceeds address space"))
}
