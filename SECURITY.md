# Security

## Reporting

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/hadar01/ripindex/security/advisories/new)
rather than a public issue. I'll acknowledge within a week.

## What ripindex is trusted with

Worth being explicit, because it's more than it looks: **the index contains the contents of
your files.** Terms, positions, and enough structure to reconstruct a great deal of any
indexed document all live in `.ripindex/` inside the indexed tree, at whatever permissions
that directory has. Treat an index directory as being as sensitive as the corpus it covers,
and don't commit one — the shipped `.gitignore` excludes `.ripindex/` for that reason.

## Design decisions that exist for this reason

- **The daemon never listens on TCP.** Not on `127.0.0.1`, not behind a flag. It uses a Unix
  domain socket at mode `0600`, or a Windows named pipe with a DACL restricted to the
  owning user. A localhost port would be reachable by every other process on the machine,
  and by web pages via DNS rebinding — for a service that can return the contents of your
  files, that's the wrong default and there is no opt-in.
- **The protocol has no eval-like surface.** Queries are parsed into a fixed AST of terms,
  phrases, booleans and negations. There is no regex engine, no path traversal in requests,
  and no method that reads a file the daemon wasn't asked to index.
- **Installers verify what they download.** `install.sh` and `install.ps1` check the
  SHA-256 of the release archive against the `SHA256SUMS` published with it and refuse to
  install on a mismatch.

## Known gaps

- **Unix peer-UID checking is not wired.** The socket's `0600` mode inside a user-owned
  directory is the actual access control, and it is sufficient on a normally-configured
  system. The additional `SO_PEERCRED` / `LOCAL_PEERCRED` check — which would reject a
  connection from another UID even if the mode bits were loosened — is documented in
  `src/daemon/transport.rs` but not implemented, because it could not be exercised on the
  Windows machine this was developed on. Shipping an untested security check would be worse
  than saying it's absent.
- **The daemon log can contain file paths** from indexed roots. It lives in your state
  directory at default permissions. Redact before pasting it into an issue.
- **No sandboxing of the crawl.** ripindex reads what you point it at, with your
  permissions. Pointing it at a directory you don't control means reading files you may not
  expect to read; it does not cross permission boundaries, but it also doesn't defend
  against a hostile tree beyond skipping binaries and honouring size limits.

## Supported versions

Pre-1.0: only the latest release gets fixes.
