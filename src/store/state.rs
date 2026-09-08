//! The file-state view used for reconciliation, and the staged, cheapest-first
//! change-detection algorithm (FORMAT.md's design note on incremental updates).
//!
//! The watcher is a hint, never a source of truth: it says where to look.
//! This module says what is actually true, by comparing a fresh crawl against
//! the last-committed state (doc records, with any overlay already applied).
//! `reconcile` is pure with respect to the committed state — its only I/O is
//! reading a file's current bytes, and only when metadata alone cannot
//! decide, which is the expensive case (content truly changed) far more often
//! than not (rsync/`git checkout`/build-system rewrites of identical bytes).

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use crate::crawler::{CrawledFile, FileKind};
use crate::index::builder::hash_bytes;
use crate::index::{DocId, DocMeta, DocStatus};
use crate::store::segment::time_to_nanos;
use crate::store::writer::relative_path;

/// One doc's last-committed state, keyed by root-relative path.
#[derive(Debug, Clone)]
pub struct FileState {
    pub doc_id: DocId,
    pub inode: u64,
    pub mtime_nanos: i64,
    pub size: u64,
    pub content_hash: u64,
    pub status: DocStatus,
}

/// `path -> state` and `inode -> path` over everything the index currently
/// records (any status — `Indexed`, `Skipped`, `Binary`, `TooLarge`).
#[derive(Debug, Default)]
pub struct StateIndex {
    by_path: HashMap<String, FileState>,
    by_inode: HashMap<u64, String>,
}

impl StateIndex {
    /// Build from a doc iterator with **root-joined** paths (what
    /// `Index::docs().iter()` yields) — the natural output of the reader,
    /// with any overlay patch already folded in.
    pub fn from_docs(root: &Path, docs: impl Iterator<Item = (DocId, DocMeta)>) -> Self {
        let mut by_path = HashMap::new();
        let mut by_inode = HashMap::new();
        for (id, m) in docs {
            let rel = relative_path(root, &m.path);
            if m.inode != 0 {
                by_inode.insert(m.inode, rel.clone());
            }
            by_path.insert(
                rel,
                FileState {
                    doc_id: id,
                    inode: m.inode,
                    mtime_nanos: time_to_nanos(m.mtime),
                    size: m.size,
                    content_hash: m.content_hash,
                    status: m.status,
                },
            );
        }
        Self { by_path, by_inode }
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    pub fn get(&self, rel_path: &str) -> Option<&FileState> {
        self.by_path.get(rel_path)
    }
}

/// One unit of work the incremental writer must perform to converge.
#[derive(Debug)]
pub enum Change {
    /// Tombstone this doc id: its path is gone, or it's the "old" side of a
    /// reindex (content changed, possibly also moved).
    Delete(DocId),
    /// Metadata-only update — a rename and/or a touch with identical content.
    /// No reindex, no new segment entry: an overlay patch.
    Patch { doc_id: DocId, inode: u64, mtime_nanos: i64, size: u64, path: String },
    /// Needs a full (re)read and, for `Text`, tokenizing. `bytes` is `Some`
    /// when reconcile already read the file to make this decision (a changed
    /// existing doc) — the writer still re-reads today (see M3 notes); a
    /// brand-new file has `bytes: None`.
    Reindex { path: String, kind: FileKind, inode: u64, mtime_nanos: i64, size: u64, bytes: Option<Vec<u8>> },
}

#[derive(Debug, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    /// Files stat'd (cheap) during this reconcile — for the cost bench.
    pub files_examined: u64,
    /// Bytes actually read (hashing or otherwise) — the expensive part.
    pub bytes_read: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn deletes(&self) -> usize {
        self.changes.iter().filter(|c| matches!(c, Change::Delete(_))).count()
    }

    pub fn patches(&self) -> usize {
        self.changes.iter().filter(|c| matches!(c, Change::Patch { .. })).count()
    }

    pub fn reindexes(&self) -> usize {
        self.changes.iter().filter(|c| matches!(c, Change::Reindex { .. })).count()
    }
}

fn read_and_hash(path: &Path) -> io::Result<(Vec<u8>, u64)> {
    let bytes = std::fs::read(path)?;
    let hash = hash_bytes(&bytes);
    Ok((bytes, hash))
}

/// Compare `prev` against a fresh crawl and produce the plan to converge.
///
/// Staged, cheapest first: `(inode, mtime, size)` unchanged → skip, no I/O.
/// Metadata changed at the same path → read + hash; hash unchanged → `Patch`
/// (a rewrite with identical bytes); hash changed → `Delete` + `Reindex`.
/// A new path whose inode matches a path no longer present is a rename
/// candidate: `(mtime, size)` also unchanged → `Patch`, no I/O at all;
/// otherwise read + hash to tell "moved and edited" (`Delete` + `Reindex`)
/// from "moved, content confirmed identical" (`Patch`). Anything left in
/// `prev` that was neither seen nor consumed as a rename source is `Delete`d.
pub fn reconcile(prev: &StateIndex, root: &Path, crawled: &[CrawledFile]) -> Plan {
    let mut plan = Plan::default();
    let mut seen: HashSet<String> = HashSet::with_capacity(crawled.len());
    let mut consumed_old: HashSet<String> = HashSet::new();

    for cf in crawled {
        plan.files_examined += 1;
        let rel = relative_path(root, &cf.entry.path);
        let e = &cf.entry;
        let new_mtime = time_to_nanos(e.mtime);

        if let Some(prior) = prev.get(&rel) {
            if prior.inode == e.inode && prior.mtime_nanos == new_mtime && prior.size == e.size {
                seen.insert(rel);
                continue; // unchanged: no I/O
            }
            if cf.kind == FileKind::Text && prior.status.was_read() {
                match read_and_hash(&e.path) {
                    Ok((bytes, hash)) => {
                        plan.bytes_read += bytes.len() as u64;
                        if hash == prior.content_hash {
                            plan.changes.push(patch(prior.doc_id, e.inode, new_mtime, e.size, rel.clone()));
                        } else {
                            plan.changes.push(Change::Delete(prior.doc_id));
                            plan.changes.push(reindex(rel.clone(), cf.kind, e.inode, new_mtime, e.size, Some(bytes)));
                        }
                    }
                    Err(err) => {
                        log::warn!("reconcile: {}: {err}", e.path.display());
                        plan.changes.push(Change::Delete(prior.doc_id));
                    }
                }
            } else {
                // Binary/TooLarge (no baseline hash either side): can't tell
                // "same content" from metadata, so treat as a fresh record.
                plan.changes.push(Change::Delete(prior.doc_id));
                plan.changes.push(reindex(rel.clone(), cf.kind, e.inode, new_mtime, e.size, None));
            }
            seen.insert(rel);
            continue;
        }

        // Not a known path: is it a rename of something no longer at its old path?
        if e.inode != 0 {
            if let Some(old_path) = prev.by_inode.get(&e.inode).cloned() {
                if !consumed_old.contains(&old_path) {
                    if let Some(prior) = prev.get(&old_path) {
                        if prior.mtime_nanos == new_mtime && prior.size == e.size {
                            // Cheapest case: same inode, same mtime/size, new path. No I/O.
                            plan.changes.push(patch(prior.doc_id, e.inode, new_mtime, e.size, rel.clone()));
                            consumed_old.insert(old_path);
                            seen.insert(rel);
                            continue;
                        }
                        if cf.kind == FileKind::Text && prior.status.was_read() {
                            match read_and_hash(&e.path) {
                                Ok((bytes, hash)) => {
                                    plan.bytes_read += bytes.len() as u64;
                                    consumed_old.insert(old_path);
                                    if hash == prior.content_hash {
                                        plan.changes.push(patch(prior.doc_id, e.inode, new_mtime, e.size, rel.clone()));
                                    } else {
                                        plan.changes.push(Change::Delete(prior.doc_id));
                                        plan.changes.push(reindex(rel.clone(), cf.kind, e.inode, new_mtime, e.size, Some(bytes)));
                                    }
                                    seen.insert(rel);
                                    continue;
                                }
                                Err(err) => log::warn!("reconcile: {}: {err}", e.path.display()),
                            }
                        } else {
                            // Moved and (probably) changed, but no baseline hash to
                            // confirm either way: tombstone the old id and fall
                            // through to record this path as a fresh file.
                            plan.changes.push(Change::Delete(prior.doc_id));
                            consumed_old.insert(old_path);
                        }
                    }
                }
            }
        }

        // No usable prior state: a genuinely new file.
        plan.changes.push(reindex(rel.clone(), cf.kind, e.inode, new_mtime, e.size, None));
        seen.insert(rel);
    }

    // Anything previously known whose path we never saw, and that wasn't
    // already consumed as the source of a rename, is gone.
    for (path, state) in path_states(prev) {
        if !seen.contains(path) && !consumed_old.contains(path) {
            plan.changes.push(Change::Delete(state.doc_id));
        }
    }
    plan
}

fn patch(doc_id: DocId, inode: u64, mtime_nanos: i64, size: u64, path: String) -> Change {
    Change::Patch { doc_id, inode, mtime_nanos, size, path }
}

fn reindex(path: String, kind: FileKind, inode: u64, mtime_nanos: i64, size: u64, bytes: Option<Vec<u8>>) -> Change {
    Change::Reindex { path, kind, inode, mtime_nanos, size, bytes }
}

fn path_states(idx: &StateIndex) -> impl Iterator<Item = (&String, &FileState)> {
    idx.by_path.iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawler::FileEntry;
    use crate::store::segment::nanos_to_time;
    use std::path::PathBuf;

    fn state(entries: &[(&str, u64, i64, u64, u64, DocStatus)]) -> StateIndex {
        let mut by_path = HashMap::new();
        let mut by_inode = HashMap::new();
        for &(path, doc_id, mtime_nanos, size, inode, status) in entries {
            if inode != 0 {
                by_inode.insert(inode, path.to_string());
            }
            by_path.insert(path.to_string(), FileState { doc_id: doc_id as DocId, inode, mtime_nanos, size, content_hash: mtime_nanos as u64, status });
        }
        StateIndex { by_path, by_inode }
    }

    fn cf(root: &Path, rel: &str, inode: u64, mtime_nanos: i64, size: u64, kind: FileKind) -> CrawledFile {
        CrawledFile {
            entry: FileEntry { path: root.join(rel), inode, mtime: nanos_to_time(mtime_nanos), size },
            kind,
        }
    }

    #[test]
    fn unchanged_costs_no_io() {
        let root = PathBuf::from("/root");
        let prev = state(&[("a.txt", 1, 100, 5, 10, DocStatus::Indexed)]);
        let crawled = [cf(&root, "a.txt", 10, 100, 5, FileKind::Text)];
        let plan = reconcile(&prev, &root, &crawled);
        assert!(plan.is_empty());
        assert_eq!(plan.bytes_read, 0);
    }

    #[test]
    fn deleted_path_is_a_delete() {
        let root = PathBuf::from("/root");
        let prev = state(&[("gone.txt", 1, 100, 5, 10, DocStatus::Indexed)]);
        let plan = reconcile(&prev, &root, &[]);
        assert_eq!(plan.deletes(), 1);
        assert!(matches!(plan.changes[0], Change::Delete(1)));
    }

    #[test]
    fn new_path_is_a_reindex() {
        let root = PathBuf::from("/root");
        let prev = StateIndex::default();
        let crawled = [cf(&root, "new.txt", 5, 1, 3, FileKind::Text)];
        let plan = reconcile(&prev, &root, &crawled);
        assert_eq!(plan.reindexes(), 1);
        assert!(matches!(&plan.changes[0], Change::Reindex { path, .. } if path == "new.txt"));
    }

    #[test]
    fn metadata_touch_with_identical_content_is_a_patch_not_a_reindex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "same bytes").unwrap();
        let hash = hash_bytes(b"same bytes");
        let mut prev_state = state(&[("a.txt", 1, 1, 999, 10, DocStatus::Indexed)]);
        prev_state.by_path.get_mut("a.txt").unwrap().content_hash = hash; // real hash, size deliberately stale
        let crawled = [cf(dir.path(), "a.txt", 10, 2, 10, FileKind::Text)]; // mtime/size changed, content did not
        let plan = reconcile(&prev_state, dir.path(), &crawled);
        assert_eq!(plan.changes.len(), 1);
        assert!(matches!(&plan.changes[0], Change::Patch { doc_id: 1, .. }));
        assert!(plan.bytes_read > 0, "content had to be read to confirm the hash");
    }

    #[test]
    fn metadata_touch_with_changed_content_is_delete_plus_reindex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "new bytes").unwrap();
        let mut prev_state = state(&[("a.txt", 1, 1, 999, 10, DocStatus::Indexed)]);
        prev_state.by_path.get_mut("a.txt").unwrap().content_hash = 0xdead; // won't match
        let crawled = [cf(dir.path(), "a.txt", 10, 2, 9, FileKind::Text)];
        let plan = reconcile(&prev_state, dir.path(), &crawled);
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.reindexes(), 1);
        assert!(matches!(&plan.changes[1], Change::Reindex { bytes: Some(_), .. }), "bytes are reused from the hashing read");
    }

    #[test]
    fn rename_with_unchanged_metadata_costs_no_io() {
        let root = PathBuf::from("/root");
        let prev = state(&[("old/name.txt", 1, 100, 5, 42, DocStatus::Indexed)]);
        let crawled = [cf(&root, "new/name.txt", 42, 100, 5, FileKind::Text)]; // same inode, mtime, size
        let plan = reconcile(&prev, &root, &crawled);
        assert_eq!(plan.changes.len(), 1);
        assert!(matches!(&plan.changes[0], Change::Patch { doc_id: 1, path, .. } if path == "new/name.txt"));
        assert_eq!(plan.bytes_read, 0, "a clean rename never reads the file");
    }

    #[test]
    fn rename_with_changed_metadata_but_same_hash_is_a_patch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("new")).unwrap();
        std::fs::write(dir.path().join("new/name.txt"), "content").unwrap();
        let hash = hash_bytes(b"content");
        let mut prev = state(&[("old/name.txt", 1, 1, 999, 42, DocStatus::Indexed)]);
        prev.by_path.get_mut("old/name.txt").unwrap().content_hash = hash;
        let crawled = [cf(dir.path(), "new/name.txt", 42, 2, 7, FileKind::Text)];
        let plan = reconcile(&prev, dir.path(), &crawled);
        assert_eq!(plan.changes.len(), 1);
        assert!(matches!(&plan.changes[0], Change::Patch { doc_id: 1, path, .. } if path == "new/name.txt"));
    }

    #[test]
    fn rename_with_changed_content_is_delete_plus_reindex_at_new_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "edited").unwrap();
        let mut prev = state(&[("a.txt", 1, 1, 999, 42, DocStatus::Indexed)]);
        prev.by_path.get_mut("a.txt").unwrap().content_hash = 0xbeef; // won't match "edited"
        let crawled = [cf(dir.path(), "b.txt", 42, 2, 6, FileKind::Text)];
        let plan = reconcile(&prev, dir.path(), &crawled);
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.reindexes(), 1);
        assert!(matches!(&plan.changes[1], Change::Reindex { path, .. } if path == "b.txt"));
    }

    #[test]
    fn binary_and_too_large_metadata_change_reindexes_without_hashing() {
        let root = PathBuf::from("/root");
        let prev = state(&[("img.png", 1, 100, 5, 10, DocStatus::Binary)]);
        let crawled = [cf(&root, "img.png", 10, 200, 6, FileKind::Binary)];
        let plan = reconcile(&prev, &root, &crawled);
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.reindexes(), 1);
        assert_eq!(plan.bytes_read, 0);
    }

    #[test]
    fn no_inode_support_still_detects_content_preserving_moves_by_hash() {
        // inode 0 everywhere (e.g. a filesystem without stable ids): the
        // rename fast path is unavailable, but a same-path-metadata-changed
        // check still catches identical content when the path is unchanged.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "stable content").unwrap();
        let hash = hash_bytes(b"stable content");
        let mut prev = state(&[("a.txt", 1, 1, 999, 0, DocStatus::Indexed)]);
        prev.by_path.get_mut("a.txt").unwrap().content_hash = hash;
        let crawled = [cf(dir.path(), "a.txt", 0, 2, 14, FileKind::Text)];
        let plan = reconcile(&prev, dir.path(), &crawled);
        assert!(matches!(&plan.changes[0], Change::Patch { .. }));
    }
}
