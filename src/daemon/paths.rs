//! Where the daemon's runtime and state files live. Deliberately never a
//! TCP port: the index holds the contents of the user's files, and a
//! localhost port is reachable by every other process on the machine and by
//! browser-based attacks (DNS rebinding, malicious pages hitting
//! `http://127.0.0.1:PORT`). A Unix socket at mode 0600, or a Windows named
//! pipe with a DACL restricted to the current user (see `transport`), gives
//! the correct access model for free. Remote access, if ever wanted, is an
//! explicit opt-in with real authentication — never a default.
//!
//! `RIPINDEX_TEST_RUNTIME_DIR` / `RIPINDEX_TEST_STATE_DIR` / (Windows)
//! `RIPINDEX_TEST_PIPE_NAME` / (Unix) `RIPINDEX_TEST_SOCKET_PATH` override the
//! corresponding function when set to a non-empty value. These exist solely
//! so `tests/daemon.rs` can run real daemon subprocesses (the autostart-race
//! and stale-socket-recovery tests) against a throwaway location instead of
//! the real user's runtime/state directories and pipe name — never read or
//! set outside tests.

use std::env;
use std::path::{Path, PathBuf};

/// Directory for transient runtime state: the instance lock, and (on Unix)
/// the socket file. Prefers `$XDG_RUNTIME_DIR/ripindex`, falling back to
/// [`state_dir`]. On Windows there's no equivalent widely-used convention,
/// so this is `%LOCALAPPDATA%\ripindex\run` (or `%TEMP%\ripindex` if even that
/// is unset) — used only for the lock file, since the actual transport (a
/// named pipe) lives in the kernel object namespace, not the filesystem.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = env::var("RIPINDEX_TEST_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(unix)]
    {
        if let Ok(dir) = env::var("XDG_RUNTIME_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("ripindex");
            }
        }
        state_dir()
    }
    #[cfg(windows)]
    {
        if let Ok(dir) = env::var("LOCALAPPDATA") {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("ripindex").join("run");
            }
        }
        env::temp_dir().join("ripindex")
    }
    #[cfg(not(any(unix, windows)))]
    {
        env::temp_dir().join("ripindex")
    }
}

/// Directory for durable state: the log and the config file. Prefers
/// `$XDG_STATE_HOME/ripindex`, then `~/.local/state/ripindex`; on Windows,
/// `%LOCALAPPDATA%\ripindex`.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = env::var("RIPINDEX_TEST_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(unix)]
    {
        if let Ok(dir) = env::var("XDG_STATE_HOME") {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("ripindex");
            }
        }
        if let Ok(home) = env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(".local").join("state").join("ripindex");
            }
        }
        env::temp_dir().join("ripindex")
    }
    #[cfg(windows)]
    {
        if let Ok(dir) = env::var("LOCALAPPDATA") {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("ripindex");
            }
        }
        env::temp_dir().join("ripindex")
    }
    #[cfg(not(any(unix, windows)))]
    {
        env::temp_dir().join("ripindex")
    }
}

/// Canonicalize a root the way the daemon keys and reports it.
///
/// On Windows, `fs::canonicalize` returns a *verbatim* path (`\\?\C:\...`).
/// That form is correct but unusable in practice for our purposes: it shows up
/// in every search result and status line, it leaks as visual noise, and
/// editors reject it - Vim and Neovim cannot open a `\\?\`-prefixed path, which
/// would break the Telescope client outright. So the prefix is stripped, which
/// also makes the daemon's paths match the ones the no-daemon code path
/// already produces.
///
/// The trade-off is deliberate: `\\?\` exists to allow paths beyond `MAX_PATH`
/// and to bypass path parsing. Stripping it means a root deeper than ~260
/// characters may fail where it would otherwise have worked - a narrow case,
/// versus unreadable output and broken editor integration for everyone.
/// UNC paths keep their meaning: `\\?\UNC\server\share` becomes
/// `\\server\share`.
pub fn normalize_root(path: &str) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    strip_verbatim(&canonical)
}

/// Strip Windows' verbatim prefix; a no-op everywhere else.
pub fn strip_verbatim(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_path_buf();
    }
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// The single global (not per-root) daemon-instance lock: whoever holds this
/// is *the* daemon. Bound before the transport, so autostart races resolve
/// here — the loser never gets far enough to bind. An advisory OS lock, so
/// it's released automatically on process death; no stale-lock detection needed.
pub fn lock_file() -> PathBuf {
    runtime_dir().join("daemon.lock")
}

pub fn log_file() -> PathBuf {
    state_dir().join("daemon.log")
}

pub fn config_file() -> PathBuf {
    state_dir().join("config.toml")
}

/// Unix domain socket path. Mode 0600 is set right after `bind`.
#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    if let Ok(path) = env::var("RIPINDEX_TEST_SOCKET_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    runtime_dir().join("daemon.sock")
}

/// Windows named pipe name — not a filesystem path; unique per Windows user
/// account so two users on a shared machine never collide (the DACL, not
/// this name, is what actually enforces the access restriction — see
/// `transport::windows`).
#[cfg(windows)]
pub fn pipe_name() -> String {
    if let Ok(name) = env::var("RIPINDEX_TEST_PIPE_NAME") {
        if !name.is_empty() {
            return name;
        }
    }
    let user = env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
    // Named pipe names share the Win32 filename charset restrictions loosely;
    // keep it simple and predictable rather than fully sanitizing an
    // environment-controlled string few systems ever set unusually.
    let safe: String = user.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    format!(r"\\.\pipe\ripindex-{}", if safe.is_empty() { "default".to_string() } else { safe })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_stable_and_nested_under_ripindex() {
        assert!(runtime_dir().ends_with("ripindex") || runtime_dir().ends_with("run"));
        assert!(state_dir().to_string_lossy().contains("ripindex"));
        assert_eq!(lock_file().file_name().unwrap(), "daemon.lock");
        assert_eq!(log_file().file_name().unwrap(), "daemon.log");
        assert_eq!(config_file().file_name().unwrap(), "config.toml");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_stripped_for_display_and_clients() {
        // The form `fs::canonicalize` hands back on Windows...
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\code\proj")), PathBuf::from(r"C:\code\proj"));
        // ...including the UNC spelling, which must stay a UNC path.
        assert_eq!(strip_verbatim(Path::new(r"\\?\UNC\srv\share\x")), PathBuf::from(r"\\srv\share\x"));
        // Anything already plain is untouched.
        assert_eq!(strip_verbatim(Path::new(r"C:\code\proj")), PathBuf::from(r"C:\code\proj"));
    }

    #[test]
    fn normalize_root_never_yields_a_verbatim_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = normalize_root(&dir.path().display().to_string());
        assert!(
            !normalized.to_string_lossy().starts_with(r"\\?\"),
            "normalize_root leaked a verbatim prefix: {}",
            normalized.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_name_is_well_formed_and_stable() {
        let a = pipe_name();
        let b = pipe_name();
        assert_eq!(a, b);
        assert!(a.starts_with(r"\\.\pipe\ripindex-"));
    }
}
