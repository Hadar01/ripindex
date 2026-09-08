//! Merge policy and execution.
//!
//! [`plan_merges`] is pure — segment sizes and deletion fractions in, groups
//! of segment ids to merge out — so the policy is unit-testable without
//! touching disk.
//!
//! Merging is split into two phases so it can run **unlocked**: [`prepare_merge`]
//! decodes every surviving doc and every posting of one group, remaps ids,
//! and writes the new segment's `.idx`/`.doc` to temp files — no lock, and
//! safe to run concurrently with other writers because it only *reads*
//! already-committed segments (immutable once written) and writes files
//! nobody else references yet. [`commit_merges`] takes the writer lock,
//! re-reads the current manifest, **carries forward any deletions that
//! landed on a source segment since it was snapshotted** (see below), and
//! commits — through the ordinary manifest protocol, so a merge is just
//! another commit and the M2 crash-safety machinery covers it unchanged.
//! [`merge_index`] is the synchronous convenience that does both phases
//! back-to-back for every group `plan_merges` calls for, in one commit.
//!
//! **Carrying forward deletions.** `prepare_merge` never includes a doc that
//! was already tombstoned in its source segment at snapshot time — those
//! never enter `old_to_new`. So at commit time, for every doc that *did*
//! survive into the merge (i.e. every entry in `old_to_new`), checking
//! whether it is deleted in the source segment's **current** bitmap is
//! exactly the "was this deleted after the snapshot" test — no need to keep
//! the old bitmap around for a diff. Anything caught this way is translated
//! through `old_to_new` into a fresh `.del` for the merged segment, so a
//! merge can never resurrect a doc that was deleted while it was running.
//!
//! **Scope note.** `prepare_merge` materialises the merge group's postings
//! and doc table in memory (bounded by the group — a handful of segments
//! each under `max_segment_bytes` — not the whole index) rather than
//! streaming a lazy k-way merge of the segments' `TermIter`s directly to
//! disk. That streaming version is the natural follow-up; this one is
//! simpler and correct, and ships first.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::format::del::Bitmap;
use crate::format::doc::{self, DocIn, DocKind, DocTableView};
use crate::format::envelope;
use crate::format::idx::{self, IdxSummary};
use crate::format::manifest::SegmentEntry;
use crate::fs::Fs;
use crate::index::memory::{MemPostingList, MemTermStore};
use crate::index::{DocId, DocMeta, IndexReader, PostingCursor, PostingList, SegmentReader};

use super::reader::{open, Index, OpenOptions};
use super::segment::time_to_nanos;
use super::writer::{commit, relative_path, take_lock, write_whole};
use super::{del_name, doc_name, idx_name, index_dir, segment_files, tmp_name, MANIFEST};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergePolicy {
    /// Merge a size tier once it has at least this many segments. Default 4.
    pub tier_size: usize,
    /// Never merge more than this many segments in one group. Default 10.
    pub max_merge: usize,
    /// Segments at or above this size (all files) are never merged — the
    /// cost outgrows the query-time benefit. Default 512 MiB.
    pub max_segment_bytes: u64,
    /// A segment more than this tombstoned is rewritten on its own,
    /// regardless of tier. Default 0.3.
    pub del_fraction_threshold: f32,
}

impl Default for MergePolicy {
    fn default() -> Self {
        Self { tier_size: 4, max_merge: 10, max_segment_bytes: 512 << 20, del_fraction_threshold: 0.3 }
    }
}

/// Decide what to merge, from segment sizes and deletion fractions alone.
/// Each returned group is a list of segment ids to merge into one; a
/// singleton group is a deletion-driven rewrite, not a size merge.
pub fn plan_merges(segments: &[SegmentEntry], policy: &MergePolicy) -> Vec<Vec<u32>> {
    let mut groups = Vec::new();
    let mut used: HashSet<u32> = HashSet::new();

    // 1. Deletion-driven rewrites: independent of tier, one at a time.
    for s in segments {
        if s.num_docs > 0 && s.deleted_fraction() > policy.del_fraction_threshold && s.total_bytes() <= policy.max_segment_bytes {
            groups.push(vec![s.segment_id]);
            used.insert(s.segment_id);
        }
    }

    // 2. Size-tiered merges over what's left. Tier = floor(log2(live bytes)),
    // live bytes floored at 1 MiB so many tiny segments still pool together.
    let mut tiers: BTreeMap<i32, Vec<u32>> = BTreeMap::new();
    for s in segments {
        if used.contains(&s.segment_id) || s.total_bytes() > policy.max_segment_bytes {
            continue;
        }
        let live_fraction = if s.num_docs == 0 { 0.0 } else { s.live_docs() as f64 / s.num_docs as f64 };
        let live_bytes = ((s.total_bytes() as f64) * live_fraction).max((1u64 << 20) as f64);
        let tier = live_bytes.log2().floor() as i32;
        tiers.entry(tier).or_default().push(s.segment_id);
    }
    for (_, mut ids) in tiers {
        if ids.len() < policy.tier_size {
            continue;
        }
        ids.sort_unstable();
        for chunk in ids.chunks(policy.max_merge.max(2)) {
            if chunk.len() >= 2 {
                groups.push(chunk.to_vec());
            }
        }
    }
    groups
}

fn corrupt(path: &Path, reason: impl std::fmt::Display) -> Error {
    Error::Corrupt { path: path.to_path_buf(), reason: reason.to_string() }
}

/// The result of the unlocked read/decode/write phase, ready to commit.
/// Holds no lock and references no other in-memory state — it can cross a
/// thread boundary (e.g. from a background merge task back to the actor that
/// commits it) freely.
pub struct PreparedMerge {
    source_ids: Vec<u32>,
    /// Each source's `del_gen` *at snapshot time* — commit compares this
    /// against the source's current `del_gen` to know whether a
    /// carry-forward diff is even needed.
    snapshot_del_gen: HashMap<u32, u32>,
    /// `(source segment_id, source local id) -> local id in the merged segment`.
    /// Never contains a doc that was already deleted at snapshot time.
    old_to_new: HashMap<(u32, DocId), DocId>,
    num_docs: u32,
    idx_tmp: PathBuf,
    doc_tmp: PathBuf,
    idx_summary: IdxSummary,
    doc_crc: u32,
    doc_len: u64,
}

impl PreparedMerge {
    pub fn source_ids(&self) -> &[u32] {
        &self.source_ids
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Discard without committing: remove the temp files it wrote.
    pub fn discard(self, fs: &dyn Fs) {
        let _ = fs.remove_file(&self.idx_tmp);
        let _ = fs.remove_file(&self.doc_tmp);
    }
}

/// Read-only phase: decode every surviving doc and posting of `segment_ids`
/// (as found in `index`, a snapshot that may be arbitrarily older than the
/// index by the time [`commit_merges`] runs) and write the merged segment's
/// `.idx`/`.doc` to temp files. Takes no lock; safe to run against a
/// snapshot while other writers commit against the live index concurrently.
pub fn prepare_merge(fs: &dyn Fs, root: &Path, dir: &Path, index: &Index, segment_ids: &[u32]) -> Result<PreparedMerge> {
    let segs: Vec<&super::Segment> = segment_ids
        .iter()
        .map(|&id| {
            index
                .segment_list()
                .iter()
                .find(|s| s.entry().segment_id == id)
                .unwrap_or_else(|| panic!("segment {id} named in a merge plan not found in the index"))
        })
        .collect();
    let snapshot_del_gen = segs.iter().map(|s| (s.entry().segment_id, s.entry().del_gen)).collect();

    // Surviving docs, in (segment, local) order; that order becomes the new
    // local id `0..metas.len()`, so it must be assigned before any postings
    // are remapped. Keyed by (source segment, source local) rather than the
    // old global id so `commit_merges` never needs to recompute a `base_doc`
    // that may itself be stale by the time it runs.
    let mut old_to_new: HashMap<(u32, DocId), DocId> = HashMap::new();
    let mut metas: Vec<(String, DocMeta)> = Vec::new();
    for seg in &segs {
        let seg_id = seg.entry().segment_id;
        for local in 0..seg.num_docs() {
            if seg.is_deleted(local) {
                continue;
            }
            let global = seg.base() + local;
            if let Some(meta) = index.doc(global) {
                old_to_new.insert((seg_id, local), metas.len() as DocId);
                let rel = relative_path(root, &meta.path);
                metas.push((rel, meta));
            }
        }
    }

    let mut terms: HashMap<String, MemPostingList> = HashMap::new();
    let mut num_tokens = 0u64;
    for seg in &segs {
        let seg_id = seg.entry().segment_id;
        let view = seg.idx();
        let mut it = view.terms();
        loop {
            let next = it.try_next().map_err(|e| corrupt(&dir.join(idx_name(seg_id)), e))?;
            let Some((term, _df, list)) = next else { break };
            let mut cur = list.cursor();
            while let Some(local) = cur.doc() {
                if let Some(&new_id) = old_to_new.get(&(seg_id, local)) {
                    let positions = cur.positions();
                    num_tokens += positions.len() as u64;
                    let entry = terms.entry(term.clone()).or_default();
                    for &pos in positions {
                        entry.push(new_id, pos);
                    }
                }
                cur.advance();
            }
        }
    }
    let mut store = MemTermStore::default();
    store.merge_shard(terms);

    // Staged (not under `dir` directly — see `staging_dir`'s docs) so the
    // orphan sweep in a concurrent `open()` never mistakes an in-progress
    // prepare's temp files for leftover garbage, however long prepare takes.
    let staging = super::staging_dir(root);
    fs.create_dir_all(&staging)?;
    let tag = format!("merge-{:x}", std::process::id() as u64 ^ (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)));
    let idx_tmp = staging.join(format!("{tag}.idx.tmp"));
    let doc_tmp = staging.join(format!("{tag}.doc.tmp"));

    let mut term_list: Vec<(&str, &MemPostingList)> = store.iter().collect();
    term_list.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let idx_summary = {
        let file = fs.create(&idx_tmp)?;
        let (w, summary) = idx::write(BufWriter::with_capacity(256 << 10, file), &term_list)?;
        let mut file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        summary
    };
    drop(term_list);
    debug_assert_eq!(idx_summary.num_tokens, num_tokens);

    let rel_refs: Vec<&str> = metas.iter().map(|(r, _)| r.as_str()).collect();
    let bytes = doc::encode(metas.iter().zip(&rel_refs).map(|((_, m), path)| DocIn {
        path,
        inode: m.inode,
        mtime_nanos: time_to_nanos(m.mtime),
        size: m.size,
        len: m.len,
        kind: DocKind::from(m.status),
        content_hash: m.content_hash,
    }))?;
    let doc_crc = DocTableView::parse(&bytes)?.crc();
    write_whole(fs, &doc_tmp, &bytes)?;

    Ok(PreparedMerge {
        source_ids: segment_ids.to_vec(),
        snapshot_del_gen,
        old_to_new,
        num_docs: metas.len() as u32,
        idx_tmp,
        doc_tmp,
        idx_summary,
        doc_crc,
        doc_len: bytes.len() as u64,
    })
}

/// Commit phase: takes the writer lock, re-reads the current manifest,
/// carries forward any deletions each source picked up since it was
/// snapshotted, and commits one manifest generation naming all of `prepared`
/// (so several groups from one planning pass can still land in a single
/// commit, exactly as before the split). Aborts (discarding every
/// `prepared`'s temp files) if a source segment no longer exists — merged
/// away by a racing commit, which a single-actor writer should never
/// produce, but is checked rather than assumed.
pub fn commit_merges(fs: &dyn Fs, root: &Path, prepared: Vec<PreparedMerge>) -> Result<Index> {
    if prepared.is_empty() {
        return open(fs, root, &OpenOptions::default())?.ok_or_else(|| Error::NoIndex(index_dir(root)));
    }
    let dir = index_dir(root);
    let lock = take_lock(fs, &dir)?;
    let cur = match open(fs, root, &OpenOptions::default()) {
        Ok(Some(i)) => i,
        Ok(None) => {
            for p in prepared {
                p.discard(fs);
            }
            return Err(Error::NoIndex(dir));
        }
        Err(e) => {
            for p in prepared {
                p.discard(fs);
            }
            return Err(e);
        }
    };
    let mut manifest = cur.manifest().clone();
    let mut new_files: Vec<PathBuf> = Vec::new();
    let mut merged_ids: HashSet<u32> = HashSet::new();
    let mut retired: Vec<PathBuf> = Vec::new();

    let result = (|| -> Result<()> {
        let mut new_entries = Vec::new();
        // Base for the *first* new segment; each subsequent one continues
        // from there — computing this fresh from `manifest.segments` per
        // group would give every group in this pass the same (stale) base,
        // since `manifest.segments` isn't mutated until after the loop.
        let mut next_base = manifest.segments.iter().map(|s| s.end_doc()).max().unwrap_or(0) as DocId;
        for p in &prepared {
            for &id in &p.source_ids {
                if !manifest.segments.iter().any(|s| s.segment_id == id) {
                    return Err(corrupt(&dir, format!("segment {id} no longer exists; merge aborted")));
                }
            }
            let entry = finish_one(fs, &dir, &mut manifest, cur.segment_list(), p, next_base, &mut new_files)?;
            next_base += entry.num_docs;
            for &id in &p.source_ids {
                merged_ids.insert(id);
                let old = cur.manifest().segments.iter().find(|s| s.segment_id == id).unwrap();
                retired.extend(segment_files(old).into_iter().map(|n| dir.join(n)));
            }
            new_entries.push(entry);
        }
        manifest.segments.retain(|s| !merged_ids.contains(&s.segment_id));
        manifest.segments.extend(new_entries);
        manifest.segments.sort_unstable_by_key(|s| s.base_doc);
        manifest.generation += 1;
        manifest.committed_unix_nanos = time_to_nanos(SystemTime::now());
        commit(fs, &dir, &manifest, &retired)
    })();
    if let Err(e) = result {
        for p in &new_files {
            let _ = fs.remove_file(p);
        }
        return Err(e);
    }
    drop(lock);
    open(fs, root, &OpenOptions::default())?.ok_or_else(|| Error::Corrupt { path: dir.join(MANIFEST), reason: "manifest unreadable right after commit".into() })
}

/// Allocate this prepared merge's real segment id/base_doc from `manifest`
/// (mutating `manifest.next_segment_id`), compute and write its carry-forward
/// `.del` if needed, and rename its temp files into place.
fn finish_one(
    fs: &dyn Fs,
    dir: &Path,
    manifest: &mut crate::format::manifest::Manifest,
    current_segments: &[super::Segment],
    prepared: &PreparedMerge,
    base_doc: DocId,
    new_files: &mut Vec<PathBuf>,
) -> Result<SegmentEntry> {
    let segment_id = manifest.next_segment_id;
    manifest.next_segment_id += 1;

    let idx_final = dir.join(idx_name(segment_id));
    let doc_final = dir.join(doc_name(segment_id));
    new_files.push(idx_final.clone());
    fs.rename(&prepared.idx_tmp, &idx_final)?;
    new_files.push(doc_final.clone());
    fs.rename(&prepared.doc_tmp, &doc_final)?;

    // Carry forward: a source whose del_gen advanced since the snapshot may
    // have new tombstones among the docs that survived into this merge.
    let mut carry = Bitmap::new(prepared.num_docs);
    for &src_id in &prepared.source_ids {
        let current_entry = manifest.segments.iter().find(|s| s.segment_id == src_id).unwrap();
        if current_entry.del_gen == prepared.snapshot_del_gen[&src_id] {
            continue; // no new deletions on this source since the snapshot
        }
        let seg = current_segments.iter().find(|s| s.entry().segment_id == src_id).unwrap();
        let Some(bitmap) = seg.deletions() else { continue };
        for (&(seg_id, old_local), &new_local) in &prepared.old_to_new {
            if seg_id == src_id && bitmap.get(old_local) {
                carry.set(new_local);
            }
        }
    }

    let (del_gen, del_len, del_crc, num_deleted) = if carry.count() > 0 {
        let bytes = carry.encode();
        let final_path = dir.join(del_name(segment_id, 1));
        let tmp = dir.join(tmp_name(&del_name(segment_id, 1)));
        new_files.push(tmp.clone());
        write_whole(fs, &tmp, &bytes)?;
        new_files.push(final_path.clone());
        fs.rename(&tmp, &final_path)?;
        let crc = envelope::parse(envelope::Kind::Del, &bytes)?.crc;
        (1u32, bytes.len() as u64, crc, carry.count())
    } else {
        (0, 0, 0, 0)
    };

    Ok(SegmentEntry {
        segment_id,
        del_gen,
        base_doc,
        num_docs: prepared.num_docs,
        num_deleted,
        idx_crc: prepared.idx_summary.crc,
        doc_crc: prepared.doc_crc,
        del_crc,
        idx_len: prepared.idx_summary.len,
        doc_len: prepared.doc_len,
        del_len,
        num_tokens: prepared.idx_summary.num_tokens,
        num_postings: prepared.idx_summary.num_postings,
        num_terms: prepared.idx_summary.num_terms,
    })
}

/// Plan, prepare, and commit every merge one planning pass calls for, in a
/// single manifest generation. `Ok` with the unchanged index when there is
/// nothing to merge. The synchronous convenience — a caller that wants the
/// unlocked-prepare / background-commit split (the daemon's background
/// merge task) uses [`prepare_merge`] and [`commit_merges`] directly.
pub fn merge_index(fs: &dyn Fs, root: &Path, policy: &MergePolicy) -> Result<Index> {
    let dir = index_dir(root);
    super::clear_staging(fs, root)?;
    let prev = open(fs, root, &OpenOptions::default())?.ok_or_else(|| Error::NoIndex(dir.clone()))?;
    let groups = plan_merges(&prev.manifest().segments, policy);
    if groups.is_empty() {
        return Ok(prev);
    }
    let mut prepared = Vec::with_capacity(groups.len());
    for group in &groups {
        match prepare_merge(fs, root, &dir, &prev, group) {
            Ok(p) => prepared.push(p),
            Err(e) => {
                for p in prepared {
                    p.discard(fs);
                }
                return Err(e);
            }
        }
    }
    commit_merges(fs, root, prepared)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: u32, num_docs: u32, num_deleted: u32, bytes: u64) -> SegmentEntry {
        SegmentEntry { segment_id: id, base_doc: 0, num_docs, num_deleted, idx_len: bytes, ..Default::default() }
    }

    #[test]
    fn small_tiers_do_not_merge() {
        let segs = vec![seg(0, 10, 0, 1000), seg(1, 10, 0, 1000)];
        assert!(plan_merges(&segs, &MergePolicy::default()).is_empty());
    }

    #[test]
    fn a_full_tier_merges() {
        let segs: Vec<_> = (0..4).map(|i| seg(i, 10, 0, 1_000_000)).collect(); // same tier
        let groups = plan_merges(&segs, &MergePolicy::default());
        assert_eq!(groups.len(), 1);
        let mut ids = groups[0].clone();
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2, 3]);
    }

    #[test]
    fn different_tiers_do_not_mix() {
        let mut segs: Vec<_> = (0..4).map(|i| seg(i, 10, 0, 1_000_000)).collect(); // ~1MB tier
        segs.extend((4..8).map(|i| seg(i, 10, 0, 100_000_000))); // ~100MB tier
        let groups = plan_merges(&segs, &MergePolicy::default());
        assert_eq!(groups.len(), 2);
        for g in &groups {
            assert!(g.iter().all(|id| *id < 4) || g.iter().all(|id| *id >= 4));
        }
    }

    #[test]
    fn oversized_segments_are_never_merged() {
        let mut segs: Vec<_> = (0..5).map(|i| seg(i, 10, 0, 1_000_000)).collect();
        segs.push(seg(5, 10, 0, 600 << 20)); // over the 512 MiB cap
        let groups = plan_merges(&segs, &MergePolicy::default());
        assert!(groups.iter().flatten().all(|&id| id != 5));
    }

    #[test]
    fn heavily_deleted_segment_is_rewritten_alone() {
        let segs = vec![seg(0, 100, 40, 1_000_000), seg(1, 10, 0, 1_000_000)];
        let groups = plan_merges(&segs, &MergePolicy::default());
        assert!(groups.contains(&vec![0]));
        // Segment 1 alone is below the tier threshold, so no other group forms.
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn a_deletion_driven_segment_is_not_also_tier_merged() {
        let mut segs = vec![seg(0, 100, 40, 1_000_000)]; // >30% deleted
        segs.extend((1..4).map(|i| seg(i, 10, 0, 1_000_000))); // same tier as 0's live size, but only 3 of them
        let groups = plan_merges(&segs, &MergePolicy::default());
        assert_eq!(groups, vec![vec![0]]); // the other three don't reach tier_size on their own
    }

    #[test]
    fn tier_larger_than_max_merge_is_chunked() {
        let segs: Vec<_> = (0..25).map(|i| seg(i, 10, 0, 1_000_000)).collect();
        let groups = plan_merges(&segs, &MergePolicy::default());
        assert!(groups.iter().all(|g| g.len() <= 10));
        let total: usize = groups.iter().map(|g| g.len()).sum();
        assert_eq!(total, 25);
    }

    #[test]
    fn empty_input_plans_nothing() {
        assert!(plan_merges(&[], &MergePolicy::default()).is_empty());
    }
}
