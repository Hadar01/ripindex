//! Directory crawler: turns a root directory into the ordered list of files
//! the indexer will consider.
//!
//! Filters, in order:
//! 1. regular files only (symlinks are not followed);
//! 2. `.gitignore` / `.ignore` semantics via the `ignore` crate — honored even
//!    when the root is not inside a git repository; `.git/` and `.ripindex/`
//!    (our own index) are always excluded;
//! 3. size ≤ [`CrawlConfig::max_size`], else `FileKind::TooLarge`;
//! 4. no NUL byte in the first [`CrawlConfig::sniff_bytes`] bytes, else `FileKind::Binary`.
//!
//! Per-file errors (stat/open failures) are logged and the file dropped —
//! there is nothing to record it as. Binary and too-large files, unlike M1,
//! *are* returned (by [`crawl_all`]): the doc table needs their identity so a
//! later reconcile can tell "unchanged" from "changed" by metadata alone,
//! without re-sniffing every non-text file on every pass.
//!
//! [`crawl`] keeps the M1/M2 signature (text files only) for callers that
//! don't need the rest.

use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::SystemTime;

use ignore::{WalkBuilder, WalkState};

use crate::error::{Error, Result};

/// One file, as discovered by the crawler, of any [`FileKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Filesystem identity: inode on Unix, NTFS file index on Windows.
    /// 0 if unavailable.
    pub inode: u64,
    pub mtime: SystemTime,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Passed every filter; readable as UTF-8 text (checked later, at index time).
    Text,
    /// A NUL byte in the sniff window.
    Binary,
    /// Larger than [`CrawlConfig::max_size`].
    TooLarge,
}

/// One crawled file with its classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawledFile {
    pub entry: FileEntry,
    pub kind: FileKind,
}

#[derive(Debug, Clone)]
pub struct CrawlConfig {
    /// Files larger than this are skipped. Default 10 MiB.
    pub max_size: u64,
    /// Leading bytes inspected for a NUL byte. Default 8 KiB.
    pub sniff_bytes: usize,
    /// Include dotfiles (`.gitignore`, `.github/…`). `.git/` is excluded regardless.
    pub include_hidden: bool,
    /// Also apply the user's global gitignore (`core.excludesFile`). Default on;
    /// tests turn it off so results don't depend on the machine.
    pub respect_global_gitignore: bool,
    /// Walker threads; 0 = number of CPUs.
    pub threads: usize,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            max_size: 10 * 1024 * 1024,
            sniff_bytes: 8 * 1024,
            include_hidden: true,
            respect_global_gitignore: true,
            threads: 0,
        }
    }
}

/// Counters from one crawl. Entries pruned by `.gitignore` are not counted —
/// the `ignore` crate never yields them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CrawlStats {
    /// Regular files reached by the walk (before size/binary filtering).
    pub files_seen: u64,
    pub skipped_too_large: u64,
    pub skipped_binary: u64,
    /// stat/open failures — logged, not fatal.
    pub errors: u64,
    /// Text files found (== `FileKind::Text` count); the length of [`crawl`]'s Vec.
    pub indexable: u64,
    /// Sum of sizes of text files — the corpus the index covers.
    pub indexable_bytes: u64,
}

#[derive(Default)]
struct Counters {
    files_seen: AtomicU64,
    too_large: AtomicU64,
    binary: AtomicU64,
    errors: AtomicU64,
}

/// Walk `root` in parallel and return the text files (M1/M2 behaviour).
/// See [`crawl_all`] to also get `Binary`/`TooLarge` entries.
///
/// Ordering: the parallel walk emits in nondeterministic order, so the result
/// is **sorted by path** before returning. Doc IDs are assigned in this order
/// for a fresh build, which makes builds (and tests) reproducible.
pub fn crawl(root: &Path, config: &CrawlConfig) -> Result<(Vec<FileEntry>, CrawlStats)> {
    crawl_with_progress(root, config, &|_| {})
}

/// [`crawl`], calling `progress` with the running count of files seen
/// (every 256 files, from walker threads).
pub fn crawl_with_progress(
    root: &Path,
    config: &CrawlConfig,
    progress: &(dyn Fn(u64) + Sync),
) -> Result<(Vec<FileEntry>, CrawlStats)> {
    let (all, stats) = crawl_all_with_progress(root, config, progress)?;
    let text = all.into_iter().filter(|f| f.kind == FileKind::Text).map(|f| f.entry).collect();
    Ok((text, stats))
}

/// Walk `root` in parallel and return every file of every [`FileKind`],
/// sorted by path.
pub fn crawl_all(root: &Path, config: &CrawlConfig) -> Result<(Vec<CrawledFile>, CrawlStats)> {
    crawl_all_with_progress(root, config, &|_| {})
}

/// [`crawl_all`] with a progress callback (every 256 files seen).
pub fn crawl_all_with_progress(
    root: &Path,
    config: &CrawlConfig,
    progress: &(dyn Fn(u64) + Sync),
) -> Result<(Vec<CrawledFile>, CrawlStats)> {
    if !root.is_dir() {
        return Err(Error::NotADirectory(root.to_path_buf()));
    }
    let threads = if config.threads == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    } else {
        config.threads
    };

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!config.include_hidden)
        .require_git(false)
        .git_global(config.respect_global_gitignore)
        .follow_links(false)
        .threads(threads)
        // Never index a VCS store or our own index directory.
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != crate::store::DIR_NAME);

    let entries: Mutex<Vec<CrawledFile>> = Mutex::new(Vec::new());
    let counters = Counters::default();

    builder.build_parallel().run(|| {
        Box::new(|result| {
            match result {
                Err(err) => {
                    log::warn!("crawl: {err}");
                    counters.errors.fetch_add(1, Relaxed);
                }
                Ok(entry) => {
                    if entry.file_type().is_some_and(|t| t.is_file()) {
                        let seen = counters.files_seen.fetch_add(1, Relaxed) + 1;
                        if seen % 256 == 0 {
                            progress(seen);
                        }
                        let path = entry.path();
                        match inspect(path, config) {
                            Ok(cf) => {
                                match cf.kind {
                                    FileKind::TooLarge => {
                                        log::debug!("{}: larger than {} bytes", path.display(), config.max_size);
                                        counters.too_large.fetch_add(1, Relaxed);
                                    }
                                    FileKind::Binary => {
                                        log::debug!("{}: binary", path.display());
                                        counters.binary.fetch_add(1, Relaxed);
                                    }
                                    FileKind::Text => {}
                                }
                                entries.lock().unwrap().push(cf);
                            }
                            Err(err) => {
                                log::warn!("skipping {}: {err}", path.display());
                                counters.errors.fetch_add(1, Relaxed);
                            }
                        }
                    }
                }
            }
            WalkState::Continue
        })
    });

    let mut files = entries.into_inner().unwrap();
    files.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    let stats = CrawlStats {
        files_seen: counters.files_seen.load(Relaxed),
        skipped_too_large: counters.too_large.load(Relaxed),
        skipped_binary: counters.binary.load(Relaxed),
        errors: counters.errors.load(Relaxed),
        indexable: files.iter().filter(|f| f.kind == FileKind::Text).count() as u64,
        indexable_bytes: files.iter().filter(|f| f.kind == FileKind::Text).map(|f| f.entry.size).sum(),
    };
    Ok((files, stats))
}

/// Open, stat, sniff. One open per file; the handle is also used for the
/// Windows file id. Builds a [`FileEntry`] for every outcome (the caller
/// records `Binary`/`TooLarge` files too, without their content).
fn inspect(path: &Path, config: &CrawlConfig) -> io::Result<CrawledFile> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let entry = |file: &File, meta: &Metadata| FileEntry {
        path: path.to_path_buf(),
        inode: file_id(file, meta),
        mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: meta.len(),
    };
    if meta.len() > config.max_size {
        return Ok(CrawledFile { entry: entry(&file, &meta), kind: FileKind::TooLarge });
    }
    let want = config.sniff_bytes.min(meta.len() as usize);
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let kind = if looks_binary(&buf[..filled]) { FileKind::Binary } else { FileKind::Text };
    Ok(CrawledFile { entry: entry(&file, &meta), kind })
}

/// True if `prefix` — the first `sniff_bytes` of a file — contains a NUL byte.
pub fn looks_binary(prefix: &[u8]) -> bool {
    prefix.contains(&0)
}

/// Platform file identity. See [`FileEntry::inode`].
#[cfg(unix)]
fn file_id(_file: &File, meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

/// Platform file identity. See [`FileEntry::inode`]. Stable std does not
/// expose the NTFS file index, so ask the kernel via the open handle.
#[cfg(windows)]
fn file_id(file: &File, _meta: &Metadata) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

    // SAFETY: `info` is a plain-data struct that the call fully initialises on
    // success; the handle is valid for as long as `file` is borrowed.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return 0;
    }
    ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64
}

#[cfg(not(any(unix, windows)))]
fn file_id(_file: &File, _meta: &Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_sniff() {
        assert!(!looks_binary(b""));
        assert!(!looks_binary(b"plain text\n"));
        assert!(!looks_binary("ünïcödé 日本語".as_bytes()));
        assert!(looks_binary(b"\x00"));
        assert!(looks_binary(b"MZ\x90\x00\x03"));
        assert!(looks_binary(b"text then \x00 nul"));
    }

    #[test]
    fn not_a_directory_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(crawl(&file, &CrawlConfig::default()), Err(Error::NotADirectory(_))));
        assert!(matches!(
            crawl(&dir.path().join("missing"), &CrawlConfig::default()),
            Err(Error::NotADirectory(_))
        ));
    }

    #[test]
    fn file_ids_distinguish_files_and_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let id = |p: &Path| {
            let f = File::open(p).unwrap();
            let m = f.metadata().unwrap();
            file_id(&f, &m)
        };
        assert_eq!(id(&a), id(&a));
        if id(&a) != 0 {
            assert_ne!(id(&a), id(&b));
        }
    }

    #[test]
    fn crawl_all_reports_binary_and_too_large_with_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("b.bin"), b"\x00\x01\x02").unwrap();
        std::fs::write(dir.path().join("c.big"), vec![b'x'; 100]).unwrap();
        let cfg = CrawlConfig { max_size: 50, respect_global_gitignore: false, ..CrawlConfig::default() };
        let (all, stats) = crawl_all(dir.path(), &cfg).unwrap();
        assert_eq!(all.len(), 3);
        let kinds: Vec<(String, FileKind)> =
            all.iter().map(|f| (f.entry.path.file_name().unwrap().to_string_lossy().into_owned(), f.kind)).collect();
        assert_eq!(kinds, vec![("a.txt".into(), FileKind::Text), ("b.bin".into(), FileKind::Binary), ("c.big".into(), FileKind::TooLarge)]);
        assert_eq!(stats.indexable, 1);
        assert_eq!(stats.skipped_binary, 1);
        assert_eq!(stats.skipped_too_large, 1);
        // Binary/too-large entries still carry a real inode/size, just no content.
        assert!(all[1].entry.size == 3);
        assert!(all[2].entry.size == 100);

        // crawl() filters to text only, unchanged from M1/M2.
        let (text, stats2) = crawl(dir.path(), &cfg).unwrap();
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].path.file_name().unwrap(), "a.txt");
        assert_eq!(stats2, stats);
    }
}
