# Contributing

Bug reports — especially platform ones — are the most useful thing right now. ripindex was
developed on Windows; Linux and macOS are covered by CI but have had little real-world use.

## Getting started

```sh
cargo test                              # 259 tests, ~20s
cargo clippy --all-targets -- -D warnings
cargo run --release -- bench <dir>      # build/open/query/memory report
```

MSRV is **1.89** — `File::try_lock` stabilised there and the writer mutex needs it. CI has a
job that enforces it, so a dependency bump that raises the floor will fail there rather than
in someone's build.

## House conventions

These are what the code already does; matching them keeps review short.

- **Formatting isn't gated.** `rustfmt.toml` is a hint, not a rule, and the code is
  hand-formatted in places where a grid or an aligned table reads better than what rustfmt
  produces. Match the surrounding style; please don't reformat whole files in a PR that's
  about something else.
- **Clippy is gated** with `-D warnings`. It has already caught real bugs here, so if a lint
  is genuinely wrong for a site, `#[allow(...)]` it *with a one-line reason* rather than
  loosening the gate.
- **Say what isn't done, at the definition site.** Scope cuts are fine and expected — the
  convention is a `//! Scope note.` or `// Scope note:` comment where the thing is defined,
  explaining what's missing and why, so nobody has to reverse-engineer the gap. Several
  exist today (config hot-reload, Unix peer-UID checks, query cancellation).
- **Don't build in an invariant the next change will break.** Prefer a sparse or holey
  structure that stays correct over a dense one that relies on an assumption about to
  disappear.

## If you touch the on-disk format

`docs/FORMAT.md` documents the format field by field and offset by offset, and it is meant
to stay true. A format change means:

1. Updating `docs/FORMAT.md` to match, at the same level of detail.
2. Bumping the format version, if a reader of the old version couldn't read the new one.
3. Property tests for any new encoder/decoder — every existing one round-trips under
   `proptest`.
4. `./scripts/crash_loop.sh 500` still passing, if you touched the commit or recovery path.
   This SIGKILLs the writer mid-commit and asserts the index is always in exactly one of two
   valid states.

## If you touch the daemon

`cargo test --test daemon` runs the socket-level suite: concurrent queries during a live
merge, a 20-way autostart race, and SIGKILL-then-query-immediately recovery. It uses real
sockets and real subprocesses, so it is the suite most likely to catch a genuine
concurrency mistake — and the most likely to be flaky if something is subtly wrong. If it
fails intermittently, that's a bug, not noise; please report it rather than retrying.

## Licence

Contributions are dual-licensed MIT / Apache-2.0, matching the project.
