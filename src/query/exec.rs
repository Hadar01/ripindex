//! AST evaluation over an `IndexReader`.
//!
//! Segments own disjoint, ascending doc-id ranges, so each node is evaluated
//! **per segment** on local ids and the per-segment results — each sorted by
//! local id — are offset by the segment base and concatenated. AND / OR / NOT
//! and phrases are all within-doc, so this is exact, and the output is
//! globally sorted with no k-way merge. BM25 uses global `N`, `avgdl`, and
//! per-term df summed over segments, so scores are independent of segmentation.
//!
//! Within a segment every node evaluates to a `Vec<Hit>` sorted by doc, one
//! entry per doc. Leaves pull postings through cursors; inner nodes combine
//! by linear merge (`union`) or by walking the shortest list and
//! binary-searching the rest (`intersect`), summing scores.
//!
//! A posting whose doc has no length (`SegmentReader::doc_len` → `None`:
//! skipped, deleted, or a hole) is dropped — the doc table is the authority.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::bm25::Bm25;
use super::parser::Node;
use super::Hit;
use crate::index::{IndexReader, Position, PostingCursor, PostingList, SegmentReader};

/// Global BM25 statistics: `N`, `avgdl`, and each query term's document
/// frequency **summed over all segments**. Scoring with a segment's own df
/// would make a doc's score depend on which segment it landed in.
struct Stats<'q> {
    n: u32,
    avg_len: f32,
    df: HashMap<&'q str, u32>,
}

/// Every distinct term the query names, positive or negated.
fn query_terms<'q>(node: &'q Node, out: &mut Vec<&'q str>) {
    let mut add = |t: &'q str| {
        if !out.contains(&t) {
            out.push(t);
        }
    };
    match node {
        Node::Term(t) => add(t),
        Node::Phrase(ts) => ts.iter().for_each(|t| add(t)),
        Node::And { must, must_not } => must.iter().chain(must_not).for_each(|c| query_terms(c, out)),
        Node::Or(cs) => cs.iter().for_each(|c| query_terms(c, out)),
    }
}

/// The posting lists one segment has for the query's terms — looked up once.
type Lists<'s, S> = HashMap<&'s str, <S as SegmentReader>::List<'s>>;

/// Evaluate `node` over every segment. Result is sorted by global doc, no duplicates.
pub fn evaluate<R: IndexReader>(reader: &R, node: &Node, bm25: &Bm25) -> Vec<Hit> {
    let mut terms = Vec::new();
    query_terms(node, &mut terms);
    let segments: Vec<&R::Segment> = reader.segments().collect();

    // One dictionary lookup per (term, segment), reused for df and evaluation.
    let lookups: Vec<Lists<'_, R::Segment>> = segments
        .iter()
        .map(|seg| terms.iter().filter_map(|&t| seg.postings(t).map(|l| (t, l))).collect())
        .collect();
    let df = terms
        .iter()
        .map(|&t| (t, lookups.iter().filter_map(|m| m.get(t)).map(|l| l.doc_freq()).sum::<u32>()))
        .collect();
    let stats = Stats { n: reader.indexed_count(), avg_len: reader.avg_len(), df };

    let mut out = Vec::new();
    for (seg, lists) in segments.iter().zip(&lookups) {
        let base = seg.base();
        let hits = eval_node(*seg, lists, node, bm25, &stats);
        out.extend(hits.into_iter().map(|h| Hit { doc: base + h.doc, score: h.score }));
    }
    out
}

fn eval_node<'s, S: SegmentReader>(seg: &'s S, lists: &Lists<'s, S>, node: &Node, bm25: &Bm25, stats: &Stats) -> Vec<Hit> {
    match node {
        Node::Term(t) => eval_term(seg, lists, t, bm25, stats),
        Node::Phrase(ts) => eval_phrase(seg, lists, ts, bm25, stats),
        Node::And { must, must_not } => {
            let mut acc = Vec::with_capacity(must.len());
            for n in must {
                let l = eval_node(seg, lists, n, bm25, stats);
                if l.is_empty() {
                    return Vec::new();
                }
                acc.push(l);
            }
            let mut result = intersect(acc);
            for n in must_not {
                if result.is_empty() {
                    break;
                }
                let excluded = eval_node(seg, lists, n, bm25, stats);
                result = difference(result, &excluded);
            }
            result
        }
        Node::Or(children) => union(children.iter().map(|n| eval_node(seg, lists, n, bm25, stats)).collect()),
    }
}

/// Single term: walk its cursor, BM25-score each live doc.
fn eval_term<'s, S: SegmentReader>(seg: &'s S, lists: &Lists<'s, S>, term: &str, bm25: &Bm25, stats: &Stats) -> Vec<Hit> {
    let Some(list) = lists.get(term) else {
        return Vec::new();
    };
    let idf = bm25.idf(stats.n, stats.df[term]);
    let mut cur = list.cursor();
    let mut out = Vec::with_capacity(list.doc_freq() as usize);
    while let Some(doc) = cur.doc() {
        if let Some(len) = seg.doc_len(doc) {
            let tf = cur.term_freq();
            out.push(Hit { doc, score: bm25.score(idf, tf, len, stats.avg_len) });
        }
        cur.advance();
    }
    out
}

/// Phrase: leapfrog all term cursors to their common docs, then count the
/// positions `p` of `terms[0]` such that `terms[i]` occurs at `p + i` for
/// every `i`. That count is `tf`; the phrase's idf is the sum of its terms'.
fn eval_phrase<'s, S: SegmentReader>(seg: &'s S, lists: &Lists<'s, S>, terms: &[String], bm25: &Bm25, stats: &Stats) -> Vec<Hit> {
    match terms.len() {
        0 => return Vec::new(),
        1 => return eval_term(seg, lists, &terms[0], bm25, stats),
        _ => {}
    }
    let mut cursors = Vec::with_capacity(terms.len());
    for t in terms {
        match lists.get(t.as_str()) {
            Some(l) => cursors.push(l.cursor()),
            None => return Vec::new(),
        }
    }
    let idf: f32 = terms.iter().map(|t| bm25.idf(stats.n, stats.df[t.as_str()])).sum();
    let mut out = Vec::new();

    'docs: while let Some(mut target) = cursors[0].doc() {
        // Align every cursor on `target`; whenever one overshoots, adopt its
        // doc as the new target and start over from the first cursor.
        let mut i = 1;
        while i < cursors.len() {
            match cursors[i].seek(target) {
                None => break 'docs,
                Some(d) if d == target => i += 1,
                Some(d) => {
                    target = d;
                    match cursors[0].seek(target) {
                        None => break 'docs,
                        Some(d0) => target = d0,
                    }
                    i = 1;
                }
            }
        }
        if let Some(len) = seg.doc_len(target) {
            let positions: Vec<&[Position]> = cursors.iter_mut().map(|c| c.positions()).collect();
            let tf = phrase_matches(&positions);
            if tf > 0 {
                out.push(Hit { doc: target, score: bm25.score(idf, tf, len, stats.avg_len) });
            }
        }
        cursors[0].advance();
    }
    out
}

/// Count phrase occurrences in one doc. `positions[i]` are the ascending
/// positions of the i-th phrase term.
fn phrase_matches(positions: &[&[Position]]) -> u32 {
    let Some((first, rest)) = positions.split_first() else {
        return 0;
    };
    first
        .iter()
        .filter(|&&p0| {
            rest.iter().enumerate().all(|(k, ps)| {
                p0.checked_add(k as u32 + 1).is_some_and(|p| ps.binary_search(&p).is_ok())
            })
        })
        .count() as u32
}

/// Docs present in every list; scores summed. Walks the shortest list and
/// binary-searches forward in each other list.
fn intersect(mut lists: Vec<Vec<Hit>>) -> Vec<Hit> {
    if lists.is_empty() || lists.iter().any(|l| l.is_empty()) {
        return Vec::new();
    }
    lists.sort_by_key(|l| l.len());
    let mut lists = lists.into_iter();
    let mut result = lists.next().unwrap();
    for other in lists {
        let mut j = 0;
        result.retain_mut(|h| {
            j += other[j..].partition_point(|o| o.doc < h.doc);
            match other.get(j) {
                Some(o) if o.doc == h.doc => {
                    h.score += o.score;
                    true
                }
                _ => false,
            }
        });
        if result.is_empty() {
            break;
        }
    }
    result
}

/// Docs present in any list; scores summed. Pairwise linear merges.
fn union(lists: Vec<Vec<Hit>>) -> Vec<Hit> {
    lists.into_iter().fold(Vec::new(), merge_sum)
}

fn merge_sum(a: Vec<Hit>, b: Vec<Hit>) -> Vec<Hit> {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].doc.cmp(&b[j].doc) {
            Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push(Hit { doc: a[i].doc, score: a[i].score + b[j].score });
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// `base` minus every doc in `excluded`. Scores of `base` are kept as-is.
fn difference(mut base: Vec<Hit>, excluded: &[Hit]) -> Vec<Hit> {
    if excluded.is_empty() {
        return base;
    }
    let mut j = 0;
    base.retain(|h| {
        j += excluded[j..].partition_point(|e| e.doc < h.doc);
        !matches!(excluded.get(j), Some(e) if e.doc == h.doc)
    });
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::DocId;
    use crate::query::parser::parse;
    use crate::store::writer::test_support::{index_from_texts, TestIndex};

    fn corpus() -> TestIndex {
        index_from_texts(&[
            "the quick brown fox",                        // 0
            "the lazy dog",                               // 1
            "quick quick quick brown",                    // 2
            "fox and dog",                                // 3
            "parse_http_response returns HttpResponse",   // 4
            "",                                           // 5: empty, indexed with len 0
        ])
    }

    fn run<R: IndexReader>(index: &R, q: &str) -> Vec<Hit> {
        evaluate(index, &parse(q).unwrap(), &Bm25::default())
    }

    fn ids(hits: &[Hit]) -> Vec<DocId> {
        hits.iter().map(|h| h.doc).collect()
    }

    fn score_of(hits: &[Hit], doc: DocId) -> f32 {
        hits.iter().find(|h| h.doc == doc).unwrap().score
    }

    #[test]
    fn corpus_spans_several_segments() {
        let idx = corpus();
        assert_eq!(idx.stats().segments, 3);
        assert_eq!(idx.docs().id_bound(), 6);
    }

    #[test]
    fn single_term() {
        let idx = corpus();
        assert_eq!(ids(&run(&idx, "the")), vec![0, 1]);
        assert_eq!(ids(&run(&idx, "fox")), vec![0, 3]);
        assert_eq!(ids(&run(&idx, "THE")), vec![0, 1]); // folded
    }

    #[test]
    fn missing_term_is_empty() {
        let idx = corpus();
        assert!(run(&idx, "zebra").is_empty());
        assert!(run(&idx, "zebra fox").is_empty());
        assert!(run(&idx, "\"zebra fox\"").is_empty());
        assert_eq!(ids(&run(&idx, "zebra OR fox")), vec![0, 3]);
    }

    #[test]
    fn empty_index() {
        let idx = index_from_texts(&[]);
        assert!(run(&idx, "anything").is_empty());
        assert!(run(&idx, "a OR b").is_empty());
        assert!(run(&idx, "\"a b\"").is_empty());
    }

    #[test]
    fn higher_tf_scores_higher() {
        let idx = corpus();
        let hits = run(&idx, "quick");
        assert_eq!(ids(&hits), vec![0, 2]);
        assert!(score_of(&hits, 2) > score_of(&hits, 0));
        assert!(hits.iter().all(|h| h.score.is_finite() && h.score > 0.0));
    }

    #[test]
    fn rarer_term_scores_higher() {
        let idx = corpus();
        // "lazy" appears in 1 doc, "the" in 2; both tf=1 in doc 1.
        assert!(score_of(&run(&idx, "lazy"), 1) > score_of(&run(&idx, "the"), 1));
    }

    #[test]
    fn and_intersects_and_sums() {
        let idx = corpus();
        assert_eq!(ids(&run(&idx, "quick brown")), vec![0, 2]);
        assert_eq!(ids(&run(&idx, "quick AND fox")), vec![0]);
        assert!(run(&idx, "quick dog").is_empty());
        assert_eq!(ids(&run(&idx, "the quick brown fox")), vec![0]);

        let both = score_of(&run(&idx, "quick brown"), 0);
        let q = score_of(&run(&idx, "quick"), 0);
        let b = score_of(&run(&idx, "brown"), 0);
        assert!((both - (q + b)).abs() < 1e-5);
    }

    #[test]
    fn or_unions_and_sums() {
        let idx = corpus();
        let hits = run(&idx, "fox OR dog");
        assert_eq!(ids(&hits), vec![0, 1, 3]);
        // Doc 3 has both terms, so it outscores docs with one.
        assert!(score_of(&hits, 3) > score_of(&hits, 0));
        assert!(score_of(&hits, 3) > score_of(&hits, 1));
        assert_eq!(ids(&run(&idx, "lazy OR lazy")), vec![1]); // no duplicates
    }

    #[test]
    fn negation() {
        let idx = corpus();
        assert_eq!(ids(&run(&idx, "quick -fox")), vec![2]);
        assert_eq!(ids(&run(&idx, "the -dog")), vec![0]);
        assert!(run(&idx, "the -dog -fox").is_empty());
        assert_eq!(ids(&run(&idx, "quick -zebra")), vec![0, 2]); // excluding nothing
        assert_eq!(ids(&run(&idx, "(fox OR dog) -the")), vec![3]);
        assert_eq!(ids(&run(&idx, "fox -\"lazy dog\"")), vec![0, 3]);
        // Negation does not change the surviving scores.
        assert_eq!(score_of(&run(&idx, "quick -fox"), 2), score_of(&run(&idx, "quick"), 2));
    }

    #[test]
    fn phrases_use_positions() {
        let idx = corpus();
        assert_eq!(ids(&run(&idx, "\"quick brown\"")), vec![0, 2]);
        assert_eq!(ids(&run(&idx, "\"brown fox\"")), vec![0]);
        assert!(run(&idx, "\"fox quick\"").is_empty()); // wrong order
        assert!(run(&idx, "\"quick fox\"").is_empty()); // not adjacent
        assert_eq!(ids(&run(&idx, "\"the quick brown fox\"")), vec![0]);
        assert_eq!(ids(&run(&idx, "\"quick quick\"")), vec![2]);
    }

    #[test]
    fn phrase_tf_counts_occurrences() {
        // "quick quick quick": "quick quick" occurs at positions 0 and 1.
        assert_eq!(phrase_matches(&[&[0, 1, 2], &[0, 1, 2]]), 2);
        assert_eq!(phrase_matches(&[&[0, 5], &[1, 6], &[2, 7]]), 2);
        assert_eq!(phrase_matches(&[&[0, 5], &[1, 6], &[2, 8]]), 1);
        assert_eq!(phrase_matches(&[&[3], &[1]]), 0);
        assert_eq!(phrase_matches(&[&[u32::MAX], &[0]]), 0); // no overflow wraparound
        assert_eq!(phrase_matches(&[]), 0);
    }

    #[test]
    fn identifier_parts_match_terms_but_not_phrases() {
        let idx = corpus();
        assert_eq!(ids(&run(&idx, "http")), vec![4]);
        assert_eq!(ids(&run(&idx, "response")), vec![4]);
        assert_eq!(ids(&run(&idx, "parse_http_response")), vec![4]);
        assert_eq!(ids(&run(&idx, "HttpResponse")), vec![4]);
        assert_eq!(ids(&run(&idx, "parse http response")), vec![4]); // AND of parts
        // Documented asymmetry: parts share one position, so no phrase across them...
        assert!(run(&idx, "\"parse http\"").is_empty());
        // ...but a part does stand in for its atom in a phrase with a neighbour.
        assert_eq!(ids(&run(&idx, "\"response returns\"")), vec![4]);
        assert_eq!(ids(&run(&idx, "\"returns httpresponse\"")), vec![4]);
        assert_eq!(ids(&run(&idx, "\"returns response\"")), vec![4]);
    }

    #[test]
    fn removed_docs_are_skipped_even_with_stale_postings() {
        let idx = corpus().with_deleted(&[0]);
        assert_eq!(ids(&run(&idx, "the")), vec![1]);
        assert_eq!(ids(&run(&idx, "\"quick brown\"")), vec![2]);
        assert_eq!(ids(&run(&idx, "fox OR dog")), vec![1, 3]);
        assert_eq!(ids(&run(&idx, "quick -fox")), vec![2]);
        assert!(idx.doc(0).is_none());
        assert_eq!(idx.indexed_count(), 5);
    }

    #[test]
    fn in_memory_and_persistent_agree() {
        use crate::index::builder::mem_index_from_texts;
        let texts = ["alpha beta gamma", "beta beta", "gamma alpha alpha", "delta", "alpha_beta gammaDelta"];
        let mem = mem_index_from_texts(&texts);
        let disk = index_from_texts(&texts);
        for q in ["alpha", "beta OR gamma", "alpha -gamma", "\"alpha beta\"", "\"gamma alpha alpha\"", "delta", "alpha_beta", "gamma AND delta"] {
            let a = run(&mem, q);
            let b = run(&disk, q);
            assert_eq!(ids(&a), ids(&b), "{q}");
            for (x, y) in a.iter().zip(&b) {
                assert!((x.score - y.score).abs() < 1e-5, "{q}: {} vs {}", x.score, y.score);
            }
        }
    }

    #[test]
    fn set_ops_directly() {
        let h = |docs: &[DocId]| -> Vec<Hit> { docs.iter().map(|&d| Hit { doc: d, score: 1.0 }).collect() };
        assert_eq!(ids(&intersect(vec![h(&[1, 2, 3, 5, 8]), h(&[2, 3, 4, 8]), h(&[0, 3, 8, 9])])), vec![3, 8]);
        assert!(intersect(vec![h(&[1, 2]), h(&[])]).is_empty());
        assert!(intersect(vec![]).is_empty());
        assert_eq!(ids(&union(vec![h(&[1, 3]), h(&[2, 3]), h(&[]), h(&[0])])), vec![0, 1, 2, 3]);
        assert_eq!(score_of(&union(vec![h(&[1, 3]), h(&[2, 3])]), 3), 2.0);
        assert_eq!(ids(&difference(h(&[1, 2, 3, 4]), &h(&[2, 4, 6]))), vec![1, 3]);
        assert_eq!(ids(&difference(h(&[1, 2]), &h(&[]))), vec![1, 2]);
        assert!(difference(h(&[1, 2]), &h(&[0, 1, 2, 3])).is_empty());
    }
}
