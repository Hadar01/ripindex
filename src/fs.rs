//! File-system access behind a trait, so the commit protocol can run under
//! injected faults.
//!
//! * [`RealFs`] — production.
//! * [`FaultFs`] — test double over a real directory: fails the Nth write,
//!   fails the Nth sync, discards the bytes of the Nth write (after which that
//!   file's next `sync_all` fails with EIO, as Linux reports lost writeback),
//!   or "crashes" at the Nth operation (that op and every later one fail, as
//!   if the process were gone). Every operation is logged so tests can assert
//!   the exact syscall sequence of the protocol.
//! * [`DelayFs`] — sleeps before every operation, stretching the commit window
//!   so the kill-9 harness lands inside it.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use memmap2::Mmap;

/// A file open for writing.
pub trait FsFile: Write + Send {
    fn sync_all(&mut self) -> io::Result<()>;
}

/// An exclusive lock, released on drop (and by the OS on process death).
pub trait FsLock: Send {}

pub trait Fs: Send + Sync {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Create or truncate `path` for writing.
    fn create(&self, path: &Path) -> io::Result<Box<dyn FsFile + '_>>;
    /// Whole small file.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Read-only mapping of a whole, non-empty file.
    fn mmap(&self, path: &Path) -> io::Result<Mmap>;
    fn file_len(&self, path: &Path) -> io::Result<u64>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Entries of `dir` as full paths.
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    /// Make `dir`'s entries durable: open it as a file and `sync_all`.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Exclusive advisory lock on `path` (created if absent).
    /// `ErrorKind::WouldBlock` if another process holds it.
    fn lock(&self, path: &Path) -> io::Result<Box<dyn FsLock + '_>>;
}

// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

struct RealFile(File);

impl Write for RealFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl FsFile for RealFile {
    fn sync_all(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }
}

struct RealLock(#[allow(dead_code)] File);
impl FsLock for RealLock {}

impl Fs for RealFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn FsFile + '_>> {
        Ok(Box::new(RealFile(File::create(path)?)))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn mmap(&self, path: &Path) -> io::Result<Mmap> {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cannot map an empty file"));
        }
        // SAFETY: committed segment files are never modified in place by this
        // crate (temp-write + rename only). A file truncated or rewritten by
        // something else while mapped is undefined behaviour at the OS level —
        // a SIGBUS/access violation on the faulting read, which no `Result`
        // can catch. The envelope checks on open do NOT make reads safe
        // against that; tantivy accepts the same risk.
        unsafe { Mmap::map(&file) }
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::metadata(path)?.len())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn list_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            out.push(entry?.path());
        }
        out.sort();
        Ok(out)
    }

    #[cfg(unix)]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    /// NTFS journals directory metadata, and `FlushFileBuffers` on a directory
    /// handle is refused on some configurations; a refusal is logged, not fatal.
    #[cfg(windows)]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        let handle = OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir)?;
        if let Err(e) = handle.sync_all() {
            log::debug!("sync_dir {}: {e} (ignored on Windows)", dir.display());
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn FsLock + '_>> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "index is locked by another writer"));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
        // Diagnostics only; the OS lock is the authority.
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Box::new(RealLock(file)))
    }
}

// ---------------------------------------------------------------------------

/// What to break. Counters are 1-based and count calls since the last
/// [`FaultFs::reset`]. `crash_at_op` counts every logged operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FaultPlan {
    pub fail_write: Option<u64>,
    pub fail_sync: Option<u64>,
    pub discard_write: Option<u64>,
    pub crash_at_op: Option<u64>,
}

#[derive(Default)]
struct FaultState {
    plan: FaultPlan,
    ops: u64,
    writes: u64,
    syncs: u64,
    crashed: bool,
    /// Files with a discarded write; their next sync fails.
    poisoned: HashSet<PathBuf>,
    log: Vec<String>,
}

pub struct FaultFs {
    inner: RealFs,
    /// Stripped from paths in the log.
    base: PathBuf,
    state: Mutex<FaultState>,
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("injected fault: {what}"))
}

impl FaultFs {
    pub fn new(base: &Path) -> Self {
        Self { inner: RealFs, base: base.to_path_buf(), state: Mutex::new(FaultState::default()) }
    }

    pub fn set_plan(&self, plan: FaultPlan) {
        self.state.lock().unwrap().plan = plan;
    }

    /// Clear counters, crash state, poison and log; keep the plan.
    pub fn reset(&self) {
        let mut s = self.state.lock().unwrap();
        let plan = s.plan;
        *s = FaultState { plan, ..FaultState::default() };
    }

    /// Every operation so far, e.g. `create seg-00000.idx.tmp`, `write seg-00000.idx.tmp`,
    /// `sync seg-00000.idx.tmp`, `close …`, `rename a -> b`, `sync_dir .`, `remove x`,
    /// `lock LOCK`, `unlock LOCK`.
    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn op_count(&self) -> u64 {
        self.state.lock().unwrap().ops
    }

    pub fn write_count(&self) -> u64 {
        self.state.lock().unwrap().writes
    }

    pub fn sync_count(&self) -> u64 {
        self.state.lock().unwrap().syncs
    }

    fn rel(&self, p: &Path) -> String {
        let r = p.strip_prefix(&self.base).unwrap_or(p);
        let s = r.to_string_lossy().replace('\\', "/");
        if s.is_empty() { ".".into() } else { s }
    }

    /// Record an operation; apply crash injection.
    fn op(&self, desc: String) -> io::Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.crashed {
            return Err(injected("process is dead"));
        }
        s.ops += 1;
        if s.plan.crash_at_op == Some(s.ops) {
            s.crashed = true;
            s.log.push(format!("CRASH before: {desc}"));
            return Err(injected("crash"));
        }
        s.log.push(desc);
        Ok(())
    }
}

struct FaultFile<'a> {
    fs: &'a FaultFs,
    path: PathBuf,
    rel: String,
    inner: Option<File>,
}

impl Write for FaultFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.fs.op(format!("write {}", self.rel))?;
        let action = {
            let mut s = self.fs.state.lock().unwrap();
            s.writes += 1;
            if s.plan.fail_write == Some(s.writes) {
                Err(injected("write failed"))
            } else if s.plan.discard_write == Some(s.writes) {
                s.poisoned.insert(self.path.clone());
                Ok(false)
            } else {
                Ok(true)
            }
        };
        // A `false` action means this write is deliberately discarded:
        // the bytes are silently lost, which is the fault being injected.
        if action? {
            self.inner.as_mut().unwrap().write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FsFile for FaultFile<'_> {
    fn sync_all(&mut self) -> io::Result<()> {
        self.fs.op(format!("sync {}", self.rel))?;
        {
            let mut s = self.fs.state.lock().unwrap();
            s.syncs += 1;
            if s.plan.fail_sync == Some(s.syncs) {
                return Err(injected("sync failed"));
            }
            if s.poisoned.contains(&self.path) {
                return Err(injected("writeback failed (EIO)"));
            }
        }
        self.inner.as_mut().unwrap().sync_all()
    }
}

impl Drop for FaultFile<'_> {
    fn drop(&mut self) {
        // A crash here means "between the last write/sync and the next op".
        let _ = self.fs.op(format!("close {}", self.rel));
        self.inner.take();
    }
}

struct FaultLock<'a> {
    fs: &'a FaultFs,
    rel: String,
    _inner: Box<dyn FsLock + 'a>,
}

impl FsLock for FaultLock<'_> {}

impl Drop for FaultLock<'_> {
    fn drop(&mut self) {
        let _ = self.fs.op(format!("unlock {}", self.rel));
    }
}

impl Fs for FaultFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.op(format!("create_dir_all {}", self.rel(dir)))?;
        self.inner.create_dir_all(dir)
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn FsFile + '_>> {
        let rel = self.rel(path);
        self.op(format!("create {rel}"))?;
        let file = File::create(path)?;
        Ok(Box::new(FaultFile { fs: self, path: path.to_path_buf(), rel, inner: Some(file) }))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.op(format!("read {}", self.rel(path)))?;
        self.inner.read(path)
    }

    fn mmap(&self, path: &Path) -> io::Result<Mmap> {
        self.op(format!("mmap {}", self.rel(path)))?;
        self.inner.mmap(path)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.op(format!("stat {}", self.rel(path)))?;
        self.inner.file_len(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.op(format!("rename {} -> {}", self.rel(from), self.rel(to)))?;
        self.inner.rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.op(format!("remove {}", self.rel(path)))?;
        self.inner.remove_file(path)
    }

    fn list_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.op(format!("list_dir {}", self.rel(dir)))?;
        self.inner.list_dir(dir)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.op(format!("sync_dir {}", self.rel(dir)))?;
        {
            let mut s = self.state.lock().unwrap();
            s.syncs += 1;
            if s.plan.fail_sync == Some(s.syncs) {
                return Err(injected("dir sync failed"));
            }
        }
        self.inner.sync_dir(dir)
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn FsLock + '_>> {
        let rel = self.rel(path);
        self.op(format!("lock {rel}"))?;
        let inner = self.inner.lock(path)?;
        Ok(Box::new(FaultLock { fs: self, rel, _inner: inner }))
    }
}

// ---------------------------------------------------------------------------

/// Sleeps before every operation. For the crash harness.
pub struct DelayFs {
    inner: RealFs,
    delay: Duration,
}

impl DelayFs {
    pub fn new(delay: Duration) -> Self {
        Self { inner: RealFs, delay }
    }

    fn pause(&self) {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
    }
}

struct DelayFile<'a> {
    fs: &'a DelayFs,
    inner: Box<dyn FsFile + 'a>,
}

impl Write for DelayFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.fs.pause();
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl FsFile for DelayFile<'_> {
    fn sync_all(&mut self) -> io::Result<()> {
        self.fs.pause();
        self.inner.sync_all()
    }
}

impl Fs for DelayFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.pause();
        self.inner.create_dir_all(dir)
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn FsFile + '_>> {
        self.pause();
        let inner = self.inner.create(path)?;
        Ok(Box::new(DelayFile { fs: self, inner }))
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.pause();
        self.inner.read(path)
    }
    fn mmap(&self, path: &Path) -> io::Result<Mmap> {
        self.pause();
        self.inner.mmap(path)
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.pause();
        self.inner.file_len(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.pause();
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.pause();
        self.inner.remove_file(path)
    }
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.pause();
        self.inner.list_dir(dir)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.pause();
        self.inner.sync_dir(dir)
    }
    fn lock(&self, path: &Path) -> io::Result<Box<dyn FsLock + '_>> {
        self.pause();
        self.inner.lock(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_fs_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("LOCK");
        let fs = RealFs;
        let held = fs.lock(&lock_path).unwrap();
        let again = fs.lock(&lock_path);
        assert_eq!(again.err().map(|e| e.kind()), Some(io::ErrorKind::WouldBlock));
        drop(held);
        fs.lock(&lock_path).unwrap();
    }

    #[test]
    fn real_fs_roundtrip_and_sync_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let p = dir.path().join("a.bin");
        {
            let mut f = fs.create(&p).unwrap();
            f.write_all(b"hello").unwrap();
            f.sync_all().unwrap();
        }
        assert_eq!(fs.read(&p).unwrap(), b"hello");
        assert_eq!(fs.file_len(&p).unwrap(), 5);
        assert_eq!(&fs.mmap(&p).unwrap()[..], b"hello");
        fs.sync_dir(dir.path()).unwrap();
        let q = dir.path().join("b.bin");
        fs.rename(&p, &q).unwrap();
        assert_eq!(fs.list_dir(dir.path()).unwrap(), vec![q.clone()]);
        fs.remove_file(&q).unwrap();
        assert!(fs.list_dir(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn fault_fs_logs_and_injects() {
        let dir = tempfile::tempdir().unwrap();
        let fs = FaultFs::new(dir.path());
        let p = dir.path().join("f");

        // Clean run.
        {
            let mut f = fs.create(&p).unwrap();
            f.write_all(b"abc").unwrap();
            f.sync_all().unwrap();
        }
        assert_eq!(fs.log(), vec!["create f", "write f", "sync f", "close f"]);
        assert_eq!(fs.read(&p).unwrap(), b"abc");

        // Failed write.
        fs.reset();
        fs.set_plan(FaultPlan { fail_write: Some(1), ..Default::default() });
        let mut f = fs.create(&p).unwrap();
        assert!(f.write_all(b"x").is_err());
        drop(f);

        // Discarded write: write succeeds, sync fails, file is empty.
        fs.reset();
        fs.set_plan(FaultPlan { discard_write: Some(1), ..Default::default() });
        let mut f = fs.create(&p).unwrap();
        f.write_all(b"lost").unwrap();
        assert!(f.sync_all().is_err());
        drop(f);
        assert_eq!(fs.read(&p).unwrap(), b"");

        // Failed sync (counting dir syncs too).
        fs.reset();
        fs.set_plan(FaultPlan { fail_sync: Some(2), ..Default::default() });
        let mut f = fs.create(&p).unwrap();
        f.write_all(b"ok").unwrap();
        f.sync_all().unwrap();
        drop(f);
        assert!(fs.sync_dir(dir.path()).is_err());

        // Crash: the op fails and everything after it fails.
        fs.reset();
        fs.set_plan(FaultPlan { crash_at_op: Some(2), ..Default::default() });
        let mut f = fs.create(&p).unwrap(); // op 1
        assert!(f.write_all(b"x").is_err()); // op 2: crash
        assert!(f.sync_all().is_err());
        assert!(fs.rename(&p, &dir.path().join("g")).is_err());
        assert!(fs.log().iter().any(|l| l.starts_with("CRASH before: write f")));
    }
}
