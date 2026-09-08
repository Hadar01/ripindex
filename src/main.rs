//! `ripindex` CLI.
//!
//! `search`/`update`/`merge` prefer the daemon when one is reachable (and,
//! for `search` only, autostart one if none is): every mutation to a root
//! then goes through the daemon's single-writer actor rather than racing a
//! direct write against it. If the daemon can't be reached at all (refused
//! to start, a transport error), these fall back to the direct, no-daemon
//! path this project has always had — degraded (no live watcher, no
//! background merge) but functional. `index`/`verify`/`bench` are always
//! direct: diagnostic tools with no reason to involve a long-lived process.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};

use ripindex::crawler::CrawlConfig;
use ripindex::daemon::client::{Client, Reply};
use ripindex::daemon::protocol::Method;
use ripindex::daemon::run as daemon_run;
use ripindex::daemon::transport;
use ripindex::fs::RealFs;
use ripindex::index::{BuildConfig, Index, IndexReader};
use ripindex::query::{self, Query, SearchOptions};
use ripindex::report::{commas, human_bytes, human_duration};
use ripindex::store::{self, BuildProgress, OpenOptions};
use ripindex::{bench, snippet};

/// Snippet width in characters.
const SNIPPET_WIDTH: usize = 160;

#[derive(Parser)]
#[command(name = "ripindex", version, about = "Indexed code and text search with a crash-safe on-disk index and a background daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build PATH's index now and print statistics
    ///
    /// Crawls PATH, writes the index under PATH/.ripindex, and reports what it
    /// indexed. Always direct — never involves the daemon.
    Index {
        path: PathBuf,
    },
    /// Search an indexed directory
    ///
    /// Prefers the daemon, autostarting one if none answers, and indexes on
    /// first use. Falls back to a direct, no-daemon build and search if the
    /// daemon cannot be reached at all.
    Search {
        /// Query: terms, AND / OR, "quoted phrase", -negated, ( grouping ).
        /// Starts with `-`? Put `--` before it so it isn't read as a flag.
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Indexed directory. Falls back to $RIPINDEX_ROOT, then `.`.
        #[arg(long, env = "RIPINDEX_ROOT", default_value = ".")]
        root: PathBuf,
        /// Maximum hits to print.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
        /// Disable ANSI highlighting (auto-disabled when stdout is not a TTY).
        #[arg(long)]
        no_color: bool,
        /// Skip the daemon entirely, even if one is running.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Verify every checksum in PATH's index
    ///
    /// Reads and checks each file's CRC and structure. Always direct.
    Verify {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Measure build, open, query and memory cost
    ///
    /// Builds an index over PATH and reports crawl/build time, cold and warm
    /// open, per-query percentiles, and RSS. Always direct.
    Bench {
        path: PathBuf,
        /// Query to time (repeatable). Defaults to a built-in set.
        #[arg(long = "query", short = 'q')]
        queries: Vec<String>,
        /// Timed runs per query.
        #[arg(long, default_value_t = 20)]
        iterations: u32,
    },
    /// Reconcile PATH's index with the filesystem once
    ///
    /// Routed through the daemon if one is *already* running; direct
    /// otherwise. Deliberately never autostarts a daemon — a one-shot update
    /// should not leave a persistent process behind.
    Update {
        path: PathBuf,
    },
    /// Compact small and heavily-deleted segments
    ///
    /// Merges them in a single manifest commit. Same
    /// daemon-if-already-running, no-autostart rule as `update`.
    Merge {
        path: PathBuf,
    },
    /// Watch PATH and reindex on change, without the daemon
    ///
    /// Runs in the foreground until interrupted (Ctrl+C). To watch several
    /// roots from one process instead, run `ripindex daemon` and add roots
    /// over its socket.
    Watch {
        path: PathBuf,
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
        #[arg(long, default_value_t = 600)]
        periodic_reconcile_secs: u64,
    },
    /// Run or control the background daemon
    ///
    /// Bare `ripindex daemon` runs it in the foreground. That is exactly what
    /// autostart execs, so it takes no required subcommand.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
    /// Show what the daemon is doing
    ///
    /// Per-root document and segment counts, disk usage, last reconcile, and
    /// whether a merge is running. Reports cleanly when no daemon is up.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Print (never install) the systemd user unit or launchd agent that
    /// would start the daemon on login, and the command to install it.
    InstallHint,
    /// Ask a running daemon to shut down gracefully. A no-op, successfully,
    /// if none is running.
    Stop,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { path } => cmd_index(&path),
        Cmd::Search { query, root, limit, no_color, no_daemon } => cmd_search(&root, &query, limit, !no_color, no_daemon),
        Cmd::Verify { path } => cmd_verify(&path),
        Cmd::Bench { path, queries, iterations } => cmd_bench(&path, &queries, iterations),
        Cmd::Update { path } => cmd_update(&path),
        Cmd::Merge { path } => cmd_merge(&path),
        Cmd::Watch { path, debounce_ms, periodic_reconcile_secs } => cmd_watch(&path, debounce_ms, periodic_reconcile_secs),
        Cmd::Daemon { action } => cmd_daemon(action),
        Cmd::Status { json } => cmd_status(json),
    }
}

/// One current-thread runtime for whichever command needs one — a CLI
/// invocation does one thing and exits, so there is no benefit to a
/// multi-thread runtime here (the daemon itself, a genuinely concurrent
/// long-lived process, uses `rt-multi-thread`).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("building a single-threaded tokio runtime").block_on(fut)
}

/// Progress on stderr: a live line on a TTY, milestones otherwise.
struct StderrProgress {
    tty: bool,
    last: Mutex<Instant>,
}

impl StderrProgress {
    fn new() -> Self {
        Self { tty: std::io::stderr().is_terminal(), last: Mutex::new(Instant::now() - Duration::from_secs(1)) }
    }

    fn throttled(&self) -> bool {
        let mut last = self.last.lock().unwrap();
        if last.elapsed() < Duration::from_millis(100) {
            return true;
        }
        *last = Instant::now();
        false
    }
}

impl BuildProgress for StderrProgress {
    fn crawling(&self, files_seen: u64) {
        if self.tty && !self.throttled() {
            eprint!("\r  crawling: {} files seen", commas(files_seen));
        }
    }

    fn crawled(&self, indexable: u64) {
        if self.tty {
            eprint!("\r\x1b[K");
        }
        eprintln!("  crawled: {} indexable files", commas(indexable));
    }

    fn indexed(&self, docs: u64, total: u64, segments: u32) {
        if self.tty && (!self.throttled() || docs == total) {
            eprint!("\r  indexing: {} / {} files, {segments} segments written", commas(docs), commas(total));
            if docs == total {
                eprintln!();
            }
        }
    }
}

fn build(root: &Path) -> anyhow::Result<Index> {
    let index = store::build_index(&RealFs, root, &CrawlConfig::default(), &BuildConfig::default(), &StderrProgress::new())?;
    Ok(index)
}

fn cmd_index(path: &Path) -> anyhow::Result<()> {
    eprintln!("indexing {} into {}", path.display(), store::index_dir(path).display());
    let index = build(path)?;
    print!("{}", index.stats());
    Ok(())
}

/// Open, or build when absent — announced before the crawl, since indexing a
/// large tree can take minutes.
fn open_or_build(root: &Path) -> anyhow::Result<Index> {
    match store::open(&RealFs, root, &OpenOptions::default())? {
        Some(index) => Ok(index),
        None => {
            eprintln!(
                "no index at {} — building one now (indexes every text file under {})",
                store::index_dir(root).display(),
                root.display()
            );
            build(root)
        }
    }
}

async fn daemon_query(root: &Path, query_str: &str, limit: usize) -> anyhow::Result<Reply> {
    let exe = std::env::current_exe()?;
    let mut client = Client::connect_or_spawn(&exe).await?;
    let root_str = root.display().to_string();
    if let Reply::Error(e) = client.call(Method::AddRoot { path: root_str.clone() }).await? { anyhow::bail!("daemon: add_root failed: {e}") }
    let reply = client
        .call(Method::Query { roots: Some(vec![root_str]), query: query_str.to_string(), limit, offset: 0, snippet: true })
        .await?;
    Ok(reply)
}

fn cmd_search(root: &Path, query_str: &str, limit: usize, color: bool, no_daemon: bool) -> anyhow::Result<()> {
    // Parse first, always, so a bad query fails fast whichever path we take.
    let query = Query::parse(query_str).with_context(|| format!("invalid query {query_str:?}"))?;

    if !no_daemon {
        // Bounded, generously: a cold daemon may need to build a fresh
        // index for a large root before it can answer, which can
        // legitimately take a while and must not be mistaken for a hang.
        // But *some* bound is required — with none at all, a daemon that's
        // wedged or has crashed mid-response leaves `Client::call` blocked
        // on `read_line` forever, taking every future search down with it.
        // 30s comfortably covers a warm daemon (milliseconds) and a sizable
        // first build, while still eventually recovering from a stuck one.
        match block_on(async { tokio::time::timeout(Duration::from_secs(30), daemon_query(root, query_str, limit)).await }) {
            Ok(Ok(Reply::Query { hits, total, elapsed_us })) => {
                print_daemon_hits(&hits, color)?;
                eprintln!(
                    "{} of {total} matching files shown (via daemon); query took {}",
                    hits.len(),
                    human_duration(Duration::from_micros(elapsed_us))
                );
                return Ok(());
            }
            Ok(Ok(Reply::Error(e))) => {
                log::warn!("daemon query failed ({e}); falling back to a direct search");
            }
            Ok(Ok(Reply::Result(_))) => log::warn!("daemon returned an unexpected reply shape; falling back to a direct search"),
            Ok(Err(e)) => log::debug!("daemon unreachable ({e}); falling back to a direct search"),
            Err(_elapsed) => log::warn!("daemon did not respond within 30s; falling back to a direct search"),
        }
    }
    cmd_search_direct(root, &query, limit, color)
}

fn print_daemon_hits(hits: &[ripindex::daemon::client::QueryHit], color: bool) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for hit in hits {
        match (&hit.snippet, hit.line_no) {
            (Some(s), Some(line)) => {
                let rendered = if color { s.clone() } else { strip_ansi(s) };
                writeln!(out, "{:>8.3}  {}:{line}\n          {rendered}", hit.score, hit.path)?;
            }
            _ => writeln!(out, "{:>8.3}  {}\n          (no snippet: file changed or unreadable since indexing)", hit.score, hit.path)?,
        }
    }
    out.flush()?;
    Ok(())
}

/// The daemon always renders snippets in plain `[bracket]` form (it has no
/// notion of the client's terminal); this only strips them back out if the
/// user asked for `--no-color`, since brackets aren't ANSI and need no
/// stripping — kept trivial rather than pretending the daemon path supports
/// real ANSI highlighting today.
fn strip_ansi(s: &str) -> String {
    s.to_string()
}

/// The original, no-daemon path: build (if needed) and search in this
/// process. What every `search` used before M4, and the fallback M4 keeps.
fn cmd_search_direct(root: &Path, query: &Query, limit: usize, color: bool) -> anyhow::Result<()> {
    let index = open_or_build(root)?;

    let t = Instant::now();
    let result = query::search(&index, query, &SearchOptions { limit, ..Default::default() });
    let query_time = t.elapsed();

    let color = color && std::io::stdout().is_terminal();
    let terms = query.highlight_terms();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for hit in &result.hits {
        let Some(meta) = index.doc(hit.doc) else { continue };
        match snippet::snippet_for(&meta.path, &terms, SNIPPET_WIDTH) {
            Some(s) => writeln!(
                out,
                "{:>8.3}  {}:{}\n          {}",
                hit.score,
                meta.path.display(),
                s.line_no,
                snippet::render(&s, color)
            )?,
            None => writeln!(
                out,
                "{:>8.3}  {}\n          (no snippet: file changed or unreadable since indexing)",
                hit.score,
                meta.path.display()
            )?,
        }
    }
    out.flush()?;

    let stats = index.stats();
    eprintln!(
        "{} of {} matching files shown; {} files indexed in {} segments; opened in {}; query took {}",
        result.hits.len(),
        result.total_matches,
        commas(stats.docs_indexed as u64),
        stats.segments,
        human_duration(stats.open_time),
        human_duration(query_time),
    );
    Ok(())
}

fn cmd_verify(path: &Path) -> anyhow::Result<()> {
    let t = Instant::now();
    let index = store::open(&RealFs, path, &OpenOptions { verify: true })?
        .ok_or_else(|| ripindex::Error::NoIndex(store::index_dir(path)))?;
    let s = index.stats();
    println!(
        "OK: generation {}, {} segments, {} docs ({} indexed), {} on disk, verified in {}",
        index.manifest().generation,
        s.segments,
        commas(s.docs_total as u64),
        commas(s.docs_indexed as u64),
        human_bytes(s.on_disk_bytes),
        human_duration(t.elapsed()),
    );
    Ok(())
}

fn cmd_bench(path: &Path, queries: &[String], iterations: u32) -> anyhow::Result<()> {
    let report = bench::run(path, queries, iterations)?;
    print!("{report}");
    Ok(())
}

/// Reachable-daemon check with **no autostart** — `update`/`merge` must
/// never spin up a persistent daemon on their own (see the module docs).
async fn connect_if_running() -> Option<Client> {
    Client::connect().await.ok()
}

fn cmd_update(path: &Path) -> anyhow::Result<()> {
    // Same normalisation the daemon keys roots by, or the lookup misses.
    let root_str = ripindex::daemon::paths::normalize_root(&path.display().to_string()).display().to_string();
    if let Some(mut client) = block_on(connect_if_running()) {
        match block_on(client.call(Method::AddRoot { path: root_str.clone() })) {
            Ok(Reply::Error(e)) => log::warn!("daemon add_root failed ({e}); reconciling directly instead"),
            Ok(_) => match block_on(client.call(Method::Reconcile { root: Some(root_str.clone()), subtree: None })) {
                Ok(Reply::Result(v)) => {
                    println!("reconciled via daemon: {}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    return Ok(());
                }
                Ok(Reply::Error(e)) => log::warn!("daemon reconcile failed ({e}); reconciling directly instead"),
                Err(e) => log::warn!("daemon call failed ({e}); reconciling directly instead"),
                Ok(_) => log::warn!("daemon returned an unexpected reply; reconciling directly instead"),
            },
            Err(e) => log::warn!("daemon call failed ({e}); reconciling directly instead"),
        }
    }
    eprintln!("reconciling {} against {}", store::index_dir(path).display(), path.display());
    let index = store::update_index(&RealFs, path, &CrawlConfig::default(), &BuildConfig::default(), &StderrProgress::new())?;
    print!("{}", index.stats());
    Ok(())
}

fn cmd_merge(path: &Path) -> anyhow::Result<()> {
    // Same normalisation the daemon keys roots by, or the lookup misses.
    let root_str = ripindex::daemon::paths::normalize_root(&path.display().to_string()).display().to_string();
    if let Some(mut client) = block_on(connect_if_running()) {
        if let Ok(Reply::Result(_)) = block_on(client.call(Method::AddRoot { path: root_str.clone() })) {
            match block_on(client.call(Method::Merge { root: Some(root_str) })) {
                Ok(Reply::Result(v)) => {
                    println!("merge requested via daemon: {}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    return Ok(());
                }
                _ => log::warn!("daemon merge request failed; merging directly instead"),
            }
        }
    }
    let before = store::open(&RealFs, path, &OpenOptions::default())?
        .ok_or_else(|| ripindex::Error::NoIndex(store::index_dir(path)))?
        .stats()
        .segments;
    let index = store::merge_index(&RealFs, path, &store::MergePolicy::default())?;
    println!("generation {}: {} segments -> {} segments", index.manifest().generation, before, index.stats().segments);
    Ok(())
}

/// Runs until interrupted; every trigger reconciles the whole tree once.
/// Standalone — see the module docs on why this doesn't touch the daemon.
fn cmd_watch(path: &Path, debounce_ms: u64, periodic_reconcile_secs: u64) -> anyhow::Result<()> {
    if !path.is_dir() {
        return Err(ripindex::Error::NotADirectory(path.to_path_buf()).into());
    }
    eprintln!("watching {} (Ctrl+C to stop)", path.display());
    let mut source = store::NotifySource::watch(path).map_err(|e| anyhow::anyhow!("failed to start the watcher: {e}"))?;
    let clock = store::RealClock::new();
    let cfg = store::WatchConfig { debounce_ms, periodic_reconcile_ms: periodic_reconcile_secs.saturating_mul(1000) };
    let progress = StderrProgress::new();
    store::run_watcher(&mut source, &clock, &cfg, || false, || match store::update_index(
        &RealFs,
        path,
        &CrawlConfig::default(),
        &BuildConfig::default(),
        &progress,
    ) {
        Ok(index) => eprintln!(
            "reconciled: generation {}, {} docs, {} segments",
            index.manifest().generation,
            commas(index.docs().len() as u64),
            index.stats().segments
        ),
        Err(e) => eprintln!("update failed: {e}"),
    });
    Ok(())
}

fn cmd_daemon(action: Option<DaemonAction>) -> anyhow::Result<()> {
    match action {
        None => block_on(daemon_run::run_foreground()),
        Some(DaemonAction::InstallHint) => {
            let exe = std::env::current_exe()?.display().to_string();
            print!("{}", daemon_run::install_hint(&exe));
            Ok(())
        }
        Some(DaemonAction::Stop) => block_on(async {
            match Client::connect().await {
                Ok(mut client) => {
                    client.shutdown().await?;
                    println!("daemon stopped");
                    Ok(())
                }
                Err(e) if transport::is_not_running(&e) => {
                    println!("no daemon was running");
                    Ok(())
                }
                Err(e) => Err(e.into()),
            }
        }),
    }
}

fn cmd_status(json: bool) -> anyhow::Result<()> {
    let reply = block_on(async {
        let mut client = Client::connect().await?;
        client.call(Method::Status).await
    });
    match reply {
        Ok(Reply::Result(v)) => {
            if json {
                println!("{v}");
            } else {
                println!("ripindex daemon: pid {}, up {}s, {} connections", v["pid"], v["uptime_secs"], v["connections"]);
                if let Some(roots) = v["roots"].as_array() {
                    if roots.is_empty() {
                        println!("  (no roots registered)");
                    }
                    for r in roots {
                        println!(
                            "  {}: {} docs, {} segments, {} on disk, generation {}{}",
                            r["root"].as_str().unwrap_or("?"),
                            r["docs"],
                            r["segments"],
                            human_bytes(r["on_disk_bytes"].as_u64().unwrap_or(0)),
                            r["generation"],
                            if r["merge_running"].as_bool().unwrap_or(false) { " (merging)" } else { "" }
                        );
                        if !r["watcher_healthy"].as_bool().unwrap_or(true) {
                            println!("    warning: watcher is not healthy for this root");
                        }
                    }
                }
            }
            Ok(())
        }
        Ok(other) => anyhow::bail!("unexpected reply from daemon: {other:?}"),
        Err(_) => {
            if json {
                println!("{{\"running\": false}}");
            } else {
                println!("no ripindex daemon is running (`ripindex search` will autostart one; `ripindex daemon install-hint` for autostart-on-login)");
            }
            Ok(())
        }
    }
}
