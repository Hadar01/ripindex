//! `ripindex bench`: build, measure open (cold, best-effort, and warm), time a
//! set of queries, and watch RSS while they run.

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::crawler::CrawlConfig;
use crate::error::Result;
use crate::fs::RealFs;
use crate::index::{rss_bytes, BuildConfig, IndexStats};
use crate::query::{search, Query, SearchOptions};
use crate::report::{commas, human_bytes, human_bytes_signed, human_duration};
use crate::store::{build_index, open, NoProgress, OpenOptions};

/// Queries used when none are given. Chosen to exercise each operator on a
/// typical source tree; override with `-q`.
pub const DEFAULT_QUERIES: &[&str] = &[
    "fn",
    "error",
    "impl AND struct",
    "error OR result",
    "\"pub fn\"",
    "\"use std\"",
    "test -mod",
    "http OR response OR request",
    "the quick brown fox",
    "zzzz_no_such_term",
];

#[derive(Debug, Clone)]
pub struct QueryTiming {
    pub query: String,
    /// Matching docs (before the result limit).
    pub matches: usize,
    pub iterations: u32,
    pub mean: Duration,
    pub min: Duration,
    pub p50: Duration,
    pub max: Duration,
}

#[derive(Debug, Clone)]
pub struct BenchReport {
    /// Build stats: files, terms, build time, corpus and on-disk size.
    pub index: IndexStats,
    /// First open after a best-effort page-cache eviction.
    pub open_cold: Duration,
    /// How the eviction was attempted, for honest reporting.
    pub eviction: &'static str,
    /// Second open, page cache warm.
    pub open_warm: Duration,
    pub queries: Vec<QueryTiming>,
    pub rss_before_queries: Option<usize>,
    /// Highest RSS sampled after any query run.
    pub rss_max_during_queries: Option<usize>,
}

/// Build `root`, drop the index, evict its pages (best effort), time a cold
/// and a warm open, then run each query `iterations` times after one warm-up
/// run, sampling RSS. Queries that fail to parse are logged and skipped.
pub fn run(root: &Path, queries: &[String], iterations: u32) -> Result<BenchReport> {
    let built = build_index(&RealFs, root, &CrawlConfig::default(), &BuildConfig::default(), &NoProgress)?;
    let build_stats = built.stats().clone();
    let files = built.files();
    drop(built); // unmap before evicting

    let eviction = evict_page_cache(&files);
    let t = Instant::now();
    let cold = open(&RealFs, root, &OpenOptions::default())?.expect("index was just built");
    let open_cold = t.elapsed();
    drop(cold);
    let t = Instant::now();
    let index = open(&RealFs, root, &OpenOptions::default())?.expect("index was just built");
    let open_warm = t.elapsed();

    let queries: Vec<String> = if queries.is_empty() {
        DEFAULT_QUERIES.iter().map(|s| s.to_string()).collect()
    } else {
        queries.to_vec()
    };
    let opts = SearchOptions::default();
    let iterations = iterations.max(1);
    let rss_before_queries = rss_bytes();
    let mut rss_max = rss_before_queries;

    let mut timings = Vec::with_capacity(queries.len());
    for q in queries {
        let query = match Query::parse(&q) {
            Ok(query) => query,
            Err(e) => {
                log::warn!("bench: skipping query {q:?}: {e}");
                continue;
            }
        };
        let matches = search(&index, &query, &opts).total_matches; // warm-up
        let mut samples: Vec<Duration> = (0..iterations)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(search(&index, &query, &opts));
                t.elapsed()
            })
            .collect();
        samples.sort();
        let n = samples.len();
        timings.push(QueryTiming {
            query: q,
            matches,
            iterations,
            mean: samples.iter().sum::<Duration>() / n as u32,
            min: samples[0],
            p50: samples[n / 2],
            max: samples[n - 1],
        });
        rss_max = rss_max.max(rss_bytes());
    }
    Ok(BenchReport {
        index: build_stats,
        open_cold,
        eviction,
        open_warm,
        queries: timings,
        rss_before_queries,
        rss_max_during_queries: rss_max,
    })
}

/// Ask the OS to drop the cached pages of these files. Unprivileged and
/// best-effort on every platform; the returned string says what was tried.
fn evict_page_cache(files: &[std::path::PathBuf]) -> &'static str {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut ok = true;
        for p in files {
            if let Ok(f) = std::fs::File::open(p) {
                // SAFETY: plain syscall on a valid descriptor; advisory only.
                let r = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
                ok &= r == 0;
            }
        }
        return if ok { "posix_fadvise(DONTNEED) on every index file" } else { "posix_fadvise(DONTNEED) — some calls failed" };
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
        let mut ok = true;
        for p in files {
            // Opening non-cached makes the cache manager drop the file's cached
            // pages once no cached handle remains. Not guaranteed; reported as such.
            ok &= std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_NO_BUFFERING).open(p).is_ok();
        }
        return if ok { "FILE_FLAG_NO_BUFFERING open of every index file (best effort)" } else { "FILE_FLAG_NO_BUFFERING open — some failed" };
    }
    #[allow(unreachable_code)]
    {
        let _ = files;
        "not attempted on this platform"
    }
}

impl fmt::Display for BenchReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "== build ==")?;
        write!(f, "{}", self.index)?;
        writeln!(f)?;
        writeln!(f, "== open ==")?;
        writeln!(f, "cold (best effort)    {}   eviction: {}", human_duration(self.open_cold), self.eviction)?;
        writeln!(f, "warm                  {}", human_duration(self.open_warm))?;
        writeln!(f)?;
        let iters = self.queries.first().map_or(0, |q| q.iterations);
        writeln!(f, "== queries ({iters} timed runs each, after warm-up) ==")?;
        writeln!(
            f,
            "{:<34} {:>9} {:>11} {:>11} {:>11} {:>11}",
            "query", "matches", "mean", "p50", "min", "max"
        )?;
        for q in &self.queries {
            let shown: String = if q.query.chars().count() > 32 {
                format!("{}…", q.query.chars().take(31).collect::<String>())
            } else {
                q.query.clone()
            };
            writeln!(
                f,
                "{:<34} {:>9} {:>11} {:>11} {:>11} {:>11}",
                shown,
                commas(q.matches as u64),
                human_duration(q.mean),
                human_duration(q.p50),
                human_duration(q.min),
                human_duration(q.max),
            )?;
        }
        writeln!(f)?;
        writeln!(f, "== memory while querying ==")?;
        match (self.rss_before_queries, self.rss_max_during_queries) {
            (Some(before), Some(max)) => {
                let growth = max as i64 - before as i64;
                writeln!(f, "RSS after open        {}", human_bytes(before as u64))?;
                writeln!(f, "RSS max during queries {}  ({} growth)", human_bytes(max as u64), human_bytes_signed(growth))?;
                let disk = self.index.on_disk_bytes.max(1);
                writeln!(
                    f,
                    "index on disk         {}  → queries touched at most {:.1}% of it",
                    human_bytes(disk),
                    100.0 * growth.max(0) as f64 / disk as f64
                )?;
            }
            _ => writeln!(f, "RSS                   unavailable on this platform")?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_skips_bad_queries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha beta gamma").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta delta").unwrap();
        let queries = ["beta".to_string(), "-only_negation".to_string(), "\"alpha beta\"".to_string()];
        let report = run(dir.path(), &queries, 3).unwrap();

        assert_eq!(report.index.docs_indexed, 2);
        assert_eq!(report.queries.len(), 2); // the bad one is skipped
        assert_eq!(report.queries[0].query, "beta");
        assert_eq!(report.queries[0].matches, 2);
        assert_eq!(report.queries[0].iterations, 3);
        assert!(report.queries[0].min <= report.queries[0].p50 && report.queries[0].p50 <= report.queries[0].max);
        assert_eq!(report.queries[1].matches, 1);
        // Bounded, not exact: a warm open of a 2-doc index is microseconds, but
        // asserting a *lower* bound flakes on a fast machine (it can round to zero).
        // A generous upper bound still catches the failures that matter here - a hang,
        // or a units mix-up that reports milliseconds as seconds.
        assert!(report.open_warm < Duration::from_secs(60), "warm open took {:?}", report.open_warm);

        let text = report.to_string();
        assert!(text.contains("== build =="));
        assert!(text.contains("== open =="));
        assert!(text.contains("unique terms"));
        assert!(text.contains("\"alpha beta\""));
        assert!(text.contains("== memory while querying =="));
        // The index it built is reusable afterwards.
        assert!(open(&RealFs, dir.path(), &OpenOptions { verify: true }).unwrap().is_some());
    }

    #[test]
    fn default_queries_all_parse() {
        for q in DEFAULT_QUERIES {
            Query::parse(q).unwrap_or_else(|e| panic!("{q:?}: {e}"));
        }
    }
}
