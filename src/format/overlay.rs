//! `state-GGGGG.ovl`: patches to the mutable doc fields (path, inode, mtime,
//! size) for docs whose *content* is unchanged since their segment was
//! written — renames and metadata-only touches (rsync, `git checkout`,
//! build systems that rewrite identical bytes). A patch never changes
//! `content_hash` or `len`: if those would change, the correct action is a
//! tombstone + a freshly indexed doc, not an overlay entry.
//!
//! Small and always fully crc-verified on open (FORMAT.md §2). Rewritten
//! whole on every commit that touches it; a merge folds its patches into the
//! merged segment's doc table and the overlay shrinks back down.

use super::envelope::{self, Envelope, Kind};
use super::{read_i64_le, read_u32_le, read_u64_le, slice, to_usize, FormatError, Result};

pub const BODY_HEADER_LEN: usize = 24;
pub const PATCH_LEN: usize = 36;

/// One patch as handed to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchIn<'a> {
    pub doc_id: u32,
    pub inode: u64,
    pub mtime_nanos: i64,
    pub size: u64,
    pub path: &'a str,
}

/// Encode a complete `.ovl` file. `patches` must be sorted and unique by `doc_id`.
pub fn encode(patches: &[PatchIn<'_>]) -> Result<Vec<u8>> {
    debug_assert!(patches.windows(2).all(|w| w[0].doc_id < w[1].doc_id), "overlay patches must be sorted and unique");
    let mut body = vec![0u8; BODY_HEADER_LEN + patches.len() * PATCH_LEN];
    let mut paths: Vec<u8> = Vec::new();
    for (i, p) in patches.iter().enumerate() {
        if p.path.len() > u32::MAX as usize || paths.len() + p.path.len() > u32::MAX as usize {
            return Err(FormatError::Invariant("overlay path heap exceeds 4 GiB"));
        }
        let at = BODY_HEADER_LEN + i * PATCH_LEN;
        body[at..at + 4].copy_from_slice(&p.doc_id.to_le_bytes());
        body[at + 4..at + 12].copy_from_slice(&p.inode.to_le_bytes());
        body[at + 12..at + 20].copy_from_slice(&p.mtime_nanos.to_le_bytes());
        body[at + 20..at + 28].copy_from_slice(&p.size.to_le_bytes());
        body[at + 28..at + 32].copy_from_slice(&(paths.len() as u32).to_le_bytes());
        body[at + 32..at + 36].copy_from_slice(&(p.path.len() as u32).to_le_bytes());
        paths.extend_from_slice(p.path.as_bytes());
    }
    let paths_off = (envelope::HEADER_LEN + body.len()) as u64;
    body[0..4].copy_from_slice(&(patches.len() as u32).to_le_bytes());
    body[8..16].copy_from_slice(&paths_off.to_le_bytes());
    body[16..24].copy_from_slice(&(paths.len() as u64).to_le_bytes());
    body.extend_from_slice(&paths);
    Ok(envelope::seal(Kind::Overlay, &body))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch {
    pub doc_id: u32,
    pub inode: u64,
    pub mtime_nanos: i64,
    pub size: u64,
    path_off: u32,
    path_len: u32,
}

/// Zero-copy view over a verified `.ovl` file.
#[derive(Debug, Clone)]
pub struct OverlayView<'a> {
    bytes: &'a [u8],
    env: Envelope,
    num_patches: u32,
    paths: &'a [u8],
}

impl<'a> OverlayView<'a> {
    /// Overlay files are small and always crc-verified on open.
    pub fn parse_verified(bytes: &'a [u8]) -> Result<Self> {
        let env = envelope::parse_verified(Kind::Overlay, bytes)?;
        let body = &bytes[env.body.clone()];
        if body.len() < BODY_HEADER_LEN {
            return Err(FormatError::Truncated { at: body.len(), needed: BODY_HEADER_LEN });
        }
        let num_patches = read_u32_le(body, 0)?;
        let paths_off = to_usize(read_u64_le(body, 8)?)?;
        let paths_len = to_usize(read_u64_le(body, 16)?)?;
        let records_end = envelope::HEADER_LEN + BODY_HEADER_LEN + num_patches as usize * PATCH_LEN;
        if paths_off != records_end || paths_off + paths_len != env.body.end {
            return Err(FormatError::corrupt("overlay regions do not tile the body"));
        }
        let paths = slice(bytes, paths_off, paths_len)?;
        let v = Self { bytes, env, num_patches, paths };
        let mut prev: Option<u32> = None;
        for i in 0..num_patches {
            let p = v.patch(i).ok_or_else(|| FormatError::corrupt("overlay patch out of range"))?;
            if let Some(pr) = prev {
                if pr >= p.doc_id {
                    return Err(FormatError::corrupt("overlay patches not sorted/unique"));
                }
            }
            v.path_of(&p).ok_or_else(|| FormatError::corrupt("overlay patch has a bad path"))?;
            prev = Some(p.doc_id);
        }
        Ok(v)
    }

    fn record_at(&self, i: u32) -> usize {
        envelope::HEADER_LEN + BODY_HEADER_LEN + i as usize * PATCH_LEN
    }

    pub fn num_patches(&self) -> u32 {
        self.num_patches
    }

    pub fn crc(&self) -> u32 {
        self.env.crc
    }

    pub fn patch(&self, i: u32) -> Option<Patch> {
        if i >= self.num_patches {
            return None;
        }
        let b = self.bytes;
        let at = self.record_at(i);
        Some(Patch {
            doc_id: read_u32_le(b, at).ok()?,
            inode: read_u64_le(b, at + 4).ok()?,
            mtime_nanos: read_i64_le(b, at + 12).ok()?,
            size: read_u64_le(b, at + 20).ok()?,
            path_off: read_u32_le(b, at + 28).ok()?,
            path_len: read_u32_le(b, at + 32).ok()?,
        })
    }

    pub fn path_of(&self, p: &Patch) -> Option<&'a str> {
        let s = self.paths.get(p.path_off as usize..(p.path_off as usize).checked_add(p.path_len as usize)?)?;
        std::str::from_utf8(s).ok()
    }

    /// Binary search by doc_id (patches are sorted and unique).
    pub fn find(&self, doc_id: u32) -> Option<(Patch, &'a str)> {
        let (mut lo, mut hi) = (0u32, self.num_patches);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let p = self.patch(mid)?;
            match p.doc_id.cmp(&doc_id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some((p, self.path_of(&p)?)),
            }
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = (Patch, &'a str)> + '_ {
        (0..self.num_patches).filter_map(move |i| {
            let p = self.patch(i)?;
            let path = self.path_of(&p)?;
            Some((p, path))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<PatchIn<'static>> {
        vec![
            PatchIn { doc_id: 1, inode: 10, mtime_nanos: 100, size: 5, path: "a/b.txt" },
            PatchIn { doc_id: 5, inode: 50, mtime_nanos: -1, size: 0, path: "" },
            PatchIn { doc_id: 9, inode: u64::MAX, mtime_nanos: i64::MIN, size: u64::MAX, path: "日本/renamed.rs" },
        ]
    }

    #[test]
    fn roundtrip_and_find() {
        let patches = sample();
        let bytes = encode(&patches).unwrap();
        let v = OverlayView::parse_verified(&bytes).unwrap();
        assert_eq!(v.num_patches(), 3);
        for p in &patches {
            let (found, path) = v.find(p.doc_id).unwrap();
            assert_eq!((found.doc_id, found.inode, found.mtime_nanos, found.size), (p.doc_id, p.inode, p.mtime_nanos, p.size));
            assert_eq!(path, p.path);
        }
        assert!(v.find(0).is_none());
        assert!(v.find(6).is_none());
        assert!(v.find(100).is_none());
        let via_iter: Vec<u32> = v.iter().map(|(p, _)| p.doc_id).collect();
        assert_eq!(via_iter, vec![1, 5, 9]);
    }

    #[test]
    fn empty_overlay() {
        let bytes = encode(&[]).unwrap();
        let v = OverlayView::parse_verified(&bytes).unwrap();
        assert_eq!(v.num_patches(), 0);
        assert!(v.find(0).is_none());
    }

    #[test]
    fn corruption() {
        let bytes = encode(&sample()).unwrap();
        let mut flipped = bytes.clone();
        flipped[16 + 24] ^= 1; // first patch's doc_id
        assert!(matches!(OverlayView::parse_verified(&flipped), Err(FormatError::Checksum { .. })));

        // Swap the first two fixed-size records in place (path heap untouched,
        // so offsets stay valid) to produce out-of-order doc_ids with a
        // correct crc, and confirm that is caught structurally.
        let mut body = bytes[16..bytes.len() - 16].to_vec();
        let (a, b) = (BODY_HEADER_LEN, BODY_HEADER_LEN + PATCH_LEN);
        let (rec_a, rec_b) = (body[a..b].to_vec(), body[b..b + PATCH_LEN].to_vec());
        body[a..b].copy_from_slice(&rec_b);
        body[b..b + PATCH_LEN].copy_from_slice(&rec_a);
        let resealed = envelope::seal(Kind::Overlay, &body);
        assert!(matches!(OverlayView::parse_verified(&resealed), Err(FormatError::Corrupt(_))));
    }

    #[test]
    #[should_panic(expected = "sorted and unique")]
    fn encode_asserts_order_in_debug() {
        let _ = encode(&[sample()[1], sample()[0]]);
    }
}
