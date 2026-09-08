//! Doc IDs and the doc table.
//!
//! The table is **sparse**: a `DocId` may have no entry. M3's incremental
//! writer tombstones and re-appends, so holes are the normal, permanent
//! state. Use [`DocTable::iter`] or [`DocTable::get`]; never `0..len()`.

use std::mem::size_of;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::crawler::FileEntry;

/// Document identifier. Assigned by the builder; ids are never reused within
/// one index (a merge assigns a *new* id to a surviving doc rather than
/// keeping the old one).
pub type DocId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocStatus {
    /// Read, tokenized, postings present.
    Indexed,
    /// Crawled but not indexed (read error, invalid UTF-8, vanished before
    /// read). Recorded so the failure is visible; has no postings and is
    /// excluded from BM25's `N` and `avgdl`.
    Skipped,
    /// Recognised as binary by the crawler's NUL-byte sniff. Never read;
    /// recorded so a reconcile can detect a later change from metadata alone.
    Binary,
    /// Over the crawler's size limit. Never read, for the same reason.
    TooLarge,
}

impl DocStatus {
    /// Whether this status implies the file's bytes were fully read (and
    /// therefore has a meaningful `content_hash`).
    pub fn was_read(self) -> bool {
        matches!(self, DocStatus::Indexed | DocStatus::Skipped)
    }
}

#[derive(Debug, Clone)]
pub struct DocMeta {
    pub path: PathBuf,
    pub inode: u64,
    pub mtime: SystemTime,
    pub size: u64,
    /// Length in atoms (== number of positions), for BM25. 0 unless `Indexed`.
    pub len: u32,
    pub status: DocStatus,
    /// xxh3-64 of the raw file bytes. 0 for `Binary`/`TooLarge` (never read).
    /// Used by the reconciler to tell "rewritten with identical content"
    /// (rsync, `git checkout`, build systems) from a real change, without
    /// retokenizing.
    pub content_hash: u64,
}

impl DocMeta {
    /// A not-yet-indexed entry for a crawled file.
    pub(crate) fn from_entry(e: FileEntry) -> Self {
        Self { path: e.path, inode: e.inode, mtime: e.mtime, size: e.size, len: 0, status: DocStatus::Skipped, content_hash: 0 }
    }
}

/// `DocId -> DocMeta`, sparse. See the module docs.
#[derive(Debug, Default)]
pub struct DocTable {
    /// Index = `DocId`; `None` is a hole (never assigned, or removed).
    slots: Vec<Option<DocMeta>>,
    live: u32,
    indexed: u32,
    total_len: u64,
}

impl DocTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert at `id`, growing the table with holes as needed.
    /// Panics if `id` is occupied — ids are never reused within a build.
    pub(crate) fn insert(&mut self, id: DocId, meta: DocMeta) {
        let i = id as usize;
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || None);
        }
        assert!(self.slots[i].is_none(), "doc {id} inserted twice");
        if meta.status == DocStatus::Indexed {
            self.indexed += 1;
            self.total_len += meta.len as u64;
        }
        self.live += 1;
        self.slots[i] = Some(meta);
    }

    /// Record that `id` was tokenized to `len` atoms.
    pub(crate) fn mark_indexed(&mut self, id: DocId, len: u32) {
        let m = self.slots.get_mut(id as usize).and_then(Option::as_mut).expect("mark_indexed: unknown doc");
        match m.status {
            DocStatus::Indexed => self.total_len -= m.len as u64,
            DocStatus::Skipped | DocStatus::Binary | DocStatus::TooLarge => self.indexed += 1,
        }
        m.status = DocStatus::Indexed;
        m.len = len;
        self.total_len += len as u64;
    }

    /// Set a non-indexed status (`Binary`/`TooLarge`) without touching len/hash counts.
    pub(crate) fn set_status(&mut self, id: DocId, status: DocStatus) {
        debug_assert_ne!(status, DocStatus::Indexed, "use mark_indexed for that");
        let m = self.slots.get_mut(id as usize).and_then(Option::as_mut).expect("set_status: unknown doc");
        if m.status == DocStatus::Indexed {
            self.indexed -= 1;
            self.total_len -= m.len as u64;
            m.len = 0;
        }
        m.status = status;
    }

    /// Record the content hash of a doc that was actually read.
    pub(crate) fn set_hash(&mut self, id: DocId, hash: u64) {
        if let Some(m) = self.slots.get_mut(id as usize).and_then(Option::as_mut) {
            m.content_hash = hash;
        }
    }

    /// Remove `id`, leaving a hole. Counts and averages adjust accordingly.
    pub fn remove(&mut self, id: DocId) -> Option<DocMeta> {
        let m = self.slots.get_mut(id as usize)?.take()?;
        self.live -= 1;
        if m.status == DocStatus::Indexed {
            self.indexed -= 1;
            self.total_len -= m.len as u64;
        }
        Some(m)
    }

    pub fn get(&self, id: DocId) -> Option<&DocMeta> {
        self.slots.get(id as usize)?.as_ref()
    }

    /// Number of live docs of any status. Not an id range — see [`Self::id_bound`].
    pub fn len(&self) -> usize {
        self.live as usize
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// One past the highest id ever inserted. Any id below it may still be a hole.
    pub fn id_bound(&self) -> DocId {
        self.slots.len() as DocId
    }

    /// `N` for BM25: docs with status `Indexed`.
    pub fn indexed_count(&self) -> u32 {
        self.indexed
    }

    /// `avgdl` for BM25, over indexed docs. 0.0 when nothing is indexed.
    pub fn avg_len(&self) -> f32 {
        if self.indexed == 0 {
            0.0
        } else {
            self.total_len as f32 / self.indexed as f32
        }
    }

    /// Live docs in id order; holes are skipped.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, &DocMeta)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|m| (i as DocId, m)))
    }

    /// Approximate heap usage: slot vector plus every path's buffer.
    pub fn heap_bytes(&self) -> usize {
        self.slots.capacity() * size_of::<Option<DocMeta>>()
            + self.iter().map(|(_, m)| m.path.capacity()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str) -> DocMeta {
        DocMeta {
            path: PathBuf::from(name),
            inode: 0,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            len: 0,
            status: DocStatus::Skipped,
            content_hash: 0,
        }
    }

    #[test]
    fn insert_get_iter() {
        let mut t = DocTable::new();
        t.insert(0, meta("a"));
        t.insert(1, meta("b"));
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(1).unwrap().path, PathBuf::from("b"));
        assert!(t.get(2).is_none());
        let ids: Vec<DocId> = t.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn holes_are_normal() {
        let mut t = DocTable::new();
        t.insert(5, meta("e")); // gap 0..5 never assigned
        t.insert(2, meta("c"));
        assert_eq!(t.len(), 2);
        assert_eq!(t.id_bound(), 6);
        assert!(t.get(0).is_none());
        assert!(t.get(3).is_none());
        let ids: Vec<DocId> = t.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![2, 5]);
    }

    #[test]
    fn mark_indexed_and_averages() {
        let mut t = DocTable::new();
        t.insert(0, meta("a"));
        t.insert(1, meta("b"));
        t.insert(2, meta("c"));
        assert_eq!(t.indexed_count(), 0);
        assert_eq!(t.avg_len(), 0.0);

        t.mark_indexed(0, 10);
        t.mark_indexed(2, 30);
        assert_eq!(t.indexed_count(), 2);
        assert_eq!(t.avg_len(), 20.0);
        assert_eq!(t.get(1).unwrap().status, DocStatus::Skipped);
        assert_eq!(t.get(2).unwrap().status, DocStatus::Indexed);

        // Re-marking replaces the old length rather than adding to it.
        t.mark_indexed(2, 50);
        assert_eq!(t.indexed_count(), 2);
        assert_eq!(t.avg_len(), 30.0);
    }

    #[test]
    fn set_status_and_hash() {
        let mut t = DocTable::new();
        t.insert(0, meta("a.png"));
        t.set_status(0, DocStatus::Binary);
        assert_eq!(t.get(0).unwrap().status, DocStatus::Binary);
        assert_eq!(t.indexed_count(), 0);

        t.insert(1, meta("b.txt"));
        t.mark_indexed(1, 5);
        t.set_status(1, DocStatus::TooLarge); // e.g. grew past the limit on reconcile
        assert_eq!(t.get(1).unwrap().status, DocStatus::TooLarge);
        assert_eq!(t.get(1).unwrap().len, 0);
        assert_eq!(t.indexed_count(), 0);
        assert_eq!(t.avg_len(), 0.0);

        t.set_hash(0, 0xabc);
        assert_eq!(t.get(0).unwrap().content_hash, 0xabc);
    }

    #[test]
    fn remove_leaves_hole_and_fixes_counts() {
        let mut t = DocTable::new();
        t.insert(0, meta("a"));
        t.insert(1, meta("b"));
        t.insert(2, meta("c"));
        t.mark_indexed(0, 10);
        t.mark_indexed(1, 20);
        t.mark_indexed(2, 30);

        let removed = t.remove(1).unwrap();
        assert_eq!(removed.path, PathBuf::from("b"));
        assert!(t.get(1).is_none());
        assert_eq!(t.len(), 2);
        assert_eq!(t.id_bound(), 3); // id space does not shrink
        assert_eq!(t.indexed_count(), 2);
        assert_eq!(t.avg_len(), 20.0);
        let ids: Vec<DocId> = t.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![0, 2]);

        assert!(t.remove(1).is_none()); // already a hole
        assert!(t.remove(99).is_none()); // never existed
    }

    #[test]
    #[should_panic(expected = "inserted twice")]
    fn double_insert_panics() {
        let mut t = DocTable::new();
        t.insert(0, meta("a"));
        t.insert(0, meta("b"));
    }
}
