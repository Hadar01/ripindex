//! Index abstractions and the in-memory segment.
//!
//! The query engine sees an index only through [`IndexReader`] — an ordered
//! list of [`SegmentReader`]s plus global statistics. Segments own disjoint,
//! ascending doc-id ranges (`base()..base()+num_docs()`), so evaluation runs
//! per segment on local ids and concatenates.
//!
//! * [`MemIndex`] — one in-RAM segment (`HashMap` postings). The builder
//!   accumulates into it before serialising; tests use it as the reference.
//! * [`Index`] — the persistent, memory-mapped, multi-segment index from
//!   [`crate::store`]. What the CLI opens.

pub mod builder;
pub mod doc_table;
pub mod memory;
pub mod postings;

use std::time::Duration;

pub use builder::{BuildConfig, IndexBuilder};
pub use doc_table::{DocId, DocMeta, DocStatus, DocTable};
pub use memory::MemTermStore;
pub use postings::{Position, PostingCursor, PostingList, TermStore};

pub use crate::store::{build_from_dir, Index};
use crate::crawler::CrawlStats;

/// Read side of one segment. Ids are **local** (`0..num_docs()`); the global
/// id is `base() + local`.
pub trait SegmentReader {
    type List<'a>: PostingList
    where
        Self: 'a;

    /// Global id of local doc 0.
    fn base(&self) -> DocId;

    /// One past the highest local id.
    fn num_docs(&self) -> u32;

    /// Posting list for a lowercased term, local doc ids.
    fn postings(&self, term: &str) -> Option<Self::List<'_>>;

    /// Length in atoms of a live doc — indexed, not deleted, not a hole.
    /// `None` makes the query engine skip the posting.
    fn doc_len(&self, local: DocId) -> Option<u32>;
}

/// Read side of a whole index.
pub trait IndexReader {
    type Segment: SegmentReader;

    /// Segments in ascending `base()` order.
    fn segments(&self) -> impl Iterator<Item = &Self::Segment> + '_;

    /// `N` for BM25: indexed docs that are not deleted.
    fn indexed_count(&self) -> u32;

    /// `avgdl` for BM25.
    fn avg_len(&self) -> f32;

    /// Metadata for a global id; `None` for holes and deleted docs.
    fn doc(&self, id: DocId) -> Option<DocMeta>;

    fn stats(&self) -> &IndexStats;
}

/// A single in-memory segment with base 0.
pub struct MemIndex<S: TermStore = MemTermStore> {
    docs: DocTable,
    terms: S,
    stats: IndexStats,
}

impl<S: TermStore> MemIndex<S> {
    pub(crate) fn new(docs: DocTable, terms: S, stats: IndexStats) -> Self {
        Self { docs, terms, stats }
    }

    pub fn docs(&self) -> &DocTable {
        &self.docs
    }

    pub fn terms(&self) -> &S {
        &self.terms
    }

    /// Consume into parts — the segment writer serialises these.
    pub fn into_parts(self) -> (DocTable, S, IndexStats) {
        (self.docs, self.terms, self.stats)
    }
}

impl<S: TermStore> SegmentReader for MemIndex<S> {
    type List<'a>
        = S::List<'a>
    where
        Self: 'a;

    fn base(&self) -> DocId {
        0
    }

    fn num_docs(&self) -> u32 {
        self.docs.id_bound()
    }

    fn postings(&self, term: &str) -> Option<S::List<'_>> {
        self.terms.get(term)
    }

    fn doc_len(&self, local: DocId) -> Option<u32> {
        self.docs.get(local).filter(|m| m.status == DocStatus::Indexed).map(|m| m.len)
    }
}

impl<S: TermStore> IndexReader for MemIndex<S> {
    type Segment = Self;

    fn segments(&self) -> impl Iterator<Item = &Self> + '_ {
        std::iter::once(self)
    }

    fn indexed_count(&self) -> u32 {
        self.docs.indexed_count()
    }

    fn avg_len(&self) -> f32 {
        self.docs.avg_len()
    }

    fn doc(&self, id: DocId) -> Option<DocMeta> {
        self.docs.get(id).cloned()
    }

    fn stats(&self) -> &IndexStats {
        &self.stats
    }
}

/// Build/open statistics, reported by `index`, `verify` and `bench`.
#[derive(Debug, Default, Clone)]
pub struct IndexStats {
    pub crawl: CrawlStats,
    /// Live docs (any status), i.e. not deleted.
    pub docs_total: u32,
    pub docs_indexed: u32,
    /// Docs recorded but without postings: read/decode failure, `Binary`,
    /// or `TooLarge`. Computed as `num_docs - num_indexed` per segment, so
    /// open stays O(segments), not O(docs) — it does not distinguish the
    /// three causes; see `crawl` for the binary/too-large counts specifically.
    pub docs_skipped: u32,
    /// Total (term, position) pairs stored — "total terms".
    pub total_tokens: u64,
    /// Total (term, doc) pairs — posting count.
    pub total_postings: u64,
    /// Unique terms; for a multi-segment index the sum over segments.
    pub unique_terms: usize,
    pub segments: u32,
    pub crawl_time: Duration,
    /// Tokenise + serialise + commit, excluding the crawl.
    pub build_time: Duration,
    /// Time to open (map + validate) the index most recently.
    pub open_time: Duration,
    /// Accounted heap of the in-process structures. For the persistent index
    /// this is the reader's bookkeeping; segment data lives in the mapping.
    pub memory_bytes: usize,
    /// Sum of all files under `.ripindex/`.
    pub on_disk_bytes: u64,
    /// Process resident set right after the build, if the platform reports it.
    pub rss_bytes: Option<usize>,
    /// RSS growth across crawl + build. The gap between this and
    /// `memory_bytes` is allocator overhead and fragmentation.
    pub rss_delta_bytes: Option<i64>,
}

/// Current process RSS, if the platform reports it.
pub fn rss_bytes() -> Option<usize> {
    memory_stats::memory_stats().map(|m| m.physical_mem)
}
