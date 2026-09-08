//! The storage boundary: the only view of postings the query engine gets.
//!
//! M1 implements these for `HashMap<String, Vec<Posting>>` ([`super::memory`]).
//! M2 implements them over a memory-mapped, delta-encoded file. The cursor
//! shape (forward-only, `seek`, positions decoded on demand) is chosen so the
//! on-disk version can be implemented without buffering whole lists.

use super::DocId;

/// Token position within a document (atom index). See `tokenizer`.
pub type Position = u32;

/// `term -> posting list` lookup.
pub trait TermStore {
    type List<'a>: PostingList
    where
        Self: 'a;

    /// Posting list for a (lowercased) term, or `None` if absent.
    fn get(&self, term: &str) -> Option<Self::List<'_>>;

    /// Number of unique terms.
    fn term_count(&self) -> usize;

    /// Approximate bytes held by the store: heap for in-memory, mapped region for on-disk.
    fn memory_bytes(&self) -> usize;
}

/// A term's postings, sorted by `DocId`.
pub trait PostingList {
    type Cursor<'a>: PostingCursor
    where
        Self: 'a;

    /// Number of documents containing the term (`df` for BM25).
    fn doc_freq(&self) -> u32;

    /// A cursor positioned on the first posting (or exhausted if empty).
    fn cursor(&self) -> Self::Cursor<'_>;
}

/// Forward-only cursor over one posting list.
pub trait PostingCursor {
    /// Current document, or `None` once exhausted.
    fn doc(&self) -> Option<DocId>;

    /// Step to the next posting; returns the new current doc.
    fn advance(&mut self) -> Option<DocId>;

    /// Move to the first posting with `doc >= target`. No-op if already there.
    /// Intersections use this to leapfrog.
    fn seek(&mut self, target: DocId) -> Option<DocId>;

    /// Positions of the term in the current doc, ascending. Undefined (may
    /// panic) when exhausted. `&mut self` so encoded formats can decode into a
    /// buffer owned by the cursor.
    fn positions(&mut self) -> &[Position];

    /// `tf` in the current doc. Default derives from `positions()`; encoded
    /// formats can override with a cheaper read.
    fn term_freq(&mut self) -> u32 {
        self.positions().len() as u32
    }
}

/// `&T` is a `PostingList` whenever `T` is — lets an in-memory store hand out
/// `&MemPostingList` as its `List<'a>` without wrapping.
impl<T: PostingList + ?Sized> PostingList for &T {
    type Cursor<'a>
        = T::Cursor<'a>
    where
        Self: 'a;

    fn doc_freq(&self) -> u32 {
        (**self).doc_freq()
    }

    fn cursor(&self) -> Self::Cursor<'_> {
        (**self).cursor()
    }
}
