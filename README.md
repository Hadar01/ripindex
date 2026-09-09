# ripindex

**Indexed code and text search.** Point it at a directory once; every search after that
returns in microseconds instead of rescanning the tree.

[![CI](https://github.com/Hadar01/ripindex/actions/workflows/ci.yml/badge.svg)](https://github.com/Hadar01/ripindex/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ripindex.svg)](https://crates.io/crates/ripindex)
[![license](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)

```console
$ cd ~/cpython                      # 4,579 files, 90 MiB of source. Indexed once.

$ ripindex search --root . PyUnicode_FromString -n 3
   7.398  Modules\_testcapi\vectorcall_limited.c:12
          return [PyUnicode_FromString]("tp_call called");
   7.201  Python\Python-tokenize.c:56
          PyObject *filename = [PyUnicode_FromString]("<string>");
   6.848  Modules\_testcapi\watchers.c:27
          msg = [PyUnicode_FromString]("clear");
3 of 3 matching files shown (via daemon); query took 749 µs

$ ripindex search --root . 'asyncio AND subprocess' -n 3
  12.565  Lib\test\test_asyncio\test_subprocess.py:9
          from [asyncio] import [base_subprocess]
  12.522  Doc\library\asyncio-subprocess.rst:3
          .. [_asyncio]-[subprocess]:
  12.356  Doc\library\asyncio-protocol.rst:82
          [asyncio] implements transports for TCP, UDP, SSL, and [subprocess] pipes.
3 of 3 matching files shown (via daemon); query took 918 µs
```

Real output, copied verbatim from a run on the same corpus the benchmarks below use.
Matches are bracketed in the snippet, and highlighted in colour on a terminal. Paths
are backslashed because this was captured on Windows - see [Benchmarks](#benchmarks).

`ripgrep` is the right tool when you search a tree once. `ripindex` is for the other
case: the same tree, over and over, all day. It builds a persistent inverted index in
`.ripindex/`, keeps it current with a filesystem watcher, and answers queries from a
memory-mapped index through a background daemon. The interesting part isn't the search —
it's the storage engine underneath: a segmented, crash-safe on-disk format with atomic
commits, a documented byte layout, and a test suite that kills the process mid-write
thousands of times to prove recovery works.

## Benchmarks

Corpus: **CPython v3.12.0**, 4,579 files, 90.0 MiB of indexable text.
Machine: AMD Ryzen 5 7600X (6C/12T), 31 GiB RAM, NVMe SSD, Windows 11.
Reproduce it yourself: [`scripts/demo-corpus.sh`](scripts/demo-corpus.sh).

### Where it wins: repeat queries

| query | matches | ripindex p50 | ripgrep 15.1.0 p50 |
|---|---:|---:|---:|
| `PyObject` | 699 | **18 µs** | 196 ms |
| `PyUnicode_FromString` | 124 | **8 µs** | 195 ms |
| `asyncio AND subprocess` | 90 | **21 µs** | — (not expressible) |
| `"reference count"` (phrase) | 78 | **99 µs** | 153 ms |

20 timed runs each, warm, after warm-up. Both tools respect `.gitignore` and skip binaries.

### Where it loses: the first run

| | ripindex | ripgrep |
|---|---:|---:|
| First search on a cold tree | **3.6 s** (186 ms crawl + 3.38 s index) | 196 ms |
| Every search after | 18 µs | 196 ms |
| Disk used | 33.7 MiB (37% of corpus) | 0 |

**ripgrep beats ripindex by ~18× on a single search, and always will** — it does one pass
and exits, while ripindex pays to build an index first. The break-even is about
**18 searches** on this corpus. Below that, use ripgrep. Above it, the index has already
paid for itself. If you search a repo twice a week, `rg` is the better tool and you should
keep using it.

Other honest numbers: **18.3 MiB** RSS with the index open (the index is mmap'd, so the
reader's own heap is ~2 KiB and the OS pages in only what queries touch — they touched
1.9% of a 33.7 MiB index). Warm open is **1.28 ms**; cold **4.31 ms**. Incremental
reconciles re-read only files whose metadata or content hash changed, so keeping the index
current costs far less than the initial build.

## Query semantics: tokens, not substrings

This is the biggest behavioural difference from grep, and it cuts both ways:

| query | matches `PyObject y;` | matches `PyObject_HEAD x;` |
|---|---|---|
| `PyObject` | yes | **no** |
| `PyObject_HEAD` | no | yes |
| `Object` | yes | yes |

Identifiers are indexed as the whole token *plus* their `camelCase`/`snake_case` parts, so
`Object` finds both, but `PyObject` will not match `PyObject_HEAD` the way `grep` would.
You get no substring noise; you also can't search for arbitrary substrings. This is why
the match counts above differ from ripgrep's — the two tools are answering different
questions, and any benchmark that hides that is lying to you.

Supported: `AND` / `OR`, `"quoted phrases"` (positional), `-negation`, `( grouping )`.
Ranking is BM25 (k1=1.2, b=0.75).

## Install

**macOS / Linux**

```sh
curl -LsSf https://github.com/Hadar01/ripindex/releases/latest/download/ripindex-installer.sh | sh
```

**Windows**

```powershell
irm https://github.com/Hadar01/ripindex/releases/latest/download/ripindex-installer.ps1 | iex
```

**Cargo** (needs Rust 1.89+)

```sh
cargo install ripindex
```

Prebuilt binaries: Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64. Every
release publishes `SHA256SUMS`; the installers verify against it and refuse to install on
a mismatch.

## Usage

```sh
# Search. Indexes on first use, autostarts the daemon, then stays fast.
ripindex search --root ~/code/myproject parse_config

# Query syntax
ripindex search --root . "config AND -test"
ripindex search --root . '"connection refused"'

# What is the daemon doing?
ripindex status
ripindex status --json

# Explicit index management (all optional - search does this for you)
ripindex index   ~/code/myproject   # build now
ripindex update  ~/code/myproject   # reconcile against the filesystem
ripindex merge   ~/code/myproject   # compact segments
ripindex verify  ~/code/myproject   # check every checksum

# The daemon
ripindex daemon                 # run in the foreground
ripindex daemon stop            # graceful shutdown
ripindex daemon install-hint    # prints a systemd unit / launchd plist; installs nothing

# Skip the daemon entirely
ripindex search --no-daemon --root . needle

# Paths print relative to the searched root, like ripgrep. Full paths:
ripindex search --root ~/code/myproject --absolute parse_config
```

Result paths are shown relative to the root you searched, and snippets are
sized to your terminal so a result stays one line per file. `--absolute` prints
full paths; the daemon protocol always carries the full path, so editor
integrations get something they can open.

`$RIPINDEX_ROOT` sets the default root. Config lives at `$XDG_STATE_HOME/ripindex/config.toml`
(`%LOCALAPPDATA%\ripindex\config.toml` on Windows) and is optional — every setting has a
working default.

The daemon speaks newline-delimited JSON over a Unix socket (or a Windows named
pipe), so an editor or tool integration is a small client rather than a
subprocess-and-parse job. The protocol is defined in `src/daemon/protocol.rs`.

## How it works

The daemon is the sole writer for every indexed root. One actor task per root owns the
index; queries never go through it — they take a refcounted snapshot and run on a blocking
pool, so a merge or a reconcile can never stall a search. Merges snapshot the segments,
do the expensive work unlocked, then carry forward any deletions that landed mid-merge in
a brief locked commit.

Writes are crash-safe by construction: segment files are immutable once written, and a
generation becomes visible only when a new `MANIFEST` is atomically renamed into place.
A torn write leaves the previous generation intact.

**[docs/FORMAT.md](docs/FORMAT.md) documents the on-disk format field by field, offset by
offset** — every file type, the commit protocol, the recovery algorithm, and the reasoning
behind each choice. If you only read one file in this repo, read that one.

How it's tested:

- **4,600+ kill-9 iterations.** [`scripts/crash_loop.sh`](scripts/crash_loop.sh) SIGKILLs
  the writer mid-commit, reopens, and asserts the index is one of exactly two valid states.
- **Fault injection.** A filesystem wrapper fails or discards writes at every syscall in
  turn and asserts all-or-nothing commits.
- **Property tests.** Every encoder/decoder round-trips under `proptest`.
- **Socket-level stress.** Concurrent clients querying through the real socket while a
  merge and reconciles run; a 20-way autostart race asserting exactly one daemon wins;
  SIGKILL-the-daemon-and-query-immediately recovery.

265 tests, and `cargo test` runs all of them in about 20 seconds.

## Limitations

Read this before filing a bug — some of these are deliberate.

- **Text and code only.** No PDF, docx, or archive extraction. Binary files are detected
  and skipped, not parsed. Extractors are the obvious next feature; they aren't here yet.
- **Token matching, not substring or fuzzy.** See above. No typo tolerance, no regex, no
  `PyObj*` prefix search. Deliberate: BM25 over tokens is predictable, and "did you mean"
  ranking is a different product. Use `rg` when you need a regex.
- **Single machine.** No distributed index, no client/server over a network. The daemon
  listens on a Unix socket or a named pipe and never on TCP, on purpose — the index holds
  the contents of your files, and a localhost port is reachable by every process on the
  box, browsers included.
- **Windows: directory fsync is best-effort.** The commit protocol wants a durable
  directory entry after rename. Windows has no `fsync(dir)` equivalent, so on Windows a
  power loss (not a process crash — that case is covered and tested) in a narrow window
  could in principle lose the most recent commit. The previous generation still opens
  cleanly. On Linux and macOS the directory is fsynced properly.
- **Peer-UID checking isn't wired on Unix yet.** The socket is mode 0600 in a
  user-owned directory, which is the real protection; the belt-and-braces `SO_PEERCRED`
  check is written up in the source but not implemented. It was untestable on the Windows
  machine this was developed on, and shipping an unexercised security check is worse than
  documenting its absence.
- **Config is read at startup, not hot-reloaded.** Restart the daemon after editing it.
- **A query that's already running isn't cancelled when its client disconnects.** At
  microsecond query latencies the wasted work is irrelevant; the daemon drops the response
  rather than pretending to abort.
- **First-run indexing is slower than one `rg` scan.** See the benchmark table. This is
  inherent to the approach, not a bug.

Developed and tested primarily on Windows; Linux and macOS are covered by CI on every push
but have had less real-world use. Platform bug reports are especially welcome.

## Development

```sh
cargo test                              # 265 tests, ~20s
cargo test --test daemon                # socket, autostart race, kill recovery
./scripts/crash_loop.sh 2000            # kill-9 soak
cargo run --release -- bench <dir>      # build/open/query/memory report
```

MSRV is **1.89** — that's where `File::try_lock` stabilised, and the writer mutex is a real
OS advisory lock. Formatting isn't gated in CI; the code is hand-formatted and
`rustfmt.toml` is a hint, not a rule. Clippy is gated with `-D warnings`.

## Contributing

Bug reports, especially platform ones, are the most useful thing right now — see
[CONTRIBUTING.md](CONTRIBUTING.md) for the house conventions (clippy is gated, formatting
isn't, and format changes must update `docs/FORMAT.md`).

Security policy and threat model: [SECURITY.md](SECURITY.md). Short version — the index
contains the contents of your files, so treat `.ripindex/` as being as sensitive as the
corpus it covers, and report vulnerabilities privately rather than in an issue.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
