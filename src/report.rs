//! Human-readable formatting shared by `index` and `bench`.

use std::fmt;
use std::time::Duration;

use crate::index::IndexStats;

/// `1234567` → `1,234,567`.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `12.3 MiB`, `456 B`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Signed variant for deltas: `+12.3 MiB`, `-456 B`.
pub fn human_bytes_signed(delta: i64) -> String {
    let sign = if delta < 0 { "-" } else { "+" };
    format!("{sign}{}", human_bytes(delta.unsigned_abs()))
}

/// `1.234 s`, `12.3 ms`, `456 µs`.
pub fn human_duration(d: Duration) -> String {
    let us = d.as_micros();
    if us >= 1_000_000 {
        format!("{:.3} s", d.as_secs_f64())
    } else if us >= 1_000 {
        format!("{:.2} ms", us as f64 / 1000.0)
    } else {
        format!("{us} µs")
    }
}

impl fmt::Display for IndexStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.crawl;
        writeln!(f, "files seen            {}", commas(c.files_seen))?;
        writeln!(
            f,
            "  skipped             {} too large, {} binary, {} errors",
            commas(c.skipped_too_large),
            commas(c.skipped_binary),
            commas(c.errors)
        )?;
        writeln!(f, "docs indexed          {}", commas(self.docs_indexed as u64))?;
        if self.docs_skipped > 0 {
            writeln!(f, "  not indexed         {}", commas(self.docs_skipped as u64))?;
        }
        writeln!(f, "total terms           {}", commas(self.total_tokens))?;
        writeln!(f, "postings              {}", commas(self.total_postings))?;
        writeln!(f, "unique terms          {}{}", commas(self.unique_terms as u64), if self.segments > 1 { "  (summed over segments)" } else { "" })?;
        writeln!(f, "segments              {}", self.segments)?;
        if self.crawl.indexable_bytes > 0 {
            writeln!(
                f,
                "corpus size           {}",
                human_bytes(self.crawl.indexable_bytes)
            )?;
        }
        writeln!(
            f,
            "index on disk         {}{}",
            human_bytes(self.on_disk_bytes),
            if self.crawl.indexable_bytes > 0 {
                format!("  ({:.1}% of corpus)", 100.0 * self.on_disk_bytes as f64 / self.crawl.indexable_bytes as f64)
            } else {
                String::new()
            }
        )?;
        if !self.crawl_time.is_zero() {
            writeln!(f, "crawl time            {}", human_duration(self.crawl_time))?;
        }
        if !self.build_time.is_zero() {
            writeln!(f, "build time            {}", human_duration(self.build_time))?;
        }
        writeln!(f, "open time             {}", human_duration(self.open_time))?;
        writeln!(f, "reader heap           {}  (accounted; segment data is mapped)", human_bytes(self.memory_bytes as u64))?;
        match (self.rss_bytes, self.rss_delta_bytes) {
            (Some(rss), Some(delta)) => writeln!(
                f,
                "process RSS           {}  ({} during crawl + build + open)",
                human_bytes(rss as u64),
                human_bytes_signed(delta)
            ),
            (Some(rss), None) => writeln!(f, "process RSS           {}", human_bytes(rss as u64)),
            _ => writeln!(f, "process RSS           unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comma_grouping() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1000), "1,000");
        assert_eq!(commas(1234567), "1,234,567");
    }

    #[test]
    fn byte_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(12_900_000), "12.3 MiB");
        assert_eq!(human_bytes_signed(-2048), "-2.0 KiB");
        assert_eq!(human_bytes_signed(512), "+512 B");
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(Duration::from_micros(456)), "456 µs");
        assert_eq!(human_duration(Duration::from_micros(12_345)), "12.35 ms");
        assert_eq!(human_duration(Duration::from_millis(1234)), "1.234 s");
    }
}
