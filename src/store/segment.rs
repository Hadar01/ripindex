//! One on-disk segment: mapped `.idx` and `.doc`, plus the deletion bitmap
//! (read fully — it is small and always crc-verified).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use memmap2::Mmap;

use super::{del_name, doc_name, idx_name};
use crate::error::{Error, Result};
use crate::format::del::{Bitmap, DelView};
use crate::format::doc::{self, DocTableView, STATUS_INDEXED};
use crate::format::envelope::{self, Envelope};
use crate::format::idx::{DiskPostingList, IdxView, Toc};
use crate::format::manifest::SegmentEntry;
use crate::fs::Fs;
use crate::index::{DocId, DocMeta, DocStatus, SegmentReader};

pub struct Segment {
    entry: SegmentEntry,
    idx_map: Mmap,
    idx_env: Envelope,
    toc: Toc,
    doc_map: Mmap,
    doc_env: Envelope,
    num_docs: u32,
    num_indexed: u32,
    total_len: u64,
    paths: std::ops::Range<usize>,
    del: Option<Bitmap>,
}

fn corrupt(path: &Path, reason: impl std::fmt::Display) -> Error {
    Error::Corrupt { path: path.to_path_buf(), reason: reason.to_string() }
}

/// A file-open failure: `NotFound` might just be a race with a concurrent
/// commit retiring the file (see [`Error::Vanished`]); anything else is
/// treated as real corruption straight away.
fn open_err(path: &Path, e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::Vanished(path.to_path_buf())
    } else {
        corrupt(path, e)
    }
}

impl Segment {
    /// Map and validate one committed segment (FORMAT.md §8 step 3; step 4 if
    /// `verify`). Any failure is `Error::Corrupt` — the manifest committed
    /// this segment, so it must be whole.
    pub fn open(fs: &dyn Fs, dir: &Path, entry: &SegmentEntry, verify: bool) -> Result<Segment> {
        let idx_path = dir.join(idx_name(entry.segment_id));
        let doc_path = dir.join(doc_name(entry.segment_id));

        let idx_map = fs.mmap(&idx_path).map_err(|e| open_err(&idx_path, e))?;
        if idx_map.len() as u64 != entry.idx_len {
            return Err(corrupt(&idx_path, format!("length {} but manifest says {}", idx_map.len(), entry.idx_len)));
        }
        let (idx_env, toc) = {
            let view = IdxView::parse(&idx_map).map_err(|e| corrupt(&idx_path, e))?;
            if view.crc() != entry.idx_crc {
                return Err(corrupt(&idx_path, "footer checksum differs from manifest"));
            }
            if view.num_terms() != entry.num_terms {
                return Err(corrupt(&idx_path, "term count differs from manifest"));
            }
            if verify {
                view.verify().map_err(|e| corrupt(&idx_path, e))?;
            }
            (view.envelope().clone(), *view.toc())
        };

        let doc_map = fs.mmap(&doc_path).map_err(|e| open_err(&doc_path, e))?;
        if doc_map.len() as u64 != entry.doc_len {
            return Err(corrupt(&doc_path, format!("length {} but manifest says {}", doc_map.len(), entry.doc_len)));
        }
        let (doc_env, num_docs, num_indexed, total_len, paths) = {
            let view = DocTableView::parse(&doc_map).map_err(|e| corrupt(&doc_path, e))?;
            if view.crc() != entry.doc_crc {
                return Err(corrupt(&doc_path, "footer checksum differs from manifest"));
            }
            if view.num_docs() != entry.num_docs {
                return Err(corrupt(&doc_path, "doc count differs from manifest"));
            }
            if verify {
                view.verify().map_err(|e| corrupt(&doc_path, e))?;
            }
            (view.envelope().clone(), view.num_docs(), view.num_indexed(), view.total_len(), view.paths_range())
        };

        let del = if entry.del_gen > 0 {
            let del_path = dir.join(del_name(entry.segment_id, entry.del_gen));
            let bytes = fs.read(&del_path).map_err(|e| open_err(&del_path, e))?;
            if bytes.len() as u64 != entry.del_len {
                return Err(corrupt(&del_path, format!("length {} but manifest says {}", bytes.len(), entry.del_len)));
            }
            let view = DelView::parse_verified(&bytes).map_err(|e| corrupt(&del_path, e))?;
            if view.crc() != entry.del_crc {
                return Err(corrupt(&del_path, "footer checksum differs from manifest"));
            }
            if view.num_docs() != entry.num_docs {
                return Err(corrupt(&del_path, "bitmap size differs from segment"));
            }
            if view.num_deleted() != entry.num_deleted {
                return Err(corrupt(&del_path, "deleted count differs from manifest"));
            }
            Some(view.to_bitmap())
        } else {
            None
        };

        Ok(Segment {
            entry: entry.clone(),
            idx_map,
            idx_env,
            toc,
            doc_map,
            doc_env,
            num_docs,
            num_indexed,
            total_len,
            paths,
            del,
        })
    }

    pub fn entry(&self) -> &SegmentEntry {
        &self.entry
    }

    pub fn idx(&self) -> IdxView<'_> {
        IdxView::from_parts(&self.idx_map, self.idx_env.clone(), self.toc)
    }

    pub fn docs(&self) -> DocTableView<'_> {
        DocTableView::from_parts(&self.doc_map, self.doc_env.clone(), self.num_docs, self.num_indexed, self.total_len, self.paths.clone())
    }

    pub fn deletions(&self) -> Option<&Bitmap> {
        self.del.as_ref()
    }

    pub fn is_deleted(&self, local: DocId) -> bool {
        self.del.as_ref().is_some_and(|b| b.get(local))
    }

    /// Indexed docs in this segment (before deletions).
    pub fn num_indexed(&self) -> u32 {
        self.num_indexed
    }

    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Full checksum + structural verification of both mapped files.
    pub fn verify(&self, dir: &Path) -> Result<()> {
        self.idx().verify().map_err(|e| corrupt(&dir.join(idx_name(self.entry.segment_id)), e))?;
        self.docs().verify().map_err(|e| corrupt(&dir.join(doc_name(self.entry.segment_id)), e))?;
        Ok(())
    }

    /// Metadata of a local doc with its path resolved under `root`.
    /// `None` for out-of-range or deleted docs.
    pub fn doc_meta(&self, local: DocId, root: &Path) -> Option<DocMeta> {
        if self.is_deleted(local) {
            return None;
        }
        let view = self.docs();
        let r = view.record(local)?;
        let rel = view.path(local)?;
        let mut path = root.to_path_buf();
        path.extend(rel.split('/').filter(|c| !c.is_empty()));
        Some(DocMeta {
            path,
            inode: r.inode,
            mtime: nanos_to_time(r.mtime_nanos),
            size: r.size,
            len: r.len,
            status: r.kind().map(DocStatus::from).unwrap_or(DocStatus::Skipped),
            content_hash: r.content_hash,
        })
    }

    /// Approximate heap held by this struct (the data itself is mapped).
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.del.as_ref().map_or(0, |b| b.num_docs().div_ceil(8) as usize)
    }

    pub fn files(&self, dir: &Path) -> Vec<PathBuf> {
        super::segment_files(&self.entry).into_iter().map(|n| dir.join(n)).collect()
    }
}

impl SegmentReader for Segment {
    type List<'a> = DiskPostingList<'a>;

    fn base(&self) -> DocId {
        self.entry.base_doc
    }

    fn num_docs(&self) -> u32 {
        self.num_docs
    }

    fn postings(&self, term: &str) -> Option<DiskPostingList<'_>> {
        self.idx().lookup(term)
    }

    /// The scoring hot path: one bounds check, one status byte, one u32.
    /// The records region was validated to tile the body at open, so the
    /// offsets are in range for every `local < num_docs`.
    #[inline]
    fn doc_len(&self, local: DocId) -> Option<u32> {
        if local >= self.num_docs || self.is_deleted(local) {
            return None;
        }
        let at = envelope::HEADER_LEN + doc::BODY_HEADER_LEN + local as usize * doc::RECORD_LEN;
        let b = &self.doc_map[at..at + doc::RECORD_LEN];
        if b[36] != STATUS_INDEXED {
            return None;
        }
        Some(u32::from_le_bytes(b[24..28].try_into().unwrap()))
    }
}

pub fn time_to_nanos(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

pub fn nanos_to_time(n: i64) -> SystemTime {
    let d = Duration::from_nanos(n.unsigned_abs());
    let t = if n >= 0 { SystemTime::UNIX_EPOCH.checked_add(d) } else { SystemTime::UNIX_EPOCH.checked_sub(d) };
    t.unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_roundtrip() {
        for t in [
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789),
            SystemTime::UNIX_EPOCH - Duration::from_secs(86_400),
            SystemTime::now(),
        ] {
            assert_eq!(nanos_to_time(time_to_nanos(t)), t);
        }
        assert_eq!(time_to_nanos(nanos_to_time(i64::MIN)), time_to_nanos(nanos_to_time(i64::MIN))); // no panic
    }
}
