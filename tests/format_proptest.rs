//! Property tests: every encoder round-trips through its decoder, including
//! the shapes the tokenizer actually produces (repeated identifier parts).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use proptest::prelude::*;

use ripindex::format::del::{Bitmap, DelView};
use ripindex::format::doc::{self, DocIn, DocKind, DocTableView};
use ripindex::format::idx::{self, DiskCursor, IdxView};
use ripindex::format::manifest::{Manifest, SegmentEntry};
use ripindex::format::varint;
use ripindex::index::memory::MemPostingList;
use ripindex::index::{PostingCursor, PostingList};
use ripindex::tokenizer::tokenize;

type Lists = Vec<(u32, Vec<u32>)>;

fn collect(mut c: DiskCursor<'_>) -> Lists {
    let mut out = vec![];
    while let Some(d) = c.doc() {
        assert_eq!(c.term_freq() as usize, c.positions().len());
        out.push((d, c.positions().to_vec()));
        c.advance();
    }
    out
}

fn encode(list: &Lists) -> Vec<u8> {
    let mut out = Vec::new();
    idx::encode_postings(list.iter().map(|(d, p)| (*d, p.as_slice())), &mut out, &mut Vec::new()).unwrap();
    out
}

/// Sorted unique docs, each with sorted unique non-empty positions.
fn posting_list(max_docs: usize, doc_range: u32, pos_range: u32) -> impl Strategy<Value = Lists> {
    prop::collection::btree_map(
        0..doc_range,
        prop::collection::btree_set(0..pos_range, 1..24),
        0..=max_docs,
    )
    .prop_map(|m| m.into_iter().map(|(d, p)| (d, p.into_iter().collect())).collect())
}

proptest! {
    #[test]
    fn varint_u64_roundtrip(v in any::<u64>()) {
        let mut buf = Vec::new();
        varint::put_u64(&mut buf, v);
        let mut pos = 0;
        prop_assert_eq!(varint::read_u64(&buf, &mut pos).unwrap(), v);
        prop_assert_eq!(pos, buf.len());
        prop_assert!(buf.len() <= 10);
    }

    #[test]
    fn varint_sequences_roundtrip(vs in prop::collection::vec(any::<u32>(), 0..64)) {
        let mut buf = Vec::new();
        for &v in &vs {
            varint::put_u32(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vs {
            prop_assert_eq!(varint::read_u32(&buf, &mut pos).unwrap(), v);
        }
        prop_assert_eq!(pos, buf.len());
    }

    #[test]
    fn postings_roundtrip_sparse(list in posting_list(64, u32::MAX, u32::MAX)) {
        let bytes = encode(&list);
        prop_assert_eq!(collect(DiskCursor::new(&bytes)), list);
    }

    #[test]
    fn postings_roundtrip_dense(list in posting_list(200, 400, 60)) {
        let bytes = encode(&list);
        prop_assert_eq!(collect(DiskCursor::new(&bytes)), list);
    }

    /// Disk cursor and in-memory cursor agree on every seek target.
    #[test]
    fn disk_cursor_seek_matches_memory_cursor(
        list in posting_list(80, 1000, 100),
        targets in prop::collection::vec(0u32..1100, 1..40),
    ) {
        let bytes = encode(&list);
        let mut mem = MemPostingList::default();
        for (d, ps) in &list {
            for &p in ps {
                mem.push(*d, p);
            }
        }
        let mut sorted = targets.clone();
        sorted.sort_unstable(); // seek never moves backwards
        let mut a = DiskCursor::new(&bytes);
        let mut b = mem.cursor();
        for t in sorted {
            prop_assert_eq!(a.seek(t), b.seek(t), "seek {}", t);
            if a.doc().is_some() {
                prop_assert_eq!(a.positions(), b.positions());
                prop_assert_eq!(a.term_freq(), b.term_freq());
            }
        }
        // Then walk both to the end.
        loop {
            prop_assert_eq!(a.advance(), b.advance());
            if a.doc().is_none() {
                break;
            }
        }
    }

    /// Identifiers whose parts repeat (`foo_foo`, `getFooFoo`, `x_x`) run
    /// through the real tokenizer and the encoder must still produce
    /// strictly increasing positions and round-trip exactly.
    #[test]
    fn repeated_identifier_parts_roundtrip(
        docs in prop::collection::vec(
            prop::collection::vec(
                prop::collection::vec(prop::sample::select(vec!["foo", "Foo", "FOO", "bar", "x", "get", "Http", "HTTP", "a"]), 1..5)
                    .prop_map(|parts| {
                        // Half snake_case, half camelCase-by-concatenation.
                        if parts.len() % 2 == 0 { parts.join("_") } else { parts.concat() }
                    }),
                1..12,
            ).prop_map(|idents| idents.join(" ")),
            1..6,
        )
    ) {
        let mut terms: HashMap<String, MemPostingList> = HashMap::new();
        for (doc, text) in docs.iter().enumerate() {
            for tok in tokenize(text) {
                terms.entry(tok.term.into_owned()).or_default().push(doc as u32, tok.position);
            }
        }
        for (term, list) in &terms {
            for p in list.as_slice() {
                prop_assert!(p.positions.windows(2).all(|w| w[0] < w[1]), "{}: {:?}", term, p.positions);
            }
            let mut out = Vec::new();
            idx::encode_postings(list.as_slice().iter().map(|p| (p.doc, p.positions.as_slice())), &mut out, &mut Vec::new())
                .map_err(|e| TestCaseError::fail(format!("{term}: {e}")))?;
            let decoded = collect(DiskCursor::new(&out));
            let expected: Lists = list.as_slice().iter().map(|p| (p.doc, p.positions.clone())).collect();
            prop_assert_eq!(decoded, expected, "{}", term);
        }
    }

    #[test]
    fn term_dictionary_roundtrip(
        terms in prop::collection::btree_map(
            prop::string::string_regex("[a-c_]{1,6}|[a-z]{1,10}|caf[eé]s?|日本語?|[а-я]{1,4}").unwrap(),
            posting_list(4, 50, 20).prop_filter("non-empty", |l| !l.is_empty()),
            0..200,
        ),
        probes in prop::collection::vec(prop::string::string_regex("[a-c_]{0,7}").unwrap(), 0..10),
    ) {
        let sorted: Vec<(&str, Lists)> = terms.iter().map(|(t, l)| (t.as_str(), l.clone())).collect();
        let (bytes, summary) = idx::write(Vec::new(), &sorted).unwrap();
        prop_assert_eq!(summary.len as usize, bytes.len());
        prop_assert_eq!(summary.num_terms as usize, terms.len());
        let view = IdxView::parse(&bytes).unwrap();
        view.verify().map_err(|e| TestCaseError::fail(e.to_string()))?;
        for (t, l) in &terms {
            let found = view.lookup(t).ok_or_else(|| TestCaseError::fail(format!("missing {t:?}")))?;
            prop_assert_eq!(found.doc_freq() as usize, l.len());
            prop_assert_eq!(collect(found.cursor()), l.clone());
        }
        for p in &probes {
            prop_assert_eq!(view.lookup(p).is_some(), terms.contains_key(p), "probe {:?}", p);
        }
        // Iteration yields every term in byte order.
        let mut it = view.terms();
        let mut seen = Vec::new();
        while let Some((t, _, _)) = it.try_next().unwrap() {
            seen.push(t);
        }
        prop_assert_eq!(seen, terms.keys().cloned().collect::<Vec<_>>());
    }

    #[test]
    fn doc_table_roundtrip(
        docs in prop::collection::vec(
            (
                prop::string::string_regex("([a-zA-Z0-9_.é日]{1,8}/){0,3}[a-zA-Z0-9_.é日]{0,12}").unwrap(),
                any::<u64>(),
                any::<i64>(),
                any::<u64>(),
                any::<u32>(),
                0u8..4,
                any::<u64>(),
            ),
            0..64,
        )
    ) {
        let kind_of = |b: u8| match b { 0 => DocKind::Skipped, 1 => DocKind::Indexed, 2 => DocKind::Binary, _ => DocKind::TooLarge };
        let owned: Vec<DocIn> = docs
            .iter()
            .map(|(p, inode, mtime, size, len, kind_byte, hash)| {
                let kind = kind_of(*kind_byte);
                DocIn {
                    path: p,
                    inode: *inode,
                    mtime_nanos: *mtime,
                    size: *size,
                    len: if kind == DocKind::Indexed { *len } else { 0 },
                    kind,
                    content_hash: if kind == DocKind::Binary || kind == DocKind::TooLarge { 0 } else { *hash },
                }
            })
            .collect();
        let bytes = doc::encode(owned.iter().cloned()).unwrap();
        let view = DocTableView::parse(&bytes).unwrap();
        view.verify().map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(view.num_docs() as usize, owned.len());
        prop_assert_eq!(view.num_indexed() as usize, owned.iter().filter(|d| d.kind == DocKind::Indexed).count());
        prop_assert_eq!(view.total_len(), owned.iter().filter(|d| d.kind == DocKind::Indexed).map(|d| d.len as u64).sum::<u64>());
        for (i, d) in owned.iter().enumerate() {
            let r = view.record(i as u32).unwrap();
            prop_assert_eq!(
                (r.inode, r.mtime_nanos, r.size, r.len, r.kind(), r.content_hash),
                (d.inode, d.mtime_nanos, d.size, d.len, Some(d.kind), d.content_hash)
            );
            prop_assert_eq!(view.path(i as u32), Some(d.path));
            prop_assert_eq!(view.doc_len_if_indexed(i as u32), if d.kind == DocKind::Indexed { Some(d.len) } else { None });
        }
        prop_assert!(view.record(owned.len() as u32).is_none());
    }

    #[test]
    fn deletion_bitmap_roundtrip(num_docs in 0u32..300, dels in prop::collection::btree_set(0u32..400, 0..64)) {
        let mut b = Bitmap::new(num_docs);
        let in_range: BTreeSet<u32> = dels.iter().copied().filter(|&d| d < num_docs).collect();
        for &d in &dels {
            let was = b.get(d);
            let newly = b.set(d);
            prop_assert_eq!(newly, d < num_docs && !was, "set {}", d);
        }
        prop_assert_eq!(b.count() as usize, in_range.len());
        let bytes = b.encode();
        let v = DelView::parse_verified(&bytes).unwrap();
        prop_assert_eq!(v.num_docs(), num_docs);
        prop_assert_eq!(v.num_deleted() as usize, in_range.len());
        for d in 0..num_docs + 8 {
            prop_assert_eq!(v.is_deleted(d), in_range.contains(&d));
        }
        prop_assert_eq!(v.to_bitmap(), b);
    }

    #[test]
    fn manifest_roundtrip(
        generation in any::<u64>(),
        nanos in any::<i64>(),
        segs in prop::collection::vec((1u32..1000, 0u32..5, 0u32..1000, any::<u32>(), any::<u64>(), any::<u64>()), 0..20),
    ) {
        // Assign unique ascending ids and non-overlapping bases.
        let mut base = 0u64;
        let mut ids: BTreeMap<u32, ()> = BTreeMap::new();
        let mut segments = Vec::new();
        for (i, (num_docs, del_gen, del_docs, crc, len, tokens)) in segs.into_iter().enumerate() {
            if base + num_docs as u64 > u32::MAX as u64 {
                break;
            }
            let id = i as u32 * 3;
            ids.insert(id, ());
            segments.push(SegmentEntry {
                segment_id: id,
                del_gen,
                base_doc: base as u32,
                num_docs,
                num_deleted: del_docs.min(num_docs),
                idx_crc: crc,
                doc_crc: !crc,
                del_crc: if del_gen > 0 { crc ^ 0x5555 } else { 0 },
                idx_len: len,
                doc_len: len / 2,
                del_len: if del_gen > 0 { 40 } else { 0 },
                num_tokens: tokens,
                num_postings: tokens / 3,
                num_terms: tokens / 7,
            });
            base += num_docs as u64 + (i as u64 % 2); // occasional gap
        }
        let m = Manifest {
            generation,
            next_segment_id: ids.keys().last().map_or(0, |k| k + 1),
            committed_unix_nanos: nanos,
            state_gen: 0,
            state_crc: 0,
            state_len: 0,
            segments,
        };
        let bytes = m.encode();
        prop_assert_eq!(Manifest::decode(&bytes).unwrap(), m);
    }

    #[test]
    fn overlay_roundtrip(
        patches in prop::collection::btree_map(
            0u32..2000,
            (any::<u64>(), any::<i64>(), any::<u64>(), prop::string::string_regex("([a-z0-9_]{1,6}/){0,2}[a-z0-9_.]{0,10}").unwrap()),
            0..40,
        )
    ) {
        use ripindex::format::overlay::{self, OverlayView, PatchIn};
        let ins: Vec<PatchIn> = patches.iter().map(|(&doc_id, (inode, mtime, size, path))| PatchIn { doc_id, inode: *inode, mtime_nanos: *mtime, size: *size, path }).collect();
        let bytes = overlay::encode(&ins).unwrap();
        let v = OverlayView::parse_verified(&bytes).unwrap();
        prop_assert_eq!(v.num_patches() as usize, patches.len());
        for (&doc_id, (inode, mtime, size, path)) in &patches {
            let (found, found_path) = v.find(doc_id).unwrap();
            prop_assert_eq!((found.inode, found.mtime_nanos, found.size), (*inode, *mtime, *size));
            prop_assert_eq!(found_path, path.as_str());
        }
        prop_assert!(v.find(2000).is_none());
    }
}
