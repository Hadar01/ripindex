//! The daemon's foreground entry point (`ripindex daemon`): logging, the
//! instance lock, config, registering configured roots, binding the
//! transport, and idle shutdown. Everything else (`server`, `actor`,
//! `transport`) is written to be testable without this glue; this module is
//! the one place that actually touches global process state (the log file,
//! the lock file, the well-known transport address) and so is the one place
//! not exercised by the unit/integration tests — `tests/daemon.rs` drives
//! `server::serve` directly against an in-test transport instead.

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::config::Config;
use crate::daemon::server::{serve, Registry};
use crate::daemon::{paths, transport};
use crate::fs::{Fs, RealFs};

/// Rotate the log file once it passes this size, keeping one previous
/// generation (`daemon.log` -> `daemon.log.1`, old `.1` discarded). A real
/// rotation policy (age-based, several generations, compression) is a
/// follow-up; this bounds unbounded growth with about five lines of code.
const LOG_ROTATE_BYTES: u64 = 10 << 20;

fn init_logging() -> anyhow::Result<()> {
    let log_path = paths::log_file();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0) > LOG_ROTATE_BYTES {
        let rotated = log_path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&log_path, &rotated);
    }
    let file = fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(file)))
        .format_timestamp_millis()
        .try_init()
        .ok(); // ignore "already initialised" — e.g. under a test harness
    Ok(())
}

/// Non-blocking: `Ok(Some(lock))` if this process is now *the* daemon,
/// `Ok(None)` if another instance already holds it (the caller should exit
/// quietly — this is what makes an autostart race resolve to exactly one
/// daemon), `Err` for any other failure (e.g. the runtime dir isn't creatable).
fn acquire_instance_lock<'f>(fs: &'f dyn Fs) -> anyhow::Result<Option<Box<dyn crate::fs::FsLock + 'f>>> {
    let lock_path = paths::lock_file();
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs.lock(&lock_path) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Public so `tests/daemon.rs` can exercise idle-shutdown timing directly
/// against an in-test `Registry` without going through a real daemon
/// subprocess and its fixed, minutes-long default timeout.
pub fn spawn_idle_shutdown(registry: Arc<Registry>, stop: Arc<tokio::sync::Notify>, timeout: Duration) {
    tokio::spawn(async move {
        let poll = Duration::from_secs(30).min(timeout).max(Duration::from_secs(1));
        let mut idle_since: Option<Instant> = None;
        loop {
            tokio::time::sleep(poll).await;
            let idle_now = registry.connection_count() == 0 && registry.all().await.is_empty();
            if idle_now {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= timeout {
                    log::info!("idle for {timeout:?} with no clients and no watched roots; shutting down");
                    stop.notify_one();
                    return;
                }
            } else {
                idle_since = None;
            }
        }
    });
}

/// Run until stopped (a `shutdown` RPC, or idle timeout). Exits immediately,
/// successfully, if another instance already holds the lock — the losing
/// side of an autostart race.
pub async fn run_foreground() -> anyhow::Result<()> {
    init_logging()?;
    let config = Config::load(&paths::config_file())?;

    let Some(_lock) = acquire_instance_lock(&RealFs)? else {
        log::info!("another daemon instance already holds the lock; exiting");
        return Ok(());
    };

    let registry = Registry::new(&config);
    for r in &config.roots {
        match registry.add_root(&r.path).await {
            Ok(h) => log::info!("watching configured root {}", h.root.display()),
            Err(e) => log::warn!("failed to add configured root {:?}: {e}", r.path),
        }
    }

    let stop = Arc::new(tokio::sync::Notify::new());
    if config.idle_timeout_secs > 0 {
        spawn_idle_shutdown(registry.clone(), stop.clone(), Duration::from_secs(config.idle_timeout_secs));
    } else {
        log::info!("idle shutdown disabled (idle_timeout_secs = 0)");
    }

    #[cfg(windows)]
    {
        let listener = transport::bind(&paths::pipe_name())?;
        log::info!("ripindex daemon listening on {} (pid {})", paths::pipe_name(), std::process::id());
        serve(listener, registry, stop).await;
    }
    #[cfg(unix)]
    {
        let listener = transport::bind(&paths::socket_path()).await?;
        log::info!("ripindex daemon listening on {} (pid {})", paths::socket_path().display(), std::process::id());
        serve(listener, registry, stop).await;
    }
    Ok(())
}

/// The systemd user unit and launchd agent this build ships but never
/// installs — printed by `ripindex daemon install-hint`, left for the user
/// to place and enable themselves. Autostart on login is a user decision.
pub fn systemd_unit(exe: &str) -> String {
    format!(
        "[Unit]\nDescription=ripindex daemon\n\n[Service]\nType=simple\nExecStart={exe} daemon\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n"
    )
}

pub fn launchd_plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.ripindex.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#
    )
}

pub fn install_hint(exe: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        format!(
            "Install a systemd user unit so ripindex starts on login:\n\n  mkdir -p ~/.config/systemd/user\n  cat > ~/.config/systemd/user/ripindex.service <<'EOF'\n{}EOF\n  systemctl --user enable --now ripindex\n",
            systemd_unit(exe)
        )
    }
    #[cfg(target_os = "macos")]
    {
        format!(
            "Install a launchd agent so ripindex starts on login:\n\n  cat > ~/Library/LaunchAgents/com.ripindex.daemon.plist <<'EOF'\n{}EOF\n  launchctl load ~/Library/LaunchAgents/com.ripindex.daemon.plist\n",
            launchd_plist(exe)
        )
    }
    #[cfg(windows)]
    {
        let _ = exe;
        "ripindex autostarts on demand (the first `ripindex search`/`status`/etc. spawns it if it isn't running already); \
         there is no login-time autostart hook shipped for Windows in this build. A Scheduled Task \
         (`schtasks /create /sc onlogon /tn ripindex /tr \"<path to ripindex.exe> daemon\"`) achieves the same effect \
         if you want the daemon warm before your first command."
            .to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = exe;
        "No autostart hint is available for this platform.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_files_are_well_formed_enough() {
        let s = systemd_unit("/usr/bin/ripindex");
        assert!(s.contains("ExecStart=/usr/bin/ripindex daemon"));
        let p = launchd_plist("/usr/local/bin/ripindex");
        assert!(p.contains("<string>daemon</string>"));
    }

    #[test]
    fn install_hint_never_touches_disk() {
        // Just a text generator — must not create or modify any file.
        let count = |dir: &std::path::Path| fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
        let before = count(&paths::state_dir());
        let _ = install_hint("ripindex");
        assert_eq!(before, count(&paths::state_dir()));
    }
}
