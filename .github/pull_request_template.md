## What this changes

<!-- And why. Link an issue if there is one. -->

## Checks

- [ ] `cargo test` passes (265 tests at time of writing)
- [ ] `cargo clippy --all-targets -- -D warnings` is clean — CI gates on this
- [ ] Touched the on-disk format? `docs/FORMAT.md` updated to match, field by field
- [ ] Touched the commit or recovery path? `./scripts/crash_loop.sh 500` still passes
- [ ] Touched the daemon? `cargo test --test daemon` passes

## Notes

<!--
Formatting isn't gated: the code is hand-formatted and rustfmt.toml is a hint, so
please match the surrounding style rather than reformatting whole files.

Scope cuts and known-incomplete work are fine, but say so in a doc comment at the
definition site rather than leaving it implicit — that's the convention here.
-->
