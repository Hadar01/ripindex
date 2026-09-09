# Changelog

All notable changes to this project are documented here. This project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-08

First public release.

### Added

- **Search.** Tokenised inverted index with BM25 ranking (k1=1.2, b=0.75). Query syntax:
  `AND` / `OR`, quoted phrases (positional), `-negation`, and `( grouping )`.
  Unicode-aware tokenisation that indexes identifiers as the whole token plus their
  `camelCase` / `snake_case` parts.
- **Persistent, crash-safe on-disk index** under `.ripindex/`: immutable memory-mapped
  segments, CRC32-checksummed envelopes, LEB128 delta-encoded postings, a front-coded term
  dictionary, and atomic commits via `MANIFEST` rename. The byte layout is documented
  field by field in `docs/FORMAT.md`.
- **Incremental updates.** Staged change detection (metadata, then xxh3-64 content hash)
  re-reads only what actually changed; renames are detected by inode plus hash and never
  re-index content. Deletions are tombstoned, never mutated in place.
- **Tiered merging.** Lucene-style size tiers, with heavily-tombstoned segments rewritten
  alone. The expensive phase runs unlocked against a snapshot; a brief locked commit
  carries forward any deletions that landed mid-merge.
- **Filesystem watcher.** Debounced, treated strictly as a hint — a periodic full
  reconcile is always the source of truth, so a missed or coalesced event can't leave the
  index permanently stale.
- **Background daemon.** Sole writer for every indexed root, one actor task per root.
  Queries bypass the actor entirely via refcounted snapshots, so a merge or reconcile
  never stalls a search. Newline-delimited JSON protocol with a versioned handshake, over
  a Unix socket (mode 0600) or a Windows named pipe with an owner-only DACL — never TCP.
  Autostart on first use, stale-socket recovery, idle shutdown, and a direct read-only
  fallback if the daemon can't be reached at all.
- **`ripindex status`** (human and `--json`): per-root document counts, segment counts,
  disk usage, last reconcile time and duration, whether a merge is running, watcher health.
- **Resource governor.** Token-bucket IO cap plus a CPU duty cycle, with real
  battery and memory-pressure detection (`GetSystemPowerStatus` / `GlobalMemoryStatusEx`
  on Windows; sysfs and `/proc/meminfo` on Linux).
- **Neovim Telescope extension** in `contrib/nvim`, talking to the daemon socket directly.
- Prebuilt binaries for Linux x86_64/aarch64, macOS x86_64/aarch64, and Windows x86_64,
  with `SHA256SUMS` and checksum-verifying shell and PowerShell installers.

### Testing

265 tests, including 4,600+ kill-9 crash-recovery iterations, per-syscall fault injection
asserting all-or-nothing commits, `proptest` round-trips for every encoder, and
socket-level daemon stress tests (concurrent queries during live merges, a 20-way autostart
race, and SIGKILL-then-query-immediately recovery).

[Unreleased]: https://github.com/hadar01/ripindex/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/hadar01/ripindex/releases/tag/v0.1.0
