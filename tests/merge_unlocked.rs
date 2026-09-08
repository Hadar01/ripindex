//! The unlocked prepare/commit merge split (M4): a merge's read/decode/write
//! phase runs against a snapshot with no lock held; `commit_merges` is the
//! only part that takes the lock, and it must carry forward any deletions a
//! source segment picked up between the snapshot and the commit rather than
//! silently resurrecting them.

use std::fs;
use std::path::Path;

use ripindex::crawler::CrawlConfig;
use ripindex::fs::RealFs;
use ripindex::index::{BuildConfig, IndexReader};
use ripindex::query::{search, Query, SearchOptions};
use ripindex::store::{build_index, delete_docs, merge_index, open, prepare_merge, MergePolicy, NoProgress, OpenOptions};

fn cfg() -> CrawlConfig {
    CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
}

fn corpus(root: &Path, n: usize) -> Vec<String> {
    let vocab = ["alpha", "beta", "gamma", "delta"];
    let mut names = Vec::new();
    for i in 0..n {
        let name = format!("f{i:03}.txt");
        fs::write(root.join(&name), format!("{} common{i}", vocab[i % vocab.len()])).unwrap();
        names.push(name);
    }
    names
}

fn hits(index: &impl IndexReader, q: &str) -> Vec<String> {
    let r = search(index, &Query::parse(q).unwrap(), &SearchOptions { limit: 10_000, ..Default::default() });
    let mut v: Vec<String> =
        r.hits.iter().map(|h| index.doc(h.doc).unwrap().path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn a_deletion_arriving_between_snapshot_and_commit_is_carried_into_the_merged_segment() {
    let dir = tempfile::tempdir().unwrap();
    let names = corpus(dir.path(), 8);
    let small = BuildConfig { shard_size: 1, docs_per_segment: 4, ..BuildConfig::default() };
    let index = build_index(&RealFs, dir.path(), &cfg(), &small, &NoProgress).unwrap();
    assert_eq!(index.stats().segments, 2);
    let before_alpha = hits(&index, "alpha");
    assert!(!before_alpha.is_empty());

    // 1. Snapshot for the merge — this is the "unlocked read" phase.
    let plan = ripindex::store::plan_merges(&index.manifest().segments, &MergePolicy { tier_size: 2, ..MergePolicy::default() });
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].len(), 2);
    let prepared = prepare_merge(&RealFs, dir.path(), index.dir(), &index, &plan[0]).unwrap();
    assert_eq!(prepared.num_docs(), 8);

    // 2. A concurrent writer deletes a doc from one of the segments being
    // merged, *after* the snapshot was taken, and commits — while the merge
    // is still only holding `prepared` in memory, no lock.
    let victim_id = index.docs().iter().find(|(_, m)| m.path.file_name().unwrap().to_string_lossy() == before_alpha[0]).unwrap().0;
    let after_delete = delete_docs(&RealFs, &index, &[victim_id]).unwrap();
    assert_eq!(after_delete.docs().len(), 7);
    drop((index, after_delete));

    // 3. Now commit the merge, prepared *before* the deletion.
    let merged = ripindex::store::commit_merges(&RealFs, dir.path(), vec![prepared]).unwrap();
    assert_eq!(merged.stats().segments, 1);

    // The deletion must have survived the merge: the victim is gone, and
    // every other original doc is still there and still searchable.
    let after_merge_alpha = hits(&merged, "alpha");
    assert_eq!(after_merge_alpha.len(), before_alpha.len() - 1, "the concurrently-deleted doc must not be resurrected");
    assert!(!after_merge_alpha.contains(&before_alpha[0]));
    for n in &before_alpha[1..] {
        assert!(after_merge_alpha.contains(n));
    }
    assert_eq!(merged.docs().len(), 7);
    assert_eq!(names.len(), 8);

    // The merged segment's own del bitmap actually exists and carries the count.
    let seg = &merged.manifest().segments[0];
    assert_eq!(seg.del_gen, 1, "the merge must write its own carry-forward .del");
    assert_eq!(seg.num_deleted, 1);

    // Reopen with full verification: the carried-forward deletion is durable.
    drop(merged);
    let reopened = open(&RealFs, dir.path(), &OpenOptions { verify: true }).unwrap().unwrap();
    assert_eq!(hits(&reopened, "alpha"), after_merge_alpha);
}

#[test]
fn no_intervening_deletion_means_no_del_file_is_written() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path(), 8);
    let small = BuildConfig { shard_size: 1, docs_per_segment: 4, ..BuildConfig::default() };
    let index = build_index(&RealFs, dir.path(), &cfg(), &small, &NoProgress).unwrap();
    let plan = ripindex::store::plan_merges(&index.manifest().segments, &MergePolicy { tier_size: 2, ..MergePolicy::default() });
    let prepared = prepare_merge(&RealFs, dir.path(), index.dir(), &index, &plan[0]).unwrap();
    drop(index);

    let merged = ripindex::store::commit_merges(&RealFs, dir.path(), vec![prepared]).unwrap();
    let seg = &merged.manifest().segments[0];
    assert_eq!(seg.del_gen, 0, "nothing changed between snapshot and commit, so no .del is needed");
    assert_eq!(seg.num_deleted, 0);
    assert_eq!(merged.docs().len(), 8);
}

#[test]
fn deleting_all_survivors_between_snapshot_and_commit_yields_an_empty_but_valid_segment() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path(), 4);
    let index = build_index(&RealFs, dir.path(), &cfg(), &BuildConfig { shard_size: 1, docs_per_segment: 4, ..BuildConfig::default() }, &NoProgress).unwrap();
    assert_eq!(index.stats().segments, 1);
    let plan = [vec![index.manifest().segments[0].segment_id]];
    let prepared = prepare_merge(&RealFs, dir.path(), index.dir(), &index, &plan[0]).unwrap();

    let ids: Vec<u32> = index.docs().iter().map(|(id, _)| id).collect();
    let after_delete = delete_docs(&RealFs, &index, &ids).unwrap();
    assert_eq!(after_delete.docs().indexed_count(), 0);
    drop((index, after_delete));

    let merged = ripindex::store::commit_merges(&RealFs, dir.path(), vec![prepared]).unwrap();
    assert_eq!(merged.manifest().segments[0].num_deleted, 4);
    assert_eq!(merged.docs().indexed_count(), 0);
    merged.verify().unwrap();
}

#[test]
fn a_source_segment_removed_before_commit_aborts_and_cleans_up_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path(), 8);
    let small = BuildConfig { shard_size: 1, docs_per_segment: 4, ..BuildConfig::default() };
    let index = build_index(&RealFs, dir.path(), &cfg(), &small, &NoProgress).unwrap();
    let plan = ripindex::store::plan_merges(&index.manifest().segments, &MergePolicy { tier_size: 2, ..MergePolicy::default() });
    let prepared = prepare_merge(&RealFs, dir.path(), index.dir(), &index, &plan[0]).unwrap();

    // Simulate a racing merge that already consumed these same source
    // segments (shouldn't happen with a single actor, but must not corrupt
    // anything if it somehow does): merge the corpus away entirely first.
    drop(index);
    merge_index(&RealFs, dir.path(), &MergePolicy { tier_size: 2, ..MergePolicy::default() }).unwrap();

    let before: Vec<_> = fs::read_dir(ripindex::store::index_dir(dir.path())).unwrap().map(|e| e.unwrap().file_name()).collect();
    let result = ripindex::store::commit_merges(&RealFs, dir.path(), vec![prepared]);
    assert!(result.is_err());
    let after: Vec<_> = fs::read_dir(ripindex::store::index_dir(dir.path())).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before, after, "the aborted merge's temp files must be cleaned up, and nothing else touched");

    // The index committed by the racing merge is untouched and still valid.
    let still_there = open(&RealFs, dir.path(), &OpenOptions { verify: true }).unwrap().unwrap();
    assert_eq!(still_there.docs().len(), 8);
}

#[test]
fn merge_index_convenience_still_produces_one_commit_for_several_groups() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path(), 16);
    let small = BuildConfig { shard_size: 1, docs_per_segment: 2, ..BuildConfig::default() };
    let index = build_index(&RealFs, dir.path(), &cfg(), &small, &NoProgress).unwrap();
    assert_eq!(index.stats().segments, 8);
    let before = hits(&index, "alpha");
    drop(index);

    let merged = merge_index(&RealFs, dir.path(), &MergePolicy { tier_size: 2, max_merge: 4, ..MergePolicy::default() }).unwrap();
    assert!(merged.stats().segments < 8, "several groups merged in one pass");
    assert_eq!(merged.manifest().generation, 2, "one generation for the whole pass, however many groups");
    assert_eq!(hits(&merged, "alpha"), before);
    merged.verify().unwrap();
}
