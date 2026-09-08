//! Commit-protocol tests under injected faults (FORMAT.md §7/§8).
//!
//! Setup: an index over corpus A is committed; the corpus is then changed to B
//! and a rebuild is attempted through a `FaultFs` with one fault. After the
//! attempt the directory is reopened with the real file system and **full
//! verification**, and must equal exactly state A or exactly state B — never
//! a mix, never absent, never corrupt. Every operation of a clean run is an
//! injection point; the clean run's operation log is also asserted verbatim
//! against the documented protocol.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ripindex::crawler::CrawlConfig;
use ripindex::fs::{FaultFs, FaultPlan, Fs, RealFs};
use ripindex::index::{BuildConfig, DocStatus, Index, IndexReader};
use ripindex::query::{search, Query, SearchOptions};
use ripindex::store::{self, build_index, delete_docs, open, NoProgress, OpenOptions};

const QUERIES: &[&str] = &["alpha", "beta OR gamma", "\"alpha beta\"", "alpha -gamma", "delta"];

/// Everything a reader can observe, keyed by path so doc ids need not match.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    docs: BTreeSet<(String, u32, bool)>,
    queries: Vec<BTreeSet<String>>,
    generation: u64,
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")
}

fn snapshot(index: &Index) -> State {
    let root = index.root();
    let docs = index
        .docs()
        .iter()
        .map(|(_, m)| (rel(root, &m.path), m.len, m.status == DocStatus::Indexed))
        .collect();
    let queries = QUERIES
        .iter()
        .map(|q| {
            let r = search(index, &Query::parse(q).unwrap(), &SearchOptions { limit: 1000, ..Default::default() });
            r.hits.iter().map(|h| rel(root, &index.doc(h.doc).unwrap().path)).collect()
        })
        .collect();
    State { docs, queries, generation: index.manifest().generation }
}

fn crawl_cfg() -> CrawlConfig {
    CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
}

fn build_cfg() -> BuildConfig {
    BuildConfig { shard_size: 1, docs_per_segment: 2, ..BuildConfig::default() }
}

fn write_corpus(root: &Path, files: &[(&str, &str)]) {
    for entry in fs::read_dir(root).unwrap() {
        let p = entry.unwrap().path();
        if p.is_file() {
            fs::remove_file(&p).unwrap();
        }
    }
    for (name, content) in files {
        fs::write(root.join(name), content).unwrap();
    }
}

const CORPUS_A: &[(&str, &str)] = &[("a.txt", "alpha beta"), ("b.txt", "beta gamma"), ("c.txt", "alpha gamma delta")];
const CORPUS_B: &[(&str, &str)] =
    &[("a.txt", "alpha beta changed"), ("c.txt", "alpha gamma delta"), ("d.txt", "alpha alpha beta"), ("e.txt", "delta")];

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// Reopen with the real fs and full verification; the directory must also be
/// tidy (nothing unreferenced left behind).
fn reopen_verified(root: &Path) -> Index {
    let index = open(&RealFs, root, &OpenOptions { verify: true })
        .expect("reopen must not error")
        .expect("index must not vanish");
    let referenced: BTreeSet<String> =
        index.files().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    for entry in fs::read_dir(index.dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name == store::LOCK {
            continue;
        }
        assert!(referenced.contains(&name), "unreferenced file left behind: {name}");
    }
    index
}

/// Template = corpus B on disk + the committed index of corpus A.
struct Fixture {
    _dir: tempfile::TempDir,
    template: PathBuf,
    state_a: State,
    state_b: State,
    clean_log: Vec<String>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let template = dir.path().join("template");
    fs::create_dir_all(&template).unwrap();
    write_corpus(&template, CORPUS_A);
    let a = build_index(&RealFs, &template, &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    assert_eq!(a.stats().segments, 2, "A should span two segments");
    let state_a = snapshot(&a);
    drop(a);
    write_corpus(&template, CORPUS_B);

    // Clean rebuild on a copy: reference state B and the protocol's op log.
    let clean = dir.path().join("clean");
    copy_dir(&template, &clean);
    let fs_ = FaultFs::new(&store::index_dir(&clean));
    let b = build_index(&fs_, &clean, &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    assert_eq!(b.stats().segments, 2, "B should span two segments");
    let state_b = snapshot(&b);
    drop(b);
    assert_ne!(state_a.docs, state_b.docs);
    Fixture { _dir: dir, template, state_a, state_b, clean_log: fs_.log() }
}

/// Collapse consecutive `write X` entries (buffer flushes) into one.
fn normalize(log: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in log {
        if l.starts_with("write ") && out.last() == Some(l) {
            continue;
        }
        out.push(l.clone());
    }
    out
}

#[test]
fn clean_commit_follows_the_documented_protocol_exactly() {
    let f = fixture();
    let expected: Vec<String> = [
        // build_index: directory, lock (outer, succeeds)
        "create_dir_all .",
        "lock LOCK",
        // open() to read the previous index: its own cleanup-lock probe
        // conflicts with the outer lock we already hold (self-conflict, by
        // design — see `open_once`), so it fails and cleanup is skipped:
        // no `list_dir` here. Segments 0 and 1 are A's.
        "lock LOCK",
        "read MANIFEST",
        "mmap seg-00000.idx",
        "mmap seg-00000.doc",
        "mmap seg-00001.idx",
        "mmap seg-00001.doc",
        // §7 step 2, per new file: temp → write → sync → close → rename
        "create seg-00002.idx.tmp",
        "write seg-00002.idx.tmp",
        "sync seg-00002.idx.tmp",
        "close seg-00002.idx.tmp",
        "rename seg-00002.idx.tmp -> seg-00002.idx",
        "create seg-00002.doc.tmp",
        "write seg-00002.doc.tmp",
        "sync seg-00002.doc.tmp",
        "close seg-00002.doc.tmp",
        "rename seg-00002.doc.tmp -> seg-00002.doc",
        "create seg-00003.idx.tmp",
        "write seg-00003.idx.tmp",
        "sync seg-00003.idx.tmp",
        "close seg-00003.idx.tmp",
        "rename seg-00003.idx.tmp -> seg-00003.idx",
        "create seg-00003.doc.tmp",
        "write seg-00003.doc.tmp",
        "sync seg-00003.doc.tmp",
        "close seg-00003.doc.tmp",
        "rename seg-00003.doc.tmp -> seg-00003.doc",
        // step 3
        "sync_dir .",
        // step 4
        "create MANIFEST.tmp",
        "write MANIFEST.tmp",
        "sync MANIFEST.tmp",
        "close MANIFEST.tmp",
        // step 5: the commit point
        "rename MANIFEST.tmp -> MANIFEST",
        // step 6
        "sync_dir .",
        // step 7
        "remove seg-00000.idx",
        "remove seg-00000.doc",
        "remove seg-00001.idx",
        "remove seg-00001.doc",
        "unlock LOCK",
        // reopen for the caller: the outer lock is free now, so this open's
        // cleanup-lock probe succeeds and the sweep runs (finding nothing
        // left to remove — retirement above already did that).
        "lock LOCK",
        "read MANIFEST",
        "list_dir .",
        "unlock LOCK",
        "mmap seg-00002.idx",
        "mmap seg-00002.doc",
        "mmap seg-00003.idx",
        "mmap seg-00003.doc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(normalize(&f.clean_log), expected);
}

/// Run one faulted rebuild from the template; return the state observed after.
fn faulted_rebuild(f: &Fixture, scratch: &Path, plan: FaultPlan) -> (State, Result<(), String>) {
    if scratch.exists() {
        fs::remove_dir_all(scratch).unwrap();
    }
    copy_dir(&f.template, scratch);
    let fs_ = FaultFs::new(&store::index_dir(scratch));
    fs_.set_plan(plan);
    let outcome = build_index(&fs_, scratch, &crawl_cfg(), &build_cfg(), &NoProgress).map(drop).map_err(|e| e.to_string());
    let state = snapshot(&reopen_verified(scratch));
    (state, outcome)
}

fn assert_all_or_nothing(f: &Fixture, plan: FaultPlan, state: &State, outcome: &Result<(), String>) {
    assert!(
        *state == f.state_a || *state == f.state_b,
        "{plan:?}: recovered to a state that is neither A nor B: {state:#?}\n(build outcome: {outcome:?})"
    );
}

#[test]
fn crash_at_every_operation_recovers_to_a_or_b() {
    let f = fixture();
    let scratch = f.template.parent().unwrap().join("scratch");
    let n_ops = f.clean_log.len() as u64;
    let (mut ended_a, mut ended_b) = (0, 0);
    for k in 1..=n_ops {
        let plan = FaultPlan { crash_at_op: Some(k), ..Default::default() };
        let (state, outcome) = faulted_rebuild(&f, &scratch, plan);
        assert_all_or_nothing(&f, plan, &state, &outcome);
        assert!(outcome.is_err(), "a crash injected at op {k} must surface as an error");
        if state == f.state_b {
            ended_b += 1;
        } else {
            ended_a += 1;
        }
    }
    // The commit point is the manifest rename; crashes before it leave A, at
    // or after it leave B. Both sides must be exercised.
    assert!(ended_a > 0 && ended_b > 0, "A: {ended_a}, B: {ended_b}");
    let rename_at = f.clean_log.iter().position(|l| l == "rename MANIFEST.tmp -> MANIFEST").unwrap() as u64 + 1;
    // Ops are 1-based; a crash *at* op k means op k did not happen. So a crash
    // at the rename itself still leaves A; only crashes after it leave B.
    assert_eq!(ended_a, rename_at, "every crash up to and including the rename must yield A");
    assert_eq!(ended_b, n_ops - rename_at, "every op after the rename must yield B");
}

#[test]
fn failed_write_at_every_write_leaves_a() {
    let f = fixture();
    let scratch = f.template.parent().unwrap().join("scratch");
    let n_writes = f.clean_log.iter().filter(|l| l.starts_with("write ")).count() as u64;
    assert!(n_writes >= 5);
    for w in 1..=n_writes {
        let plan = FaultPlan { fail_write: Some(w), ..Default::default() };
        let (state, outcome) = faulted_rebuild(&f, &scratch, plan);
        assert!(outcome.is_err(), "write {w}");
        assert_eq!(state, f.state_a, "a failed write ({w}) happens before the commit point, so A must remain");
    }
}

#[test]
fn failed_sync_at_every_sync_recovers_to_a_or_b() {
    let f = fixture();
    let scratch = f.template.parent().unwrap().join("scratch");
    let syncs: Vec<&String> = f.clean_log.iter().filter(|l| l.starts_with("sync ") || l.starts_with("sync_dir ")).collect();
    assert!(syncs.len() >= 5);
    for (i, what) in syncs.iter().enumerate() {
        let s = i as u64 + 1;
        let plan = FaultPlan { fail_sync: Some(s), ..Default::default() };
        let (state, outcome) = faulted_rebuild(&f, &scratch, plan);
        assert_all_or_nothing(&f, plan, &state, &outcome);
        // Only the directory sync *after* the manifest rename can fail and still
        // leave B — and then the build reports success (with a warning), since
        // the commit is complete and merely may not be durable.
        let after_commit = i == syncs.len() - 1;
        if after_commit {
            assert_eq!(state, f.state_b, "sync {s} ({what}) is after the commit point");
            assert!(outcome.is_ok(), "post-commit sync failure must not be reported as a failed build: {outcome:?}");
        } else {
            assert!(outcome.is_err(), "sync {s} ({what})");
            assert_eq!(state, f.state_a, "sync {s} ({what}) is before the commit point");
        }
    }
}

#[test]
fn discarded_write_is_caught_by_the_following_sync_and_leaves_a() {
    let f = fixture();
    let scratch = f.template.parent().unwrap().join("scratch");
    let n_writes = f.clean_log.iter().filter(|l| l.starts_with("write ")).count() as u64;
    for w in 1..=n_writes {
        // Lost bytes, process keeps going: the file's sync_all reports EIO.
        let plan = FaultPlan { discard_write: Some(w), ..Default::default() };
        let (state, outcome) = faulted_rebuild(&f, &scratch, plan);
        assert!(outcome.is_err(), "discard {w}");
        assert_eq!(state, f.state_a, "discarded write {w} must abort before the commit point");
    }
}

#[test]
fn discarded_write_followed_by_crash_leaves_a() {
    let f = fixture();
    let scratch = f.template.parent().unwrap().join("scratch");
    // Op index of each write in the clean run, so we can crash right after it
    // (before its sync) — a torn file that never reaches a rename.
    let write_ops: Vec<u64> = f
        .clean_log
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("write "))
        .map(|(i, _)| i as u64 + 1)
        .collect();
    for (w, op) in write_ops.iter().enumerate() {
        let plan = FaultPlan { discard_write: Some(w as u64 + 1), crash_at_op: Some(op + 1), ..Default::default() };
        let (state, outcome) = faulted_rebuild(&f, &scratch, plan);
        assert!(outcome.is_err());
        assert_eq!(state, f.state_a, "{plan:?}");
    }
}

#[test]
fn deletion_commit_under_crashes_is_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    write_corpus(&root, CORPUS_B);
    let index = build_index(&RealFs, &root, &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    let before = snapshot(&index);
    // Delete a.txt (segment 0) and d.txt (segment 1): two new .del generations.
    let ids: Vec<u32> = index
        .docs()
        .iter()
        .filter(|(_, m)| matches!(rel(&root, &m.path).as_str(), "a.txt" | "d.txt"))
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids.len(), 2);
    drop(index);
    let template = dir.path().join("template");
    copy_dir(&root, &template);

    // Reference "after" state and op count.
    let fs_ = FaultFs::new(&store::index_dir(&root));
    let index = open(&fs_, &root, &OpenOptions::default()).unwrap().unwrap();
    let after_index = delete_docs(&fs_, &index, &ids).unwrap();
    let after = snapshot(&after_index);
    drop((after_index, index));
    assert_ne!(before, after);
    assert!(after.queries[0].len() + 2 == before.queries[0].len(), "alpha hits drop by the two deleted docs");
    let log = fs_.log();
    let rename_at = log.iter().position(|l| l == "rename MANIFEST.tmp -> MANIFEST").unwrap();
    // Ops before delete_docs itself (the open, and its own cleanup-lock
    // probe) are not part of the protocol under test. `open()` above may
    // have already taken and released the lock once for its orphan sweep,
    // so `delete_docs`'s own lock is the *last* one taken before the commit.
    let first_delete_op = log[..rename_at].iter().rposition(|l| l == "lock LOCK").unwrap();
    assert!(log[first_delete_op..rename_at].iter().any(|l| l.starts_with("rename seg-00000.00001.del.tmp -> seg-00000.00001.del")));
    assert!(log[first_delete_op..rename_at].iter().any(|l| l.starts_with("rename seg-00001.00001.del.tmp -> seg-00001.00001.del")));

    let scratch = dir.path().join("scratch");
    let (mut ended_before, mut ended_after) = (0, 0);
    for k in (first_delete_op as u64 + 1)..=(log.len() as u64) {
        if scratch.exists() {
            fs::remove_dir_all(&scratch).unwrap();
        }
        copy_dir(&template, &scratch);
        let fs_ = FaultFs::new(&store::index_dir(&scratch));
        let index = open(&fs_, &scratch, &OpenOptions::default()).unwrap().unwrap();
        // Counters include the open above; crash at the same absolute op index.
        fs_.set_plan(FaultPlan { crash_at_op: Some(k), ..Default::default() });
        let outcome = delete_docs(&fs_, &index, &ids).map(drop).map_err(|e| e.to_string());
        drop(index);
        assert!(outcome.is_err(), "crash at op {k}");
        let state = snapshot(&reopen_verified(&scratch));
        assert!(state == before || state == after, "crash at op {k}: neither before nor after: {state:#?}");
        if state == after {
            ended_after += 1;
        } else {
            ended_before += 1;
        }
    }
    assert!(ended_before > 0 && ended_after > 0, "before: {ended_before}, after: {ended_after}");
}

#[test]
fn corrupt_manifest_is_absent_and_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write_corpus(dir.path(), CORPUS_A);
    build_index(&RealFs, dir.path(), &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    let manifest = store::index_dir(dir.path()).join(store::MANIFEST);
    let files_before: BTreeSet<_> = fs::read_dir(store::index_dir(dir.path())).unwrap().map(|e| e.unwrap().file_name()).collect();

    // Flip a byte in the body.
    let mut bytes = fs::read(&manifest).unwrap();
    bytes[40] ^= 0xff;
    fs::write(&manifest, &bytes).unwrap();
    assert!(open(&RealFs, dir.path(), &OpenOptions::default()).unwrap().is_none());
    let files_after: BTreeSet<_> = fs::read_dir(store::index_dir(dir.path())).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(files_before, files_after, "a corrupt manifest must not trigger cleanup");

    // Truncated.
    fs::write(&manifest, &bytes[..20]).unwrap();
    assert!(open(&RealFs, dir.path(), &OpenOptions::default()).unwrap().is_none());

    // Missing.
    fs::remove_file(&manifest).unwrap();
    assert!(open(&RealFs, dir.path(), &OpenOptions::default()).unwrap().is_none());

    // A rebuild recovers and the next open cleans the orphans.
    let rebuilt = build_index(&RealFs, dir.path(), &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    assert_eq!(rebuilt.manifest().generation, 1, "no readable predecessor, so generation restarts");
    drop(rebuilt);
    reopen_verified(dir.path());
}

#[test]
fn newer_format_version_is_an_error_not_absence() {
    let dir = tempfile::tempdir().unwrap();
    write_corpus(dir.path(), CORPUS_A);
    build_index(&RealFs, dir.path(), &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    let manifest = store::index_dir(dir.path()).join(store::MANIFEST);
    let mut bytes = fs::read(&manifest).unwrap();
    bytes[8..12].copy_from_slice(&99u32.to_le_bytes()); // format_version = 99 (newer than we support)
    fs::write(&manifest, &bytes).unwrap();
    match open(&RealFs, dir.path(), &OpenOptions::default()) {
        Err(ripindex::Error::Incompatible { version: 99, .. }) => {}
        other => panic!("expected Incompatible, got {:?}", other.map(|o| o.is_some())),
    }
}

#[test]
fn committed_segment_that_fails_checks_is_corrupt_not_absent() {
    let dir = tempfile::tempdir().unwrap();
    write_corpus(dir.path(), CORPUS_A);
    let index = build_index(&RealFs, dir.path(), &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
    let seg = index.dir().join("seg-00000.idx");
    drop(index);

    // Truncate a referenced segment: length check fails on open.
    let bytes = fs::read(&seg).unwrap();
    fs::write(&seg, &bytes[..bytes.len() - 1]).unwrap();
    assert!(matches!(open(&RealFs, dir.path(), &OpenOptions::default()), Err(ripindex::Error::Corrupt { .. })));

    // Same length, flipped byte deep in the body: envelope passes, verify catches it.
    let mut flipped = bytes.clone();
    flipped[24] ^= 0x01;
    fs::write(&seg, &flipped).unwrap();
    assert!(open(&RealFs, dir.path(), &OpenOptions::default()).unwrap().is_some(), "opens without verify");
    assert!(matches!(open(&RealFs, dir.path(), &OpenOptions { verify: true }), Err(ripindex::Error::Corrupt { .. })));

    // Missing.
    fs::remove_file(&seg).unwrap();
    assert!(matches!(open(&RealFs, dir.path(), &OpenOptions::default()), Err(ripindex::Error::Corrupt { .. })));
}

#[test]
fn second_writer_is_refused_while_lock_is_held() {
    let dir = tempfile::tempdir().unwrap();
    write_corpus(dir.path(), CORPUS_A);
    let index_dir = store::index_dir(dir.path());
    fs::create_dir_all(&index_dir).unwrap();
    let held = RealFs.lock(&index_dir.join(store::LOCK)).unwrap();
    match build_index(&RealFs, dir.path(), &crawl_cfg(), &build_cfg(), &NoProgress) {
        Err(ripindex::Error::Locked(_)) => {}
        other => panic!("expected Locked, got {:?}", other.map(|_| ())),
    }
    drop(held);
    build_index(&RealFs, dir.path(), &crawl_cfg(), &build_cfg(), &NoProgress).unwrap();
}
