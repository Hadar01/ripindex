//! Kill-9 harness for the commit protocol.
//!
//! `run` loops: spawn a `child` that builds state *i* (a deterministic corpus
//! derived from *i*), kill it with SIGKILL/TerminateProcess at a uniformly
//! random moment inside the build window, then reopen the index with full
//! checksum verification and check that it is exactly state *i−1* or exactly
//! state *i*. The child stretches every file-system operation with a small
//! delay so kills land inside the commit protocol, not just before or after
//! it. Any other outcome — a corrupt file, a missing segment, a mixed doc set,
//! a query answering wrongly — fails the run.
//!
//! Drive it with `scripts/crash_loop.ps1` / `.sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use ripindex::crawler::CrawlConfig;
use ripindex::fs::{DelayFs, RealFs};
use ripindex::index::{BuildConfig, DocStatus};
use ripindex::query::{search, Query, SearchOptions};
use ripindex::store::{build_index, open, NoProgress, OpenOptions};
use ripindex::tokenizer::tokenize;

#[derive(Parser)]
#[command(name = "crash-harness", about = "Kill-9 crash-consistency harness for ripindex")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the kill loop.
    Run {
        /// Working directory (created; wiped first).
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value_t = 500)]
        iterations: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Per file-system-operation delay in the child, in microseconds.
        #[arg(long, default_value_t = 400)]
        delay_us: u64,
    },
    /// Internal: build one state, then exit 0.
    Child {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        state: u64,
        #[arg(long)]
        delay_us: u64,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Child { dir, state, delay_us } => child(&dir, state, delay_us),
        Cmd::Run { dir, iterations, seed, delay_us } => run(&dir, iterations, seed, delay_us),
    }
}

// --------------------------------------------------------------------------- corpus

const VOCAB: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "parse_http_response", "HttpResponse", "x_x", "foo_foo",
    "getFooFoo", "café", "naïve", "日本語", "std::fs::read", "snake_case_name", "XMLHttpRequest", "sha256", "utf8Decode",
];

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn doc_count(state: u64) -> u64 {
    20 + (state * 7) % 40
}

/// `(relative path, content)` for every doc of `state`.
fn corpus(state: u64) -> Vec<(String, String)> {
    (0..doc_count(state))
        .map(|j| {
            let mut rng = XorShift::new(state * 1_000_003 + j);
            let n = 3 + rng.below(25) as usize;
            let mut words: Vec<&str> = Vec::with_capacity(n + 4);
            words.push("alpha");
            if j % 3 == 0 {
                words.push("beta");
            }
            for _ in 0..n {
                words.push(VOCAB[rng.below(VOCAB.len() as u64) as usize]);
            }
            (format!("doc_{j:03}.txt"), format!("state{state} {}\n", words.join(" ")))
        })
        .collect()
}

fn write_corpus(root: &Path, state: u64) {
    fs::create_dir_all(root).unwrap();
    let docs = corpus(state);
    let keep: std::collections::HashSet<&str> = docs.iter().map(|(n, _)| n.as_str()).collect();
    for entry in fs::read_dir(root).unwrap() {
        let p = entry.unwrap().path();
        if p.is_file() && !keep.contains(p.file_name().unwrap().to_str().unwrap()) {
            fs::remove_file(p).unwrap();
        }
    }
    for (name, content) in &docs {
        fs::write(root.join(name), content).unwrap();
    }
}

/// What an index over `state` must look like.
struct Expected {
    docs: Vec<(String, u32)>, // (name, len) sorted
    /// Docs containing the word `beta` anywhere (the random tail can add one).
    beta_docs: usize,
    /// Docs where `beta` directly follows `alpha`.
    phrase_docs: usize,
}

fn expected(state: u64) -> Expected {
    let docs = corpus(state);
    let mut d: Vec<(String, u32)> =
        docs.iter().map(|(n, c)| (n.clone(), tokenize(c).map(|t| t.position + 1).max().unwrap_or(0))).collect();
    d.sort();
    let (mut beta_docs, mut phrase_docs) = (0, 0);
    for (_, c) in &docs {
        // Every vocabulary word is either one atom or never `alpha`/`beta`, so
        // whitespace adjacency equals position adjacency for this phrase.
        let words: Vec<&str> = c.split_whitespace().collect();
        beta_docs += words.contains(&"beta") as usize;
        phrase_docs += words.windows(2).any(|w| w == ["alpha", "beta"]) as usize;
    }
    Expected { docs: d, beta_docs, phrase_docs }
}

fn crawl_cfg() -> CrawlConfig {
    CrawlConfig { respect_global_gitignore: false, ..CrawlConfig::default() }
}

fn build_cfg() -> BuildConfig {
    // Several small segments per build so every commit renames many files.
    BuildConfig { shard_size: 4, docs_per_segment: 12, ..BuildConfig::default() }
}

// --------------------------------------------------------------------------- child

fn child(dir: &Path, state: u64, delay_us: u64) {
    let root = dir.join("corpus");
    write_corpus(&root, state);
    // The previous commit must be intact before we build on top of it.
    match open(&RealFs, &root, &OpenOptions { verify: true }) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("child: existing index failed verification: {e}");
            std::process::exit(2);
        }
    }
    let fs_ = DelayFs::new(Duration::from_micros(delay_us));
    match build_index(&fs_, &root, &crawl_cfg(), &build_cfg(), &NoProgress) {
        Ok(_) => {
            println!("COMMITTED {state}");
        }
        Err(e) => {
            eprintln!("child: build failed: {e}");
            std::process::exit(3);
        }
    }
}

// --------------------------------------------------------------------------- run

/// Which state the index on disk is, or why it is neither.
fn check(root: &Path, candidates: &[u64]) -> Result<Option<u64>, String> {
    let index = match open(&RealFs, root, &OpenOptions { verify: true }) {
        Ok(Some(i)) => i,
        Ok(None) => return Ok(None),
        Err(e) => return Err(format!("open/verify failed: {e}")),
    };
    let mut docs: Vec<(String, u32)> = index
        .docs()
        .iter()
        .map(|(_, m)| {
            assert_eq!(m.status, DocStatus::Indexed, "{}", m.path.display());
            (m.path.file_name().unwrap().to_string_lossy().into_owned(), m.len)
        })
        .collect();
    docs.sort();
    for &s in candidates {
        let e = expected(s);
        if docs != e.docs {
            continue;
        }
        let q = |text: &str| {
            search(&index, &Query::parse(text).unwrap(), &SearchOptions { limit: 10_000, ..Default::default() }).total_matches
        };
        let n = e.docs.len();
        let checks = [
            (format!("state{s}"), n),
            ("alpha".to_string(), n),
            ("beta".to_string(), e.beta_docs),
            ("\"alpha beta\"".to_string(), e.phrase_docs),
            (format!("state{}", s + 1), 0),
            ("alpha -beta".to_string(), n - e.beta_docs),
        ];
        for (text, want) in checks {
            let got = q(&text);
            if got != want {
                return Err(format!("state {s}: query {text:?} matched {got}, expected {want}"));
            }
        }
        return Ok(Some(s));
    }
    Err(format!(
        "doc set matches none of {candidates:?}: {} docs, generation {}, {} segments",
        docs.len(),
        index.manifest().generation,
        index.stats().segments
    ))
}

fn spawn_child(dir: &Path, state: u64, delay_us: u64) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child", "--dir"])
        .arg(dir)
        .args(["--state", &state.to_string(), "--delay-us", &delay_us.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child")
}

fn run(dir: &Path, iterations: u32, seed: u64, delay_us: u64) {
    if dir.exists() {
        fs::remove_dir_all(dir).expect("wipe working dir");
    }
    fs::create_dir_all(dir).unwrap();
    let root = dir.join("corpus");

    // Calibrate on a build that has a predecessor to read and retire — the
    // very first build is shorter and would leave the post-commit ops unaimed.
    let out = spawn_child(dir, 1, delay_us).wait_with_output().unwrap();
    assert!(out.status.success(), "calibration child failed");
    assert_eq!(check(&root, &[1]).unwrap(), Some(1));
    let t = Instant::now();
    let out = spawn_child(dir, 2, delay_us).wait_with_output().unwrap();
    assert!(out.status.success(), "calibration child failed");
    let window = t.elapsed();
    let mut prev = 2u64;
    assert_eq!(check(&root, &[2]).unwrap(), Some(2));
    println!("calibrated: a build over an existing index takes {window:.2?}; kills land uniformly in [0, {:.2?}]", window.mul_f64(1.05));

    let mut rng = XorShift::new(seed);
    let (mut kept_old, mut took_new, mut absent) = (0u32, 0u32, 0u32);
    let started = Instant::now();
    for i in 0..iterations {
        let state = prev + 1 + rng.below(3); // occasionally skip a state so doc counts vary
        let mut child = spawn_child(dir, state, delay_us);
        let kill_after = Duration::from_micros(rng.below(window.mul_f64(1.05).as_micros() as u64));
        std::thread::sleep(kill_after);
        let _ = child.kill();
        let _ = child.wait();

        match check(&root, &[state, prev]) {
            Ok(Some(s)) if s == state => {
                took_new += 1;
                prev = state;
            }
            Ok(Some(_)) => kept_old += 1,
            Ok(None) => {
                // Only legal if there was never a commit — there was (calibration).
                eprintln!("FAIL iteration {i}: index absent after kill at {kill_after:.2?} (prev state {prev})");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("FAIL iteration {i}: {e} (killed at {kill_after:.2?}, prev {prev}, target {state})");
                eprintln!("working dir left for inspection: {}", dir.display());
                std::process::exit(1);
            }
        }
        if (i + 1) % 100 == 0 {
            println!(
                "{}/{iterations} ok — kept old: {kept_old}, took new: {took_new}, absent: {absent} ({:.1?} elapsed)",
                i + 1,
                started.elapsed()
            );
        }
        absent += 0;
    }
    println!(
        "PASS: {iterations} kills, index always exactly the previous or the new state (kept old: {kept_old}, took new: {took_new}) in {:.1?}",
        started.elapsed()
    );
}
