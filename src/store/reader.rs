//! Opening a persistent index (FORMAT.md §8) and reading it.
//!
//! Overlay patches (renames and metadata-only touches, written by the
//! incremental updater) are applied here, once, so every other reader —
//! `Docs`, `IndexReader::doc`, the query engine's snippet path — sees
//! already-current paths and never has to know the overlay exists.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{index_dir, is_cleanable, overlay_name, referenced_files, Segment, MANIFEST};
use crate::error::{Error, Result};
use crate::format::manifest::Manifest;
use crate::format::overlay::OverlayView;
use crate::format::FormatError;
use crate::fs::Fs;
use crate::index::{DocId, DocMeta, IndexReader, IndexStats, SegmentReader};

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Also crc-verify every `.idx` and `.doc` body. Reads every byte of the
    /// index, so it is off by default (see FORMAT.md §2).
    pub verify: bool,
}

/// One overlay patch, materialised (the file is small; no reason to keep it mapped).
#[derive(Debug, Clone)]
struct OwnedPatch {
    doc_id: DocId,
    inode: u64,
    mtime_nanos: i64,
    size: u64,
    path: String,
}

/// A memory-mapped, multi-segment index.
pub struct Index {
    root: PathBuf,
    dir: PathBuf,
    manifest: Manifest,
    manifest_len: u64,
    segments: Vec<Segment>,
    /// Sorted by `doc_id`; empty when `manifest.state_gen == 0`.
    overlay: Vec<OwnedPatch>,
    overlay_len: u64,
    stats: IndexStats,
    n_indexed: u32,
    avg_len: f32,
    live_docs: u64,
    id_bound: DocId,
}

fn corrupt(path: &Path, reason: impl std::fmt::Display) -> Error {
    Error::Corrupt { path: path.to_path_buf(), reason: reason.to_string() }
}

/// `NotFound` might just be a race with a concurrent commit retiring the
/// file between our manifest read and this read (see [`Error::Vanished`]);
/// anything else is real corruption.
fn open_err(path: &Path, e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::Vanished(path.to_path_buf())
    } else {
        corrupt(path, e)
    }
}

/// One successful `open_once`: the manifest and its encoded length, the opened
/// segments, the overlay's patches, and the overlay's encoded length.
type OpenedIndex = (Manifest, u64, Vec<Segment>, Vec<OwnedPatch>, u64);

/// Overlay patches as plain tuples: `(doc_id, inode, mtime_nanos, size, path)`.
type OverlayParts = Vec<(DocId, u64, i64, u64, String)>;

/// Read the manifest, sweep orphans if uncontended, and open every segment
/// and the overlay it names. Bundled so the retry loop in [`open`] can redo
/// all of it — including a fresh manifest read — on [`Error::Vanished`].
fn open_once(fs: &dyn Fs, dir: &Path, opts: &OpenOptions) -> Result<Option<OpenedIndex>> {
    // Acquire the cleanup lock *before* reading the manifest, when we can.
    // Reading the manifest first and only then locking would leave a window
    // where a concurrent commit completes in between: cleanup would then
    // compute `referenced_files` from our now-stale manifest and delete a
    // brand-new segment the *current* (later) manifest legitimately
    // references. Locking first guarantees that if we get the lock, no
    // commit can start or finish until we release it, so the manifest we
    // read while holding it is authoritative for the sweep.
    //
    // If the lock is contested, skip both the lock and the ordering
    // guarantee: read the manifest anyway (a writer's changes are then
    // simply not reflected yet, which is fine) and skip cleanup entirely.
    // Any reference that turns out to be stale by the time we try to open it
    // surfaces as `Error::Vanished`, which `open`'s outer retry loop handles
    // by re-reading the manifest from scratch.
    let cleanup_lock = fs.lock(&dir.join(super::LOCK)).ok();

    let manifest_path = dir.join(MANIFEST);
    let bytes = match fs.read(&manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let manifest = match Manifest::decode(&bytes) {
        Ok(m) => m,
        Err(FormatError::Version(version)) => return Err(Error::Incompatible { path: manifest_path, version }),
        Err(e) => {
            log::warn!("{}: {e}; treating the index as absent", manifest_path.display());
            return Ok(None);
        }
    };

    match cleanup_lock {
        Some(lock) => {
            let referenced = referenced_files(&manifest);
            for p in fs.list_dir(dir)? {
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
                if is_cleanable(name) && !referenced.contains(name) {
                    match fs.remove_file(&p) {
                        Ok(()) => log::debug!("removed orphan {}", p.display()),
                        Err(e) => log::debug!("could not remove orphan {}: {e}", p.display()),
                    }
                }
            }
            drop(lock);
        }
        None => log::debug!("{}: a writer holds the lock; skipping the orphan sweep this open", dir.display()),
    }

    let mut segments = Vec::with_capacity(manifest.segments.len());
    for e in &manifest.segments {
        segments.push(Segment::open(fs, dir, e, opts.verify)?);
    }

    let (overlay, overlay_len) = if manifest.state_gen > 0 {
        let path = dir.join(overlay_name(manifest.state_gen));
        let bytes = fs.read(&path).map_err(|e| open_err(&path, e))?;
        if bytes.len() as u64 != manifest.state_len {
            return Err(corrupt(&path, format!("length {} but manifest says {}", bytes.len(), manifest.state_len)));
        }
        let view = OverlayView::parse_verified(&bytes).map_err(|e| corrupt(&path, e))?;
        if view.crc() != manifest.state_crc {
            return Err(corrupt(&path, "footer checksum differs from manifest"));
        }
        let owned = view
            .iter()
            .map(|(p, path)| OwnedPatch { doc_id: p.doc_id, inode: p.inode, mtime_nanos: p.mtime_nanos, size: p.size, path: path.to_string() })
            .collect();
        (owned, bytes.len() as u64)
    } else {
        (Vec::new(), 0)
    };

    Ok(Some((manifest, bytes.len() as u64, segments, overlay, overlay_len)))
}

/// Open `<root>/.ripindex`. `Ok(None)` when there is no usable manifest —
/// missing, or failing any envelope/crc check — in which case **nothing is
/// deleted**. A manifest of a different format version is `Error::Incompatible`.
/// Once a manifest is accepted, unreferenced segment/overlay/temp files are
/// removed and every referenced file must validate, else `Error::Corrupt`.
///
/// Retries a bounded number of times, re-reading the manifest each time, on
/// [`Error::Vanished`] — a file the manifest named was gone by the time we
/// tried to open it, which a plain single-threaded writer never produces but
/// a reader racing a concurrent commit or merge legitimately can: the file
/// really did exist under the manifest we read, and a later commit retired
/// it before we got to it. Re-reading picks up that later, self-consistent
/// manifest instead. If the same file keeps vanishing across every retry,
/// that is no longer a plausible race and surfaces as `Error::Corrupt`.
pub fn open(fs: &dyn Fs, root: &Path, opts: &OpenOptions) -> Result<Option<Index>> {
    let t = Instant::now();
    let dir = index_dir(root);

    const MAX_ATTEMPTS: u32 = 20;
    let mut last_vanished = None;
    let (manifest, manifest_len, segments, overlay, overlay_len) = 'opened: {
        for attempt in 0..MAX_ATTEMPTS {
            match open_once(fs, &dir, opts) {
                Ok(None) => return Ok(None),
                Ok(Some(v)) => break 'opened v,
                Err(Error::Vanished(p)) => {
                    log::debug!("{}: vanished on attempt {}; likely a concurrent commit, retrying", p.display(), attempt + 1);
                    last_vanished = Some(p);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
        let p = last_vanished.unwrap();
        return Err(corrupt(&p, format!("still missing after {MAX_ATTEMPTS} attempts to reconcile with a concurrent commit")));
    };

    let mut index = Index {
        root: root.to_path_buf(),
        dir,
        manifest,
        manifest_len,
        segments,
        overlay,
        overlay_len,
        stats: IndexStats::default(),
        n_indexed: 0,
        avg_len: 0.0,
        live_docs: 0,
        id_bound: 0,
    };
    index.recompute_stats();
    index.stats.open_time = t.elapsed();
    Ok(Some(index))
}

impl Index {
    fn recompute_stats(&mut self) {
        let mut n_indexed = 0u64;
        let mut total_indexed = 0u64;
        let mut total_len = 0u64;
        let mut live = 0u64;
        let mut skipped = 0u64;
        let mut tokens = 0u64;
        let mut postings = 0u64;
        let mut terms = 0u64;
        let mut disk = self.manifest_len + self.overlay_len;
        let mut heap = std::mem::size_of::<Self>()
            + self.manifest.segments.len() * std::mem::size_of::<crate::format::manifest::SegmentEntry>()
            + self.overlay.iter().map(|p| p.path.capacity() + std::mem::size_of::<OwnedPatch>()).sum::<usize>();
        let mut bound = 0u64;
        for s in &self.segments {
            let e = s.entry();
            let deleted = e.num_deleted as u64;
            n_indexed += (s.num_indexed() as u64).saturating_sub(deleted);
            total_indexed += s.num_indexed() as u64;
            total_len += s.total_len();
            live += e.num_docs as u64 - deleted;
            skipped += (e.num_docs - s.num_indexed()) as u64;
            tokens += e.num_tokens;
            postings += e.num_postings;
            terms += e.num_terms;
            disk += e.idx_len + e.doc_len + e.del_len;
            heap += s.heap_bytes();
            bound = bound.max(e.end_doc());
        }
        self.n_indexed = n_indexed as u32;
        self.avg_len = if total_indexed == 0 { 0.0 } else { total_len as f32 / total_indexed as f32 };
        self.live_docs = live;
        self.id_bound = bound.min(u32::MAX as u64) as DocId;
        let prev = std::mem::take(&mut self.stats);
        self.stats = IndexStats {
            docs_total: live as u32,
            docs_indexed: self.n_indexed,
            docs_skipped: skipped as u32,
            total_tokens: tokens,
            total_postings: postings,
            unique_terms: terms as usize,
            segments: self.segments.len() as u32,
            memory_bytes: heap,
            on_disk_bytes: disk,
            // Build-side numbers are filled in by the writer.
            crawl: prev.crawl,
            crawl_time: prev.crawl_time,
            build_time: prev.build_time,
            open_time: prev.open_time,
            rss_bytes: prev.rss_bytes,
            rss_delta_bytes: prev.rss_delta_bytes,
        };
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn segment_list(&self) -> &[Segment] {
        &self.segments
    }

    pub fn stats(&self) -> &IndexStats {
        &self.stats
    }

    pub(crate) fn stats_mut(&mut self) -> &mut IndexStats {
        &mut self.stats
    }

    /// Doc-table view for display and inspection.
    pub fn docs(&self) -> Docs<'_> {
        Docs { index: self }
    }

    /// Every file the manifest references, plus the manifest itself.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.segments.iter().flat_map(|s| s.files(&self.dir)).collect();
        if self.manifest.state_gen > 0 {
            v.push(self.dir.join(overlay_name(self.manifest.state_gen)));
        }
        v.push(self.dir.join(MANIFEST));
        v
    }

    /// Segment holding a global id, with the local id. `None` for gaps.
    pub fn locate(&self, id: DocId) -> Option<(&Segment, DocId)> {
        let i = self.segments.partition_point(|s| s.entry().end_doc() <= id as u64);
        let s = self.segments.get(i)?;
        if id < s.base() {
            return None;
        }
        Some((s, id - s.base()))
    }

    fn overlay_patch(&self, id: DocId) -> Option<&OwnedPatch> {
        self.overlay.binary_search_by_key(&id, |p| p.doc_id).ok().map(|i| &self.overlay[i])
    }

    /// Current overlay patches in ascending `doc_id` order, for the
    /// incremental writer to merge new ones into.
    pub(crate) fn overlay_patches(&self) -> impl Iterator<Item = (DocId, u64, i64, u64, &str)> {
        self.overlay.iter().map(|p| (p.doc_id, p.inode, p.mtime_nanos, p.size, p.path.as_str()))
    }

    /// Full crc + structural verification of every segment (FORMAT.md §8 step 4).
    pub fn verify(&self) -> Result<()> {
        for s in &self.segments {
            s.verify(&self.dir)?;
        }
        Ok(())
    }

    /// Consume into parts, for [`super::live::LiveIndex`] to rewrap the
    /// segments in `Arc`s without re-opening them.
    pub(crate) fn into_parts(self) -> (Manifest, Vec<Segment>, OverlayParts, PathBuf) {
        let overlay = self.overlay.into_iter().map(|p| (p.doc_id, p.inode, p.mtime_nanos, p.size, p.path)).collect();
        (self.manifest, self.segments, overlay, self.root)
    }
}

impl IndexReader for Index {
    type Segment = Segment;

    fn segments(&self) -> impl Iterator<Item = &Segment> + '_ {
        self.segments.iter()
    }

    fn indexed_count(&self) -> u32 {
        self.n_indexed
    }

    fn avg_len(&self) -> f32 {
        self.avg_len
    }

    fn doc(&self, id: DocId) -> Option<DocMeta> {
        let (s, local) = self.locate(id)?;
        let mut meta = s.doc_meta(local, &self.root)?;
        if let Some(p) = self.overlay_patch(id) {
            meta.inode = p.inode;
            meta.mtime = super::segment::nanos_to_time(p.mtime_nanos);
            meta.size = p.size;
            meta.path = self.root.clone();
            meta.path.extend(p.path.split('/').filter(|c| !c.is_empty()));
        }
        Some(meta)
    }

    fn stats(&self) -> &IndexStats {
        &self.stats
    }
}

/// The doc table as one logical, sparse table over global ids.
#[derive(Clone, Copy)]
pub struct Docs<'a> {
    index: &'a Index,
}

impl<'a> Docs<'a> {
    /// Live docs of any status (deleted docs excluded).
    pub fn len(&self) -> usize {
        self.index.live_docs as usize
    }

    pub fn is_empty(&self) -> bool {
        self.index.live_docs == 0
    }

    /// One past the highest global id; any id below may be a gap.
    pub fn id_bound(&self) -> DocId {
        self.index.id_bound
    }

    /// `N` for BM25.
    pub fn indexed_count(&self) -> u32 {
        self.index.n_indexed
    }

    pub fn avg_len(&self) -> f32 {
        self.index.avg_len
    }

    pub fn get(&self, id: DocId) -> Option<DocMeta> {
        self.index.doc(id)
    }

    /// Live docs in ascending global id, with overlay patches applied.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, DocMeta)> + 'a {
        let index = self.index;
        index.segments.iter().flat_map(move |s| (0..s.num_docs()).filter_map(move |local| {
            let id = s.base() + local;
            index.doc(id).map(|m| (id, m))
        }))
    }
}
