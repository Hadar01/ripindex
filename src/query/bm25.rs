//! BM25 scoring.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25 {
    pub k1: f32,
    pub b: f32,
}

impl Default for Bm25 {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25 {
    /// `ln(1 + (N - df + 0.5) / (df + 0.5))` — the non-negative variant.
    pub fn idf(&self, n_docs: u32, doc_freq: u32) -> f32 {
        let n = n_docs as f32;
        let df = (doc_freq as f32).min(n);
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// `idf * tf * (k1 + 1) / (tf + k1 * (1 - b + b * dl / avgdl))`.
    /// `avg_len == 0` (empty index) is treated as 1 to avoid NaN.
    pub fn score(&self, idf: f32, tf: u32, doc_len: u32, avg_len: f32) -> f32 {
        if tf == 0 {
            return 0.0;
        }
        let avg = if avg_len > 0.0 { avg_len } else { 1.0 };
        let tf = tf as f32;
        let norm = self.k1 * (1.0 - self.b + self.b * doc_len as f32 / avg);
        idf * tf * (self.k1 + 1.0) / (tf + norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let b = Bm25::default();
        assert_eq!(b.k1, 1.2);
        assert_eq!(b.b, 0.75);
    }

    #[test]
    fn idf_falls_as_df_rises_and_stays_non_negative() {
        let b = Bm25::default();
        let rare = b.idf(1000, 1);
        let common = b.idf(1000, 500);
        let everywhere = b.idf(1000, 1000);
        assert!(rare > common && common > everywhere);
        assert!(everywhere >= 0.0);
        // df > N can't happen, but must not go negative if it does.
        assert!(b.idf(10, 20) >= 0.0);
    }

    #[test]
    fn tf_saturates() {
        let b = Bm25::default();
        let s1 = b.score(1.0, 1, 100, 100.0);
        let s2 = b.score(1.0, 2, 100, 100.0);
        let s10 = b.score(1.0, 10, 100, 100.0);
        let s100 = b.score(1.0, 100, 100, 100.0);
        assert!(s1 < s2 && s2 < s10 && s10 < s100);
        assert!(s2 - s1 > s100 - s10); // diminishing returns
        assert!(s100 < 1.0 * (b.k1 + 1.0)); // bounded by idf * (k1 + 1)
    }

    #[test]
    fn shorter_docs_score_higher_for_equal_tf() {
        let b = Bm25::default();
        assert!(b.score(1.0, 3, 50, 100.0) > b.score(1.0, 3, 200, 100.0));
    }

    #[test]
    fn zero_tf_and_empty_index_are_finite() {
        let b = Bm25::default();
        assert_eq!(b.score(1.0, 0, 10, 10.0), 0.0);
        assert!(b.score(1.0, 1, 10, 0.0).is_finite());
        assert!(b.idf(0, 0).is_finite());
    }
}
