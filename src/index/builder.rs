//! Parallel map step and in-memory reduce.
//!
//! * **Map:** files are cut into contiguous shards of [`BuildConfig::shard_size`]
//!   and each shard is processed on a rayon worker. `Text` files: read -> hash
//!   (xxh3-64 of the raw bytes) -> UTF-8 check -> tokenize -> private
//!   `HashMap<term, MemPostingList>`. `Binary`/`TooLarge` files: recorded with
//!   no read at all. A file that fails to read or decode is logged and
//!   reported as skipped (with a hash, if bytes were read); the shard never fails.
//! * **Reduce:** shards are folded into one [`MemTermStore`] in shard order.
//!   Because shards are contiguous doc-id ranges, each term's list stays
//!   sorted by plain append — no per-term sort.
//!
//! [`IndexBuilder::build`] reduces everything into one [`MemIndex`]; the
//! persistent writer (`store::writer`) instead reduces shard-by-shard into a
//! segment accumulator and flushes to disk when a budget is hit.

use std::collections::HashMap;
use std::io;
#[cfg(test)]
use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;

use super::doc_table::{DocMeta, DocStatus, DocTable};
use super::memory::{MemPostingList, MemTermStore};
use super::{DocId, IndexStats, MemIndex, TermStore};
use crate::crawler::{CrawledFile, FileEntry, FileKind};
use crate::tokenizer::tokenize;

#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Files per shard. Smaller = better load balance, more merge work. Default 128.
    pub shard_size: usize,
    /// Flush a segment once it holds this many docs. Default 8192.
    pub docs_per_segment: usize,
    /// ...or once its accounted postings heap reaches this many bytes,
    /// whichever comes first. Bounds peak build memory. Default 64 MiB.
    pub segment_bytes: usize,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self { shard_size: 128, docs_per_segment: 8192, segment_bytes: 64 << 20 }
    }
}

/// xxh3-64 of raw bytes. Used both to decide whether a rewritten file's
/// content actually changed, and (in the segment record) to answer that same
/// question again on the next reconcile without re-tokenizing.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// Output of the map step for one shard. Doc ids are whatever `first_doc`
/// the caller chose — global ids in the persistent writer.
pub(crate) struct Shard {
    pub(crate) first_doc: DocId,
    pub(crate) terms: HashMap<String, MemPostingList>,
    /// `(doc, length in atoms, content hash)` for `Text` docs that indexed cleanly.
    pub(crate) doc_lens: Vec<(DocId, u32, u64)>,
    /// `Text` docs that failed to read/decode — already logged. The hash is
    /// `Some` when the bytes were read (decode failed after that); `None` on
    /// an outright read failure (permissions, vanished).
    pub(crate) skipped: Vec<(DocId, Option<u64>)>,
    /// `Binary`/`TooLarge` docs — never read, recorded for reconcile.
    pub(crate) non_text: Vec<(DocId, DocStatus)>,
    /// (term, position) pairs emitted, for stats.
    pub(crate) tokens: u64,
}

pub struct IndexBuilder {
    config: BuildConfig,
}

impl IndexBuilder {
    pub fn new(config: BuildConfig) -> Self {
        Self { config }
    }

    /// Assign ids `0..files.len()` in order and build one in-memory segment.
    /// All entries are treated as `Text` — `Binary`/`TooLarge` classification
    /// happens in the crawler, which this test-and-demo path bypasses.
    pub fn build(&self, files: Vec<FileEntry>) -> MemIndex<MemTermStore> {
        let crawled: Vec<CrawledFile> = files.into_iter().map(|entry| CrawledFile { entry, kind: FileKind::Text }).collect();
        self.build_crawled(crawled)
    }

    pub(crate) fn build_crawled(&self, files: Vec<CrawledFile>) -> MemIndex<MemTermStore> {
        let start = Instant::now();
        let shard_size = self.config.shard_size.max(1);

        let shards: Vec<Shard> = files
            .par_chunks(shard_size)
            .enumerate()
            .map(|(i, chunk)| index_shard((i * shard_size) as DocId, chunk))
            .collect();

        let mut docs = DocTable::new();
        for (i, cf) in files.into_iter().enumerate() {
            docs.insert(i as DocId, DocMeta::from_entry(cf.entry));
        }
        let mut total_tokens = 0u64;
        let mut skipped = 0u32;
        for shard in &shards {
            for &(doc, len, hash) in &shard.doc_lens {
                docs.mark_indexed(doc, len);
                docs.set_hash(doc, hash);
            }
            for &(doc, hash) in &shard.skipped {
                if let Some(h) = hash {
                    docs.set_hash(doc, h);
                }
            }
            for &(doc, status) in &shard.non_text {
                docs.set_status(doc, status);
            }
            skipped += shard.skipped.len() as u32;
            total_tokens += shard.tokens;
        }

        let terms = merge(shards);
        let stats = IndexStats {
            docs_total: docs.len() as u32,
            docs_indexed: docs.indexed_count(),
            docs_skipped: skipped,
            total_tokens,
            total_postings: terms.total_postings(),
            unique_terms: terms.term_count(),
            segments: 1,
            build_time: start.elapsed(),
            memory_bytes: docs.heap_bytes() + terms.memory_bytes(),
            ..Default::default()
        };
        MemIndex::new(docs, terms, stats)
    }
}

/// Map step for one shard. Infallible; per-file failures land in `Shard::skipped`.
pub(crate) fn index_shard(first_doc: DocId, files: &[CrawledFile]) -> Shard {
    let mut shard = Shard {
        first_doc,
        terms: HashMap::new(),
        doc_lens: Vec::with_capacity(files.len()),
        skipped: Vec::new(),
        non_text: Vec::new(),
        tokens: 0,
    };
    for (i, cf) in files.iter().enumerate() {
        let doc = first_doc + i as DocId;
        match cf.kind {
            FileKind::Binary => shard.non_text.push((doc, DocStatus::Binary)),
            FileKind::TooLarge => shard.non_text.push((doc, DocStatus::TooLarge)),
            FileKind::Text => match std::fs::read(&cf.entry.path) {
                Ok(bytes) => {
                    let hash = hash_bytes(&bytes);
                    match decode_text(bytes) {
                        Ok(text) => {
                            let (len, n) = index_document(doc, &text, &mut shard.terms);
                            shard.doc_lens.push((doc, len, hash));
                            shard.tokens += n;
                        }
                        Err(e) => {
                            log::warn!("skipping {}: {e}", cf.entry.path.display());
                            shard.skipped.push((doc, Some(hash)));
                        }
                    }
                }
                Err(e) => {
                    log::warn!("skipping {}: {e}", cf.entry.path.display());
                    shard.skipped.push((doc, None));
                }
            },
        }
    }
    shard
}

/// UTF-8 decode (BOM stripped) of already-read bytes.
fn decode_text(bytes: Vec<u8>) -> io::Result<String> {
    let mut text = String::from_utf8(bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid UTF-8 at byte {}", e.utf8_error().valid_up_to()),
        )
    })?;
    if text.starts_with('\u{feff}') {
        text.drain(..'\u{feff}'.len_utf8());
    }
    Ok(text)
}

/// Read a whole file as UTF-8 (BOM stripped), and its raw-byte content hash.
/// `index_shard` inlines this instead of calling it, so it can keep the raw
/// bytes around on a decode failure too; kept here for tests.
#[cfg(test)]
pub(crate) fn read_text(path: &Path) -> io::Result<(String, u64)> {
    let bytes = std::fs::read(path)?;
    let hash = hash_bytes(&bytes);
    decode_text(bytes).map(|t| (t, hash))
}

/// Tokenize one document into `terms`. Returns `(length in atoms, tokens emitted)`.
pub(crate) fn index_document(doc: DocId, text: &str, terms: &mut HashMap<String, MemPostingList>) -> (u32, u64) {
    let mut len = 0u32;
    let mut count = 0u64;
    for tok in tokenize(text) {
        len = tok.position + 1;
        count += 1;
        // Two lookups on a miss, one on a hit; misses are rare after the first
        // few files of a shard.
        match terms.get_mut(tok.term.as_ref()) {
            Some(list) => list.push(doc, tok.position),
            None => {
                let mut list = MemPostingList::default();
                list.push(doc, tok.position);
                terms.insert(tok.term.into_owned(), list);
            }
        }
    }
    (len, count)
}

/// Reduce step. `shards` must be in ascending `first_doc` order.
pub(crate) fn merge(shards: Vec<Shard>) -> MemTermStore {
    debug_assert!(shards.windows(2).all(|w| w[0].first_doc < w[1].first_doc));
    let mut store = MemTermStore::default();
    for shard in shards {
        store.merge_shard(shard.terms);
    }
    store
}

/// Test helper: one in-memory segment over texts, doc `i` at id `i`.
#[cfg(test)]
pub(crate) fn mem_index_from_texts(texts: &[&str]) -> MemIndex<MemTermStore> {
    use std::path::PathBuf;
    use std::time::SystemTime;

    let mut docs = DocTable::new();
    let mut terms = HashMap::new();
    let mut total_tokens = 0;
    for (i, text) in texts.iter().enumerate() {
        let id = i as DocId;
        docs.insert(
            id,
            DocMeta {
                path: PathBuf::from(format!("doc{i}")),
                inode: 0,
                mtime: SystemTime::UNIX_EPOCH,
                size: text.len() as u64,
                len: 0,
                status: DocStatus::Skipped,
                content_hash: 0,
            },
        );
        let (len, n) = index_document(id, text, &mut terms);
        docs.mark_indexed(id, len);
        docs.set_hash(id, hash_bytes(text.as_bytes()));
        total_tokens += n;
    }
    let mut store = MemTermStore::default();
    store.merge_shard(terms);
    let stats = IndexStats {
        docs_total: docs.len() as u32,
        docs_indexed: docs.indexed_count(),
        total_tokens,
        total_postings: store.total_postings(),
        unique_terms: store.term_count(),
        segments: 1,
        ..Default::default()
    };
    MemIndex::new(docs, store, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::doc_table::DocStatus;
    use crate::index::postings::{PostingCursor, PostingList};
    use crate::index::SegmentReader;
    use std::fs;

    fn docs_of<S: TermStore>(index: &MemIndex<S>, term: &str) -> Vec<DocId> {
        let mut out = vec![];
        if let Some(list) = index.postings(term) {
            let mut c = list.cursor();
            while let Some(d) = c.doc() {
                out.push(d);
                c.advance();
            }
        }
        out
    }

    #[test]
    fn index_document_records_positions_and_length() {
        let mut terms = HashMap::new();
        let (len, n) = index_document(7, "foo bar_baz foo", &mut terms);
        assert_eq!(len, 3); // three atoms
        assert_eq!(n, 5); // foo, bar_baz, bar, baz, foo
        let foo = &terms["foo"];
        assert_eq!(foo.as_slice()[0].doc, 7);
        assert_eq!(foo.as_slice()[0].positions, vec![0, 2]);
        assert_eq!(terms["bar"].as_slice()[0].positions, vec![1]);
        assert_eq!(terms["bar_baz"].as_slice()[0].positions, vec![1]);
    }

    #[test]
    fn repeated_parts_never_repeat_a_position() {
        let mut terms = HashMap::new();
        index_document(1, "foo_foo x_x getFooFoo Foo_foo_FOO a_a_a", &mut terms);
        for (term, list) in &terms {
            for p in list.as_slice() {
                assert!(p.positions.windows(2).all(|w| w[0] < w[1]), "{term}: {:?}", p.positions);
            }
        }
        assert_eq!(terms["foo"].as_slice()[0].positions, vec![0, 2, 3]);
        assert_eq!(terms["x"].as_slice()[0].positions, vec![1]);
        assert_eq!(terms["a"].as_slice()[0].positions, vec![4]);
    }

    #[test]
    fn read_text_strips_bom_rejects_invalid_utf8_and_hashes_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let bom = dir.path().join("bom.txt");
        fs::write(&bom, b"\xEF\xBB\xBFhello").unwrap();
        let (text, hash) = read_text(&bom).unwrap();
        assert_eq!(text, "hello");
        assert_eq!(hash, hash_bytes(b"\xEF\xBB\xBFhello")); // hash covers the raw bytes, BOM included

        let bad = dir.path().join("bad.txt");
        fs::write(&bad, b"ok \xFF\xFE bad").unwrap();
        let err = read_text(&bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        assert_eq!(read_text(&dir.path().join("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn hash_is_stable_and_sensitive_to_a_single_byte() {
        assert_eq!(hash_bytes(b"hello"), hash_bytes(b"hello"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"hellp"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"hello\n")); // trailing newline changes it
    }

    fn crawled(dir: &Path, name: &str, content: &[u8]) -> CrawledFile {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        let meta = fs::metadata(&p).unwrap();
        CrawledFile {
            entry: FileEntry { path: p, inode: 0, mtime: meta.modified().unwrap(), size: meta.len() },
            kind: FileKind::Text,
        }
    }

    #[test]
    fn build_merges_shards_in_doc_order_and_skips_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let contents: [&[u8]; 5] = [b"alpha beta", b"beta gamma", b"\xFF\xFE not utf8", b"alpha", b"gamma alpha"];
        let files: Vec<FileEntry> = contents.iter().enumerate().map(|(i, c)| crawled(dir.path(), &format!("f{i}.txt"), c).entry).collect();

        // shard_size 2 → shards {0,1}, {2,3}, {4}
        let index = IndexBuilder::new(BuildConfig { shard_size: 2, ..Default::default() }).build(files);

        assert_eq!(index.docs().len(), 5);
        assert_eq!(index.docs().indexed_count(), 4);
        assert_eq!(index.docs().get(2).unwrap().status, DocStatus::Skipped);
        assert_eq!(index.docs().get(2).unwrap().len, 0);
        assert_ne!(index.docs().get(2).unwrap().content_hash, 0, "hash recorded even though decode failed");
        assert_eq!(index.docs().get(0).unwrap().len, 2);
        assert_eq!(index.docs().get(0).unwrap().content_hash, hash_bytes(b"alpha beta"));
        assert_eq!(index.stats.docs_skipped, 1);
        assert_eq!(index.stats.docs_indexed, 4);

        assert_eq!(docs_of(&index, "alpha"), vec![0, 3, 4]); // spans all three shards
        assert_eq!(docs_of(&index, "beta"), vec![0, 1]);
        assert_eq!(docs_of(&index, "gamma"), vec![1, 4]);
        assert_eq!(docs_of(&index, "not"), Vec::<DocId>::new()); // skipped doc contributes nothing
        assert_eq!(index.stats.unique_terms, 3);
        assert_eq!(index.stats.total_tokens, 7);
        assert_eq!(index.stats.total_postings, 7);
        assert!(index.stats.memory_bytes > 0);
    }

    #[test]
    fn binary_and_too_large_are_recorded_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            crawled(dir.path(), "a.txt", b"alpha"),
            CrawledFile { entry: crawled(dir.path(), "b.bin", b"\x00\x01").entry, kind: FileKind::Binary },
            CrawledFile { entry: crawled(dir.path(), "c.big", b"x").entry, kind: FileKind::TooLarge },
        ];
        let index = IndexBuilder::new(BuildConfig::default()).build_crawled(files);
        assert_eq!(index.docs().len(), 3);
        assert_eq!(index.docs().indexed_count(), 1);
        assert_eq!(index.docs().get(1).unwrap().status, DocStatus::Binary);
        assert_eq!(index.docs().get(1).unwrap().content_hash, 0);
        assert_eq!(index.docs().get(2).unwrap().status, DocStatus::TooLarge);
    }

    #[test]
    fn empty_build() {
        let index = IndexBuilder::new(BuildConfig::default()).build(vec![]);
        assert!(index.docs().is_empty());
        assert_eq!(index.terms().term_count(), 0);
        assert_eq!(index.docs().avg_len(), 0.0);
    }
}
