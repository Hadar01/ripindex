//! One root, one actor: a single tokio task draining one mailbox, which is
//! what actually collapses the multi-writer lock problem into a
//! single-writer one. The per-root `.ripindex/LOCK` advisory lock still
//! exists and still matters — it's what stops a second daemon, or a stray
//! direct CLI write bypassing the socket entirely, from racing this actor —
//! but it is no longer *how* two writes against the same root are kept from
//! racing each other: that's simply "there is exactly one task, and it
//! processes one command before the next," by construction.
//!
//! **Queries never touch this actor.** [`RootHandle::live`] is an
//! `Arc<LiveIndex>` any connection task can call `.snapshot()` on directly —
//! `LiveIndex` is already internally synchronized, so routing a query
//! through the actor's mailbox would only add latency (and, worse, queue it
//! behind a slow reconcile or merge) for no correctness benefit.
//!
//! **Reconcile** runs on `spawn_blocking`, awaited directly by the actor —
//! it blocks the mailbox for its duration, which is acceptable because
//! reconciles are the fast, frequent operation the watcher fires often (see
//! the M3 reconcile-cost numbers). **Merge is different in kind, not just
//! degree**: it can take tens of seconds on a large corpus, and a reconcile
//! blocked behind one means edits go unsearchable for that whole time. So
//! merge is genuinely backgrounded: the actor spawns an independent task
//! that snapshots the index, runs [`store::prepare_merge`] (no lock, doesn't
//! touch the actor at all), and reports back into the actor's own mailbox
//! when ready; only the short `commit_merges` step — a lock, a manifest
//! read, the carry-forward diff, a rename, a commit — runs on the actor's
//! serialized loop, so a reconcile arriving mid-merge is blocked for
//! milliseconds, not minutes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::crawler::CrawlConfig;
use crate::daemon::config::{GovernorConfig, MergeConfig};
use crate::fs::RealFs;
use crate::index::builder::BuildConfig;
use crate::store::{self, LiveIndex, MergePolicy, NoProgress, PreparedMerge};

#[derive(Debug, Serialize, Clone)]
pub struct ReconcileOutcome {
    pub generation: u64,
    pub docs: usize,
    pub segments: u32,
    pub changed: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct MergeStarted {
    pub started: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct RootStatus {
    pub root: String,
    pub docs: usize,
    pub docs_indexed: u32,
    pub segments: u32,
    pub on_disk_bytes: u64,
    pub generation: u64,
    pub last_reconcile_unix_ms: Option<u64>,
    pub last_reconcile_ms: Option<u64>,
    pub last_reconcile_error: Option<String>,
    pub merge_running: bool,
    pub last_merge_unix_ms: Option<u64>,
    pub last_merge_result: Option<String>,
    pub watcher_healthy: bool,
}

enum Command {
    Reconcile { reply: oneshot::Sender<Result<ReconcileOutcome, String>> },
    Merge { reply: oneshot::Sender<MergeStarted> },
    /// Sent by the background merge task to itself (via the actor's own
    /// mailbox) once `prepare_merge` has finished for every planned group.
    MergeReady { prepared: Result<Vec<PreparedMerge>, String>, plan_len: usize },
    Status { reply: oneshot::Sender<RootStatus> },
    SetWatcherHealthy(bool),
    Shutdown,
}

/// What a client (a connection task, or the server's registry) holds for one
/// root. Cheap to clone.
#[derive(Clone)]
pub struct RootHandle {
    pub root: PathBuf,
    pub live: Arc<LiveIndex>,
    cmd_tx: mpsc::Sender<Command>,
    /// Set by the server after it spawns this root's watcher thread; the
    /// watcher polls it (via `store::watcher::run`'s `should_stop`) and the
    /// actor itself doesn't touch it — a getter so the server can flip it on
    /// `remove_root`/shutdown.
    pub watcher_stop: Arc<std::sync::atomic::AtomicBool>,
}

impl RootHandle {
    pub async fn reconcile(&self) -> Result<ReconcileOutcome, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(Command::Reconcile { reply: tx }).await.map_err(|_| "root actor is gone".to_string())?;
        rx.await.map_err(|_| "root actor dropped the request".to_string())?
    }

    pub async fn merge(&self) -> MergeStarted {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(Command::Merge { reply: tx }).await.is_err() {
            return MergeStarted { started: false, reason: Some("root actor is gone".into()) };
        }
        rx.await.unwrap_or(MergeStarted { started: false, reason: Some("root actor dropped the request".into()) })
    }

    pub async fn status(&self) -> Option<RootStatus> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(Command::Status { reply: tx }).await.ok()?;
        rx.await.ok()
    }

    pub async fn set_watcher_healthy(&self, healthy: bool) {
        let _ = self.cmd_tx.send(Command::SetWatcherHealthy(healthy)).await;
    }

    pub async fn shutdown(&self) {
        self.watcher_stop.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(Command::Shutdown).await;
    }
}

struct ActorState {
    root: PathBuf,
    live: Arc<LiveIndex>,
    merge_policy: MergePolicy,
    governor: GovernorConfig,
    merging: bool,
    last_reconcile: Option<(SystemTime, Duration, Option<String>)>,
    last_merge: Option<(SystemTime, String)>,
    watcher_healthy: bool,
    self_tx: mpsc::Sender<Command>,
}

/// Spawn the actor for a root that already has an index (built if
/// necessary) and return the handle everything else talks to.
pub fn spawn(root: PathBuf, live: Arc<LiveIndex>, merge_cfg: MergeConfig, governor: GovernorConfig) -> RootHandle {
    let (tx, rx) = mpsc::channel(64);
    let handle = RootHandle {
        root: root.clone(),
        live: live.clone(),
        cmd_tx: tx.clone(),
        watcher_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let state = ActorState {
        root,
        live,
        merge_policy: merge_cfg.into(),
        governor,
        merging: false,
        last_reconcile: None,
        last_merge: None,
        watcher_healthy: true,
        self_tx: tx,
    };
    tokio::spawn(run(state, rx));
    handle
}

async fn run(mut state: ActorState, mut rx: mpsc::Receiver<Command>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Reconcile { reply } => {
                let outcome = do_reconcile(&mut state).await;
                let _ = reply.send(outcome);
            }
            Command::Merge { reply } => {
                let started = start_merge(&mut state);
                let _ = reply.send(started);
            }
            Command::MergeReady { prepared, plan_len } => {
                finish_merge(&mut state, prepared, plan_len).await;
            }
            Command::Status { reply } => {
                let _ = reply.send(status(&state));
            }
            Command::SetWatcherHealthy(h) => state.watcher_healthy = h,
            Command::Shutdown => break,
        }
    }
}

async fn do_reconcile(state: &mut ActorState) -> Result<ReconcileOutcome, String> {
    let root = state.root.clone();
    let before_gen = state.live.generation();
    let t = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        store::update_index(&RealFs, &root, &CrawlConfig::default(), &BuildConfig::default(), &NoProgress)
    })
    .await
    .map_err(|e| format!("reconcile task panicked: {e}"))?;
    let elapsed = t.elapsed();
    match result {
        Ok(_) => {
            let _ = state.live.refresh();
            state.last_reconcile = Some((SystemTime::now(), elapsed, None));
            let docs = state.live.snapshot();
            use crate::index::IndexReader;
            Ok(ReconcileOutcome {
                generation: state.live.generation(),
                docs: docs.stats().docs_total as usize,
                segments: docs.stats().segments,
                changed: state.live.generation() != before_gen,
                elapsed_ms: elapsed.as_millis() as u64,
            })
        }
        Err(e) => {
            let msg = e.to_string();
            state.last_reconcile = Some((SystemTime::now(), elapsed, Some(msg.clone())));
            Err(msg)
        }
    }
}

fn start_merge(state: &mut ActorState) -> MergeStarted {
    if state.merging {
        return MergeStarted { started: false, reason: Some("a merge is already running for this root".into()) };
    }
    state.merging = true;
    let root = state.root.clone();
    let policy = state.merge_policy;
    let self_tx = state.self_tx.clone();
    let governor = Arc::new(crate::store::governor::Governor::new(state.governor.bytes_per_sec, state.governor.cpu_fraction));

    tokio::spawn(async move {
        let plan_and_prepare = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<PreparedMerge>, String> {
                let dir = store::index_dir(&root);
                store::clear_staging(&RealFs, &root).map_err(|e| e.to_string())?;
                let index = store::open(&RealFs, &root, &store::OpenOptions::default())
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "no index".to_string())?;
                let groups = store::plan_merges(&index.manifest().segments, &policy);
                let mut prepared = Vec::with_capacity(groups.len());
                for group in &groups {
                    // Governor: throttle by the group's on-disk footprint as
                    // a proxy for the IO this prepare is about to do.
                    let bytes: u64 = group
                        .iter()
                        .filter_map(|id| index.manifest().segments.iter().find(|s| s.segment_id == *id))
                        .map(|s| s.total_bytes())
                        .sum();
                    let wait_ms = governor.throttle(bytes);
                    if wait_ms > 0 {
                        std::thread::sleep(Duration::from_millis(wait_ms));
                    }
                    let t = Instant::now();
                    match store::prepare_merge(&RealFs, &root, &dir, &index, group) {
                        Ok(p) => prepared.push(p),
                        Err(e) => {
                            for p in prepared {
                                p.discard(&RealFs);
                            }
                            return Err(e.to_string());
                        }
                    }
                    let spent_ms = t.elapsed().as_millis() as u64;
                    let idle_ms = governor.after_cpu_work(spent_ms);
                    if idle_ms > 0 {
                        std::thread::sleep(Duration::from_millis(idle_ms));
                    }
                }
                Ok(prepared)
            })
            .await
        };
        let (prepared, plan_len) = match plan_and_prepare {
            Ok(Ok(p)) => {
                let len = p.len();
                (Ok(p), len)
            }
            Ok(Err(e)) => (Err(e), 0),
            Err(e) => (Err(format!("merge prepare task panicked: {e}")), 0),
        };
        let _ = self_tx.send(Command::MergeReady { prepared, plan_len }).await;
    });

    MergeStarted { started: true, reason: None }
}

async fn finish_merge(state: &mut ActorState, prepared: Result<Vec<PreparedMerge>, String>, plan_len: usize) {
    state.merging = false;
    let outcome = match prepared {
        Err(e) => Err(e),
        Ok(prepared) if prepared.is_empty() => Ok(format!("nothing to merge (planned {plan_len} groups, prepared 0)")),
        Ok(prepared) => {
            let root = state.root.clone();
            let n = prepared.len();
            let result = tokio::task::spawn_blocking(move || store::commit_merges(&RealFs, &root, prepared)).await;
            match result {
                Ok(Ok(_)) => {
                    let _ = state.live.refresh();
                    Ok(format!("merged {n} group(s)"))
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(format!("merge commit task panicked: {e}")),
            }
        }
    };
    let msg = match &outcome {
        Ok(m) => m.clone(),
        Err(e) => format!("error: {e}"),
    };
    state.last_merge = Some((SystemTime::now(), msg));
    if let Err(e) = outcome {
        log::warn!("merge failed for {}: {e}", state.root.display());
    }
}

fn status(state: &ActorState) -> RootStatus {
    use crate::index::IndexReader;
    let snap = state.live.snapshot();
    let stats = snap.stats().clone();
    let to_unix_ms = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    RootStatus {
        root: state.root.display().to_string(),
        docs: stats.docs_total as usize,
        docs_indexed: stats.docs_indexed,
        segments: stats.segments,
        on_disk_bytes: stats.on_disk_bytes,
        generation: state.live.generation(),
        last_reconcile_unix_ms: state.last_reconcile.as_ref().map(|(t, _, _)| to_unix_ms(*t)),
        last_reconcile_ms: state.last_reconcile.as_ref().map(|(_, d, _)| d.as_millis() as u64),
        last_reconcile_error: state.last_reconcile.as_ref().and_then(|(_, _, e)| e.clone()),
        merge_running: state.merging,
        last_merge_unix_ms: state.last_merge.as_ref().map(|(t, _)| to_unix_ms(*t)),
        last_merge_result: state.last_merge.as_ref().map(|(_, m)| m.clone()),
        watcher_healthy: state.watcher_healthy,
    }
}

/// Monotonically increasing counter, for the moment something needs a
/// process-wide unique id (kept trivial and dependency-free rather than
/// pulling in a UUID crate for one use site).
pub static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexReader;
    use std::fs;

    async fn build_root(dir: &std::path::Path) -> Arc<LiveIndex> {
        fs::write(dir.join("a.txt"), "alpha beta").unwrap();
        let cfg = CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() };
        store::update_index(&RealFs, dir, &cfg, &BuildConfig::default(), &NoProgress).unwrap();
        Arc::new(LiveIndex::open(Arc::new(RealFs), dir).unwrap().unwrap())
    }

    #[tokio::test]
    async fn reconcile_picks_up_new_files_and_reports_the_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let live = build_root(dir.path()).await;
        let handle = spawn(dir.path().to_path_buf(), live, MergeConfig::default(), GovernorConfig::default());
        let gen0 = handle.live.generation();

        fs::write(dir.path().join("b.txt"), "gamma delta").unwrap();
        let outcome = handle.reconcile().await.unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.generation, handle.live.generation());
        assert!(handle.live.generation() > gen0);
        assert_eq!(outcome.docs, 2);

        // A second reconcile with nothing changed reports `changed: false`.
        let again = handle.reconcile().await.unwrap();
        assert!(!again.changed);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn merge_runs_in_the_background_and_status_reports_completion() {
        let dir = tempfile::tempdir().unwrap();
        let live = build_root(dir.path()).await;
        // Force multiple segments so there's something to merge.
        for i in 1..8 {
            fs::write(dir.path().join(format!("f{i}.txt")), format!("alpha extra{i}")).unwrap();
            let cfg = CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() };
            store::update_index(&RealFs, dir.path(), &cfg, &BuildConfig { shard_size: 1, docs_per_segment: 1, ..Default::default() }, &NoProgress).unwrap();
        }
        live.refresh().unwrap();
        let segments_before = live.snapshot().stats().segments;
        assert!(segments_before >= 4);

        let handle = spawn(dir.path().to_path_buf(), live.clone(), MergeConfig { tier_size: 2, ..MergeConfig::default() }, GovernorConfig::default());
        let started = handle.merge().await;
        assert!(started.started);

        // A second merge request while one is running is rejected, not queued silently.
        let second = handle.merge().await;
        assert!(!second.started);

        // Poll status until the background merge lands (bounded, not sleep-and-hope forever).
        let mut ok = false;
        for _ in 0..200 {
            let st = handle.status().await.unwrap();
            if !st.merge_running && st.last_merge_result.is_some() {
                assert!(st.segments < segments_before, "the merge must have reduced the segment count");
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ok, "merge did not complete in time");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn status_reports_docs_and_generation() {
        let dir = tempfile::tempdir().unwrap();
        let live = build_root(dir.path()).await;
        let handle = spawn(dir.path().to_path_buf(), live, MergeConfig::default(), GovernorConfig::default());
        let st = handle.status().await.unwrap();
        assert_eq!(st.docs, 1);
        assert_eq!(st.generation, 1);
        assert!(!st.merge_running);
        assert!(st.watcher_healthy);
        handle.set_watcher_healthy(false).await;
        assert!(!handle.status().await.unwrap().watcher_healthy);
    }
}
