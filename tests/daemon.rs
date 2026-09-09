//! Socket-level stress tests for the daemon, from `docs`/the M4 spec's build
//! order: protocol round-trip against a *real* transport (partial reads, a
//! malformed line, a client disconnecting mid-stream), concurrent clients
//! querying while a merge and reconciles run, multi-root isolation, idle
//! shutdown, the autostart race, and killed-daemon recovery.
//!
//! Most tests bind their own uniquely-named transport and drive
//! `daemon::server::serve` directly — exactly the pattern `run.rs`'s module
//! docs describe. Only the last two (`autostart_race_*`,
//! `a_killed_daemon_is_recovered_*`) need a *real* `ripindex daemon`
//! subprocess (that's what they're testing), so they redirect the daemon's
//! well-known paths to a throwaway location via the `RIPINDEX_TEST_*` env
//! overrides (see `daemon::paths`'s docs) and are serialized against each
//! other with `ENV_GUARD`, since process env is global state.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

use ripindex::daemon::client::{Client, Reply};
use ripindex::daemon::config::{Config, MergeConfig};
use ripindex::daemon::protocol::{to_line, ClientMessage, Method, ServerMessage, PROTOCOL_VERSION};
use ripindex::daemon::server::{self, Registry};
use ripindex::daemon::transport;

#[cfg(windows)]
type Addr = String;
#[cfg(unix)]
type Addr = PathBuf;

fn unique_addr(tag: &str) -> Addr {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\ripindex-test-{tag}-{}-{n}", std::process::id())
    }
    #[cfg(unix)]
    {
        std::env::temp_dir().join(format!("ripindex-test-{tag}-{}-{n}.sock", std::process::id()))
    }
}

#[cfg(windows)]
async fn connect_raw(addr: &Addr) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    transport::connect(addr).await
}
#[cfg(unix)]
async fn connect_raw(addr: &Addr) -> std::io::Result<tokio::net::UnixStream> {
    transport::connect(addr).await
}

/// Bind a fresh listener at a unique address and run `serve` against it
/// until `external_stop` fires. Returns the registry (so tests can drive
/// roots directly alongside the wire protocol), the address, and the stop
/// signal.
async fn start_server(config: &Config) -> (std::sync::Arc<Registry>, Addr, std::sync::Arc<tokio::sync::Notify>) {
    let addr = unique_addr("srv");
    let registry = Registry::new(config);
    let stop = std::sync::Arc::new(tokio::sync::Notify::new());
    #[cfg(windows)]
    let listener = transport::bind(&addr).expect("bind test pipe");
    #[cfg(unix)]
    let listener = transport::bind(&addr).await.expect("bind test socket");
    tokio::spawn(server::serve(listener, registry.clone(), stop.clone()));
    (registry, addr, stop)
}

async fn handshake<C>(conn: C) -> (BufReader<ReadHalf<C>>, WriteHalf<C>)
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (rd, mut wr) = tokio::io::split(conn);
    let mut reader = BufReader::new(rd);
    wr.write_all(to_line(&ClientMessage::Hello { protocol: PROTOCOL_VERSION }).as_bytes()).await.unwrap();
    wr.flush().await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let msg: ServerMessage = serde_json::from_str(line.trim_end()).expect("valid handshake reply");
    assert!(matches!(msg, ServerMessage::HelloOk { protocol, .. } if protocol == PROTOCOL_VERSION), "{msg:?}");
    (reader, wr)
}

async fn connect_and_handshake(addr: &Addr) -> (impl AsyncBufReadExt + Unpin, impl AsyncWriteExt + Unpin) {
    let conn = connect_raw(addr).await.expect("connect");
    handshake(conn).await
}

async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, id: u64, method: Method) {
    w.write_all(to_line(&ClientMessage::Request { id, method }).as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

async fn read_msg<R: AsyncBufReadExt + Unpin>(r: &mut R) -> ServerMessage {
    let mut line = String::new();
    let n = r.read_line(&mut line).await.unwrap();
    assert!(n > 0, "connection closed unexpectedly");
    serde_json::from_str(line.trim_end()).unwrap_or_else(|e| panic!("malformed line {line:?}: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_round_trip_over_a_real_transport_survives_partial_reads_disconnects_and_garbage() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello alpha world").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta gamma").unwrap();
    let (_registry, addr, stop) = start_server(&Config::default()).await;

    // Partial reads: the hello line arrives in two writes with a gap.
    let conn = connect_raw(&addr).await.unwrap();
    let (rd, mut wr) = tokio::io::split(conn);
    let mut reader = BufReader::new(rd);
    let hello = to_line(&ClientMessage::Hello { protocol: PROTOCOL_VERSION });
    let mid = hello.len() / 2;
    wr.write_all(&hello.as_bytes()[..mid]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    wr.write_all(&hello.as_bytes()[mid..]).await.unwrap();
    wr.flush().await.unwrap();
    let hello_ok = read_msg(&mut reader).await;
    assert!(matches!(hello_ok, ServerMessage::HelloOk { protocol, .. } if protocol == PROTOCOL_VERSION));

    send(&mut wr, 1, Method::AddRoot { path: dir.path().display().to_string() }).await;
    assert!(matches!(read_msg(&mut reader).await, ServerMessage::Result { id: 1, .. }));

    // Malformed JSON: an error tagged with no id, and the connection survives it.
    wr.write_all(b"not json at all\n").await.unwrap();
    wr.flush().await.unwrap();
    assert!(matches!(read_msg(&mut reader).await, ServerMessage::Error { .. }));

    // The same connection still answers a normal query afterward.
    send(&mut wr, 2, Method::Query { roots: None, query: "alpha".into(), limit: 10, offset: 0, snippet: true }).await;
    let mut hits = 0;
    loop {
        match read_msg(&mut reader).await {
            ServerMessage::Hit { id: 2, .. } => hits += 1,
            ServerMessage::Done { id: 2, total, .. } => {
                assert_eq!(total, 1);
                break;
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    assert_eq!(hits, 1);

    // A client that sends a request and disconnects before reading the
    // reply must not wedge the server for anyone else.
    {
        let conn2 = connect_raw(&addr).await.unwrap();
        let (reader2, mut wr2) = handshake(conn2).await;
        send(&mut wr2, 9, Method::Query { roots: None, query: "beta".into(), limit: 10, offset: 0, snippet: false }).await;
        drop(wr2);
        drop(reader2);
    }
    let conn3 = connect_raw(&addr).await.unwrap();
    let (mut reader3, mut wr3) = handshake(conn3).await;
    send(&mut wr3, 3, Method::Status).await;
    assert!(matches!(read_msg(&mut reader3).await, ServerMessage::Result { id: 3, .. }), "server wedged after a mid-stream disconnect");

    stop.notify_one();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn concurrent_clients_query_successfully_while_merge_and_reconcile_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("seed.txt"), "alpha seed").unwrap();
    let config = Config { merge: MergeConfig { tier_size: 2, ..MergeConfig::default() }, ..Config::default() };
    let (registry, addr, stop) = start_server(&config).await;

    {
        let (mut reader, mut wr) = connect_and_handshake(&addr).await;
        send(&mut wr, 1, Method::AddRoot { path: dir.path().display().to_string() }).await;
        assert!(matches!(read_msg(&mut reader).await, ServerMessage::Result { .. }));
    }
    let handle = registry.get(&dir.path().display().to_string()).await.expect("root registered");
    // Fragment the corpus into several segments so there's real merge work.
    for i in 0..10 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("alpha extra{i}")).unwrap();
        handle.reconcile().await.unwrap();
    }
    let segments_before = handle.status().await.unwrap().segments;
    assert!(segments_before > 2, "expected several segments to merge, got {segments_before}");

    let mut query_tasks = Vec::new();
    for _ in 0..6 {
        let addr = addr.clone();
        query_tasks.push(tokio::spawn(async move {
            for _ in 0..15 {
                let (mut reader, mut wr) = connect_and_handshake(&addr).await;
                send(&mut wr, 1, Method::Query { roots: None, query: "alpha".into(), limit: 20, offset: 0, snippet: false }).await;
                loop {
                    match read_msg(&mut reader).await {
                        ServerMessage::Hit { .. } => {}
                        ServerMessage::Done { total, .. } => {
                            assert!(total >= 1, "a query mid-merge/reconcile returned no hits");
                            break;
                        }
                        other => panic!("query failed mid-merge/reconcile: {other:?}"),
                    }
                }
            }
        }));
    }

    let merge_task = {
        let handle = handle.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                handle.merge().await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = handle.reconcile().await;
            }
        })
    };

    for t in query_tasks {
        t.await.expect("a query task panicked");
    }
    merge_task.await.unwrap();

    let mut merged = false;
    for _ in 0..150 {
        if let Some(st) = handle.status().await {
            if !st.merge_running && st.last_merge_result.is_some() {
                merged = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(merged, "no background merge completed during the stress run");
    stop.notify_one();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_root_does_not_break_reconcile_or_query_on_another_root() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("a.txt"), "alpha in a").unwrap();
    std::fs::write(dir_b.path().join("b.txt"), "alpha in b").unwrap();
    let (registry, addr, stop) = start_server(&Config::default()).await;
    registry.add_root(&dir_a.path().display().to_string()).await.unwrap();
    registry.add_root(&dir_b.path().display().to_string()).await.unwrap();

    // Drop A's actor (and with it its mmap'd `LiveIndex`) before corrupting
    // its segment data on disk — Windows refuses to overwrite a file with a
    // live mapped section, and dropping the reader is exactly what would
    // really happen before something external clobbers a root's index
    // (the daemon isn't running against it at that instant either).
    registry.remove_root(&dir_a.path().display().to_string()).await;
    // A segment file whose bytes no longer match the manifest's recorded
    // CRC is a genuine, non-self-healing `Error::Corrupt` — unlike a
    // corrupt/missing *manifest*, which `update_index` recovers from by
    // rebuilding (that's the crash-safety this project promises, not a bug).
    // `remove_root` drops the actor's `Arc<LiveIndex>`, but its mmap isn't
    // guaranteed unmapped the instant the drop call returns, so retry
    // briefly rather than racing it.
    let seg_path = dir_a.path().join(".ripindex").join("seg-00000.idx");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::write(&seg_path, b"not a valid segment, but the right file name") {
            Ok(()) => break,
            Err(e) if std::time::Instant::now() < deadline => {
                log::debug!("segment still mapped, retrying corruption write: {e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("segment file never became writable: {e}"),
        }
    }

    let (mut reader, mut wr) = connect_and_handshake(&addr).await;

    // Re-registering the now-corrupt root must fail cleanly, not wedge or
    // crash the daemon serving everyone else.
    send(&mut wr, 1, Method::AddRoot { path: dir_a.path().display().to_string() }).await;
    assert!(matches!(read_msg(&mut reader).await, ServerMessage::Error { .. }), "corrupt root A should fail to (re-)register, not succeed");

    // Root B must still be fully queryable despite A being broken.
    send(&mut wr, 2, Method::Query { roots: Some(vec![dir_b.path().display().to_string()]), query: "alpha".into(), limit: 10, offset: 0, snippet: false })
        .await;
    let mut b_hits = 0;
    loop {
        match read_msg(&mut reader).await {
            ServerMessage::Hit { .. } => b_hits += 1,
            ServerMessage::Done { total, .. } => {
                assert_eq!(total, 1);
                break;
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    assert_eq!(b_hits, 1);

    // And a fan-out query across every currently-registered root (just B,
    // since A never made it back in) must not be affected by A's breakage.
    send(&mut wr, 3, Method::Query { roots: None, query: "alpha".into(), limit: 10, offset: 0, snippet: false }).await;
    loop {
        match read_msg(&mut reader).await {
            ServerMessage::Hit { .. } => {}
            ServerMessage::Done { total, .. } => {
                assert_eq!(total, 1);
                break;
            }
            other => panic!("fan-out query broke because of a corrupt root: {other:?}"),
        }
    }

    stop.notify_one();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_shutdown_waits_for_the_last_connection_to_close() {
    let (registry, addr, stop) = start_server(&Config::default()).await;
    ripindex::daemon::run::spawn_idle_shutdown(registry.clone(), stop.clone(), Duration::from_millis(200));

    // Hold a connection open across (more than) the idle window: shutdown must not fire.
    let (mut reader, mut wr) = connect_and_handshake(&addr).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    send(&mut wr, 1, Method::Status).await;
    assert!(matches!(read_msg(&mut reader).await, ServerMessage::Result { id: 1, .. }), "server idle-shut-down despite a live connection");

    // Once the connection closes, idle shutdown should fire within a bounded wait.
    drop(wr);
    drop(reader);
    let mut down = false;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if connect_raw(&addr).await.is_err() {
            down = true;
            break;
        }
    }
    assert!(down, "daemon did not idle-shut-down after its last connection closed");
}

/// Serializes the two tests below, which mutate process-wide environment
/// variables to redirect the daemon's well-known paths — process env is
/// global state, and `cargo test` runs test functions concurrently by
/// default.
static ENV_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII guard: points every `RIPINDEX_TEST_*` override at a fresh temp
/// location for the lifetime of one test, then clears them.
struct EnvOverride {
    _dir: tempfile::TempDir,
}

impl EnvOverride {
    fn install() -> Self {
        // Every isolated env must get its own pipe name/socket path too, not
        // just its own lock-file directory: `std::process::id()` alone is
        // constant across every test in this binary, so two subprocess
        // tests sharing it would also share a pipe name — letting a
        // lingering daemon from one test answer (and then, mid-shutdown,
        // drop) a connection meant for the other's freshly-spawned daemon.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // Only the Windows pipe name needs this: it lives in a global kernel
        // namespace, so two tests in one binary would otherwise collide. The Unix
        // socket path is already unique because it sits inside `dir`.
        #[cfg_attr(unix, allow(unused_variables))]
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RIPINDEX_TEST_RUNTIME_DIR", dir.path().join("run"));
            std::env::set_var("RIPINDEX_TEST_STATE_DIR", dir.path().join("state"));
            #[cfg(windows)]
            std::env::set_var("RIPINDEX_TEST_PIPE_NAME", format!(r"\\.\pipe\ripindex-test-daemon-{}-{n}", std::process::id()));
            #[cfg(unix)]
            std::env::set_var("RIPINDEX_TEST_SOCKET_PATH", dir.path().join("daemon.sock"));
        }
        Self { _dir: dir }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("RIPINDEX_TEST_RUNTIME_DIR");
            std::env::remove_var("RIPINDEX_TEST_STATE_DIR");
            #[cfg(windows)]
            std::env::remove_var("RIPINDEX_TEST_PIPE_NAME");
            #[cfg(unix)]
            std::env::remove_var("RIPINDEX_TEST_SOCKET_PATH");
        }
    }
}

async fn status_pid(client: &mut Client) -> u64 {
    match client.call(Method::Status).await.unwrap() {
        Reply::Result(v) => v["pid"].as_u64().expect("status carries a pid"),
        other => panic!("unexpected status reply: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn autostart_race_resolves_to_exactly_one_daemon() {
    let _guard = ENV_GUARD.lock().await;
    let _env = EnvOverride::install();

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_ripindex"));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha race").unwrap();
    let root = dir.path().display().to_string();

    let mut tasks = Vec::new();
    for _ in 0..20 {
        let exe = exe.clone();
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            let mut client = Client::connect_or_spawn(&exe).await.expect("connect_or_spawn");
            client.call(Method::AddRoot { path: root.clone() }).await.unwrap();
            let reply = client.call(Method::Query { roots: Some(vec![root]), query: "alpha".into(), limit: 10, offset: 0, snippet: false }).await.unwrap();
            let pid = status_pid(&mut client).await;
            (reply, pid)
        }));
    }
    let mut pids = std::collections::HashSet::new();
    for t in tasks {
        let (reply, pid) = t.await.expect("a racing client task panicked");
        match reply {
            Reply::Query { total, .. } => assert_eq!(total, 1),
            other => panic!("unexpected reply: {other:?}"),
        }
        pids.insert(pid);
    }
    assert_eq!(pids.len(), 1, "more than one daemon process answered the race: {pids:?}");

    let mut client = Client::connect().await.unwrap();
    let _ = client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_daemon_is_recovered_by_the_next_autostart() {
    let _guard = ENV_GUARD.lock().await;
    let _env = EnvOverride::install();

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_ripindex"));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha killed").unwrap();
    let root = dir.path().display().to_string();

    // Spawn directly (not via connect_or_spawn) so the `Child` handle survives to be killed.
    let mut child = std::process::Command::new(&exe).arg("daemon").spawn().expect("spawn daemon");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if Client::connect().await.is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon never came up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    {
        let mut client = Client::connect().await.unwrap();
        client.call(Method::AddRoot { path: root.clone() }).await.unwrap();
        let reply = client.call(Method::Query { roots: Some(vec![root.clone()]), query: "alpha".into(), limit: 10, offset: 0, snippet: false }).await.unwrap();
        assert!(matches!(reply, Reply::Query { total: 1, .. }), "{reply:?}");
    }

    let _ = child.kill();
    let _ = child.wait();

    // Immediately query again: autostart should recover transparently, with
    // no stale socket/lock getting in the way.
    let mut client = Client::connect_or_spawn(&exe).await.expect("recovery autostart after kill");
    client.call(Method::AddRoot { path: root.clone() }).await.unwrap();
    let reply = client.call(Method::Query { roots: Some(vec![root]), query: "alpha".into(), limit: 10, offset: 0, snippet: false }).await.unwrap();
    assert!(matches!(reply, Reply::Query { total: 1, .. }), "{reply:?}");

    let _ = client.shutdown().await;
}
