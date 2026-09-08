//! Crawl → build → search → snippet over a real directory fixture, plus a CLI
//! smoke test against the built binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ripindex::crawler::{crawl, CrawlConfig};
use ripindex::index::{build_from_dir, BuildConfig, DocStatus, Index};
use ripindex::query::{search, Query, SearchOptions};
use ripindex::snippet;

/// A small tree exercising every crawler filter and both identifier styles.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let w = |rel: &str, content: &[u8]| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w(".gitignore", b"ignored/\n*.log\n");
    w("README.md", b"# Project\n\nThis is a test project. Hello world.\n");
    w(
        "src/main.rs",
        b"fn main() {\n    parse_http_response();\n}\n\nfn parse_http_response() -> HttpResponse {\n    HttpResponse::default()\n}\n",
    );
    w("src/lib.rs", "pub struct HttpResponse;\n\npub fn snake_case_name() {}\n// na\u{ef}ve caf\u{e9} r\u{e9}sum\u{e9}\n".as_bytes());
    w(".hidden/notes.txt", b"hidden hello");
    w("big.txt", "hello world ".repeat(20).as_bytes()); // 240 bytes
    w("bad_utf8.txt", b"hello \xFF\xFE world"); // no NUL: passes the sniff, fails at read
    w("bin.dat", b"\x00\x01\x02hello world");
    w("ignored/secret.txt", b"hello world");
    w("debug.log", b"hello world");
    w(".git/config", b"hello world");
    dir
}

fn cfg() -> CrawlConfig {
    CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")
}

fn paths_of(root: &Path, index: &Index, hits: &[ripindex::query::Hit]) -> Vec<String> {
    hits.iter().map(|h| rel(root, &index.docs().get(h.doc).unwrap().path)).collect()
}

fn run(root: &Path, index: &Index, q: &str) -> Vec<String> {
    let query = Query::parse(q).unwrap();
    let r = search(index, &query, &SearchOptions { limit: 100, ..Default::default() });
    let mut paths = paths_of(root, index, &r.hits);
    paths.sort();
    paths
}

#[test]
fn crawl_applies_every_filter() {
    let dir = fixture();
    let root = dir.path();
    // big.txt is 240 bytes, everything else is under 200.
    let (files, stats) = crawl(root, &CrawlConfig { max_size: 200, ..cfg() }).unwrap();

    let got: Vec<String> = files.iter().map(|f| rel(root, &f.path)).collect();
    let mut expected = vec![".gitignore", ".hidden/notes.txt", "README.md", "bad_utf8.txt", "src/lib.rs", "src/main.rs"];
    expected.sort();
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(sorted, expected);
    // Output is in path order, so doc ids will be too.
    let paths: Vec<&PathBuf> = files.iter().map(|f| &f.path).collect();
    assert!(paths.windows(2).all(|w| w[0] < w[1]));

    assert_eq!(stats.files_seen, 8); // + bin.dat, big.txt; never ignored/, *.log, .git/
    assert_eq!(stats.skipped_binary, 1);
    assert_eq!(stats.skipped_too_large, 1);
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.indexable, 6);

    for f in &files {
        assert!(f.size > 0);
        assert!(f.mtime > std::time::SystemTime::UNIX_EPOCH);
    }
}

#[test]
fn hidden_files_can_be_excluded() {
    let dir = fixture();
    let (files, _) = crawl(dir.path(), &CrawlConfig { include_hidden: false, ..cfg() }).unwrap();
    let got: Vec<String> = files.iter().map(|f| rel(dir.path(), &f.path)).collect();
    assert!(!got.iter().any(|p| p.starts_with('.')), "{got:?}");
    assert!(got.contains(&"README.md".to_string()));
}

#[test]
fn build_assigns_ids_in_path_order_and_records_skipped_docs() {
    let dir = fixture();
    let root = dir.path();
    let index = build_from_dir(root, &cfg(), &BuildConfig { shard_size: 2, ..Default::default() }).unwrap();

    // 8 files pass the crawl filters; bin.dat is now recorded as `Binary`
    // (M3), not silently dropped, so it and bad_utf8.txt both count toward
    // docs_total without postings.
    assert_eq!(index.docs().len(), 8);
    assert_eq!(index.docs().id_bound(), 8);
    let entries: Vec<(u32, String, DocStatus)> =
        index.docs().iter().map(|(id, m)| (id, rel(root, &m.path), m.status)).collect();
    let ids: Vec<u32> = entries.iter().map(|e| e.0).collect();
    assert_eq!(ids, (0..8).collect::<Vec<_>>());
    let paths: Vec<&String> = entries.iter().map(|e| &e.1).collect();
    assert!(paths.windows(2).all(|w| w[0] < w[1]), "{paths:?}");

    let bad = entries.iter().find(|e| e.1 == "bad_utf8.txt").unwrap();
    assert_eq!(bad.2, DocStatus::Skipped);
    let binary = entries.iter().find(|e| e.1 == "bin.dat").unwrap();
    assert_eq!(binary.2, DocStatus::Binary);
    assert!(entries.iter().filter(|e| e.2 == DocStatus::Indexed).count() == 6);
    assert_eq!(index.docs().indexed_count(), 6);

    let s = index.stats();
    assert_eq!(s.docs_total, 8);
    assert_eq!(s.docs_indexed, 6);
    assert_eq!(s.docs_skipped, 2); // bad_utf8.txt (Skipped) + bin.dat (Binary)
    assert_eq!(s.crawl.indexable, 7); // text files only
    assert!(s.unique_terms > 0 && s.total_tokens >= s.total_postings && s.total_postings > 0);
    assert!(s.memory_bytes > 0);
    assert!(s.rss_bytes.is_some(), "RSS should be available on this platform");
}

#[test]
fn search_end_to_end() {
    let dir = fixture();
    let root = dir.path();
    let index = build_from_dir(root, &cfg(), &BuildConfig::default()).unwrap();

    // Ignored, binary, .git and undecodable files never match.
    assert_eq!(run(root, &index, "hello"), vec![".hidden/notes.txt", "README.md", "big.txt"]);
    assert_eq!(run(root, &index, "\"hello world\""), vec!["README.md", "big.txt"]);
    assert_eq!(run(root, &index, "hello -world"), vec![".hidden/notes.txt"]);
    assert_eq!(run(root, &index, "hello AND project"), vec!["README.md"]);
    assert_eq!(run(root, &index, "hidden OR project"), vec![".hidden/notes.txt", "README.md"]);

    // Identifier parts on the index side, whole atoms on the query side.
    assert_eq!(run(root, &index, "http"), vec!["src/lib.rs", "src/main.rs"]);
    assert_eq!(run(root, &index, "HttpResponse"), vec!["src/lib.rs", "src/main.rs"]);
    assert_eq!(run(root, &index, "parse_http_response"), vec!["src/main.rs"]);
    assert_eq!(run(root, &index, "snake_case_name"), vec!["src/lib.rs"]);
    assert_eq!(run(root, &index, "case"), vec!["src/lib.rs"]);
    assert_eq!(run(root, &index, "snake AND case"), vec!["src/lib.rs"]);
    // Parts share their atom's position: no phrase across the parts of one identifier...
    assert!(run(root, &index, "\"snake case\"").is_empty());
    // ...but parts of *adjacent* atoms are consecutive: `parse_http_response() -> HttpResponse`.
    assert!(!run(root, &index, "\"parse http\"").is_empty());
    assert_eq!(run(root, &index, "\"response http\""), vec!["src/main.rs"]);
    assert_eq!(run(root, &index, "\"fn parse_http_response\""), vec!["src/main.rs"]);

    // Unicode folding.
    assert_eq!(run(root, &index, "café"), vec!["src/lib.rs"]);
    assert_eq!(run(root, &index, "CAFÉ"), vec!["src/lib.rs"]);

    // Empty results.
    let none = search(&index, &Query::parse("nonexistent_term_xyz").unwrap(), &SearchOptions::default());
    assert!(none.hits.is_empty());
    assert_eq!(none.total_matches, 0);

    // Ranking: big.txt has tf=20 for "hello" and outranks single occurrences.
    let r = search(&index, &Query::parse("hello").unwrap(), &SearchOptions::default());
    assert_eq!(r.total_matches, 3);
    assert_eq!(rel(root, &index.docs().get(r.hits[0].doc).unwrap().path), "big.txt");
    assert!(r.hits.windows(2).all(|w| w[0].score >= w[1].score));

    // Limit.
    let r = search(&index, &Query::parse("hello").unwrap(), &SearchOptions { limit: 1, ..Default::default() });
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.total_matches, 3);
}

#[test]
fn snippets_re_read_files_and_highlight_atoms() {
    let dir = fixture();
    let root = dir.path();
    let index = build_from_dir(root, &cfg(), &BuildConfig::default()).unwrap();

    let query = Query::parse("http").unwrap();
    let r = search(&index, &query, &SearchOptions::default());
    let main_rs = r
        .hits
        .iter()
        .map(|h| index.docs().get(h.doc).unwrap())
        .find(|m| rel(root, &m.path) == "src/main.rs")
        .unwrap();
    let s = snippet::snippet_for(&main_rs.path, &query.highlight_terms(), 160).unwrap();
    assert_eq!(s.line_no, 2);
    assert_eq!(snippet::render(&s, false), "[parse_http_response]();");

    // A file that changed underneath us just loses its snippet.
    fs::remove_file(&main_rs.path).unwrap();
    assert!(snippet::snippet_for(&main_rs.path, &query.highlight_terms(), 160).is_none());
}

#[test]
fn cli_smoke() {
    let dir = fixture();
    let root = dir.path();
    let bin = env!("CARGO_BIN_EXE_ripindex");

    let out = Command::new(bin).args(["index"]).arg(root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("docs indexed"), "{stdout}");
assert!(stdout.contains("not indexed         2"), "{stdout}");
    assert!(stdout.contains("unique terms"), "{stdout}");
    // The undecodable file is reported on stderr, not fatal.
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad_utf8.txt"));

    let out = Command::new(bin)
        .args(["search", "hello", "--no-color", "--no-daemon", "--root"])
        .arg(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("README.md:3"), "{stdout}");
    assert!(stdout.contains("[Hello] world"), "{stdout}");
    assert!(!stdout.contains("secret.txt") && !stdout.contains("debug.log"), "{stdout}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("3 of 3 matching files shown"));

    // Root via environment variable.
    let out = Command::new(bin)
        .args(["search", "\"hello world\" -project", "--no-color", "--no-daemon"])
        .env("RIPINDEX_ROOT", root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("big.txt") && !stdout.contains("README.md"), "{stdout}");

    // Bad query: fails before building, with the parser's message.
    let out = Command::new(bin).args(["search", "--root"]).arg(root).args(["--", "-foo"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("negation"));

    // Bad root.
    let out = Command::new(bin).args(["index"]).arg(root.join("nope")).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory"));
}
