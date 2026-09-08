//! A long-lived index handle, for a process that keeps running across
//! updates and merges — the watcher daemon, or any embedder that wants to
//! refresh without reopening.
//!
//! **The sharp edge this exists for:** once updates and merges are real,
//! segment files a query is mid-iteration over can be retired *while the
//! query runs*. Unmapping (or on Windows, deleting) a file a `Mmap` still
//! covers is undefined behaviour — a segfault, not a `Result`. The fix is
//! reference counting: every segment lives in an `Arc`; [`IndexSnapshot`]s
//! handed to queries hold clones; [`LiveIndex::refresh`] swaps in a new
//! segment list but only *marks* superseded segments for retirement — their
//! files are removed when the **last** `Arc` drops, which is only after
//! every query that started before the swap has finished. On Windows, where
//! a mapped file can't be deleted at all, this is often "immediately"; when
//! it isn't, the segment simply outlives its retirement request until the
//! last reader goes away, and M2's orphan sweep at the next `open()` is the
//! backstop if even that races.
//!
//! **Scope note.** `refresh` re-opens (re-`mmap`s) every segment named by the
//! new manifest, even ones whose bytes haven't changed, rather than reusing
//! the existing mapping for an unchanged segment. Simpler; costs one extra
//! `mmap` + validate per segment per refresh, which is cheap next to a merge.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use crate::error::Result;
use crate::fs::Fs;
use crate::index::{DocId, DocMeta, IndexReader, IndexStats, SegmentReader};

use super::reader::{open, OpenOptions};
use super::segment::nanos_to_time;
use super::{index_dir, Segment};

/// A segment kept alive by every [`IndexSnapshot`] that references it,
/// independent of whether the current manifest still names it.
pub struct LiveSegment {
    inner: Segment,
    dir: PathBuf,
    fs: Arc<dyn Fs>,
    /// Set by `refresh` when this segment is no longer in the manifest.
    /// Checked on `Drop`, when this is the last reference.
    retire: AtomicBool,
}

impl std::ops::Deref for LiveSegment {
    type Target = Segment;
    fn deref(&self) -> &Segment {
        &self.inner
    }
}

impl SegmentReader for LiveSegment {
    type List<'a>
        = <Segment as SegmentReader>::List<'a>
    where
        Self: 'a;

    fn base(&self) -> DocId {
        self.inner.base()
    }

    fn num_docs(&self) -> u32 {
        self.inner.num_docs()
    }

    fn postings(&self, term: &str) -> Option<Self::List<'_>> {
        self.inner.postings(term)
    }

    fn doc_len(&self, local: DocId) -> Option<u32> {
        self.inner.doc_len(local)
    }
}

impl Drop for LiveSegment {
    fn drop(&mut self) {
        if self.retire.load(Ordering::Acquire) {
            for p in self.inner.files(&self.dir) {
                if let Err(e) = self.fs.remove_file(&p) {
                    log::debug!("retire {}: {e} (will be cleaned up on next open)", p.display());
                }
            }
        }
    }
}

/// Overlay patch, owned. `(doc_id, inode, mtime_nanos, size, root-relative path)`.
type OverlayPatch = (DocId, u64, i64, u64, String);

struct Snapshot0 {
    generation: u64,
    segments: Vec<Arc<LiveSegment>>,
    overlay: Vec<OverlayPatch>,
    n_indexed: u32,
    avg_len: f32,
    stats: IndexStats,
    root: PathBuf,
}

/// A long-lived, refreshable handle on a persistent index.
pub struct LiveIndex {
    fs: Arc<dyn Fs>,
    root: PathBuf,
    dir: PathBuf,
    current: RwLock<Arc<Snapshot0>>,
}

/// Wrap a freshly opened `Index`'s parts into a snapshot's worth of `Arc<LiveSegment>`s.
fn wrap(index: super::Index, fs: &Arc<dyn Fs>, dir: &Path) -> Snapshot0 {
    let stats = index.stats().clone();
    let n_indexed = index.indexed_count();
    let avg_len = index.avg_len();
    let (manifest, segments, overlay, root) = index.into_parts();
    let segments = segments
        .into_iter()
        .map(|inner| Arc::new(LiveSegment { inner, dir: dir.to_path_buf(), fs: fs.clone(), retire: AtomicBool::new(false) }))
        .collect();
    Snapshot0 { generation: manifest.generation, segments, overlay, n_indexed, avg_len, stats, root }
}

impl LiveIndex {
    /// Open `root`'s index. `Ok(None)` if there is none yet.
    pub fn open(fs: Arc<dyn Fs>, root: &Path) -> Result<Option<LiveIndex>> {
        let Some(index) = open(&*fs, root, &OpenOptions::default())? else { return Ok(None) };
        let dir = index_dir(root);
        let snapshot0 = wrap(index, &fs, &dir);
        Ok(Some(LiveIndex { fs, root: root.to_path_buf(), dir, current: RwLock::new(Arc::new(snapshot0)) }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn generation(&self) -> u64 {
        self.current.read().unwrap().generation
    }

    /// A cheap (`O(1)`, one `Arc` clone), point-in-time, thread-safe view.
    /// Every segment it names stays mapped until this snapshot (and every
    /// clone of it) is dropped, no matter what later commits do.
    pub fn snapshot(&self) -> IndexSnapshot {
        IndexSnapshot(self.current.read().unwrap().clone())
    }

    /// Re-read the manifest; if the generation advanced, swap in a fresh
    /// snapshot and mark every segment the new manifest no longer names for
    /// retirement (their files are removed once the last reference —
    /// possibly a snapshot already handed to a running query — drops).
    /// Returns whether anything changed.
    pub fn refresh(&self) -> Result<bool> {
        let Some(fresh) = open(&*self.fs, &self.root, &OpenOptions::default())? else {
            return Ok(false); // index vanished; keep serving the last good snapshot
        };
        let old = self.current.read().unwrap().clone();
        if fresh.manifest().generation == old.generation {
            return Ok(false);
        }
        let new_ids: HashSet<u32> = fresh.manifest().segments.iter().map(|s| s.segment_id).collect();
        for seg in &old.segments {
            if !new_ids.contains(&seg.inner.entry().segment_id) {
                seg.retire.store(true, Ordering::Release);
            }
        }
        let snapshot0 = wrap(fresh, &self.fs, &self.dir);
        *self.current.write().unwrap() = Arc::new(snapshot0);
        Ok(true)
    }
}

/// A point-in-time, `Clone`-cheap view of a [`LiveIndex`]. Implements
/// [`IndexReader`], so `query::search` runs against it exactly as it would
/// against a plain [`super::Index`].
#[derive(Clone)]
pub struct IndexSnapshot(Arc<Snapshot0>);

impl IndexSnapshot {
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
}

impl IndexReader for IndexSnapshot {
    type Segment = LiveSegment;

    fn segments(&self) -> impl Iterator<Item = &LiveSegment> + '_ {
        self.0.segments.iter().map(|a| a.as_ref())
    }

    fn indexed_count(&self) -> u32 {
        self.0.n_indexed
    }

    fn avg_len(&self) -> f32 {
        self.0.avg_len
    }

    fn doc(&self, id: DocId) -> Option<DocMeta> {
        let i = self.0.segments.partition_point(|s| s.inner.entry().end_doc() <= id as u64);
        let seg = self.0.segments.get(i)?;
        if id < seg.inner.base() {
            return None;
        }
        let mut meta = seg.inner.doc_meta(id - seg.inner.base(), &self.0.root)?;
        if let Ok(pos) = self.0.overlay.binary_search_by_key(&id, |p| p.0) {
            let (_, inode, mtime_nanos, size, path) = &self.0.overlay[pos];
            meta.inode = *inode;
            meta.mtime = nanos_to_time(*mtime_nanos);
            meta.size = *size;
            meta.path = self.0.root.clone();
            meta.path.extend(path.split('/').filter(|c| !c.is_empty()));
        }
        Some(meta)
    }

    fn stats(&self) -> &IndexStats {
        &self.0.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawler::CrawlConfig;
    use crate::fs::RealFs;
    use crate::index::builder::BuildConfig;
    use crate::query::{search, Query, SearchOptions};
    use crate::store::{merge_index, update_index, MergePolicy, NoProgress};
    use std::fs;
    use std::sync::Barrier;
    use std::thread;

    fn cfg() -> CrawlConfig {
        CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
    }

    fn small_build() -> BuildConfig {
        BuildConfig { shard_size: 1, docs_per_segment: 2, ..BuildConfig::default() }
    }

    fn hits(snap: &IndexSnapshot, q: &str) -> usize {
        search(snap, &Query::parse(q).unwrap(), &SearchOptions { limit: 10_000, ..Default::default() }).total_matches
    }

    #[test]
    fn refresh_picks_up_new_generations_and_old_snapshot_keeps_working() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha beta").unwrap();
        update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();

        let live = LiveIndex::open(Arc::new(RealFs), dir.path()).unwrap().unwrap();
        let before = live.snapshot();
        assert_eq!(hits(&before, "alpha"), 1);

        fs::write(dir.path().join("b.txt"), "gamma delta").unwrap();
        update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();

        // The old snapshot is frozen: it must not see the new file, and must
        // not crash even though its segment files may already be gone from
        // the directory (Unix: unlinked at commit but still mapped; Windows:
        // never removed while mapped).
        assert_eq!(hits(&before, "alpha"), 1);
        assert_eq!(hits(&before, "gamma"), 0);

        assert!(live.refresh().unwrap());
        let after = live.snapshot();
        assert_eq!(hits(&after, "alpha"), 1);
        assert_eq!(hits(&after, "gamma"), 1);
        assert!(!live.refresh().unwrap(), "no further change");
    }

    /// The property the whole module exists for: a query holding a snapshot
    /// must keep working, with no crash, no matter how many updates and
    /// merges run concurrently and retire the segments underneath it.
    #[test]
    fn merge_and_update_never_invalidate_an_in_flight_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            fs::write(dir.path().join(format!("f{i}.txt")), format!("alpha beta{i} gamma")).unwrap();
        }
        update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();

        let live = Arc::new(LiveIndex::open(Arc::new(RealFs), dir.path()).unwrap().unwrap());
        let policy = MergePolicy { tier_size: 2, max_merge: 4, ..Default::default() };
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start = Arc::new(Barrier::new(2));

        let writer = {
            let root = dir.path().to_path_buf();
            let stop = stop.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                for i in 0..15 {
                    fs::write(root.join(format!("f{}.txt", 100 + i)), format!("alpha extra{i} gamma")).unwrap();
                    update_index(&RealFs, &root, &cfg(), &small_build(), &NoProgress).unwrap();
                    let _ = merge_index(&RealFs, &root, &policy);
                }
                stop.store(true, std::sync::atomic::Ordering::Release);
            })
        };

        let reader = {
            let live = live.clone();
            let stop = stop.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                let mut held = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let snap = live.snapshot();
                    assert!(hits(&snap, "alpha") >= 12, "every generation must still contain the original docs");
                    // Deliberately hold on to some old snapshots across
                    // refreshes, so their segments outlive the manifest.
                    held.push(snap);
                    if held.len() > 5 {
                        held.remove(0);
                    }
                    let _ = live.refresh();
                }
                for snap in &held {
                    assert!(hits(snap, "alpha") >= 12);
                }
            })
        };

        writer.join().unwrap();
        reader.join().unwrap();
        live.refresh().unwrap();
        let final_snap = live.snapshot();
        assert_eq!(hits(&final_snap, "alpha"), 27);
    }
}

