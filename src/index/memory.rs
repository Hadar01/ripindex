//! M1 backend: `HashMap<String, MemPostingList>`.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::mem::size_of;

use super::postings::{Position, PostingCursor, PostingList, TermStore};
use super::DocId;

/// One document's entry in a posting list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    pub doc: DocId,
    /// Ascending, deduplicated.
    pub positions: Vec<Position>,
}

/// `Vec<Posting>` kept sorted by `doc`. Append-only during build.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemPostingList {
    postings: Vec<Posting>,
}

impl MemPostingList {
    /// Record `position` for `doc`. `doc` must be `>=` the last doc pushed;
    /// equal docs extend the existing posting. A repeat of the last position
    /// (identifier parts sharing their atom's slot) is dropped.
    pub fn push(&mut self, doc: DocId, position: Position) {
        match self.postings.last_mut() {
            Some(last) if last.doc == doc => {
                if last.positions.last() != Some(&position) {
                    debug_assert!(last.positions.last().is_none_or(|&p| p < position));
                    last.positions.push(position);
                }
            }
            Some(last) => {
                debug_assert!(last.doc < doc, "postings must be pushed in doc order");
                self.postings.push(Posting { doc, positions: vec![position] });
            }
            None => self.postings.push(Posting { doc, positions: vec![position] }),
        }
    }

    /// Shard merge: append `other`, whose docs are all greater than ours.
    pub(crate) fn append(&mut self, other: MemPostingList) {
        debug_assert!(
            match (self.postings.last(), other.postings.first()) {
                (Some(a), Some(b)) => a.doc < b.doc,
                _ => true,
            },
            "appended list must start after this one ends"
        );
        self.postings.extend(other.postings);
    }

    pub fn as_slice(&self) -> &[Posting] {
        &self.postings
    }

    pub fn heap_bytes(&self) -> usize {
        self.postings.capacity() * size_of::<Posting>()
            + self.postings.iter().map(|p| p.positions.capacity() * size_of::<Position>()).sum::<usize>()
    }
}

pub struct MemPostingCursor<'a> {
    postings: &'a [Posting],
    idx: usize,
}

impl PostingCursor for MemPostingCursor<'_> {
    fn doc(&self) -> Option<DocId> {
        self.postings.get(self.idx).map(|p| p.doc)
    }

    fn advance(&mut self) -> Option<DocId> {
        if self.idx < self.postings.len() {
            self.idx += 1;
        }
        self.doc()
    }

    /// Galloping search forward from the current index.
    fn seek(&mut self, target: DocId) -> Option<DocId> {
        let len = self.postings.len();
        if self.idx >= len {
            return None;
        }
        if self.postings[self.idx].doc >= target {
            return Some(self.postings[self.idx].doc);
        }
        // Invariant: postings[lo].doc < target. Double the step until we
        // overshoot, then binary-search the last interval.
        let mut lo = self.idx;
        let mut step = 1;
        let mut hi = lo + step;
        while hi < len && self.postings[hi].doc < target {
            lo = hi;
            step *= 2;
            hi = lo + step;
        }
        let hi = hi.min(len);
        let off = self.postings[lo + 1..hi].partition_point(|p| p.doc < target);
        self.idx = lo + 1 + off;
        self.doc()
    }

    fn positions(&mut self) -> &[Position] {
        &self.postings[self.idx].positions
    }
}

impl PostingList for MemPostingList {
    type Cursor<'a> = MemPostingCursor<'a>;

    fn doc_freq(&self) -> u32 {
        self.postings.len() as u32
    }

    fn cursor(&self) -> MemPostingCursor<'_> {
        MemPostingCursor { postings: &self.postings, idx: 0 }
    }
}

/// The in-memory term store.
#[derive(Debug, Default)]
pub struct MemTermStore {
    terms: HashMap<String, MemPostingList>,
}

impl MemTermStore {
    /// Iterate all terms — for stats and debugging, not used by queries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &MemPostingList)> {
        self.terms.iter().map(|(t, p)| (t.as_str(), p))
    }

    /// Total (term, doc) pairs.
    pub fn total_postings(&self) -> u64 {
        self.terms.values().map(|l| l.postings.len() as u64).sum()
    }

    /// Reduce step: fold a shard's map in. Shards must be merged in ascending
    /// doc-id order so each list stays sorted by simple append.
    pub(crate) fn merge_shard(&mut self, shard: HashMap<String, MemPostingList>) {
        if self.terms.is_empty() {
            self.terms = shard;
            return;
        }
        for (term, list) in shard {
            match self.terms.entry(term) {
                Entry::Occupied(mut e) => e.get_mut().append(list),
                Entry::Vacant(e) => {
                    e.insert(list);
                }
            }
        }
    }
}

impl TermStore for MemTermStore {
    type List<'a> = &'a MemPostingList;

    fn get(&self, term: &str) -> Option<&MemPostingList> {
        self.terms.get(term)
    }

    fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// Accounted heap: hashbrown table (one control byte plus a `(K, V)` slot
    /// per bucket; buckets are the next power of two above `capacity * 8/7`)
    /// plus every key's and list's own heap.
    fn memory_bytes(&self) -> usize {
        let cap = self.terms.capacity();
        let buckets = if cap == 0 { 0 } else { (cap * 8 / 7).next_power_of_two() };
        let table = buckets * (size_of::<String>() + size_of::<MemPostingList>() + 1);
        let contents: usize = self.terms.iter().map(|(k, v)| k.capacity() + v.heap_bytes()).sum();
        size_of::<Self>() + table + contents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(docs: &[(DocId, &[Position])]) -> MemPostingList {
        let mut l = MemPostingList::default();
        for (d, ps) in docs {
            for &p in *ps {
                l.push(*d, p);
            }
        }
        l
    }

    fn docs<C: PostingCursor>(mut c: C) -> Vec<DocId> {
        let mut out = vec![];
        while let Some(d) = c.doc() {
            out.push(d);
            c.advance();
        }
        out
    }

    #[test]
    fn push_groups_positions_by_doc_and_dedups_repeats() {
        let mut l = MemPostingList::default();
        l.push(3, 0);
        l.push(3, 0); // identifier part at the same slot
        l.push(3, 5);
        l.push(7, 1);
        assert_eq!(
            l.as_slice(),
            &[Posting { doc: 3, positions: vec![0, 5] }, Posting { doc: 7, positions: vec![1] }]
        );
        assert_eq!(l.doc_freq(), 2);
    }

    #[test]
    fn cursor_walks_in_order_and_exhausts() {
        let l = list(&[(1, &[0]), (4, &[2, 3]), (9, &[1])]);
        assert_eq!(docs(l.cursor()), vec![1, 4, 9]);
        let mut c = l.cursor();
        c.advance();
        assert_eq!(c.positions(), &[2, 3]);
        assert_eq!(c.term_freq(), 2);
        c.advance();
        c.advance();
        assert_eq!(c.doc(), None);
        assert_eq!(c.advance(), None); // stays exhausted
    }

    #[test]
    fn empty_list_cursor_is_exhausted() {
        let l = MemPostingList::default();
        let mut c = l.cursor();
        assert_eq!(c.doc(), None);
        assert_eq!(c.seek(0), None);
    }

    #[test]
    fn seek_semantics() {
        let ids: Vec<DocId> = (0..200).map(|i| i * 3).collect(); // 0,3,6,...,597
        let l = list(&ids.iter().map(|&d| (d, &[0u32][..])).collect::<Vec<_>>());

        let mut c = l.cursor();
        assert_eq!(c.seek(0), Some(0)); // already there
        assert_eq!(c.seek(4), Some(6)); // between → next
        assert_eq!(c.seek(6), Some(6)); // no-op when current >= target
        assert_eq!(c.seek(2), Some(6)); // never moves backwards
        assert_eq!(c.seek(300), Some(300)); // exact, far away (gallop)
        assert_eq!(c.seek(596), Some(597)); // near the end
        assert_eq!(c.seek(598), None); // past the end
        assert_eq!(c.doc(), None);
    }

    #[test]
    fn seek_every_target_matches_linear_scan() {
        let ids: Vec<DocId> = [1u32, 2, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377].to_vec();
        let l = list(&ids.iter().map(|&d| (d, &[0u32][..])).collect::<Vec<_>>());
        for start in 0..ids.len() {
            for target in 0..400u32 {
                let mut c = l.cursor();
                for _ in 0..start {
                    c.advance();
                }
                let expected = ids.iter().copied().find(|&d| d >= ids[start] && d >= target);
                assert_eq!(c.seek(target), expected, "start={start} target={target}");
            }
        }
    }

    #[test]
    fn append_concatenates() {
        let mut a = list(&[(1, &[0]), (2, &[1])]);
        let b = list(&[(5, &[0]), (8, &[1])]);
        a.append(b);
        assert_eq!(docs(a.cursor()), vec![1, 2, 5, 8]);
    }

    #[test]
    fn merge_shard_appends_existing_terms_and_adds_new() {
        let mut store = MemTermStore::default();
        let mut s1 = HashMap::new();
        s1.insert("foo".to_string(), list(&[(0, &[0]), (1, &[0])]));
        s1.insert("bar".to_string(), list(&[(1, &[1])]));
        let mut s2 = HashMap::new();
        s2.insert("foo".to_string(), list(&[(2, &[0])]));
        s2.insert("baz".to_string(), list(&[(3, &[0])]));
        store.merge_shard(s1);
        store.merge_shard(s2);

        assert_eq!(store.term_count(), 3);
        assert_eq!(docs(store.get("foo").unwrap().cursor()), vec![0, 1, 2]);
        assert_eq!(docs(store.get("bar").unwrap().cursor()), vec![1]);
        assert_eq!(docs(store.get("baz").unwrap().cursor()), vec![3]);
        assert!(store.get("nope").is_none());
        assert_eq!(store.total_postings(), 5);
    }

    #[test]
    fn reference_is_a_posting_list() {
        fn df<L: PostingList>(l: L) -> u32 {
            l.doc_freq()
        }
        let l = list(&[(1, &[0])]);
        assert_eq!(df(&l), 1);
        assert_eq!(df(&l), 1);
    }

    #[test]
    fn memory_accounting_is_positive_and_grows() {
        let mut store = MemTermStore::default();
        let base = store.memory_bytes();
        let mut s = HashMap::new();
        for i in 0..1000 {
            s.insert(format!("term{i}"), list(&[(i, &[0, 1, 2])]));
        }
        store.merge_shard(s);
        assert!(store.memory_bytes() > base + 1000 * 3 * size_of::<Position>());
    }
}
