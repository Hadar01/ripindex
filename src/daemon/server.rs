//! The accept loop and per-connection request dispatch. One `Registry` per
//! daemon process, holding one [`RootHandle`] per registered root; each
//! connection is an independent tokio task, so a slow or malformed client
//! never blocks another. A query runs on the blocking pool against a cheap
//! [`crate::store::IndexSnapshot`] clone, never through a root's actor —
//! see `actor`'s module docs for why that split matters.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

use crate::crawler::CrawlConfig;
use crate::daemon::actor::{self, RootHandle};
use crate::daemon::config::{Config, GovernorConfig, MergeConfig};
use crate::daemon::protocol::{parse_client_line, to_line, ClientMessage, Method, ServerMessage, PROTOCOL_VERSION};
use crate::fs::RealFs;
use crate::index::builder::BuildConfig;
use crate::index::IndexReader;
use crate::query::{search, Query, SearchOptions};
use crate::store::{self, LiveIndex, NoProgress};

/// Every root the daemon knows about, keyed by the string the client gave
/// `add_root` (after light normalisation), plus daemon-wide bookkeeping.
pub struct Registry {
    roots: RwLock<HashMap<String, RootHandle>>,
    merge_cfg: MergeConfig,
    governor_cfg: GovernorConfig,
    watch_cfg: store::WatchConfig,
    started_at: Instant,
    connections: AtomicU64,
    /// Per-registry (i.e. per daemon-process instance), never a process-wide
    /// global — several independent daemons run in one test binary during
    /// `cargo test`, and a shared global signal would let one test's
    /// shutdown wake another's `serve` loop.
    pub shutdown_signal: Arc<tokio::sync::Notify>,
}

fn normalize(path: &str) -> PathBuf {
    // Best-effort canonicalisation; a root need not exist yet at `add_root`
    // time in the general case, but ours must (we build/open it immediately),
    // so a failed canonicalize just falls back to the given path.
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn key_of(path: &Path) -> String {
    path.display().to_string()
}

impl Registry {
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Self {
            roots: RwLock::new(HashMap::new()),
            merge_cfg: config.merge,
            governor_cfg: config.governor,
            watch_cfg: store::WatchConfig { debounce_ms: config.watch_debounce_ms, periodic_reconcile_ms: config.periodic_reconcile_secs.saturating_mul(1000) },
            started_at: Instant::now(),
            connections: AtomicU64::new(0),
            shutdown_signal: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Open (building fresh if there's no index yet) and register a root.
    /// Idempotent: re-adding an already-registered root just returns its
    /// existing handle.
    pub async fn add_root(&self, path_str: &str) -> Result<RootHandle, String> {
        let path = normalize(path_str);
        let key = key_of(&path);
        if let Some(h) = self.roots.read().await.get(&key) {
            return Ok(h.clone());
        }
        if !path.is_dir() {
            return Err(format!("{}: not a directory", path.display()));
        }
        let root = path.clone();
        let live = tokio::task::spawn_blocking(move || -> Result<Arc<LiveIndex>, String> {
            let cfg = CrawlConfig::default();
            store::update_index(&RealFs, &root, &cfg, &BuildConfig::default(), &NoProgress).map_err(|e| e.to_string())?;
            store::LiveIndex::open(Arc::new(RealFs), &root)
                .map_err(|e| e.to_string())?
                .map(Arc::new)
                .ok_or_else(|| "index vanished right after building".to_string())
        })
        .await
        .map_err(|e| format!("build task panicked: {e}"))??;

        let handle = actor::spawn(path.clone(), live, self.merge_cfg, self.governor_cfg);
        spawn_watcher(handle.clone(), self.watch_cfg);
        self.roots.write().await.insert(key, handle.clone());
        Ok(handle)
    }

    pub async fn remove_root(&self, path_str: &str) -> bool {
        let key = key_of(&normalize(path_str));
        if let Some(handle) = self.roots.write().await.remove(&key) {
            handle.shutdown().await;
            true
        } else {
            false
        }
    }

    pub async fn list_roots(&self) -> Vec<String> {
        self.roots.read().await.keys().cloned().collect()
    }

    pub async fn get(&self, path_str: &str) -> Option<RootHandle> {
        self.roots.read().await.get(&key_of(&normalize(path_str))).cloned()
    }

    pub async fn all(&self) -> Vec<RootHandle> {
        self.roots.read().await.values().cloned().collect()
    }

    pub async fn shutdown_all(&self) {
        for h in self.roots.read().await.values() {
            h.shutdown().await;
        }
    }

    pub fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }
}

/// One OS thread per root, running `store::watcher::run`'s deterministic
/// scheduler (see that module's docs) over a real `NotifySource`. Every
/// trigger blocks this thread on `handle.reconcile()` via the captured
/// tokio `Handle` — a plain `std::thread`, not a tokio task, because the
/// scheduler's own loop blocks on `source.recv` for however long
/// `periodic_reconcile_ms` allows, which is not what `spawn_blocking`
/// (finite blocking work) is for.
fn spawn_watcher(handle: RootHandle, cfg: store::WatchConfig) {
    let rt = tokio::runtime::Handle::current();
    let root = handle.root.clone();
    std::thread::spawn(move || {
        let mut source = match store::NotifySource::watch(&root) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("watcher: failed to start for {}: {e}", root.display());
                rt.block_on(handle.set_watcher_healthy(false));
                return;
            }
        };
        let clock = store::RealClock::new();
        let stop = handle.watcher_stop.clone();
        store::run_watcher(
            &mut source,
            &clock,
            &cfg,
            || stop.load(Ordering::Acquire),
            || {
                if let Err(e) = rt.block_on(handle.reconcile()) {
                    log::warn!("watcher-triggered reconcile failed for {}: {e}", root.display());
                }
            },
        );
    });
}

/// Run the accept loop until a `shutdown` request arrives over the wire
/// (via `registry.shutdown_signal`) or `external_stop` fires — the latter is
/// what tests use to stop a server they spawned without going through the
/// wire protocol.
pub async fn serve<L: crate::daemon::transport::Accept>(mut listener: L, registry: Arc<Registry>, external_stop: Arc<tokio::sync::Notify>) {
    let internal_stop = registry.shutdown_signal.clone();
    loop {
        tokio::select! {
            _ = internal_stop.notified() => {
                log::info!("daemon: shutting down (requested over the wire)");
                registry.shutdown_all().await;
                return;
            }
            _ = external_stop.notified() => {
                registry.shutdown_all().await;
                return;
            }
            conn = listener.accept() => {
                match conn {
                    Ok(stream) => {
                        let registry = registry.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, registry).await {
                                log::debug!("connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => log::warn!("accept failed: {e}"),
                }
            }
        }
    }
}

async fn write_msg<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, msg: &ServerMessage) -> std::io::Result<()> {
    w.write_all(to_line(msg).as_bytes()).await?;
    w.flush().await
}

async fn handle_connection<C>(stream: C, registry: Arc<Registry>) -> std::io::Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    registry.connections.fetch_add(1, Ordering::Relaxed);
    let (rd, mut wr) = tokio::io::split(stream);
    let mut reader = BufReader::new(rd);
    let mut line = String::new();

    // Handshake: the first line must be `Hello`.
    line.clear();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        registry.connections.fetch_sub(1, Ordering::Relaxed);
        return Ok(());
    }
    match parse_client_line(&line) {
        Ok(ClientMessage::Hello { protocol }) if protocol == PROTOCOL_VERSION => {
            write_msg(&mut wr, &ServerMessage::HelloOk { protocol: PROTOCOL_VERSION, pid: std::process::id() }).await?;
        }
        Ok(ClientMessage::Hello { protocol }) => {
            write_msg(&mut wr, &ServerMessage::error(None, format!("protocol mismatch: server={PROTOCOL_VERSION}, client={protocol}"))).await?;
            registry.connections.fetch_sub(1, Ordering::Relaxed);
            return Ok(());
        }
        _ => {
            write_msg(&mut wr, &ServerMessage::error(None, "first message on a connection must be `hello`")).await?;
            registry.connections.fetch_sub(1, Ordering::Relaxed);
            return Ok(());
        }
    }

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // client closed the connection — nothing left to reply to
        }
        match parse_client_line(&line) {
            Ok(ClientMessage::Hello { .. }) => {
                write_msg(&mut wr, &ServerMessage::error(None, "unexpected second `hello`")).await?;
            }
            Ok(ClientMessage::Request { id, method }) => {
                if matches!(method, Method::Shutdown) {
                    write_msg(&mut wr, &ServerMessage::result(id, serde_json::json!({"ok": true}))).await?;
                    registry.shutdown_signal.notify_one();
                    return Ok(());
                }
                if let Err(e) = dispatch(&registry, id, method, &mut wr).await {
                    log::debug!("dispatch error: {e}");
                    break;
                }
            }
            Err((id, message)) => {
                write_msg(&mut wr, &ServerMessage::error(id, message)).await?;
            }
        }
    }
    registry.connections.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

/// One hit flattened for ranking across roots: `(root, path, score, line_no, snippet)`.
/// Named rather than repeated inline - it is the shape the fan-out collects into
/// before sorting every root's hits together.
type RankedHit = (String, String, f32, Option<usize>, Option<String>);

#[derive(Serialize)]
struct RootAdded {
    root: String,
}

async fn dispatch<W: tokio::io::AsyncWrite + Unpin>(registry: &Registry, id: u64, method: Method, wr: &mut W) -> std::io::Result<()> {
    match method {
        Method::Query { roots, query, limit, offset, snippet } => handle_query(registry, id, roots, query, limit, offset, snippet, wr).await,
        Method::AddRoot { path } => {
            let msg = match registry.add_root(&path).await {
                Ok(h) => ServerMessage::result(id, RootAdded { root: h.root.display().to_string() }),
                Err(e) => ServerMessage::error(Some(id), e),
            };
            write_msg(wr, &msg).await
        }
        Method::RemoveRoot { path } => {
            let removed = registry.remove_root(&path).await;
            write_msg(wr, &ServerMessage::result(id, serde_json::json!({"removed": removed}))).await
        }
        Method::ListRoots => write_msg(wr, &ServerMessage::result(id, registry.list_roots().await)).await,
        Method::Status => {
            let mut roots = Vec::new();
            for h in registry.all().await {
                if let Some(s) = h.status().await {
                    roots.push(s);
                }
            }
            let body = serde_json::json!({
                "pid": std::process::id(),
                "uptime_secs": registry.started_at.elapsed().as_secs(),
                "connections": registry.connections.load(Ordering::Relaxed),
                "roots": roots,
            });
            write_msg(wr, &ServerMessage::result(id, body)).await
        }
        Method::Reconcile { root, subtree: _ } => {
            let targets = match root {
                Some(r) => match registry.get(&r).await {
                    Some(h) => vec![h],
                    None => return write_msg(wr, &ServerMessage::error(Some(id), format!("no such root: {r}"))).await,
                },
                None => registry.all().await,
            };
            let mut results = Vec::new();
            for h in targets {
                results.push(match h.reconcile().await {
                    Ok(o) => serde_json::json!({"root": h.root.display().to_string(), "ok": true, "outcome": o}),
                    Err(e) => serde_json::json!({"root": h.root.display().to_string(), "ok": false, "error": e}),
                });
            }
            write_msg(wr, &ServerMessage::result(id, results)).await
        }
        Method::Merge { root } => {
            let targets = match root {
                Some(r) => match registry.get(&r).await {
                    Some(h) => vec![h],
                    None => return write_msg(wr, &ServerMessage::error(Some(id), format!("no such root: {r}"))).await,
                },
                None => registry.all().await,
            };
            let mut results = Vec::new();
            for h in targets {
                let started = h.merge().await;
                results.push(serde_json::json!({"root": h.root.display().to_string(), "started": started.started, "reason": started.reason}));
            }
            write_msg(wr, &ServerMessage::result(id, results)).await
        }
        Method::Shutdown => unreachable!("handled in handle_connection before dispatch"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_query<W: tokio::io::AsyncWrite + Unpin>(
    registry: &Registry,
    id: u64,
    roots: Option<Vec<String>>,
    query_str: String,
    limit: usize,
    offset: usize,
    want_snippet: bool,
    wr: &mut W,
) -> std::io::Result<()> {
    let query = match Query::parse(&query_str) {
        Ok(q) => q,
        Err(e) => return write_msg(wr, &ServerMessage::error(Some(id), format!("invalid query: {e}"))).await,
    };
    let targets = match roots {
        Some(list) => {
            let mut v = Vec::new();
            for r in &list {
                match registry.get(r).await {
                    Some(h) => v.push(h),
                    None => return write_msg(wr, &ServerMessage::error(Some(id), format!("no such root: {r}"))).await,
                }
            }
            v
        }
        None => registry.all().await,
    };

    let t = Instant::now();
    let fetch = (limit + offset).max(limit);
    let mut all_hits: Vec<RankedHit> = Vec::new();
    for target in &targets {
        let snap = target.live.snapshot();
        let q = query.clone();
        let root_str = target.root.display().to_string();
        let opts = SearchOptions { limit: fetch, ..Default::default() };
        // Runs on the blocking pool: term evaluation and, if requested,
        // re-reading files for snippets are both blocking work, and this
        // way a slow phrase query on one root never stalls the event loop
        // or any other connection. (Disconnect cancellation is not
        // threaded through `spawn_blocking` — see the daemon's design
        // notes: at these per-query latencies the wasted work if a client
        // vanishes mid-query is not worth the complexity of a cooperative
        // cancellation flag through the query engine.)
        let outcome = tokio::task::spawn_blocking(move || {
            let terms = if want_snippet { q.highlight_terms() } else { Vec::new() };
            let result = search(&snap, &q, &opts);
            result
                .hits
                .into_iter()
                .filter_map(|h| {
                    let meta = snap.doc(h.doc)?;
                    let snippet_text = if want_snippet {
                        crate::snippet::snippet_for(&meta.path, &terms, 160).map(|s| crate::snippet::render(&s, false))
                    } else {
                        None
                    };
                    let line_no = if want_snippet { crate::snippet::snippet_for(&meta.path, &terms, 160).map(|s| s.line_no) } else { None };
                    Some((meta.path.display().to_string(), h.score, line_no, snippet_text))
                })
                .collect::<Vec<_>>()
        })
        .await;
        match outcome {
            Ok(hits) => {
                for (path, score, line_no, snip) in hits {
                    all_hits.push((root_str.clone(), path, score, line_no, snip));
                }
            }
            // One root's query panicking (should not happen; defensive)
            // must not break the others — multi-root isolation applies to
            // queries too, not just reconcile/merge.
            Err(e) => log::error!("query on root {root_str} panicked: {e}"),
        }
    }
    all_hits.sort_by(|a, b| b.2.total_cmp(&a.2));
    let total = all_hits.len();
    for (root, path, score, line_no, snippet) in all_hits.into_iter().skip(offset).take(limit) {
        write_msg(wr, &ServerMessage::Hit { id, root, path, score, line_no, snippet }).await?;
    }
    write_msg(wr, &ServerMessage::Done { id, total, elapsed_us: t.elapsed().as_micros() as u64 }).await
}
