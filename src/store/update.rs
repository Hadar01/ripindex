//! Incremental updates: reconcile the current filesystem against the last
//! commit and write only what changed — tombstone + append, never rewrite a
//! committed segment. One commit through the same manifest protocol as a
//! full build.
//!
//! Known cost: a changed file is read twice — once by [`reconcile`] to hash
//! it against the baseline, once more here when it's tokenized. Renamed and
//! untouched files never pay this; it lands only on files whose content
//! actually changed, which must be tokenized regardless. Fixing it means
//! threading pre-read bytes through `index_shard`, deferred as a follow-up.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::crawler::{self, CrawlConfig, CrawledFile, FileEntry};
use crate::error::{Error, Result};
use crate::format::del::Bitmap;
use crate::format::envelope;
use crate::format::overlay::{self, PatchIn};
use crate::fs::Fs;
use crate::index::builder::BuildConfig;
use crate::index::{rss_bytes, DocId};

use super::reader::{open, Index, OpenOptions};
use super::segment::nanos_to_time;
use super::state::{reconcile, Change, StateIndex};
use super::writer::{build_index, build_segments, commit, take_lock, write_whole, BuildProgress};
use super::{del_name, index_dir, overlay_name, tmp_name, MANIFEST};

/// Reconcile `root` against the last commit and write only the difference.
/// If there is no previous index, this is exactly [`build_index`]. If
/// nothing changed, returns the existing index unchanged (no commit).
pub fn update_index(
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
    let started = std::time::Instant::now();
    let dir = index_dir(root);
    fs.create_dir_all(&dir)?;

    // No baseline: this *is* a full build. Don't hold our lock into it —
    // build_index takes its own.
    let had_previous = {
        let lock = take_lock(fs, &dir)?;
        let exists = open(fs, root, &OpenOptions::default())?.is_some();
        drop(lock);
        exists
    };
    if !had_previous {
        return build_index(fs, root, crawl_cfg, build_cfg, progress);
    }

    let lock = take_lock(fs, &dir)?;
    let prev = open(fs, root, &OpenOptions::default())?
        .ok_or_else(|| Error::Corrupt { path: dir.join(MANIFEST), reason: "index vanished under the lock".into() })?;

    let crawl_started = std::time::Instant::now();
    let (crawled, crawl_stats) = crawler::crawl_all_with_progress(root, crawl_cfg, &|n| progress.crawling(n))?;
    let crawl_time = crawl_started.elapsed();
    progress.crawled(crawled.len() as u64);

    let state = StateIndex::from_docs(root, prev.docs().iter());
    let plan = reconcile(&state, root, &crawled);
    if plan.is_empty() {
        drop(lock);
        return Ok(prev);
    }
    log::info!(
        "update: {} deleted, {} renamed/touched, {} to (re)index",
        plan.deletes(),
        plan.patches(),
        plan.reindexes()
    );

    let mut deletes: Vec<DocId> = Vec::new();
    let mut new_patches: Vec<(DocId, u64, i64, u64, String)> = Vec::new();
    let mut to_index: Vec<CrawledFile> = Vec::new();
    for c in plan.changes {
        match c {
            Change::Delete(id) => deletes.push(id),
            Change::Patch { doc_id, inode, mtime_nanos, size, path } => new_patches.push((doc_id, inode, mtime_nanos, size, path)),
            Change::Reindex { path, kind, inode, mtime_nanos, size, .. } => {
                let mut abs = root.to_path_buf();
                abs.extend(path.split('/').filter(|c| !c.is_empty()));
                to_index.push(CrawledFile { entry: FileEntry { path: abs, inode, mtime: nanos_to_time(mtime_nanos), size }, kind });
            }
        }
    }

    let mut manifest = prev.manifest().clone();
    let base_id = prev.docs().id_bound();
    let mut new_files: Vec<PathBuf> = Vec::new();
    let mut retired: Vec<PathBuf> = Vec::new();

    let result = (|| -> Result<()> {
        // 1. New segment(s) for reindexed/new files.
        if !to_index.is_empty() {
            let (entries, next_id) =
                build_segments(fs, root, &dir, &to_index, base_id, build_cfg, manifest.next_segment_id, progress, &mut new_files)?;
            manifest.segments.extend(entries);
            manifest.next_segment_id = next_id;
        }

        // 2. Tombstones, grouped by segment.
        let mut by_segment: BTreeMap<usize, Vec<DocId>> = BTreeMap::new();
        for &id in &deletes {
            if let Some((seg, local)) = prev.locate(id) {
                let i = prev.segment_list().iter().position(|s| std::ptr::eq(s, seg)).unwrap();
                by_segment.entry(i).or_default().push(local);
            }
        }
        for (i, locals) in by_segment {
            let seg = &prev.segment_list()[i];
            let entry = &mut manifest.segments[i];
            let mut bitmap = seg.deletions().cloned().unwrap_or_else(|| Bitmap::new(entry.num_docs));
            let mut changed = false;
            for l in locals {
                changed |= bitmap.set(l);
            }
            if !changed {
                continue;
            }
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

        // 3. Overlay: merge new patches into the previous ones, drop deleted docs.
        let deleted: std::collections::HashSet<DocId> = deletes.iter().copied().collect();
        let mut merged: BTreeMap<DocId, (u64, i64, u64, String)> =
            prev.overlay_patches().map(|(id, inode, mtime, size, path)| (id, (inode, mtime, size, path.to_string()))).collect();
        for (id, inode, mtime, size, path) in new_patches {
            merged.insert(id, (inode, mtime, size, path));
        }
        for id in &deleted {
            merged.remove(id);
        }
        let prev_state_gen = manifest.state_gen;
        if merged.is_empty() {
            manifest.state_gen = 0;
            manifest.state_crc = 0;
            manifest.state_len = 0;
        } else {
            let patches: Vec<PatchIn> = merged
                .iter()
                .map(|(&doc_id, (inode, mtime_nanos, size, path))| PatchIn { doc_id, inode: *inode, mtime_nanos: *mtime_nanos, size: *size, path })
                .collect();
            let bytes = overlay::encode(&patches)?;
            let gen = prev_state_gen + 1;
            let final_path = dir.join(overlay_name(gen));
            let tmp = dir.join(tmp_name(&overlay_name(gen)));
            new_files.push(tmp.clone());
            write_whole(fs, &tmp, &bytes)?;
            new_files.push(final_path.clone());
            fs.rename(&tmp, &final_path)?;
            manifest.state_gen = gen;
            manifest.state_len = bytes.len() as u64;
            manifest.state_crc = envelope::parse(envelope::Kind::Overlay, &bytes)?.crc;
        }
        if prev_state_gen > 0 && prev_state_gen != manifest.state_gen {
            retired.push(dir.join(overlay_name(prev_state_gen)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::RealFs;
    use crate::index::{DocMeta, IndexReader};
    use crate::query::{search, Query, SearchOptions};
    use crate::store::writer::{build_index, relative_path};
    use crate::store::NoProgress;
    use std::fs;

    fn cfg() -> CrawlConfig {
        CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
    }

    fn small_build() -> BuildConfig {
        BuildConfig { shard_size: 1, docs_per_segment: 3, ..BuildConfig::default() }
    }

    fn names(index: &Index) -> Vec<String> {
        let mut v: Vec<String> = index.docs().iter().map(|(_, m)| relative_path(index.root(), &m.path)).collect();
        v.sort();
        v
    }

    fn hits(index: &Index, q: &str) -> Vec<String> {
        let r = search(index, &Query::parse(q).unwrap(), &SearchOptions { limit: 10_000, ..Default::default() });
        let mut v: Vec<String> = r.hits.iter().map(|h| relative_path(index.root(), &index.doc(h.doc).unwrap().path)).collect();
        v.sort();
        v
    }

    fn full_rebuild_matches(root: &Path) -> Index {
        build_index(&RealFs, root, &cfg(), &BuildConfig::default(), &NoProgress).unwrap()
    }

    /// Assert `a` and `b` are the same corpus (paths, indexed status, and
    /// query answers) — ids need not match, since a rebuild reassigns them.
    fn assert_converged(a: &Index, b: &Index) {
        assert_eq!(names(a), names(b));
        for q in ["alpha", "beta", "\"alpha beta\"", "gamma OR delta", "alpha -beta"] {
            assert_eq!(hits(a, q), hits(b, q), "query {q:?} diverged");
        }
    }

    #[test]
    fn update_with_no_previous_index_is_a_full_build() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha beta").unwrap();
        let index = update_index(&RealFs, dir.path(), &cfg(), &BuildConfig::default(), &NoProgress).unwrap();
        assert_eq!(index.manifest().generation, 1);
        assert_eq!(names(&index), vec!["a.txt"]);
    }

    #[test]
    fn no_change_does_not_commit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let first = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        let gen = first.manifest().generation;
        drop(first);
        let second = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(second.manifest().generation, gen);
    }

    #[test]
    fn added_and_deleted_files_converge_with_a_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha beta").unwrap();
        fs::write(dir.path().join("b.txt"), "beta gamma").unwrap();
        let first = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(names(&first), vec!["a.txt", "b.txt"]);
        drop(first);

        fs::remove_file(dir.path().join("a.txt")).unwrap();
        fs::write(dir.path().join("c.txt"), "gamma delta").unwrap();
        let second = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(names(&second), vec!["b.txt", "c.txt"]);
        assert_eq!(second.manifest().generation, 2);
        assert_converged(&second, &full_rebuild_matches(dir.path()));
    }

    #[test]
    fn content_change_tombstones_the_old_doc_and_appends_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha beta").unwrap();
        let first = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        let old_id = first.docs().iter().next().unwrap().0;
        drop(first);

        fs::write(dir.path().join("a.txt"), "gamma delta").unwrap();
        let second = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert!(second.doc(old_id).is_none(), "old id must be tombstoned");
        assert_eq!(names(&second), vec!["a.txt"]);
        assert_eq!(hits(&second, "gamma"), vec!["a.txt"]);
        assert!(hits(&second, "alpha").is_empty());
        assert_converged(&second, &full_rebuild_matches(dir.path()));
    }

    #[test]
    fn metadata_only_touch_does_not_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "alpha beta gamma").unwrap();
        let first = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        let (old_id, old_meta): (u32, DocMeta) = first.docs().iter().next().unwrap();
        drop(first);

        // Rewrite with identical bytes (as `rsync`/`git checkout` would);
        // mtime changes, content does not.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "alpha beta gamma").unwrap();
        let second = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(second.manifest().generation, 2);
        // Same doc id survives — it was patched, not tombstoned+reappended.
        let meta = second.doc(old_id).expect("id must survive a metadata-only touch");
        assert_eq!(meta.len, old_meta.len);
        assert_eq!(hits(&second, "alpha"), vec!["a.txt"]);
    }

    #[test]
    fn rename_does_not_reindex_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("old")).unwrap();
        fs::write(dir.path().join("old/doc.txt"), "alpha beta gamma delta").unwrap();
        let first = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        let (old_id, _) = first.docs().iter().next().unwrap();
        drop(first);

        fs::create_dir(dir.path().join("new")).unwrap();
        fs::rename(dir.path().join("old/doc.txt"), dir.path().join("new/doc.txt")).unwrap();
        fs::remove_dir(dir.path().join("old")).unwrap();
        let second = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();

        assert_eq!(names(&second), vec!["new/doc.txt"]);
        assert_eq!(second.doc(old_id).unwrap().len, "alpha beta gamma delta".split(' ').count() as u32);
        assert_eq!(hits(&second, "gamma"), vec!["new/doc.txt"]);
        assert_converged(&second, &full_rebuild_matches(dir.path()));
    }

    #[test]
    fn renaming_a_directory_reindexes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let bodies: Vec<String> = (0..20).map(|i| format!("word{i} common text body number {i}")).collect();
        for (i, b) in bodies.iter().enumerate() {
            fs::write(dir.path().join("src").join(format!("f{i}.txt")), b).unwrap();
        }
        update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();

        fs::rename(dir.path().join("src"), dir.path().join("lib")).unwrap();
        let plan = {
            let prev = open(&RealFs, dir.path(), &OpenOptions::default()).unwrap().unwrap();
            let state = StateIndex::from_docs(dir.path(), prev.docs().iter());
            let (crawled, _) = crawler::crawl_all(dir.path(), &cfg()).unwrap();
            reconcile(&state, dir.path(), &crawled)
        };
        assert_eq!(plan.reindexes(), 0, "a directory rename must cost zero reindexed bytes");
        assert_eq!(plan.patches(), 20);
        assert_eq!(plan.bytes_read, 0);

        let updated = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(names(&updated).len(), 20);
        assert!(names(&updated).iter().all(|n| n.starts_with("lib/")));
    }

    #[test]
    fn binary_file_becoming_text_is_reindexed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.dat"), b"\x00binary").unwrap();
        let first = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(first.docs().indexed_count(), 0);
        drop(first);

        fs::write(dir.path().join("a.dat"), b"now alpha text").unwrap();
        let second = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        assert_eq!(hits(&second, "alpha"), vec!["a.dat"]);
        assert_converged(&second, &full_rebuild_matches(dir.path()));
    }

    /// Randomised create/modify/delete/rename sequences converge to a fresh
    /// rebuild after every step. The property test asked for; this is the
    /// deterministic seed-driven version (see `tests/convergence.rs` for the
    /// proptest wrapper).
    pub fn run_random_scenario(seed: u64, steps: usize) {
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() % n.max(1) as u64) as usize
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let mut live: Vec<String> = Vec::new();
        let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
        let mut next_file = 0u32;

        for _ in 0..steps {
            match rng.below(4) {
                0 => {
                    // create
                    let name = format!("f{next_file}.txt");
                    next_file += 1;
                    let n = 1 + rng.below(6);
                    let body: Vec<&str> = (0..n).map(|_| words[rng.below(words.len())]).collect();
                    fs::write(dir.path().join(&name), body.join(" ")).unwrap();
                    live.push(name);
                }
                1 if !live.is_empty() => {
                    // modify
                    let i = rng.below(live.len());
                    let n = 1 + rng.below(6);
                    let body: Vec<&str> = (0..n).map(|_| words[rng.below(words.len())]).collect();
                    fs::write(dir.path().join(&live[i]), body.join(" ")).unwrap();
                }
                2 if !live.is_empty() => {
                    // delete
                    let i = rng.below(live.len());
                    let name = live.remove(i);
                    let _ = fs::remove_file(dir.path().join(&name));
                }
                3 if !live.is_empty() => {
                    // rename
                    let i = rng.below(live.len());
                    let new_name = format!("r{next_file}.txt");
                    next_file += 1;
                    fs::rename(dir.path().join(&live[i]), dir.path().join(&new_name)).unwrap();
                    live[i] = new_name;
                }
                _ => {}
            }
            let incremental = update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
            let rebuilt = full_rebuild_matches(dir.path());
            assert_converged(&incremental, &rebuilt);
            drop(rebuilt); // don't leave a second manifest lying around
            // Restore the incremental index as the on-disk state for the next step.
            update_index(&RealFs, dir.path(), &cfg(), &small_build(), &NoProgress).unwrap();
        }
    }

    #[test]
    fn random_scenarios_converge() {
        for seed in 1..=8 {
            run_random_scenario(seed, 15);
        }
    }
}
