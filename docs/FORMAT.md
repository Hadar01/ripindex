# ripindex on-disk format, version 2

Status: **implemented (M2 + M3).** `src/format/` encodes and decodes it; `src/store/` implements §7 and §8 (`writer.rs`/`reader.rs`), incremental updates (`state.rs`/`update.rs`), and merging (`merge.rs`); `tests/fault_injection.rs` pins the §7 operation sequence verbatim.

Version 2 (M3) added: `content_hash` and two more statuses on doc records (§5), the `state-GGGGG.ovl` overlay file (§5.1), and matching `MANIFEST` fields (§3). It is a breaking change — a v1 index opens as `Error::Incompatible` and must be rebuilt. This was the last cheap moment to add a field for something not yet built; the one candidate considered — skip-list entries in the postings region for §4's `"pub fn"`-class dense-phrase hotspot — was not added, since it needs its own design pass (periodic doc-delta/byte-offset entries, and a decision on cadence) rather than a placeholder reservation. Still open for v3.

Conventions: all fixed-width integers are little-endian. `varint` is unsigned
LEB128 (7 payload bits per byte, least-significant group first, high bit set
on every byte but the last); a `u32` varint is at most 5 bytes, a `u64` varint
at most 10. `crc32` is CRC-32/ISO-HDLC (the zlib polynomial, `crc32fast`).
Offsets are byte offsets from the start of the file unless a base is named.

## 1. Directory layout

```
<root>/.ripindex/
  MANIFEST                 commit point; lists every live file
  MANIFEST.tmp             transient: next manifest being written
  LOCK                     writer mutex (advisory OS lock; also probed, non-blockingly, by readers — see §7)
  seg-NNNNN.idx            term dictionary + posting lists
  seg-NNNNN.doc            doc table
  seg-NNNNN.GGGGG.del      deletion bitmap, generation GGGGG >= 1; absent when nothing is deleted
  state-GGGGG.ovl          file-state overlay, generation GGGGG >= 1; absent when nothing needs it
  *.tmp                    transient files; removed on open
  staging/                 a merge's in-progress output (M4, see §12); never scanned by the orphan sweep
```

`NNNNN` is the decimal segment id, zero-padded to at least 5 digits, allocated
from `MANIFEST.next_segment_id` and **never reused** — not even by a merge:
docs surviving a merge get *new* ids in the merged segment, and the ids they
had before become permanent gaps. `GGGGG` is a generation counter, likewise
zero-padded, for `.del` (per segment) and `.ovl` (one, shared); each new
generation writes a new file and the manifest names the live one. Segment
files are immutable once renamed into place; `.del` and `.ovl` files are
replaced wholesale, never edited in place.

The crawler never descends into `.ripindex/`.

## 2. File envelope (all four file types)

| offset   | size | field            | value                                                                |
|----------|------|------------------|-----------------------------------------------------------------------|
| 0        | 8    | `magic`          | `FSRCHMAN` / `FSRCHIDX` / `FSRCHDOC` / `FSRCHDEL` / `FSRCHOVL`        |
| 8        | 4    | `format_version` | u32 = 2                                                              |
| 12       | 4    | `flags`          | u32 = 0; reserved, readers reject non-zero              |
| 16       | n    | body             | see per-file sections                                   |
| len − 16 | 8    | `body_len`       | u64 = len − 32                                          |
| len − 8  | 4    | `body_crc32`     | crc32 over bytes `[16, 16 + body_len)`                  |
| len − 4  | 4    | `footer_magic`   | `FSRF`                                                  |

Minimum file size is 32 bytes. Header and footer are validated on every open
(two page touches); the body crc is validated for `MANIFEST`, `.del`, and
`.ovl` always (all small), and for `.idx`/`.doc` only when the caller asks
(`ripindex verify`, tests, the crash harness) — see §8.

## 3. `MANIFEST`

Body:

| offset | size      | field                  | notes                                                  |
|--------|-----------|------------------------|---------------------------------------------------------|
| 0      | 8         | `generation`           | u64, 1 on the first commit, +1 per commit              |
| 8      | 4         | `next_segment_id`      | u32, first unused segment id                           |
| 12     | 4         | `num_segments`         | u32                                                    |
| 16     | 8         | `committed_unix_nanos` | i64, informational                                     |
| 24     | 4         | `state_gen`            | u32, 0 = no live overlay                               |
| 28     | 4         | `state_crc`            | u32, must equal the `.ovl` footer crc; 0 if none       |
| 32     | 8         | `state_len`            | u64, expected `.ovl` file length; 0 if none            |
| 40     | 8         | `reserved`             | u64 = 0                                                |
| 48     | 80 × n    | segment entries        | ascending `base_doc`, non-overlapping id ranges        |

Segment entry (80 bytes):

| offset | size | field          | notes                                                     |
|--------|------|----------------|-----------------------------------------------------------|
| 0      | 4    | `segment_id`   | u32                                                       |
| 4      | 4    | `del_gen`      | u32, 0 = no deletions file                                |
| 8      | 4    | `base_doc`     | u32, global id of this segment's local doc 0              |
| 12     | 4    | `num_docs`     | u32, local ids are `0..num_docs`                          |
| 16     | 4    | `num_deleted`  | u32, popcount of the live `.del` (0 if none)              |
| 20     | 4    | `idx_crc`      | u32, must equal the `.idx` footer crc                     |
| 24     | 4    | `doc_crc`      | u32, must equal the `.doc` footer crc                     |
| 28     | 4    | `del_crc`      | u32, must equal the `.del` footer crc; 0 if none          |
| 32     | 8    | `idx_len`      | u64, expected file length                                 |
| 40     | 8    | `doc_len`      | u64                                                       |
| 48     | 8    | `del_len`      | u64, 0 if none                                            |
| 56     | 8    | `num_tokens`   | u64, (term, position) pairs — stats only                  |
| 64     | 8    | `num_postings` | u64, (term, doc) pairs — stats only                       |
| 72     | 8    | `num_terms`    | u64, unique terms — stats only                            |

Global doc id = `base_doc + local id`. Consecutive segments have
`base_doc[i+1] = base_doc[i] + num_docs[i]` when written by one build; a
removed segment (future compaction) leaves a gap, which is fine — ids are
sparse by contract.

## 4. `seg-NNNNN.idx`

Body regions, in file order:

```
16                          postings region      (postings_len bytes)
postings_off + postings_len dictionary region    (dict_len bytes)
dict_off + dict_len         sparse index region  (sparse_len bytes)
sparse_off + sparse_len     TOC                  (64 bytes, ends at len − 16)
```

Readers locate the TOC at `len − 16 − 64` and everything else from it.

### 4.1 TOC (64 bytes)

| offset | size | field          |
|--------|------|----------------|
| 0      | 8    | `postings_off` | u64 (= 16 in v1, stored anyway)
| 8      | 8    | `postings_len` | u64
| 16     | 8    | `dict_off`     | u64
| 24     | 8    | `dict_len`     | u64
| 32     | 8    | `sparse_off`   | u64
| 40     | 8    | `sparse_len`   | u64
| 48     | 8    | `num_terms`    | u64
| 56     | 4    | `num_blocks`   | u32 = ceil(num_terms / block_size)
| 60     | 4    | `block_size`   | u32 = 64

### 4.2 Posting list (one per term, concatenated in term order)

```
doc_count            varint u32
repeat doc_count:
  doc_delta          varint u32   first: local doc id; then doc = prev + delta, delta >= 1
  tf                 varint u32   >= 1, number of positions
  pos_bytes          varint u32   byte length of the position deltas that follow
  repeat tf:
    pos_delta        varint u32   first: absolute position; then pos = prev + delta, delta >= 1
```

`pos_bytes` is the one field beyond the brief. It lets a cursor step to the
next doc without decoding positions, so `seek` is O(docs skipped) and
`positions()` decodes only when a phrase asks. Cost: about one byte per
posting (~2–3 % of the postings region on the M1 corpus).

### 4.3 Dictionary region

Terms sorted by UTF-8 bytes (`str::cmp`), cut into blocks of 64. Block `b`
holds terms `[64b, min(64b + 64, num_terms))`. Blocks are back to back; a
block is a sequence of front-coded entries:

```
prefix_len           varint u32   bytes shared with the previous term in this block; 0 for the first entry
suffix_len           varint u32
suffix               bytes        term = prev_term[..prefix_len] ++ suffix
doc_freq             varint u32
postings_len         varint u64   byte length of this term's posting list
```

A term's posting list starts where the previous term's ended. The block's
first list starts at `TOC.postings_off + sparse_entry.postings_off`, so a
block scan accumulates `postings_len` to find any term's list. No per-term
offsets are stored.

Terms longer than 512 bytes are not indexed (builder policy, see §10).

### 4.4 Sparse index region

```
sparse_off                       num_blocks × u32   entry_off[b], relative to `entries`
entries = sparse_off + 4·num_blocks
  entry b:
    term_len         varint u32
    term             bytes        full bytes of term 64b (first term of block b)
    block_off        varint u64   relative to TOC.dict_off
    postings_off     varint u64   relative to TOC.postings_off
```

The u32 offset table makes the entries binary-searchable directly on the
mapping; nothing is loaded into the heap on open. `num_blocks` = 0 when the
segment has no terms.

**Lookup(term):** binary search over `entry_off[0..num_blocks]` for the last
block whose first term is `<= term` (compare bytes). None → absent. Otherwise
front-decode the block from `dict_off + block_off`, accumulating postings
offsets, and compare each reconstructed term; stop at the first term `>`
the query. Equal → `(postings slice, doc_freq)`.

An FST over the term set would replace §4.4 (and make §4.3 optional) to add
prefix and range queries; the lookup entry point is the only code that
changes. That is where the `// FST:` comment will sit.

## 5. `seg-NNNNN.doc`

Body:

| offset | size      | field         | notes                                  |
|--------|-----------|---------------|----------------------------------------|
| 0      | 4         | `num_docs`    | u32, local ids `0..num_docs`, dense    |
| 4      | 4         | `num_indexed` | u32, records with `status = 1`         |
| 8      | 8         | `total_len`   | u64, sum of `len` over indexed records |
| 16     | 8         | `paths_off`   | u64, absolute                          |
| 24     | 8         | `paths_len`   | u64                                    |
| 32     | 48 × n    | records       | record `i` at body offset `32 + 48·i`  |
| …      | paths_len | path heap     | concatenated UTF-8, no separators      |

Record (48 bytes, v2 — 40 in v1):

| offset | size | field              | notes                                                     |
|--------|------|--------------------|------------------------------------------------------------|
| 0      | 8    | `inode`            | u64, 0 if unknown                                          |
| 8      | 8    | `mtime_unix_nanos` | i64, clamped                                               |
| 16     | 8    | `size`             | u64                                                        |
| 24     | 4    | `len`              | u32, atoms; 0 unless `status = 1`                          |
| 28     | 4    | `path_off`         | u32, relative to `paths_off`                               |
| 32     | 4    | `path_len`         | u32                                                        |
| 36     | 1    | `status`           | u8: 0 Skipped, 1 Indexed, 2 Binary, 3 TooLarge             |
| 37     | 3    | `reserved`         | 0                                                          |
| 40     | 8    | `content_hash`     | u64, xxh3-64 of the raw file bytes; 0 for status 2/3 (never read) |

Paths are **root-relative** with `/` separators, so the index is relocatable
and half the size. Non-Unicode path components are stored lossily (`U+FFFD`);
such files index fine but their snippet re-read may fail — logged, not fatal.

`doc_len(local)` for scoring is one fixed-offset read: `body + 32 + 48·i + 24`,
returning `None` unless `status = 1` and the deletion bit is clear — offsets
0–36 are unchanged from v1, so this hot path needed no code change for the
record growing by 8 bytes.

Binary and too-large files are recorded (status 2/3) rather than dropped, so
a reconcile (§ M3 below) can tell "unchanged" from "changed" by comparing
`(inode, mtime, size)` alone, without re-opening and re-sniffing every
non-text file on every pass.

### 5.1 `state-GGGGG.ovl` — the file-state overlay

Patches to the four *mutable-without-a-reindex* doc fields — path, inode,
mtime, size — for docs whose **content** is unchanged since their segment was
written: renames, and metadata-only touches (a file rewritten with identical
bytes by `rsync`, `git checkout`, or a build system). A patch never changes
`content_hash` or `len`; if those would change, the correct action is a
tombstone in `.del` plus a freshly indexed doc, not an overlay entry. One live
generation at a time, always fully crc-verified on open (small).

Body:

| offset | size      | field          | notes                                    |
|--------|-----------|----------------|-------------------------------------------|
| 0      | 4         | `num_patches`  | u32                                       |
| 4      | 4         | `reserved`     | 0                                          |
| 8      | 8         | `paths_off`    | u64, absolute                             |
| 16     | 8         | `paths_len`    | u64                                       |
| 24     | 36 × n    | patches        | sorted and unique by `doc_id`             |
| …      | paths_len | path heap      | concatenated UTF-8, no separators         |

Patch (36 bytes):

| offset | size | field          | notes                              |
|--------|------|----------------|--------------------------------------|
| 0      | 4    | `doc_id`       | u32, global id                       |
| 4      | 8    | `inode`        | u64                                   |
| 12     | 8    | `mtime_nanos`  | i64                                   |
| 20     | 8    | `size`         | u64                                   |
| 28     | 4    | `path_off`     | u32, relative to `paths_off`         |
| 32     | 4    | `path_len`     | u32                                   |

Lookup is a binary search over the sorted patches by `doc_id`. A reader
applies a patch, when one exists for a doc, *after* reading that doc's base
record from its segment — the overlay always wins for the four fields it
covers. Every commit that adds or drops a patch rewrites the whole file (it
stays small: bounded by docs touched since their segment was last written,
not the corpus); a merge folds surviving patches into the merged segment's
own doc table and drops them from the overlay.

## 6. `seg-NNNNN.GGGGG.del`

Body:

| offset | size              | field         |
|--------|-------------------|---------------|
| 0      | 4                 | `num_docs`    | u32, must equal the segment's
| 4      | 4                 | `num_deleted` | u32, popcount of the bitmap
| 8      | ceil(num_docs/8)  | bitmap        | local doc `i` deleted ⇔ `byte[i >> 3] & (1 << (i & 7))`

Fully crc-checked on open (small). Replacing deletions writes generation
`G + 1` to a temp name, syncs, renames, then commits a manifest naming it;
the previous generation becomes unreferenced and is removed after the commit
(or on the next open).

## 7. Commit protocol

Writers hold an OS advisory lock (`File::try_lock`) on `.ripindex/LOCK` for the
whole build, retrying non-blocking acquisition for up to 500 ms before giving
up with `Error::Locked` (the retry exists so a reader's brief lock probe,
below, never surfaces as a spurious failure to a writer that merely started a
few milliseconds later). A second concurrent *writer* still fails past that
window. The kernel releases the lock on process death, so there is no
stale-lock state. For a commit that adds segments `S₁…Sₖ` (and/or new
`.del`/`.ovl` generations) and retires files `O₁…Oₘ`:

```
 1. create_dir_all(.ripindex)
 2. for each new file F (every .idx, .doc, .del of every new segment):
      create F.tmp; write; sync_all; close
      rename F.tmp → F
 3. sync_dir(.ripindex)
 4. create MANIFEST.tmp; write manifest (envelope + body); sync_all; close
 5. rename MANIFEST.tmp → MANIFEST           ← the sole commit point
 6. sync_dir(.ripindex)
 7. remove O₁…Oₘ, best effort (failures logged; open cleans orphans)
```

Any failure before step 5 leaves the previous manifest and its files intact;
the writer removes what it created, and anything left (temp files,
renamed-but-unreferenced segments) is removed on the next open. **Nothing
after step 5 may fail the commit:** a failed step-6 sync only means the
commit might not survive a power loss (in which case the old manifest — a
complete state — reappears), and a failed step-7 removal is cleaned up later.
Both are logged and the commit reports success. The error-cleanup path
therefore never runs after the rename — an earlier draft did, and the
fault-injection sweep caught it deleting freshly committed segments. `sync_dir` opens the directory as a `File` and calls `sync_all`; on
Windows the handle needs `FILE_FLAG_BACKUP_SEMANTICS` and the flush may be
refused — that is treated as success with a debug log, since NTFS journals
directory metadata.

## 8. Open / recovery

```
 1. try (non-blocking) to take the writer lock.
    - Acquired: read MANIFEST while holding it, sweep step 2, release.
      Holding the lock across the read makes the manifest we sweep against
      authoritative — no commit can start or finish while we hold it — which
      matters: reading the manifest first and only *then* locking would leave
      a window where a commit completes in between, and cleanup would delete
      a brand-new segment the (now current, but to us still-unseen) manifest
      legitimately references.
    - Contended: a writer is active. Read MANIFEST without the lock and skip
      step 2 entirely for this open; a reference that's gone stale by the
      time we open it in step 3 surfaces as Vanished, handled by the retry
      below.
    Missing, wrong magic, wrong footer magic, body_len mismatch, or crc
    mismatch → the index is ABSENT (Ok(None)). Nothing is deleted in this case.
    format_version != 2 → Error::Incompatible (never treat a different-version
    index — older or newer — as absent).
 2. remove every seg-*, state-*, and *.tmp not named by the manifest (only
    when step 1 got the lock).
 3. for each segment entry: mmap .idx, .doc, (.del) read-only; check envelope
    magic/version/flags, footer body_len against file length, file length
    against the manifest, footer crc against the manifest crc; check
    .del.num_docs == entry.num_docs; crc-verify the .del body. Do the same
    for state-GGGGG.ovl if state_gen > 0.
    A `NotFound` opening any of these → Vanished, not Corrupt (see below).
    Any other failure → Error::Corrupt (the manifest committed it, so it
    must be whole).
 4. if verify_checksums: crc the .idx and .doc bodies too (reads every byte).
```

**Vanished, and why open retries.** A file the manifest named can be missing
for two different reasons that look identical (`NotFound`) but call for
opposite responses. If a *concurrent commit* retired it — the writer held the
lock for its entire commit, including retirement, but our manifest read (step
1's contended branch) happened *before* that commit finished — then the file
was real when we read the manifest and is correctly gone now; re-reading the
manifest picks up the newer, self-consistent one and the reference is simply
gone from it too. If the file is genuinely missing with no commit in flight,
re-reading changes nothing and the same file vanishes again on every attempt.
`open` distinguishes these the only way that's actually reliable — by trying
again: up to 20 attempts, 5 ms apart, re-running the whole procedure above
(including a fresh MANIFEST read) each time. A transient race resolves within
one or two attempts; real corruption doesn't resolve at all and surfaces as
`Error::Corrupt` once the attempts run out.

Global stats for BM25: `N = Σ (num_indexed − num_deleted)`,
`avgdl = Σ total_len / Σ num_indexed` (deleted docs' lengths stay in the
average; removing them exactly would need a scan), and each query term's
`df` is **summed over segments** before scoring — using a segment's own df
would make a doc's score depend on which segment it landed in.

## 9. Reading

Segments are independent id ranges, so query evaluation runs **per segment**
against local ids with global `N`/`avgdl`, offsets hits by `base_doc`, and
concatenates in manifest order — the result is globally sorted by doc id
without a k-way merge. Per (term, segment): one sparse-index binary search
and one block scan; nothing per query touches a whole dictionary. A cursor
holds a slice into the mapping, a byte position, the remaining doc count, and
a reusable `Vec<u32>` for positions decoded on demand.

`doc(global)` for display: binary search the manifest entries by `base_doc`,
a fixed-offset record read, a path-heap slice, then, if `state_gen > 0`, a
binary search of the overlay by `doc_id` — a hit overrides path/inode/mtime/size.

## 10. Limits and policies

- Segment files < 2⁶³ bytes; sparse region < 4 GiB (u32 offsets); paths < 4 GiB heap per segment.
- Max stored term: 512 bytes. Longer atoms are skipped by the builder (they still count toward document length).
- A fresh build flushes a segment when it reaches `docs_per_segment` (8192) docs **or** `segment_bytes` (64 MiB) of accounted postings heap, whichever first, checked after each 128-file shard. Peak build memory is one segment plus one batch of shards.
- Deleted docs keep their id forever; ids are never reused within a segment; segment ids are never reused within an index — including across a merge, where surviving docs get fresh ids in the merged segment.

## 11. Incremental updates (M3)

`update_index` reconciles the filesystem against the last commit and writes
only the difference, through the same commit protocol as a fresh build — one
manifest, one generation, all-or-nothing.

**Change detection**, cheapest first, comparing a fresh crawl against the doc
table (overlay already applied): path missing → delete. `(inode, mtime,
size)` unchanged → skip, no I/O at all. Metadata changed at the same path →
read + hash; hash unchanged (a rewrite with identical bytes — `rsync`,
`git checkout`, a build system) → an `.ovl` patch, no reindex; hash changed →
tombstone the old id, tokenize and append a new one. A new path whose inode
matches a path no longer present is a rename candidate: `(mtime, size)` also
unchanged → an `.ovl` patch with **zero I/O**; otherwise read + hash to tell
"moved and edited" from "moved, content confirmed identical." `Binary`/
`TooLarge` records have no baseline hash, so a metadata change on one of
those is always a fresh record — still no content read, since there was
never content to hash. A **known cost**: the "content changed" path reads the
file twice — once to hash it against the baseline, once more to tokenize it
during the append. Untouched and renamed files never pay this. Fixing it
means threading the already-read bytes into the tokenizer instead of letting
the append step re-read; deferred.

**Segments**: a reindexed/new file goes into new segments only, based at
`docs().id_bound()` — never rewriting a committed segment. Tombstones update
each affected segment's `.del` (a new generation, exactly like §6). Renames
and touches update `.ovl` (a new generation, §5.1), merging in over whatever
was live before and dropping entries for docs this same commit tombstones.

## 12. Merging

Segments accumulate forever under incremental updates, and query cost scales
with segment count, so merging is required, not optional, once M3 is in use.
[`plan_merges`] is pure: segments bucketed into size tiers by
`floor(log2(live_bytes))` (live bytes floored at 1 MiB so many tiny segments
still pool), a tier merges once it has ≥ 4 members, capped at 10 per merge
group; a segment more than 30% tombstoned is rewritten alone regardless of
tier; a segment ≥ 512 MiB is never merged (the cost outgrows the query
benefit).

**`prepare_merge` / `commit_merges` (M4).** A merge is split into a slow,
lock-free phase and a fast, serialized one, so a merge running for tens of
seconds never blocks the watcher's reconciles behind it for that whole
duration — only the second phase, milliseconds long, does.

- `prepare_merge` takes an `&Index` snapshot (no lock held) and one merge
  group. It decodes every doc and posting *not already deleted as of that
  snapshot* (so a doc tombstoned before the snapshot never enters the merge
  at all), remaps survivors to fresh local ids `0..`, and writes the new
  segment's `.idx`/`.doc` bytes — same encoders as a fresh build — to
  `.ripindex/staging/` rather than directly into `.ripindex/`. Staging is a
  subdirectory the orphan sweep in [§8](#8-open--recovery) never descends
  into (it only lists `.ripindex/`'s direct entries), so a merge's
  in-progress temp files are invisible to a concurrent reader's cleanup
  pass — they can't be raced away out from under it. The result is a
  `PreparedMerge`: the new segment's bytes on disk plus an in-memory
  `old_to_new: (source segment id, source local id) -> merged local id` map
  and a record of which doc ids were already deleted as of the snapshot.
- `commit_merges` takes one or more `PreparedMerge`s, holds the writer lock
  for the whole call, and for every surviving doc checks whether it is
  deleted in the source segment's *current* (not snapshot) `.del` bitmap. A
  "yes" there is exactly a deletion that landed after the snapshot was
  taken; translated through `old_to_new`, it becomes a deletion recorded
  against the merged segment's own fresh `.del` the moment the merge lands —
  no deleted doc is ever resurrected by a merge racing an update. It then
  moves the staged files into place, retires the source segments, and
  commits one new manifest generation — same protocol as any other commit
  ([§7](#7-commit-protocol)): **a merge is still just another commit**, only
  now composed from an unlocked prepare and a locked, much smaller finish.
  Several `PreparedMerge`s (from several groups planned in one pass) commit
  together as one manifest generation; `base_doc` for each is threaded
  through the call rather than recomputed from the not-yet-updated manifest,
  so concurrent groups don't collide on the same doc-id range.
- If a source segment vanishes between prepare and commit (e.g. retired by
  some other writer), `commit_merges` aborts that group and the caller is
  responsible for discarding the `PreparedMerge`'s staged files
  (`PreparedMerge::discard`); nothing is committed.
- `merge_index` remains as the synchronous convenience wrapper (plan, then
  prepare every group, then one `commit_merges` call) used by the plain CLI
  path (`ripindex merge`, and by extension `store::merge_index` when no
  daemon owns the root); the daemon's per-root actor ([M4](#)) instead runs
  `prepare_merge` on a background task and folds `commit_merges` into its
  serialized mailbox loop, which is what actually keeps a merge from
  blocking reconciles for its full duration end to end.

**Scope note.** The current merge still materialises one group's postings
and doc table in memory rather than a lazily-streamed k-way merge of the
segments' term iterators straight to disk — bounded by the merge group (a
handful of segments, each under the size cap), not the whole index, but not
the zero-buffer design described for a production system. The natural
follow-up.

## 13. Live readers and retirement

Once merges and updates run, a query's segment files can be retired while the
query is mid-iteration over their `mmap`. [`LiveIndex`] wraps every segment in
an `Arc`; [`IndexSnapshot`]s handed to queries hold clones; `refresh` swaps in
a new segment list but only *marks* superseded segments for retirement — their
files are removed when the **last** `Arc` drops, which is only after every
query that started before the swap has finished. On Windows, where a mapped
file can't be deleted at all, this is often "immediately once the last
snapshot referencing it drops"; when even that races (a snapshot outlives
the process, or another process holds a mapping), the segment simply outlives
its retirement request, and the orphan sweep at the next `open()` (§8 step 2)
is the backstop.

## 14. The watcher

The watcher is a hint, never a source of truth: inotify queues overflow and
drop events silently, a directory rename arrives as one event covering an
arbitrary number of descendants, FSEvents coalesces to directory granularity,
and nothing orders an event against the filesystem state by the time it's
handled. Every hint — a real event or an overflow — just schedules a
reconcile; §11's reconciler is what decides what actually changed. Hints are
debounced (coalesced into one trigger after a quiet window, default 500 ms)
and a trigger fires at least every `periodic_reconcile_ms` (default 10 min)
regardless of what the watcher reports — the bound on how long a watcher that
lies can leave the index wrong. The scheduling logic is decoupled from real
time and the real event source (a `Clock` trait, an `EventSource` trait), so
it is tested with a scripted, deterministic replay rather than a real
filesystem watch.

**Scope note.** Every trigger reconciles the whole root, not just the
subtree an event named — correct, since reconcile converges regardless of
scope, but not the cheapest possible response to a one-file edit in a huge
tree.

## 15. Resource politeness

A `Governor` combines a byte-rate token bucket (background IO), a CPU duty
cycle (idle time inserted in proportion to work done, so a target fraction of
wall-clock CPU holds on average with no window-boundary case to get wrong),
and battery/memory-pressure signals that override both and pause entirely.
The policy is fully unit-tested against fakes; real battery/memory-pressure
detection is OS-specific and left as a stub (see the module docs), and the
governor is not yet wired into the build/merge hot loops — both are scoped
out honestly rather than left silently unimplemented.

## 16. Future

- FST term dictionary (§4.4) for prefix/range queries.
- Skip data in long posting lists (every 128 docs: doc id + byte offset) so `seek` is sub-linear — the natural fix for §4's dense-phrase hotspot, and the leading candidate for a v3 format bump.
- A fully streaming, zero-buffer k-way merge (§12) instead of the current per-group buffered one.
- Background (unlocked) merging with snapshot-and-carry-forward deletions (§12), so a merge never blocks the watcher.
- Per-subtree reconcile scoping (§14), instead of a whole-root reconcile on every watch trigger.
- Real, per-OS battery and memory-pressure detection for the `Governor` (§15), and wiring it into the build/merge hot loops.
- A shared (non-exclusive) lock mode, so concurrent readers never contend with each other for the brief cleanup-sweep lock in §8.
