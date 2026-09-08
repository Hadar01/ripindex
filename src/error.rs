use std::path::PathBuf;
use thiserror::Error;

/// Fatal errors — the ones that abort a command.
///
/// Per-file problems (permission denied, invalid UTF-8, file vanished between
/// crawl and read) are deliberately *not* represented here: they are logged
/// with `log::warn!` and the file is skipped.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}: not a directory")]
    NotADirectory(PathBuf),
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Format(#[from] crate::format::FormatError),
    /// A file the manifest committed is missing or fails its checks.
    #[error("{path}: {reason}")]
    Corrupt { path: PathBuf, reason: String },
    /// A manifest written by a newer format. Never treated as absent.
    #[error("{path}: format version {version} is newer than this build supports")]
    Incompatible { path: PathBuf, version: u32 },
    #[error("no index at {0} (run `ripindex index` first)")]
    NoIndex(PathBuf),
    #[error("{0}: another writer holds the index lock")]
    Locked(PathBuf),
    /// A file a manifest referenced was not found on open. Distinct from
    /// [`Error::Corrupt`]: this is the expected shape of a reader racing a
    /// concurrent commit that retired the file between the reader's manifest
    /// read and its attempt to open what that manifest named. `store::reader`
    /// retries on this a bounded number of times (re-reading the manifest
    /// each time) before surfacing it; genuine corruption is stable across
    /// retries and always reports as `Corrupt` instead.
    #[error("{0}: not found (possibly retired by a concurrent commit)")]
    Vanished(PathBuf),
}

/// Query parse errors. Byte offsets refer to the original query string.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum QueryError {
    #[error("empty query")]
    Empty,
    #[error("unexpected {found:?} at byte {at}")]
    Unexpected { found: String, at: usize },
    #[error("unterminated phrase starting at byte {at}")]
    UnterminatedPhrase { at: usize },
    #[error("unbalanced parenthesis at byte {at}")]
    UnbalancedParen { at: usize },
    #[error("negation needs a positive term to subtract from (e.g. `foo -bar`)")]
    OnlyNegation,
}

pub type Result<T> = std::result::Result<T, Error>;
