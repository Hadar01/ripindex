//! ripindex — a persistent, memory-mapped inverted index over a directory tree.
//!
//! Pipeline:
//!
//! ```text
//! crawler ──► Vec<FileEntry> ──► index::builder ──► Index<MemTermStore>
//!                                  (tokenizer)          │
//!                                                        ▼
//!                       query::parse ──► query::exec ──► Vec<Hit> ──► snippet
//! ```
//!
//! **Storage boundary.** The query engine sees an index only through
//! [`index::IndexReader`] / [`index::SegmentReader`] and the posting-list
//! traits in [`index::postings`]. [`index::MemIndex`] is the in-RAM segment the
//! builder fills; [`store`] serialises it into the on-disk format in
//! `docs/FORMAT.md` and reads it back through memory maps behind the same
//! traits. Nothing in `query/` or `snippet` knows which backend it is on.
//!
//! File contents are never stored. Snippets re-read the file at query time.
//!
//! **Beyond a fresh build**, [`store`] also covers reconciling an index
//! against a changed filesystem without rebuilding it (`store::update_index`,
//! backed by [`store::state`]'s staged, cheapest-first change detection),
//! merging small or heavily-tombstoned segments ([`store::merge_index`]), a
//! refreshable handle safe to hold across concurrent updates and merges
//! ([`store::LiveIndex`]), and a hint-driven watcher with deterministically
//! testable scheduling ([`store::watcher`]). The watcher is a hint, never a
//! source of truth — [`store::state`]'s module docs say why.


pub mod bench;
pub mod crawler;
pub mod daemon;
pub mod error;
pub mod format;
pub mod fs;
pub mod index;
pub mod query;
pub mod report;
pub mod snippet;
pub mod store;
pub mod tokenizer;

pub use error::{Error, QueryError, Result};
pub use index::{DocId, Index};
