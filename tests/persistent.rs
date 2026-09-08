//! Persistent-backend behaviour beyond the M1 suite: multi-segment builds,
//! reopen, rebuild, deletions, verification, and the CLI's `verify`.

use std::fs;
use std::path::Path;
use std::process::Command;

use ripindex::crawler::CrawlConfig;
use ripindex::fs::RealFs;
use ripindex::index::{BuildConfig, IndexReader};
use ripindex::query::{search, Query, SearchOptions};
use ripindex::store::{self, build_index, delete_docs, open, NoProgress, OpenOptions};

fn cfg() -> CrawlConfig {
    CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
}

/// 25 files, five words each drawn from a small vocabulary, forced into many segments.
fn corpus(root: &Path) -> Vec<String> {
    let vocab = ["alpha", "beta", "gamma", "delta", "parse_http_response", "HttpResponse", "x_x", "café"];
    let mut names = Vec::new();
    for i in 0..25u32 {
        let words: Vec<&str> = (0..5).map(|k| vocab[((i * 7 + k * 3) % vocab.len() as u32) as usize]).collect();
        let name = format!("f{i:02}.txt");
        fs::write(root.join(&name), words.join(" ")).unwrap();
        names.push(name);
    }
    names
}

fn hits(index: &impl IndexReader, q: &str) -> Vec<String> {
    let r = search(index, &Query::parse(q).unwrap(), &SearchOptions { limit: 1000, ..Default::default() });
    let mut v: Vec<String> =
        r.hits.iter().map(|h| index.doc(h.doc).unwrap().path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn many_segments_reopen_and_agree_with_single_segment() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let small = BuildConfig { shard_size: 1, docs_per_segment: 3, ..BuildConfig::default() };
    let multi = build_index(&RealFs, dir.path(), &cfg(), &small, &NoProgress).unwrap();
    assert_eq!(multi.stats().segments, 9); // ceil(25 / 3)
    assert_eq!(multi.docs().len(), 25);
    assert_eq!(multi.docs().id_bound(), 25);
    let multi_answers: Vec<_> = ["alpha", "beta gamma", "\"alpha beta\"", "http", "x_x", "café", "alpha -beta", "gamma OR delta"]
        .iter()
        .map(|q| hits(&multi, q))
        .collect();
    assert!(multi_answers[0].len() > 5);
    drop(multi);

    // Reopen without rebuilding: identical answers, same generation.
    let reopened = open(&RealFs, dir.path(), &OpenOptions { verify: true }).unwrap().unwrap();
    assert_eq!(reopened.manifest().generation, 1);
    assert_eq!(reopened.stats().segments, 9);
    for (i, q) in ["alpha", "beta gamma", "\"alpha beta\"", "http", "x_x", "café", "alpha -beta", "gamma OR delta"].iter().enumerate() {
        assert_eq!(hits(&reopened, q), multi_answers[i], "{q}");
    }
    drop(reopened);

    // Rebuild as one segment: same answers, old segments gone, generation bumped.
    let single = build_index(&RealFs, dir.path(), &cfg(), &BuildConfig::default(), &NoProgress).unwrap();
    assert_eq!(single.stats().segments, 1);
    assert_eq!(single.manifest().generation, 2);
    assert_eq!(single.manifest().segments[0].segment_id, 9, "segment ids are never reused");
    for (i, q) in ["alpha", "beta gamma", "\"alpha beta\"", "http", "x_x", "café", "alpha -beta", "gamma OR delta"].iter().enumerate() {
        assert_eq!(hits(&single, q), multi_answers[i], "{q}");
    }
    let names: Vec<String> = fs::read_dir(single.dir()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.starts_with("seg-0000") && !n.starts_with("seg-00009")), "{names:?}");
}

#[test]
fn byte_budget_splits_segments() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    // A tiny byte budget: every shard overflows it, so one segment per shard.
    let cfg_bytes = BuildConfig { shard_size: 5, docs_per_segment: 1_000_000, segment_bytes: 1 };
    let index = build_index(&RealFs, dir.path(), &cfg(), &cfg_bytes, &NoProgress).unwrap();
    assert_eq!(index.stats().segments, 5);
    assert_eq!(hits(&index, "alpha").len(), hits(&index, "alpha").len());
}

#[test]
fn deletions_persist_and_generations_advance() {
    let dir = tempfile::tempdir().unwrap();
    let names = corpus(dir.path());
    let small = BuildConfig { shard_size: 1, docs_per_segment: 4, ..BuildConfig::default() };
    let index = build_index(&RealFs, dir.path(), &cfg(), &small, &NoProgress).unwrap();
    let all_alpha = hits(&index, "alpha");
    let n_before = index.docs().indexed_count();
    let name = |m: &ripindex::index::DocMeta| m.path.file_name().unwrap().to_string_lossy().into_owned();

    // One alpha doc from segment 0 (ids 0..4) and one from segment 1 (ids 4..8),
    // plus an id that does not exist.
    let alpha_ids: Vec<(u32, String)> =
        index.docs().iter().filter(|(_, m)| all_alpha.contains(&name(m))).map(|(id, m)| (id, name(&m))).collect();
    let v0 = alpha_ids.iter().find(|(id, _)| *id / 4 == 0).unwrap().clone();
    let v1 = alpha_ids.iter().find(|(id, _)| *id / 4 == 1).unwrap().clone();
    let deleted = delete_docs(&RealFs, &index, &[v0.0, v1.0, 9_999]).unwrap();
    drop(index);
    assert_eq!(deleted.manifest().generation, 2);
    assert_eq!(deleted.docs().len(), 23);
    assert_eq!(deleted.docs().indexed_count(), n_before - 2);
    assert!(deleted.doc(v0.0).is_none() && deleted.doc(v1.0).is_none());
    let after: Vec<String> = all_alpha.iter().filter(|n| **n != v0.1 && **n != v1.1).cloned().collect();
    assert_eq!(hits(&deleted, "alpha"), after);
    let dels: Vec<&_> = deleted.manifest().segments.iter().filter(|s| s.del_gen == 1).collect();
    assert_eq!(dels.len(), 2, "one .del generation per affected segment");

    // Deleting an already-deleted doc changes nothing and commits nothing.
    let same = delete_docs(&RealFs, &deleted, &[v0.0]).unwrap();
    assert_eq!(same.manifest().generation, 2, "no change, no commit");
    drop(same);

    // Second round on segment 0 → its generation 2; generation-1 file retired.
    let third = deleted.docs().iter().find(|(id, _)| *id != v0.0 && *id / 4 == 0).map(|(id, _)| id).unwrap();
    let round2 = delete_docs(&RealFs, &deleted, &[third]).unwrap();
    drop(deleted);
    assert_eq!(round2.manifest().generation, 3);
    let seg = round2.manifest().segments.iter().find(|s| s.base_doc == 0).unwrap();
    assert_eq!(seg.del_gen, 2);
    assert_eq!(seg.num_deleted, 2);
    let on_disk: Vec<String> =
        fs::read_dir(round2.dir()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(on_disk.contains(&store::del_name(seg.segment_id, 2)));
    assert!(!on_disk.contains(&store::del_name(seg.segment_id, 1)), "{on_disk:?}");

    // Reopen with verification: deletions are durable.
    drop(round2);
    let reopened = open(&RealFs, dir.path(), &OpenOptions { verify: true }).unwrap().unwrap();
    assert_eq!(reopened.docs().len(), 22);
    assert_eq!(reopened.docs().indexed_count(), n_before - 3);
    assert!(!hits(&reopened, "alpha").contains(&v0.1));
    assert!(!hits(&reopened, "alpha").contains(&v1.1));
    assert_eq!(names.len(), 25);
}

#[test]
fn own_index_directory_is_never_indexed() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    build_index(&RealFs, dir.path(), &cfg(), &BuildConfig::default(), &NoProgress).unwrap();
    let again = build_index(&RealFs, dir.path(), &cfg(), &BuildConfig::default(), &NoProgress).unwrap();
    assert_eq!(again.docs().len(), 25);
    assert_eq!(again.stats().crawl.files_seen, 25);
    assert!(again.docs().iter().all(|(_, m)| !m.path.to_string_lossy().contains(".ripindex")));
}

#[test]
fn empty_root_builds_an_empty_index() {
    let dir = tempfile::tempdir().unwrap();
    let index = build_index(&RealFs, dir.path(), &cfg(), &BuildConfig::default(), &NoProgress).unwrap();
    assert_eq!(index.stats().segments, 0);
    assert!(index.docs().is_empty());
    assert!(hits(&index, "anything").is_empty());
    drop(index);
    let reopened = open(&RealFs, dir.path(), &OpenOptions { verify: true }).unwrap().unwrap();
    assert!(reopened.docs().is_empty());
}

#[test]
fn stats_report_disk_and_corpus_sizes() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let index = build_index(&RealFs, dir.path(), &cfg(), &BuildConfig::default(), &NoProgress).unwrap();
    let s = index.stats();
    assert!(s.on_disk_bytes > 0);
    let actual: u64 = fs::read_dir(index.dir())
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name() != store::LOCK)
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert_eq!(s.on_disk_bytes, actual);
    assert!(s.crawl.indexable_bytes > 0);
    assert!(s.memory_bytes < 64 * 1024, "reader heap should be tiny: {}", s.memory_bytes);
    // Upper bound only: open time legitimately rounds to zero on a fast machine,
    // so a lower bound would flake. This still catches a hang or a units error.
    assert!(s.open_time < std::time::Duration::from_secs(60), "open took {:?}", s.open_time);
    let text = s.to_string();
    assert!(text.contains("index on disk") && text.contains("corpus size") && text.contains("segments"));
}

#[test]
fn cli_verify_and_reuse() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let bin = env!("CARGO_BIN_EXE_ripindex");

    // verify before any index: NoIndex error.
    let out = Command::new(bin).args(["verify"]).arg(dir.path()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no index"));

    // First search builds; second search reuses (no "building" note).
    let out = Command::new(bin).args(["search", "alpha", "--no-color", "--no-daemon", "--root"]).arg(dir.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("building one now"));
    let out = Command::new(bin).args(["search", "alpha", "--no-color", "--no-daemon", "--root"]).arg(dir.path()).output().unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("building"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("opened in"));

    let out = Command::new(bin).args(["verify"]).arg(dir.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("OK: generation 1, 1 segments, 25 docs"));

    // Corrupt a segment body: verify fails, plain open still works.
    let seg = store::index_dir(dir.path()).join("seg-00000.idx");
    let mut bytes = fs::read(&seg).unwrap();
    bytes[30] ^= 0x01;
    fs::write(&seg, &bytes).unwrap();
    let out = Command::new(bin).args(["verify"]).arg(dir.path()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum") || String::from_utf8_lossy(&out.stderr).contains("corrupt"));
}
