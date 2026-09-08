//! Building segments and committing manifests (FORMAT.md §7).
//!
//! Build pipeline: crawl → shards in parallel → fold each shard into a
//! segment accumulator (global doc ids, one `MemTermStore`) → when the
//! accumulator reaches `docs_per_segment` docs **or** `segment_bytes` of
//! accounted postings, serialise it (temp + sync + rename) and start a new
//! one → write the manifest → commit. Peak build memory is one accumulator
//! plus one batch of shards.

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use rayon::prelude::*;

use super::reader::{open, Index, OpenOptions};
use super::{del_name, doc_name, idx_name, index_dir, tmp_name, MANIFEST, MANIFEST_TMP, LOCK};
use crate::crawler::{self, CrawlConfig, CrawledFile};
use crate::error::{Error, Result};
use crate::format::del::Bitmap;
use crate::format::doc::{self, DocIn, DocKind, DocTableView};
use crate::format::envelope;
use crate::format::idx::{self, PostingSource};
use crate::format::manifest::{Manifest, SegmentEntry};
use crate::fs::{Fs, RealFs};
use crate::index::builder::{index_shard, BuildConfig, Shard};
use crate::index::memory::{MemPostingList, MemTermStore};
use crate::index::postings::PostingList;
use crate::index::{rss_bytes, DocId, DocMeta, DocStatus, TermStore};

/// Progress callbacks for long builds. All methods are optional.
pub trait BuildProgress: Sync {
    /// Called periodically during the crawl with files seen so far.
    fn crawling(&self, _files_seen: u64) {}
    /// Crawl finished with this many indexable files.
    fn crawled(&self, _indexable: u64) {}
    /// After each batch: docs done, docs total, segments flushed so far.
    fn indexed(&self, _docs: u64, _total: u64, _segments: u32) {}
}

pub struct NoProgress;
impl BuildProgress for NoProgress {}

/// Crawl `root` and build a fresh persistent index with the real file
/// system and no progress output. Signature kept from M1.
pub fn build_from_dir(root: &Path, crawl: &CrawlConfig, build: &BuildConfig) -> Result<Index> {
    build_index(&RealFs, root, crawl, build, &NoProgress)
}

/// Crawl `root`, build segments, and commit a manifest that replaces any
/// previous index under `root/.ripindex`. Returns the reopened index.
pub fn build_index(
    fs: &dyn Fs,
    root: &Path,
    crawl_cfg: &CrawlConfig,
    build_cfg: &BuildConfig,
    progress: &dyn BuildProgress,
) -> Result<Index> {
    if !root.is_dir() {
        return Err(Error::NotADirectory(root.to_path_buf()));
    }
    let rss_before = rss_bytes();
    let started = Instant::now();
    let dir = index_dir(root);
    fs.create_dir_all(&dir)?;
    let lock = take_lock(fs, &dir)?;

    // What to retire after the commit. Drop the reader before committing so
    // no mapping of ours blocks the removal (Windows).
    let (generation, next_segment_id, retired) = match open(fs, root, &OpenOptions::default())? {
        Some(prev) => (prev.manifest().generation, prev.manifest().next_segment_id, prev.files()),
        None => (0, 0, Vec::new()),
    };
    let retired: Vec<PathBuf> = retired.into_iter().filter(|p| p.file_name().is_some_and(|n| n != MANIFEST)).collect();

    let t = Instant::now();
    let (files, crawl_stats) = crawler::crawl_all_with_progress(root, crawl_cfg, &|n| progress.crawling(n))?;
    let crawl_time = t.elapsed();
    progress.crawled(files.len() as u64);

    let mut new_files: Vec<PathBuf> = Vec::new();
    let result = build_segments(fs, root, &dir, &files, 0, build_cfg, next_segment_id, progress, &mut new_files)
        .and_then(|(entries, next_id)| {
            let manifest = Manifest {
                generation: generation + 1,
                next_segment_id: next_id,
                committed_unix_nanos: super::segment::time_to_nanos(SystemTime::now()),
                state_gen: 0,
                state_crc: 0,
                state_len: 0,
                segments: entries,
            };
            commit(fs, &dir, &manifest, &retired)
        });
    if let Err(e) = result {
        // `commit` only fails before the rename, so nothing we created is
        // referenced: tidy what we can. Anything left is removed on the next open.
        for p in &new_files {
            let _ = fs.remove_file(p);
        }
        return Err(e);
    }
    drop(lock);

    let mut index = open(fs, root, &OpenOptions::default())?
        .ok_or_else(|| Error::Corrupt { path: dir.join(MANIFEST), reason: "manifest unreadable right after commit".into() })?;
    let stats = index.stats_mut();
    stats.crawl = crawl_stats;
    stats.crawl_time = crawl_time;
    stats.build_time = started.elapsed().saturating_sub(crawl_time);
    stats.rss_bytes = rss_bytes();
    stats.rss_delta_bytes = match (rss_before, stats.rss_bytes) {
        (Some(before), Some(after)) => Some(after as i64 - before as i64),
        _ => None,
    };
    Ok(index)
}

/// Acquire the writer lock, retrying briefly on contention. `open()` takes
/// this same lock non-blockingly (and gives up at once) around its orphan
/// sweep, so an ordinary reader can hold it for at most the time it takes to
/// list and unlink a few files — a bounded retry here means that never
/// surfaces as a spurious `Error::Locked` to a writer that merely started a
/// few milliseconds after a concurrent reader. A lock held by another
/// *writer* is a different matter and will still fail after the deadline.
pub(crate) fn take_lock<'f>(fs: &'f dyn Fs, dir: &Path) -> Result<Box<dyn crate::fs::FsLock + 'f>> {
    let deadline = Instant::now() + std::time::Duration::from_millis(500);
    loop {
        match fs.lock(&dir.join(LOCK)) {
            Ok(lock) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(Error::Locked(dir.to_path_buf()));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// In-progress segment: global doc ids, one merged term store.
pub(crate) struct SegmentAcc {
    base: DocId,
    /// Local order; status/len filled from shards.
    docs: Vec<DocMeta>,
    store: MemTermStore,
    tokens: u64,
}

impl SegmentAcc {
    pub(crate) fn new(base: DocId) -> Self {
        Self { base, docs: Vec::new(), store: MemTermStore::default(), tokens: 0 }
    }

    pub(crate) fn absorb(&mut self, shard: Shard, entries: &[CrawledFile]) {
        debug_assert_eq!(shard.first_doc, self.base + self.docs.len() as DocId);
        self.docs.extend(entries.iter().map(|cf| DocMeta::from_entry(cf.entry.clone())));
        for (doc, len, hash) in shard.doc_lens {
            let m = &mut self.docs[(doc - self.base) as usize];
            m.status = DocStatus::Indexed;
            m.len = len;
            m.content_hash = hash;
        }
        for (doc, hash) in shard.skipped {
            if let Some(h) = hash {
                self.docs[(doc - self.base) as usize].content_hash = h;
            }
        }
        for (doc, status) in shard.non_text {
            self.docs[(doc - self.base) as usize].status = status;
        }
        self.tokens += shard.tokens;
        self.store.merge_shard(shard.terms);
    }

    pub(crate) fn over_budget(&self, cfg: &BuildConfig) -> bool {
        self.docs.len() >= cfg.docs_per_segment.max(1) || self.store.memory_bytes() >= cfg.segment_bytes
    }
}

/// Map + reduce + flush. Returns the segment entries (ascending base) and the
/// next free segment id. Every file created is pushed to `new_files` first,
/// so a failing caller can tidy up.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_segments(
    fs: &dyn Fs,
    root: &Path,
    dir: &Path,
    files: &[CrawledFile],
    base_id: DocId,
    cfg: &BuildConfig,
    mut next_id: u32,
    progress: &dyn BuildProgress,
    new_files: &mut Vec<PathBuf>,
) -> Result<(Vec<SegmentEntry>, u32)> {
    let shard_size = cfg.shard_size.max(1);
    let batch = shard_size * rayon::current_num_threads().max(1);
    let mut entries = Vec::new();
    let mut acc = SegmentAcc::new(base_id);
    let mut done = 0u64;

    for (b, files_batch) in files.chunks(batch).enumerate() {
        let first = base_id + (b * batch) as DocId;
        let shards: Vec<Shard> = files_batch
            .par_chunks(shard_size)
            .enumerate()
            .map(|(i, chunk)| index_shard(first + (i * shard_size) as DocId, chunk))
            .collect();
        for (i, shard) in shards.into_iter().enumerate() {
            let chunk = &files_batch[i * shard_size..((i + 1) * shard_size).min(files_batch.len())];
            acc.absorb(shard, chunk);
            if acc.over_budget(cfg) {
                let next_base = acc.base + acc.docs.len() as DocId;
                let full = std::mem::replace(&mut acc, SegmentAcc::new(next_base));
                entries.push(flush_segment(fs, root, dir, next_id, full, new_files)?);
                next_id += 1;
            }
        }
        done += files_batch.len() as u64;
        progress.indexed(done, files.len() as u64, entries.len() as u32);
    }
    if !acc.docs.is_empty() {
        entries.push(flush_segment(fs, root, dir, next_id, acc, new_files)?);
        next_id += 1;
    }
    Ok((entries, next_id))
}

/// Posting list re-based to segment-local ids for encoding.
struct Rebased<'a> {
    list: &'a MemPostingList,
    base: DocId,
}

impl PostingSource for Rebased<'_> {
    fn doc_freq(&self) -> u32 {
        self.list.doc_freq()
    }
    fn encode(&self, out: &mut Vec<u8>, scratch: &mut Vec<u8>) -> crate::format::Result<u64> {
        let base = self.base;
        idx::encode_postings(self.list.as_slice().iter().map(|p| (p.doc - base, p.positions.as_slice())), out, scratch)
    }
}

/// Root-relative, `/`-separated. Lossy for non-Unicode components.
pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Write `.idx` then `.doc` for one segment: temp, sync, close, rename each.
pub(crate) fn flush_segment(
    fs: &dyn Fs,
    root: &Path,
    dir: &Path,
    segment_id: u32,
    acc: SegmentAcc,
    new_files: &mut Vec<PathBuf>,
) -> Result<SegmentEntry> {
    let num_docs = acc.docs.len() as u32;

    // .idx
    let mut terms: Vec<(&str, Rebased)> = acc.store.iter().map(|(t, l)| (t, Rebased { list: l, base: acc.base })).collect();
    terms.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let idx_final = dir.join(idx_name(segment_id));
    let idx_tmp = dir.join(tmp_name(&idx_name(segment_id)));
    new_files.push(idx_tmp.clone());
    let summary = {
        let file = fs.create(&idx_tmp)?;
        let (w, summary) = idx::write(BufWriter::with_capacity(256 << 10, file), &terms)?;
        let mut file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        summary
    };
    drop(terms);
    new_files.push(idx_final.clone());
    fs.rename(&idx_tmp, &idx_final)?;

    // .doc
    let rel: Vec<String> = acc.docs.iter().map(|m| relative_path(root, &m.path)).collect();
    let bytes = doc::encode(acc.docs.iter().zip(&rel).map(|(m, path)| DocIn {
        path,
        inode: m.inode,
        mtime_nanos: super::segment::time_to_nanos(m.mtime),
        size: m.size,
        len: m.len,
        kind: DocKind::from(m.status),
        content_hash: m.content_hash,
    }))?;
    let doc_crc = DocTableView::parse(&bytes)?.crc();
    let doc_final = dir.join(doc_name(segment_id));
    let doc_tmp = dir.join(tmp_name(&doc_name(segment_id)));
    new_files.push(doc_tmp.clone());
    write_whole(fs, &doc_tmp, &bytes)?;
    new_files.push(doc_final.clone());
    fs.rename(&doc_tmp, &doc_final)?;

    Ok(SegmentEntry {
        segment_id,
        del_gen: 0,
        base_doc: acc.base,
        num_docs,
        num_deleted: 0,
        idx_crc: summary.crc,
        doc_crc,
        del_crc: 0,
        idx_len: summary.len,
        doc_len: bytes.len() as u64,
        del_len: 0,
        num_tokens: summary.num_tokens,
        num_postings: summary.num_postings,
        num_terms: summary.num_terms,
    })
}

/// create → write → sync_all → close.
pub(crate) fn write_whole(fs: &dyn Fs, path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs.create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Steps 3–7 of the protocol. Segment files must already be renamed into place.
///
/// Returns `Err` only for failures **before** the manifest rename. Once the
/// rename has happened the new state is committed and nothing may undo it:
/// a failed directory sync afterwards only means the commit might not
/// survive a power loss (in which case the old manifest — a complete state —
/// reappears), and a failed retirement is cleaned up by the next open. Both
/// are logged and the commit reports success. Callers therefore know that an
/// `Err` means "nothing changed" and may remove the files they created.
pub(crate) fn commit(fs: &dyn Fs, dir: &Path, manifest: &Manifest, retire: &[PathBuf]) -> Result<()> {
    fs.sync_dir(dir)?;
    let tmp = dir.join(MANIFEST_TMP);
    write_whole(fs, &tmp, &manifest.encode())?;
    fs.rename(&tmp, &dir.join(MANIFEST))?; // ← the commit point
    if let Err(e) = fs.sync_dir(dir) {
        log::warn!(
            "{}: directory sync after commit failed: {e}; the commit is complete but may not be durable across power loss",
            dir.display()
        );
    }
    for p in retire {
        if let Err(e) = fs.remove_file(p) {
            log::debug!("retire {}: {e} (will be removed on next open)", p.display());
        }
    }
    Ok(())
}

/// Mark global ids deleted: a new `.del` generation per affected segment and a
/// new manifest, through the same commit protocol. Ids that are gaps or
/// already deleted are ignored. Returns the reopened index.
pub fn delete_docs(fs: &dyn Fs, index: &Index, ids: &[DocId]) -> Result<Index> {
    let dir = index.dir().to_path_buf();
    let lock = take_lock(fs, &dir)?;

    let mut by_segment: BTreeMap<usize, Vec<DocId>> = BTreeMap::new();
    for &id in ids {
        if let Some((seg, local)) = index.locate(id) {
            let i = index.segment_list().iter().position(|s| std::ptr::eq(s, seg)).unwrap();
            by_segment.entry(i).or_default().push(local);
        }
    }

    let mut manifest = index.manifest().clone();
    let mut new_files = Vec::new();
    let mut retired = Vec::new();
    let mut any_change = false;
    let result = (|| -> Result<()> {
        for (i, locals) in by_segment {
            let seg = &index.segment_list()[i];
            let entry = &mut manifest.segments[i];
            let mut bitmap = seg.deletions().cloned().unwrap_or_else(|| Bitmap::new(entry.num_docs));
            let mut changed = false;
            for l in locals {
                changed |= bitmap.set(l);
            }
            if !changed {
                continue;
            }
            any_change = true;
            let gen = entry.del_gen + 1;
            let bytes = bitmap.encode();
            let final_path = dir.join(del_name(entry.segment_id, gen));
            let tmp = dir.join(tmp_name(&del_name(entry.segment_id, gen)));
            new_files.push(tmp.clone());
            write_whole(fs, &tmp, &bytes)?;
            new_files.push(final_path.clone());
            fs.rename(&tmp, &final_path)?;
            if entry.del_gen > 0 {
                retired.push(dir.join(del_name(entry.segment_id, entry.del_gen)));
            }
            entry.del_gen = gen;
            entry.del_len = bytes.len() as u64;
            entry.del_crc = envelope::parse(envelope::Kind::Del, &bytes)?.crc;
            entry.num_deleted = bitmap.count();
        }
        if !any_change {
            return Ok(()); // nothing to commit: no new generation
        }
        manifest.generation += 1;
        manifest.committed_unix_nanos = super::segment::time_to_nanos(SystemTime::now());
        commit(fs, &dir, &manifest, &retired)
    })();
    if let Err(e) = result {
        for p in &new_files {
            let _ = fs.remove_file(p);
        }
        return Err(e);
    }
    drop(lock);
    open(fs, index.root(), &OpenOptions::default())?
        .ok_or_else(|| Error::Corrupt { path: dir.join(MANIFEST), reason: "manifest unreadable right after commit".into() })
}

/// Test support: a persistent index over in-memory texts in a temp dir.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::index::{IndexReader, IndexStats};

    /// Keeps its temp dir alive; derefs to the index.
    pub struct TestIndex {
        pub dir: tempfile::TempDir,
        pub index: Index,
    }

    impl std::ops::Deref for TestIndex {
        type Target = Index;
        fn deref(&self) -> &Index {
            &self.index
        }
    }

    impl IndexReader for TestIndex {
        type Segment = super::super::Segment;
        fn segments(&self) -> impl Iterator<Item = &Self::Segment> + '_ {
            self.index.segments()
        }
        fn indexed_count(&self) -> u32 {
            self.index.indexed_count()
        }
        fn avg_len(&self) -> f32 {
            self.index.avg_len()
        }
        fn doc(&self, id: DocId) -> Option<DocMeta> {
            self.index.doc(id)
        }
        fn stats(&self) -> &IndexStats {
            self.index.stats()
        }
    }

    impl TestIndex {
        /// Delete global ids and reopen.
        pub fn with_deleted(self, ids: &[DocId]) -> TestIndex {
            let index = delete_docs(&RealFs, &self.index, ids).unwrap();
            TestIndex { dir: self.dir, index }
        }
    }

    /// Doc `i` is `doc{i:03}.txt` with global id `i`. Two docs per segment so
    /// the multi-segment path is always exercised.
    pub fn index_from_texts(texts: &[&str]) -> TestIndex {
        index_from_texts_with(texts, 2)
    }

    pub fn index_from_texts_with(texts: &[&str], docs_per_segment: usize) -> TestIndex {
        let dir = tempfile::tempdir().unwrap();
        for (i, t) in texts.iter().enumerate() {
            std::fs::write(dir.path().join(format!("doc{i:03}.txt")), t).unwrap();
        }
        let crawl = CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() };
        // One file per shard so the budget check (per shard) can split tiny corpora.
        let build = BuildConfig { docs_per_segment, shard_size: 1, ..BuildConfig::default() };
        let index = build_index(&RealFs, dir.path(), &crawl, &build, &NoProgress).unwrap();
        assert_eq!(index.docs().len(), texts.len());
        TestIndex { dir, index }
    }
}
