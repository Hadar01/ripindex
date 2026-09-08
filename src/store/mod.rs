//! The persistent index: `.ripindex/` under the indexed root, committed
//! through an atomic manifest protocol. Layout and encodings are specified in
//! `docs/FORMAT.md`; [`writer`] implements §7 (commit), [`reader`] §8 (open).

pub mod governor;
pub mod live;
pub mod merge;
pub mod reader;
pub mod segment;
pub mod state;
pub mod update;
pub mod watcher;
pub mod writer;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use governor::Governor;
pub use live::{IndexSnapshot, LiveIndex};
pub use merge::{commit_merges, merge_index, plan_merges, prepare_merge, MergePolicy, PreparedMerge};
pub use reader::{open, Docs, Index, OpenOptions};
pub use segment::Segment;
pub use state::{reconcile, Change, FileState, Plan, StateIndex};
pub use update::update_index;
pub use watcher::{run as run_watcher, EventSource, NotifySource, RealClock, WatchConfig};
pub use writer::{build_from_dir, build_index, delete_docs, BuildProgress, NoProgress};

use crate::format::manifest::{Manifest, SegmentEntry};
use crate::fs::Fs;

pub const DIR_NAME: &str = ".ripindex";
pub const MANIFEST: &str = "MANIFEST";
pub const MANIFEST_TMP: &str = "MANIFEST.tmp";
pub const LOCK: &str = "LOCK";

pub fn index_dir(root: &Path) -> PathBuf {
    root.join(DIR_NAME)
}

pub fn idx_name(segment_id: u32) -> String {
    format!("seg-{segment_id:05}.idx")
}

pub fn doc_name(segment_id: u32) -> String {
    format!("seg-{segment_id:05}.doc")
}

pub fn del_name(segment_id: u32, del_gen: u32) -> String {
    format!("seg-{segment_id:05}.{del_gen:05}.del")
}

/// The file-state overlay (FORMAT.md, doc table §5 note): patches to mutable
/// doc fields for docs whose content is unchanged. One live generation.
pub fn overlay_name(state_gen: u32) -> String {
    format!("state-{state_gen:05}.ovl")
}

/// Scratch space for an in-progress unlocked merge prepare (M4). Lives
/// *inside* `.ripindex/` (same volume, so the eventual rename into place is a
/// same-filesystem rename) but the orphan sweep in `reader::open` only lists
/// `.ripindex/`'s direct entries, never descending into subdirectories, so
/// nothing here is ever mistaken for an orphaned top-level `.tmp` file no
/// matter how long a merge's prepare phase takes.
pub fn staging_dir(root: &Path) -> PathBuf {
    index_dir(root).join("staging")
}

/// Remove every file left in the staging directory. Safe to call whenever
/// nothing can still be relying on it — a `PreparedMerge` is a plain Rust
/// value with no on-disk record of its own, so nothing survives a process
/// restart to need it; the daemon calls this once per root at startup, and
/// the synchronous `merge_index` convenience calls it before it starts
/// (it is the only writer using staging for the lifetime of that call).
pub fn clear_staging(fs: &dyn Fs, root: &Path) -> std::io::Result<()> {
    let dir = staging_dir(root);
    match fs.list_dir(&dir) {
        Ok(entries) => {
            for p in entries {
                let _ = fs.remove_file(&p);
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn tmp_name(final_name: &str) -> String {
    format!("{final_name}.tmp")
}

/// Every file a segment entry references.
pub fn segment_files(e: &SegmentEntry) -> Vec<String> {
    let mut v = vec![idx_name(e.segment_id), doc_name(e.segment_id)];
    if e.del_gen > 0 {
        v.push(del_name(e.segment_id, e.del_gen));
    }
    v
}

/// Every file a manifest references (not the manifest itself): every
/// segment's files, plus the overlay if one is live.
pub fn referenced_files(m: &Manifest) -> HashSet<String> {
    let mut set: HashSet<String> = m.segments.iter().flat_map(segment_files).collect();
    if m.state_gen > 0 {
        set.insert(overlay_name(m.state_gen));
    }
    set
}

/// Files that `open` may delete when the manifest does not name them:
/// segment files of any generation, overlay generations, and anything transient.
pub fn is_cleanable(name: &str) -> bool {
    name.starts_with("seg-") || name.starts_with("state-") || name.ends_with(".tmp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(idx_name(3), "seg-00003.idx");
        assert_eq!(doc_name(123456), "seg-123456.doc");
        assert_eq!(del_name(3, 2), "seg-00003.00002.del");
        assert_eq!(overlay_name(7), "state-00007.ovl");
        assert_eq!(tmp_name(&idx_name(3)), "seg-00003.idx.tmp");
        assert!(is_cleanable("seg-00003.idx") && is_cleanable("MANIFEST.tmp") && is_cleanable("seg-00001.00004.del"));
        assert!(is_cleanable("state-00001.ovl") && is_cleanable("state-00001.ovl.tmp"));
        assert!(!is_cleanable("MANIFEST") && !is_cleanable("LOCK"));
    }

    #[test]
    fn referenced_files_includes_overlay_only_when_live() {
        use crate::format::manifest::SegmentEntry;
        let mut m = Manifest {
            segments: vec![SegmentEntry { segment_id: 0, num_docs: 1, ..Default::default() }],
            ..Default::default()
        };
        let no_overlay = referenced_files(&m);
        assert!(!no_overlay.iter().any(|n| n.starts_with("state-")));
        m.state_gen = 3;
        let with_overlay = referenced_files(&m);
        assert!(with_overlay.contains(&overlay_name(3)));
        assert_eq!(with_overlay.len(), no_overlay.len() + 1);
    }
}
