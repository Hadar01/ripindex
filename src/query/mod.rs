//! Query parsing, evaluation and ranking.
//!
//! `Query::parse` is the only fallible step. `search` evaluates the AST against
//! any `Index<S: TermStore>` and returns ranked hits; snippets are the caller's
//! job (`snippet` module), using `Query::highlight_terms`.

pub mod bm25;
pub mod exec;
pub mod parser;

pub use bm25::Bm25;
pub use parser::Node;

use crate::error::QueryError;
use crate::index::{DocId, IndexReader};

/// A parsed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    root: Node,
}

impl Query {
    pub fn parse(input: &str) -> Result<Query, QueryError> {
        parser::parse(input).map(|root| Query { root })
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Distinct positive terms (bare terms and phrase words, not negated ones),
    /// in first-appearance order — what snippets highlight.
    pub fn highlight_terms(&self) -> Vec<&str> {
        fn walk<'a>(n: &'a Node, out: &mut Vec<&'a str>) {
            let mut add = |t: &'a str| {
                if !out.contains(&t) {
                    out.push(t);
                }
            };
            match n {
                Node::Term(t) => add(t),
                Node::Phrase(ts) => ts.iter().for_each(|t| add(t)),
                Node::And { must, .. } => must.iter().for_each(|c| walk(c, out)),
                Node::Or(cs) => cs.iter().for_each(|c| walk(c, out)),
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Maximum hits returned. Default 20.
    pub limit: usize,
    pub bm25: Bm25,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { limit: 20, bm25: Bm25::default() }
    }
}

/// One scored document. Path and metadata come from `index.docs().get(doc)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub doc: DocId,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Sorted by score descending, ties by `DocId` ascending; at most `limit`.
    pub hits: Vec<Hit>,
    /// Matching docs before the limit was applied.
    pub total_matches: usize,
}

/// Evaluate and rank.
pub fn search<R: IndexReader>(index: &R, query: &Query, opts: &SearchOptions) -> SearchResult {
    let mut hits = exec::evaluate(index, &query.root, &opts.bm25);
    let total_matches = hits.len();
    let by_rank = |a: &Hit, b: &Hit| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc));
    if hits.len() > opts.limit {
        // Partition so the top `limit` are in front, then sort just those.
        if opts.limit > 0 {
            hits.select_nth_unstable_by(opts.limit - 1, by_rank);
        }
        hits.truncate(opts.limit);
    }
    hits.sort_unstable_by(by_rank);
    SearchResult { hits, total_matches }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::writer::test_support::index_from_texts;

    #[test]
    fn highlight_terms_skips_negations_and_dedups() {
        let q = Query::parse("foo \"bar baz\" -nope (foo OR qux) -\"no way\"").unwrap();
        assert_eq!(q.highlight_terms(), vec!["foo", "bar", "baz", "qux"]);
    }

    #[test]
    fn search_ranks_and_limits() {
        let idx = index_from_texts(&["a", "a a", "a a a", "a a a a", "b"]);
        let q = Query::parse("a").unwrap();

        let all = search(&idx, &q, &SearchOptions { limit: 100, ..Default::default() });
        assert_eq!(all.total_matches, 4);
        assert_eq!(all.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), vec![3, 2, 1, 0]);
        assert!(all.hits.windows(2).all(|w| w[0].score >= w[1].score));

        let top2 = search(&idx, &q, &SearchOptions { limit: 2, ..Default::default() });
        assert_eq!(top2.total_matches, 4);
        assert_eq!(top2.hits, all.hits[..2].to_vec());

        let none = search(&idx, &q, &SearchOptions { limit: 0, ..Default::default() });
        assert_eq!(none.total_matches, 4);
        assert!(none.hits.is_empty());
    }

    #[test]
    fn ties_break_by_doc_id() {
        let idx = index_from_texts(&["x y", "x z", "x w"]);
        let r = search(&idx, &Query::parse("x").unwrap(), &SearchOptions::default());
        assert_eq!(r.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), vec![0, 1, 2]);
    }
}
