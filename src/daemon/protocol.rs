//! The daemon's wire protocol: newline-delimited JSON, one message per line,
//! responses tagged with the originating request's `id` so a client can
//! pipeline several requests over one connection. A query streams `Hit`
//! messages followed by one `Done`; every other method replies with one
//! `Result` (or `Error`).
//!
//! Versioned from message one: the very first line each side sends is a
//! [`ClientMessage::Hello`] / [`ServerMessage::HelloOk`] handshake naming
//! [`PROTOCOL_VERSION`]; a mismatch is a clear, immediate error rather than
//! a confusing failure three requests later when a stale plugin meets a
//! newer daemon (or vice versa).

use serde::{Deserialize, Serialize};

/// Bump on any wire-incompatible change to the message shapes below.
pub const PROTOCOL_VERSION: u32 = 1;

fn default_limit() -> usize {
    20
}

fn default_true() -> bool {
    true
}

/// One line a client sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Must be the first line on a new connection.
    Hello { protocol: u32 },
    Request {
        id: u64,
        #[serde(flatten)]
        method: Method,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    /// `roots: None` searches every root the daemon has. Paths are
    /// canonical-ish strings as given to `add_root`, not required to be
    /// absolute on the wire (the daemon resolves them).
    Query {
        #[serde(default)]
        roots: Option<Vec<String>>,
        query: String,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_true")]
        snippet: bool,
    },
    AddRoot {
        path: String,
    },
    RemoveRoot {
        path: String,
    },
    ListRoots,
    Status,
    /// Force a reconcile. `subtree`, when given, is a hint passed to the
    /// crawl (still a whole-root reconcile underneath — see the watcher's
    /// module docs on why per-subtree scoping isn't wired yet); recorded so
    /// the caller's intent shows up in the status/log even though today it
    /// doesn't narrow the work.
    Reconcile {
        #[serde(default)]
        root: Option<String>,
        #[serde(default)]
        subtree: Option<String>,
    },
    Merge {
        #[serde(default)]
        root: Option<String>,
    },
    Shutdown,
}

/// One line the daemon sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    HelloOk { protocol: u32, pid: u32 },
    Hit {
        id: u64,
        root: String,
        path: String,
        score: f32,
        line_no: Option<usize>,
        snippet: Option<String>,
    },
    Done {
        id: u64,
        total: usize,
        elapsed_us: u64,
    },
    Result {
        id: u64,
        value: serde_json::Value,
    },
    /// `id: None` only for a handshake failure or a line that failed to
    /// parse at all (so there was no id to echo).
    Error {
        id: Option<u64>,
        message: String,
    },
}

impl ServerMessage {
    pub fn error(id: Option<u64>, message: impl Into<String>) -> Self {
        ServerMessage::Error { id, message: message.into() }
    }

    pub fn result(id: u64, value: impl Serialize) -> Self {
        ServerMessage::Result { id, value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null) }
    }
}

/// One physical line of input, already stripped of its trailing `\n`/`\r\n`.
/// Serialized with a compact `to_line`/parsed with `parse_line` — never with
/// bare `serde_json::to_string`/`from_str` at the call site, so every
/// message is guaranteed newline-free (a stray `\n` inside a value would
/// otherwise silently desynchronize the framing).
pub fn to_line(msg: &impl Serialize) -> String {
    let mut s = serde_json::to_string(msg).expect("protocol messages always serialize");
    debug_assert!(!s.contains('\n'), "a protocol message must never itself contain a newline");
    s.push('\n');
    s
}

/// Parse one line as a [`ClientMessage`]. On failure, returns a best-effort
/// `id` (extracted from the raw JSON if that much parsed, even when the
/// typed shape didn't) alongside a human-readable reason, so the server can
/// still tag its `Error` reply correctly when only `params` was malformed.
pub fn parse_client_line(line: &str) -> Result<ClientMessage, (Option<u64>, String)> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return Err((None, "empty line".into()));
    }
    match serde_json::from_str::<ClientMessage>(trimmed) {
        Ok(m) => Ok(m),
        Err(e) => {
            let id = serde_json::from_str::<serde_json::Value>(trimmed).ok().and_then(|v| v.get("id")?.as_u64());
            Err((id, format!("malformed request: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips() {
        let m = ClientMessage::Hello { protocol: PROTOCOL_VERSION };
        let line = to_line(&m);
        assert!(line.ends_with('\n'));
        assert_eq!(parse_client_line(&line), Ok(m));
    }

    #[test]
    fn every_method_roundtrips() {
        let methods = vec![
            Method::Query { roots: None, query: "alpha".into(), limit: 20, offset: 0, snippet: true },
            Method::Query { roots: Some(vec!["/a".into(), "/b".into()]), query: "\"quoted phrase\"".into(), limit: 5, offset: 10, snippet: false },
            Method::AddRoot { path: "/some/path".into() },
            Method::RemoveRoot { path: "/some/path".into() },
            Method::ListRoots,
            Method::Status,
            Method::Reconcile { root: Some("/a".into()), subtree: Some("src".into()) },
            Method::Reconcile { root: None, subtree: None },
            Method::Merge { root: None },
            Method::Shutdown,
        ];
        for (i, method) in methods.into_iter().enumerate() {
            let req = ClientMessage::Request { id: i as u64, method };
            let line = to_line(&req);
            assert!(!line.trim_end().contains('\n'));
            assert_eq!(parse_client_line(&line), Ok(req), "line: {line}");
        }
    }

    #[test]
    fn defaults_fill_in_missing_query_fields() {
        let line = r#"{"type":"request","id":1,"method":"query","params":{"query":"alpha"}}"#;
        let parsed = parse_client_line(line).unwrap();
        assert_eq!(
            parsed,
            ClientMessage::Request { id: 1, method: Method::Query { roots: None, query: "alpha".into(), limit: 20, offset: 0, snippet: true } }
        );
    }

    #[test]
    fn server_messages_roundtrip() {
        let msgs = vec![
            ServerMessage::HelloOk { protocol: 1, pid: 4242 },
            ServerMessage::Hit { id: 3, root: "/r".into(), path: "/r/a.txt".into(), score: 1.5, line_no: Some(4), snippet: Some("[hit]".into()) },
            ServerMessage::Hit { id: 3, root: "/r".into(), path: "/r/b.txt".into(), score: 0.9, line_no: None, snippet: None },
            ServerMessage::Done { id: 3, total: 2, elapsed_us: 123 },
            ServerMessage::result(4, serde_json::json!({"ok": true})),
            ServerMessage::error(Some(5), "no such root"),
            ServerMessage::error(None, "protocol mismatch"),
        ];
        for m in msgs {
            let line = to_line(&m);
            let back: ServerMessage = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn malformed_json_has_no_id() {
        let (id, msg) = parse_client_line("{not json at all").unwrap_err();
        assert_eq!(id, None);
        assert!(msg.contains("malformed"));
    }

    #[test]
    fn malformed_params_still_recovers_the_id() {
        // Valid JSON, valid envelope shape, but `params` doesn't match `query`'s schema.
        let line = r#"{"type":"request","id":42,"method":"query","params":{"limit":"not a number"}}"#;
        let (id, _) = parse_client_line(line).unwrap_err();
        assert_eq!(id, Some(42));
    }

    #[test]
    fn unknown_method_is_rejected_not_silently_ignored() {
        let line = r#"{"type":"request","id":1,"method":"frobnicate","params":{}}"#;
        assert!(parse_client_line(line).is_err());
    }

    #[test]
    fn empty_and_whitespace_lines_are_rejected() {
        assert!(parse_client_line("").is_err());
        assert!(parse_client_line("   \r\n").is_err());
    }

    #[test]
    fn to_line_never_embeds_a_newline_even_with_newlines_in_query_text() {
        let req = ClientMessage::Request { id: 1, method: Method::Query { roots: None, query: "a\nb".into(), limit: 1, offset: 0, snippet: true } };
        let line = to_line(&req);
        assert_eq!(line.matches('\n').count(), 1, "the query text's newline must be JSON-escaped, not literal");
        assert_eq!(parse_client_line(&line), Ok(req));
    }
}
