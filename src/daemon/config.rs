//! One optional TOML config file (`config_file()`, typically
//! `~/.local/state/ripindex/config.toml` or `%LOCALAPPDATA%\ripindex\config.toml`).
//! Every field has a working default, so the file itself is optional — a
//! daemon with no config file and no `add_root` calls just sits idle with
//! nothing to watch.
//!
//! **Scope note.** Loaded once at daemon startup. "Watched with the same
//! machinery as everything else, reloaded live" is not wired: this build
//! re-reads the file only when the daemon restarts. Live reload needs the
//! daemon to re-diff `roots` against what's currently registered (add/remove
//! actors accordingly) and push new `governor`/`idle_timeout` values into
//! already-running actors — a real feature, deliberately left for a
//! follow-up rather than half-built.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::store::MergePolicy;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct RootConfig {
    pub path: String,
    /// Overrides the crawler's default (10 MiB) for this root.
    pub max_file_size_bytes: Option<u64>,
    /// Force a full reconcile at least this often, independent of the
    /// watcher (same bound the watcher already applies globally; a per-root
    /// override for a root known to need tighter or looser bounds).
    pub reconcile_interval_secs: Option<u64>,
}


#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GovernorConfig {
    pub bytes_per_sec: u64,
    pub cpu_fraction: f64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self { bytes_per_sec: 50 << 20, cpu_fraction: 0.5 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeConfig {
    pub tier_size: usize,
    pub max_merge: usize,
    pub max_segment_bytes: u64,
    pub del_fraction_threshold: f32,
}

impl Default for MergeConfig {
    fn default() -> Self {
        let d = MergePolicy::default();
        Self { tier_size: d.tier_size, max_merge: d.max_merge, max_segment_bytes: d.max_segment_bytes, del_fraction_threshold: d.del_fraction_threshold }
    }
}

impl From<MergeConfig> for MergePolicy {
    fn from(c: MergeConfig) -> Self {
        MergePolicy { tier_size: c.tier_size, max_merge: c.max_merge, max_segment_bytes: c.max_segment_bytes, del_fraction_threshold: c.del_fraction_threshold }
    }
}

fn default_idle_timeout_secs() -> u64 {
    4 * 3600
}

fn default_debounce_ms() -> u64 {
    500
}

fn default_periodic_reconcile_secs() -> u64 {
    600
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub roots: Vec<RootConfig>,
    pub governor: GovernorConfig,
    pub merge: MergeConfig,
    /// Shut down after this long with no client connected and no watched
    /// root — 0 disables idle shutdown. Default 4 hours.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_debounce_ms")]
    pub watch_debounce_ms: u64,
    #[serde(default = "default_periodic_reconcile_secs")]
    pub periodic_reconcile_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            governor: GovernorConfig::default(),
            merge: MergeConfig::default(),
            idle_timeout_secs: default_idle_timeout_secs(),
            watch_debounce_ms: default_debounce_ms(),
            periodic_reconcile_secs: default_periodic_reconcile_secs(),
        }
    }
}

impl Config {
    /// Missing file → `Ok(Config::default())`, since the file is optional.
    /// A present-but-unparseable file is an error (silently ignoring a typo
    /// in a config the user *did* write would be worse than refusing to start).
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("Config always serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn empty_file_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            idle_timeout_secs = 60

            [[roots]]
            path = "/home/me/notes"
            "#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.idle_timeout_secs, 60);
        assert_eq!(cfg.roots.len(), 1);
        assert_eq!(cfg.roots[0].path, "/home/me/notes");
        assert_eq!(cfg.roots[0].max_file_size_bytes, None);
        assert_eq!(cfg.governor, GovernorConfig::default());
    }

    #[test]
    fn malformed_toml_is_an_error_not_a_silent_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not [valid toml").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn roundtrips_through_its_own_serializer() {
        let cfg = Config { idle_timeout_secs: 99, ..Config::default() };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, cfg.to_toml_string()).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn merge_config_converts_to_a_policy() {
        let mc = MergeConfig::default();
        let policy: MergePolicy = mc.into();
        assert_eq!(policy, MergePolicy::default());
    }
}
