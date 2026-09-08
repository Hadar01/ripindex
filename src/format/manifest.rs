//! `MANIFEST` (FORMAT.md §3).

use super::envelope::{self, Kind};
use super::{read_i64_le, read_u32_le, read_u64_le, FormatError, Result};

pub const BODY_HEADER_LEN: usize = 48;
pub const ENTRY_LEN: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentEntry {
    pub segment_id: u32,
    /// 0 = no deletions file.
    pub del_gen: u32,
    pub base_doc: u32,
    pub num_docs: u32,
    pub num_deleted: u32,
    pub idx_crc: u32,
    pub doc_crc: u32,
    pub del_crc: u32,
    pub idx_len: u64,
    pub doc_len: u64,
    pub del_len: u64,
    pub num_tokens: u64,
    pub num_postings: u64,
    pub num_terms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub generation: u64,
    pub next_segment_id: u32,
    pub committed_unix_nanos: i64,
    /// 0 = no overlay file (`state-GGGGG.ovl`).
    pub state_gen: u32,
    pub state_crc: u32,
    pub state_len: u64,
    /// Ascending `base_doc`, non-overlapping.
    pub segments: Vec<SegmentEntry>,
}

impl SegmentEntry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.segment_id.to_le_bytes());
        out.extend_from_slice(&self.del_gen.to_le_bytes());
        out.extend_from_slice(&self.base_doc.to_le_bytes());
        out.extend_from_slice(&self.num_docs.to_le_bytes());
        out.extend_from_slice(&self.num_deleted.to_le_bytes());
        out.extend_from_slice(&self.idx_crc.to_le_bytes());
        out.extend_from_slice(&self.doc_crc.to_le_bytes());
        out.extend_from_slice(&self.del_crc.to_le_bytes());
        out.extend_from_slice(&self.idx_len.to_le_bytes());
        out.extend_from_slice(&self.doc_len.to_le_bytes());
        out.extend_from_slice(&self.del_len.to_le_bytes());
        out.extend_from_slice(&self.num_tokens.to_le_bytes());
        out.extend_from_slice(&self.num_postings.to_le_bytes());
        out.extend_from_slice(&self.num_terms.to_le_bytes());
    }

    fn decode(b: &[u8], at: usize) -> Result<Self> {
        Ok(Self {
            segment_id: read_u32_le(b, at)?,
            del_gen: read_u32_le(b, at + 4)?,
            base_doc: read_u32_le(b, at + 8)?,
            num_docs: read_u32_le(b, at + 12)?,
            num_deleted: read_u32_le(b, at + 16)?,
            idx_crc: read_u32_le(b, at + 20)?,
            doc_crc: read_u32_le(b, at + 24)?,
            del_crc: read_u32_le(b, at + 28)?,
            idx_len: read_u64_le(b, at + 32)?,
            doc_len: read_u64_le(b, at + 40)?,
            del_len: read_u64_le(b, at + 48)?,
            num_tokens: read_u64_le(b, at + 56)?,
            num_postings: read_u64_le(b, at + 64)?,
            num_terms: read_u64_le(b, at + 72)?,
        })
    }

    /// One past the last global id of this segment.
    pub fn end_doc(&self) -> u64 {
        self.base_doc as u64 + self.num_docs as u64
    }

    /// Live (non-deleted) docs.
    pub fn live_docs(&self) -> u32 {
        self.num_docs - self.num_deleted
    }

    /// Fraction of docs tombstoned, for merge policy. 0.0 for an empty segment.
    pub fn deleted_fraction(&self) -> f32 {
        if self.num_docs == 0 {
            0.0
        } else {
            self.num_deleted as f32 / self.num_docs as f32
        }
    }

    /// Total bytes on disk for this segment (all files).
    pub fn total_bytes(&self) -> u64 {
        self.idx_len + self.doc_len + self.del_len
    }
}

impl Manifest {
    /// The complete file: envelope + body.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(BODY_HEADER_LEN + ENTRY_LEN * self.segments.len());
        body.extend_from_slice(&self.generation.to_le_bytes());
        body.extend_from_slice(&self.next_segment_id.to_le_bytes());
        body.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        body.extend_from_slice(&self.committed_unix_nanos.to_le_bytes());
        body.extend_from_slice(&self.state_gen.to_le_bytes());
        body.extend_from_slice(&self.state_crc.to_le_bytes());
        body.extend_from_slice(&self.state_len.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        for s in &self.segments {
            s.encode(&mut body);
        }
        envelope::seal(Kind::Manifest, &body)
    }

    /// Envelope + crc verified, then structural checks (ordering, id ranges).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let env = envelope::parse_verified(Kind::Manifest, bytes)?;
        let b = &bytes[env.body];
        if b.len() < BODY_HEADER_LEN {
            return Err(FormatError::Truncated { at: b.len(), needed: BODY_HEADER_LEN });
        }
        let generation = read_u64_le(b, 0)?;
        let next_segment_id = read_u32_le(b, 8)?;
        let num_segments = read_u32_le(b, 12)? as usize;
        let committed_unix_nanos = read_i64_le(b, 16)?;
        let state_gen = read_u32_le(b, 24)?;
        let state_crc = read_u32_le(b, 28)?;
        let state_len = read_u64_le(b, 32)?;
        if read_u64_le(b, 40)? != 0 {
            return Err(FormatError::corrupt("manifest reserved field is non-zero"));
        }
        if (state_gen == 0) != (state_len == 0) {
            return Err(FormatError::corrupt("state_gen and state_len disagree"));
        }
        if b.len() != BODY_HEADER_LEN + ENTRY_LEN * num_segments {
            return Err(FormatError::corrupt(format!(
                "manifest body is {} bytes, expected {} for {num_segments} segments",
                b.len(),
                BODY_HEADER_LEN + ENTRY_LEN * num_segments
            )));
        }
        let mut segments = Vec::with_capacity(num_segments);
        for i in 0..num_segments {
            let e = SegmentEntry::decode(b, BODY_HEADER_LEN + i * ENTRY_LEN)?;
            if e.segment_id >= next_segment_id {
                return Err(FormatError::corrupt("segment id not below next_segment_id"));
            }
            if let Some(prev) = segments.last() {
                let prev: &SegmentEntry = prev;
                if (e.base_doc as u64) < prev.end_doc() {
                    return Err(FormatError::corrupt("segment doc ranges overlap or are unordered"));
                }
                if e.segment_id == prev.segment_id {
                    return Err(FormatError::corrupt("duplicate segment id"));
                }
            }
            if e.end_doc() > u32::MAX as u64 + 1 {
                return Err(FormatError::corrupt("segment exceeds the u32 doc id space"));
            }
            if (e.del_gen == 0) != (e.del_len == 0) {
                return Err(FormatError::corrupt("del_gen and del_len disagree"));
            }
            if e.num_deleted > e.num_docs {
                return Err(FormatError::corrupt("num_deleted exceeds num_docs"));
            }
            segments.push(e);
        }
        Ok(Self { generation, next_segment_id, committed_unix_nanos, state_gen, state_crc, state_len, segments })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            generation: 7,
            next_segment_id: 5,
            committed_unix_nanos: -12345,
            state_gen: 2,
            state_crc: 0xabcd,
            state_len: 96,
            segments: vec![
                SegmentEntry {
                    segment_id: 2,
                    del_gen: 0,
                    base_doc: 0,
                    num_docs: 100,
                    idx_crc: 1,
                    doc_crc: 2,
                    idx_len: 300,
                    doc_len: 400,
                    num_tokens: 5,
                    num_postings: 6,
                    num_terms: 7,
                    ..Default::default()
                },
                SegmentEntry {
                    segment_id: 4,
                    del_gen: 3,
                    base_doc: 100,
                    num_docs: 50,
                    num_deleted: 9,
                    del_crc: 8,
                    del_len: 40,
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let m = sample();
        let bytes = m.encode();
        assert_eq!(bytes.len(), 32 + 48 + 2 * 80);
        assert_eq!(Manifest::decode(&bytes).unwrap(), m);
        let empty = Manifest::default();
        assert_eq!(Manifest::decode(&empty.encode()).unwrap(), empty);
    }

    #[test]
    fn corruption_is_detected() {
        let mut bytes = sample().encode();
        bytes[40] ^= 1; // generation byte
        assert!(matches!(Manifest::decode(&bytes), Err(FormatError::Checksum { .. })));
        assert!(matches!(Manifest::decode(&bytes[..30]), Err(FormatError::Truncated { .. })));
        assert!(matches!(Manifest::decode(b""), Err(FormatError::Truncated { .. })));
    }

    #[test]
    fn structural_checks() {
        let mut m = sample();
        m.segments[1].base_doc = 50; // overlaps segment 0
        assert!(matches!(Manifest::decode(&m.encode()), Err(FormatError::Corrupt(_))));

        let mut m = sample();
        m.segments[1].segment_id = 5; // == next_segment_id
        assert!(matches!(Manifest::decode(&m.encode()), Err(FormatError::Corrupt(_))));

        let mut m = sample();
        m.segments[0].del_len = 10; // del_gen 0 but a length
        assert!(matches!(Manifest::decode(&m.encode()), Err(FormatError::Corrupt(_))));

        let mut m = sample();
        m.state_len = 0; // state_gen != 0 but no length
        assert!(matches!(Manifest::decode(&m.encode()), Err(FormatError::Corrupt(_))));

        let mut m = sample();
        m.segments[1].num_deleted = 51; // exceeds num_docs
        assert!(matches!(Manifest::decode(&m.encode()), Err(FormatError::Corrupt(_))));
    }

    #[test]
    fn no_overlay_is_the_common_case() {
        let mut m = sample();
        m.state_gen = 0;
        m.state_len = 0;
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn segment_entry_helpers() {
        let e = SegmentEntry { num_docs: 100, num_deleted: 30, idx_len: 10, doc_len: 20, del_len: 5, ..Default::default() };
        assert_eq!(e.live_docs(), 70);
        assert_eq!(e.deleted_fraction(), 0.3);
        assert_eq!(e.total_bytes(), 35);
        assert_eq!(SegmentEntry::default().deleted_fraction(), 0.0);
    }
}
