//! The client side: autostart-and-connect, plus a small request/response
//! helper the CLI uses so its commands can be thin wrappers over the wire
//! protocol.
//!
//! **Autostart.** Try to connect; if nothing is listening, spawn `exe
//! daemon` detached and retry with a short backoff for up to ~2s. Race-safe
//! by construction, not by any coordination here: the daemon itself binds
//! the global instance lock *before* the transport (see `run_foreground`),
//! so of any number of CLIs racing to autostart, every spawned daemon but
//! one loses the lock and exits immediately — the winner is whichever
//! `connect` eventually reaches.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::daemon::protocol::{to_line, ClientMessage, Method, ServerMessage, PROTOCOL_VERSION};
use crate::daemon::transport;

#[cfg(windows)]
type Conn = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
type Conn = tokio::net::UnixStream;

pub struct Client {
    reader: BufReader<tokio::io::ReadHalf<Conn>>,
    writer: tokio::io::WriteHalf<Conn>,
    next_id: u64,
}

/// The outcome of one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A non-streaming method's answer.
    Result(serde_json::Value),
    /// A query's collected hits, plus the total match count and server-side latency.
    Query { hits: Vec<QueryHit>, total: usize, elapsed_us: u64 },
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryHit {
    pub root: String,
    pub path: String,
    pub score: f32,
    pub line_no: Option<usize>,
    pub snippet: Option<String>,
}

impl Client {
    async fn handshake(conn: Conn) -> std::io::Result<Client> {
        let (rd, mut wr) = tokio::io::split(conn);
        let mut reader = BufReader::new(rd);
        wr.write_all(to_line(&ClientMessage::Hello { protocol: PROTOCOL_VERSION }).as_bytes()).await?;
        wr.flush().await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        match serde_json::from_str::<ServerMessage>(line.trim_end()) {
            Ok(ServerMessage::HelloOk { protocol, .. }) if protocol == PROTOCOL_VERSION => {
                Ok(Client { reader, writer: wr, next_id: 1 })
            }
            Ok(ServerMessage::HelloOk { protocol, .. }) => Err(std::io::Error::other(format!(
                "protocol mismatch: this client speaks {PROTOCOL_VERSION}, the daemon speaks {protocol} — rebuild one to match the other"
            ))),
            Ok(ServerMessage::Error { message, .. }) => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!("unexpected handshake reply: {other:?}"))),
        }
    }

    /// Connect only — no autostart. Used to probe liveness (`ripindex
    /// status`'s "is a daemon even running" check) without side effects.
    #[cfg(windows)]
    pub async fn connect() -> std::io::Result<Client> {
        let conn = transport::connect(&crate::daemon::paths::pipe_name()).await?;
        Self::handshake(conn).await
    }

    #[cfg(unix)]
    pub async fn connect() -> std::io::Result<Client> {
        let conn = transport::connect(&crate::daemon::paths::socket_path()).await?;
        Self::handshake(conn).await
    }

    /// Connect, spawning and waiting for a daemon if none answers.
    pub async fn connect_or_spawn(exe: &Path) -> std::io::Result<Client> {
        match Self::connect().await {
            Ok(c) => return Ok(c),
            Err(e) if !transport::is_not_running(&e) => return Err(e),
            Err(_) => {}
        }
        spawn_detached(exe)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match Self::connect().await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Send one request and collect its full reply (blocking this call
    /// until `Result`/`Error`/`Done` — no pipelining at this layer; a CLI
    /// invocation makes one request and exits, so there is nothing to
    /// pipeline behind).
    pub async fn call(&mut self, method: Method) -> std::io::Result<Reply> {
        let id = self.next_id;
        self.next_id += 1;
        self.writer.write_all(to_line(&ClientMessage::Request { id, method }).as_bytes()).await?;
        self.writer.flush().await?;

        let mut hits = Vec::new();
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon closed the connection mid-response"));
            }
            match serde_json::from_str::<ServerMessage>(line.trim_end()) {
                Ok(ServerMessage::Result { id: rid, value }) if rid == id => return Ok(Reply::Result(value)),
                Ok(ServerMessage::Error { id: rid, message }) if rid == Some(id) || rid.is_none() => return Ok(Reply::Error(message)),
                Ok(ServerMessage::Hit { id: rid, root, path, score, line_no, snippet }) if rid == id => {
                    hits.push(QueryHit { root, path, score, line_no, snippet });
                }
                Ok(ServerMessage::Done { id: rid, total, elapsed_us }) if rid == id => {
                    return Ok(Reply::Query { hits: std::mem::take(&mut hits), total, elapsed_us })
                }
                Ok(other) => log::debug!("ignoring reply for a different request: {other:?}"),
                Err(e) => return Err(std::io::Error::other(format!("malformed reply from daemon: {e}"))),
            }
        }
    }

    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        self.call(Method::Shutdown).await.map(|_| ())
    }
}

#[cfg(windows)]
fn spawn_detached(exe: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    Command::new(exe)
        .arg("daemon")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(unix)]
fn spawn_detached(exe: &Path) -> std::io::Result<()> {
    // A plain spawn already survives the parent CLI process exiting on
    // Unix; full session detachment (`setsid`) is a refinement for signal
    // isolation, not required for autostart to work, and left out here
    // (untested on this — Windows — development sandbox).
    Command::new(exe).arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(())
}
