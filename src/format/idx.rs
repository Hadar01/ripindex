//! `seg-NNNNN.idx` (FORMAT.md §4): posting lists, a front-coded term
//! dictionary in 64-term blocks, and a sparse index over block-first terms.

use std::cmp::Ordering;
use std::io::Write;

use super::envelope::{self, BodyWriter, Envelope, Kind};
use super::varint::{put_u32, put_u64, read_u32, read_u64};
use super::{read_u32_le, read_u64_le, slice, to_usize, FormatError, Result};
use crate::index::postings::{Position, PostingCursor, PostingList};
use crate::index::DocId;

pub const BLOCK_SIZE: usize = 64;
pub const TOC_LEN: usize = 64;
/// Longer atoms are not indexed (minified bundles, base64 blobs).
pub const MAX_TERM_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toc {
    pub postings_off: u64,
    pub postings_len: u64,
    pub dict_off: u64,
    pub dict_len: u64,
    pub sparse_off: u64,
    pub sparse_len: u64,
    pub num_terms: u64,
    pub num_blocks: u32,
    pub block_size: u32,
}

impl Toc {
    fn encode(&self) -> [u8; TOC_LEN] {
        let mut b = [0u8; TOC_LEN];
        b[0..8].copy_from_slice(&self.postings_off.to_le_bytes());
        b[8..16].copy_from_slice(&self.postings_len.to_le_bytes());
        b[16..24].copy_from_slice(&self.dict_off.to_le_bytes());
        b[24..32].copy_from_slice(&self.dict_len.to_le_bytes());
        b[32..40].copy_from_slice(&self.sparse_off.to_le_bytes());
        b[40..48].copy_from_slice(&self.sparse_len.to_le_bytes());
        b[48..56].copy_from_slice(&self.num_terms.to_le_bytes());
        b[56..60].copy_from_slice(&self.num_blocks.to_le_bytes());
        b[60..64].copy_from_slice(&self.block_size.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        Ok(Self {
            postings_off: read_u64_le(b, 0)?,
            postings_len: read_u64_le(b, 8)?,
            dict_off: read_u64_le(b, 16)?,
            dict_len: read_u64_le(b, 24)?,
            sparse_off: read_u64_le(b, 32)?,
            sparse_len: read_u64_le(b, 40)?,
            num_terms: read_u64_le(b, 48)?,
            num_blocks: read_u32_le(b, 56)?,
            block_size: read_u32_le(b, 60)?,
        })
    }
}

/// Encode one posting list (§4.2) onto `out`. Returns the number of positions.
///
/// Refuses non-increasing doc ids or positions and empty position lists:
/// the format's deltas are `>= 1`, and a violation here is a bug upstream
/// that must never reach disk.
pub fn encode_postings<'a, I>(postings: I, out: &mut Vec<u8>, scratch: &mut Vec<u8>) -> Result<u64>
where
    I: ExactSizeIterator<Item = (DocId, &'a [Position])>,
{
    put_u32(out, postings.len() as u32);
    let mut prev_doc: Option<DocId> = None;
    let mut tokens = 0u64;
    for (doc, positions) in postings {
        match prev_doc {
            Some(p) if doc <= p => return Err(FormatError::Invariant("doc ids must strictly increase")),
            Some(p) => put_u32(out, doc - p),
            None => put_u32(out, doc),
        }
        prev_doc = Some(doc);
        if positions.is_empty() {
            return Err(FormatError::Invariant("posting with no positions"));
        }
        put_u32(out, positions.len() as u32);
        scratch.clear();
        let mut prev_pos: Option<Position> = None;
        for &p in positions {
            match prev_pos {
                Some(q) if p <= q => return Err(FormatError::Invariant("positions must strictly increase")),
                Some(q) => put_u32(scratch, p - q),
                None => put_u32(scratch, p),
            }
            prev_pos = Some(p);
        }
        put_u32(out, scratch.len() as u32);
        out.extend_from_slice(scratch);
        tokens += positions.len() as u64;
    }
    Ok(tokens)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdxSummary {
    /// Whole file length.
    pub len: u64,
    pub crc: u32,
    pub num_terms: u64,
    pub num_postings: u64,
    pub num_tokens: u64,
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Write a complete `.idx` to `w`. `terms` must be sorted by bytes and unique;
/// each posting list is `(doc, positions)` pairs in doc order.
pub fn write<W, P>(mut w: W, terms: &[(&str, P)]) -> std::result::Result<(W, IdxSummary), crate::Error>
where
    W: Write,
    P: PostingSource,
{
    debug_assert!(terms.windows(2).all(|p| p[0].0.as_bytes() < p[1].0.as_bytes()), "terms must be sorted and unique");
    w.write_all(&envelope::header(Kind::Idx))?;
    let mut body = BodyWriter::new(w);

    let mut dict: Vec<u8> = Vec::new();
    let mut sparse_offsets: Vec<u32> = Vec::new();
    let mut sparse_entries: Vec<u8> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut prev_term: &[u8] = &[];
    let mut postings_pos = 0u64;
    let mut summary = IdxSummary::default();

    for (term, list) in terms {
        let term = term.as_bytes();
        if term.len() > MAX_TERM_BYTES {
            continue;
        }
        buf.clear();
        let tokens = list.encode(&mut buf, &mut scratch)?;
        let i = summary.num_terms as usize;
        if i.is_multiple_of(BLOCK_SIZE) {
            sparse_offsets.push(sparse_entries.len() as u32);
            put_u32(&mut sparse_entries, term.len() as u32);
            sparse_entries.extend_from_slice(term);
            put_u64(&mut sparse_entries, dict.len() as u64);
            put_u64(&mut sparse_entries, postings_pos);
            prev_term = &[];
        }
        let prefix = common_prefix(prev_term, term);
        put_u32(&mut dict, prefix as u32);
        put_u32(&mut dict, (term.len() - prefix) as u32);
        dict.extend_from_slice(&term[prefix..]);
        put_u32(&mut dict, list.doc_freq());
        put_u64(&mut dict, buf.len() as u64);
        body.write_all(&buf)?;
        postings_pos += buf.len() as u64;
        prev_term = term;
        summary.num_terms += 1;
        summary.num_postings += list.doc_freq() as u64;
        summary.num_tokens += tokens;
    }

    let postings_len = postings_pos;
    let dict_off = envelope::HEADER_LEN as u64 + postings_len;
    body.write_all(&dict)?;
    let sparse_off = dict_off + dict.len() as u64;
    let mut sparse = Vec::with_capacity(4 * sparse_offsets.len() + sparse_entries.len());
    for o in &sparse_offsets {
        sparse.extend_from_slice(&o.to_le_bytes());
    }
    sparse.extend_from_slice(&sparse_entries);
    body.write_all(&sparse)?;
    let toc = Toc {
        postings_off: envelope::HEADER_LEN as u64,
        postings_len,
        dict_off,
        dict_len: dict.len() as u64,
        sparse_off,
        sparse_len: sparse.len() as u64,
        num_terms: summary.num_terms,
        num_blocks: sparse_offsets.len() as u32,
        block_size: BLOCK_SIZE as u32,
    };
    body.write_all(&toc.encode())?;
    let (mut w, body_len, crc) = body.finish();
    w.write_all(&envelope::footer(body_len, crc))?;
    summary.len = body_len + (envelope::HEADER_LEN + envelope::FOOTER_LEN) as u64;
    summary.crc = crc;
    Ok((w, summary))
}

/// Something `write` can encode: the in-memory posting list, or a slice of
/// `(doc, positions)` in tests.
pub trait PostingSource {
    fn doc_freq(&self) -> u32;
    fn encode(&self, out: &mut Vec<u8>, scratch: &mut Vec<u8>) -> Result<u64>;
}

impl PostingSource for &crate::index::memory::MemPostingList {
    fn doc_freq(&self) -> u32 {
        PostingList::doc_freq(*self)
    }
    fn encode(&self, out: &mut Vec<u8>, scratch: &mut Vec<u8>) -> Result<u64> {
        encode_postings(self.as_slice().iter().map(|p| (p.doc, p.positions.as_slice())), out, scratch)
    }
}

impl PostingSource for Vec<(DocId, Vec<Position>)> {
    fn doc_freq(&self) -> u32 {
        self.len() as u32
    }
    fn encode(&self, out: &mut Vec<u8>, scratch: &mut Vec<u8>) -> Result<u64> {
        encode_postings(self.iter().map(|(d, p)| (*d, p.as_slice())), out, scratch)
    }
}

// ---------------------------------------------------------------------------

/// Zero-copy view over a complete `.idx` file.
#[derive(Debug, Clone)]
pub struct IdxView<'a> {
    bytes: &'a [u8],
    env: Envelope,
    toc: Toc,
}

impl<'a> IdxView<'a> {
    /// Envelope, TOC and region tiling checks (no crc, no dictionary walk).
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let env = envelope::parse(Kind::Idx, bytes)?;
        if env.body.len() < TOC_LEN {
            return Err(FormatError::Truncated { at: env.body.len(), needed: TOC_LEN });
        }
        let toc = Toc::decode(&bytes[env.body.end - TOC_LEN..env.body.end])?;
        let h = envelope::HEADER_LEN as u64;
        let ok = toc.postings_off == h
            && toc.postings_off.checked_add(toc.postings_len) == Some(toc.dict_off)
            && toc.dict_off.checked_add(toc.dict_len) == Some(toc.sparse_off)
            && toc.sparse_off.checked_add(toc.sparse_len) == Some((env.body.end - TOC_LEN) as u64)
            && toc.block_size as usize == BLOCK_SIZE
            && toc.num_blocks as u64 == toc.num_terms.div_ceil(BLOCK_SIZE as u64)
            && toc.sparse_len >= 4 * toc.num_blocks as u64;
        if !ok {
            return Err(FormatError::corrupt("idx TOC regions do not tile the body"));
        }
        Ok(Self { bytes, env, toc })
    }

    /// Rebuild a view from parts validated by an earlier `parse`.
    pub(crate) fn from_parts(bytes: &'a [u8], env: Envelope, toc: Toc) -> Self {
        Self { bytes, env, toc }
    }

    pub fn envelope(&self) -> &Envelope {
        &self.env
    }

    pub fn crc(&self) -> u32 {
        self.env.crc
    }

    pub fn toc(&self) -> &Toc {
        &self.toc
    }

    pub fn num_terms(&self) -> u64 {
        self.toc.num_terms
    }

    /// `(first term, block_off, postings_off)` of block `b`.
    fn sparse_entry(&self, b: usize) -> Result<(&'a [u8], u64, u64)> {
        let table = to_usize(self.toc.sparse_off)?;
        let off = read_u32_le(self.bytes, table + 4 * b)? as usize;
        let entries = table + 4 * self.toc.num_blocks as usize;
        let sparse_end = to_usize(self.toc.sparse_off + self.toc.sparse_len)?;
        let mut pos = entries + off;
        if pos >= sparse_end {
            return Err(FormatError::corrupt("sparse entry offset out of range"));
        }
        let term_len = read_u32(self.bytes, &mut pos)? as usize;
        let term = slice(self.bytes, pos, term_len)?;
        pos += term_len;
        let block_off = read_u64(self.bytes, &mut pos)?;
        let postings_off = read_u64(self.bytes, &mut pos)?;
        if pos > sparse_end {
            return Err(FormatError::corrupt("sparse entry overruns region"));
        }
        Ok((term, block_off, postings_off))
    }

    /// Dictionary lookup: binary search over block-first terms, then a
    /// front-coded scan of one block. `None` on absence; decode errors are
    /// logged and reported as absence (the trait has no error channel).
    pub fn lookup(&self, term: &str) -> Option<DiskPostingList<'a>> {
        match self.lookup_inner(term.as_bytes()) {
            Ok(r) => r,
            Err(e) => {
                log::error!("corrupt term dictionary while looking up {term:?}: {e}");
                None
            }
        }
    }

    fn lookup_inner(&self, target: &[u8]) -> Result<Option<DiskPostingList<'a>>> {
        // FST: an FST over the term set would replace this sparse index and
        // block scan (and enable prefix/range queries). Only this function
        // and `write`'s dictionary emission would change.
        let nb = self.toc.num_blocks as usize;
        if nb == 0 {
            return Ok(None);
        }
        // First block whose first term is > target; the block before it is
        // the only one that can contain target.
        let (mut lo, mut hi) = (0usize, nb);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (first, _, _) = self.sparse_entry(mid)?;
            if first <= target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return Ok(None);
        }
        let b = lo - 1;
        let mut block = self.block(b)?;
        while let Some(entry) = block.next_entry()? {
            match entry.term.cmp(target) {
                Ordering::Less => {}
                Ordering::Equal => return Ok(Some(self.postings_at(entry.postings_off, entry.postings_len, entry.doc_freq)?)),
                Ordering::Greater => return Ok(None),
            }
        }
        Ok(None)
    }

    fn postings_at(&self, off: u64, len: u64, doc_freq: u32) -> Result<DiskPostingList<'a>> {
        let end = off.checked_add(len).ok_or_else(|| FormatError::corrupt("postings offset overflow"))?;
        if end > self.toc.postings_off + self.toc.postings_len {
            return Err(FormatError::corrupt("posting list extends past the postings region"));
        }
        Ok(DiskPostingList { data: slice(self.bytes, to_usize(off)?, to_usize(len)?)?, doc_freq })
    }

    fn block(&self, b: usize) -> Result<BlockScan<'a>> {
        let (_, block_off, postings_rel) = self.sparse_entry(b)?;
        let start = to_usize(self.toc.dict_off + block_off)?;
        let end = to_usize(self.toc.dict_off + self.toc.dict_len)?;
        if start > end {
            return Err(FormatError::corrupt("block offset outside dictionary"));
        }
        let remaining = (self.toc.num_terms - (b * BLOCK_SIZE) as u64).min(BLOCK_SIZE as u64) as usize;
        Ok(BlockScan {
            bytes: self.bytes,
            pos: start,
            end,
            remaining,
            term: Vec::with_capacity(64),
            postings_pos: self.toc.postings_off + postings_rel,
        })
    }

    /// Every term in order: `(term, doc_freq, postings)`. For verification and stats.
    pub fn terms(&self) -> TermIter<'a> {
        TermIter { view: self.clone(), block: 0, scan: None }
    }

    /// Full check: crc, then every block, entry, sparse entry and posting
    /// list is decoded and cross-checked.
    pub fn verify(&self) -> Result<()> {
        envelope::verify_crc(self.bytes, &self.env)?;
        let mut count = 0u64;
        let mut prev: Option<Vec<u8>> = None;
        let mut expected_postings = self.toc.postings_off;
        for b in 0..self.toc.num_blocks as usize {
            let (first, block_off, postings_rel) = self.sparse_entry(b)?;
            if self.toc.postings_off + postings_rel != expected_postings {
                return Err(FormatError::corrupt(format!("block {b}: sparse postings_off disagrees with running total")));
            }
            let mut scan = self.block(b)?;
            if b > 0 && self.block(b - 1)?.pos_after_block()? != to_usize(self.toc.dict_off + block_off)? {
                return Err(FormatError::corrupt(format!("block {b}: block_off disagrees with previous block end")));
            }
            let mut idx_in_block = 0;
            while let Some(e) = scan.next_entry()? {
                if idx_in_block == 0 && e.term != first {
                    return Err(FormatError::corrupt(format!("block {b}: sparse term differs from first entry")));
                }
                if let Some(p) = &prev {
                    if p.as_slice() >= e.term {
                        return Err(FormatError::corrupt(format!("terms out of order at {:?}", String::from_utf8_lossy(e.term))));
                    }
                }
                std::str::from_utf8(e.term).map_err(|_| FormatError::corrupt("term is not UTF-8"))?;
                let list = self.postings_at(e.postings_off, e.postings_len, e.doc_freq)?;
                list.verify()?;
                prev = Some(e.term.to_vec());
                count += 1;
                idx_in_block += 1;
                expected_postings = e.postings_off + e.postings_len;
            }
        }
        if count != self.toc.num_terms {
            return Err(FormatError::corrupt("num_terms disagrees with dictionary"));
        }
        if expected_postings != self.toc.postings_off + self.toc.postings_len {
            return Err(FormatError::corrupt("postings region has trailing bytes"));
        }
        Ok(())
    }
}

struct Entry<'t> {
    term: &'t [u8],
    doc_freq: u32,
    postings_off: u64,
    postings_len: u64,
}

/// Front-coded scan of one dictionary block.
struct BlockScan<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: usize,
    remaining: usize,
    term: Vec<u8>,
    postings_pos: u64,
}

impl<'a> BlockScan<'a> {
    fn next_entry(&mut self) -> Result<Option<Entry<'_>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let prefix = read_u32(self.bytes, &mut self.pos)? as usize;
        let suffix_len = read_u32(self.bytes, &mut self.pos)? as usize;
        if prefix > self.term.len() {
            return Err(FormatError::corrupt("front-coding prefix longer than previous term"));
        }
        let suffix = slice(self.bytes, self.pos, suffix_len)?;
        self.pos += suffix_len;
        self.term.truncate(prefix);
        self.term.extend_from_slice(suffix);
        let doc_freq = read_u32(self.bytes, &mut self.pos)?;
        let postings_len = read_u64(self.bytes, &mut self.pos)?;
        if self.pos > self.end {
            return Err(FormatError::corrupt("dictionary entry overruns region"));
        }
        let postings_off = self.postings_pos;
        self.postings_pos = postings_off
            .checked_add(postings_len)
            .ok_or_else(|| FormatError::corrupt("postings offset overflow"))?;
        self.remaining -= 1;
        Ok(Some(Entry { term: &self.term, doc_freq, postings_off, postings_len }))
    }

    fn pos_after_block(mut self) -> Result<usize> {
        while self.next_entry()?.is_some() {}
        Ok(self.pos)
    }
}

pub struct TermIter<'a> {
    view: IdxView<'a>,
    block: usize,
    scan: Option<BlockScan<'a>>,
}

impl<'a> TermIter<'a> {
    /// `Ok(None)` at the end; errors surface corruption.
    pub fn try_next(&mut self) -> Result<Option<(String, u32, DiskPostingList<'a>)>> {
        loop {
            if self.scan.is_none() {
                if self.block >= self.view.toc.num_blocks as usize {
                    return Ok(None);
                }
                self.scan = Some(self.view.block(self.block)?);
                self.block += 1;
            }
            let scan = self.scan.as_mut().unwrap();
            match scan.next_entry()? {
                Some(e) => {
                    let term = String::from_utf8(e.term.to_vec()).map_err(|_| FormatError::corrupt("term is not UTF-8"))?;
                    let list = self.view.postings_at(e.postings_off, e.postings_len, e.doc_freq)?;
                    return Ok(Some((term, e.doc_freq, list)));
                }
                None => self.scan = None,
            }
        }
    }
}

// ---------------------------------------------------------------------------

/// A term's encoded postings, borrowed from the mapping.
#[derive(Debug, Clone, Copy)]
pub struct DiskPostingList<'a> {
    data: &'a [u8],
    doc_freq: u32,
}

impl<'a> DiskPostingList<'a> {
    pub fn encoded_len(&self) -> usize {
        self.data.len()
    }

    /// Decode everything, checking deltas and the doc count against `doc_freq`.
    pub fn verify(&self) -> Result<()> {
        let mut pos = 0;
        let count = read_u32(self.data, &mut pos)?;
        if count != self.doc_freq {
            return Err(FormatError::corrupt("posting list doc_count disagrees with dictionary doc_freq"));
        }
        let mut cur = DiskCursor::new(self.data);
        let mut n = 0u32;
        while cur.doc().is_some() {
            if cur.error.is_some() {
                break;
            }
            let tf = cur.term_freq();
            let positions = cur.positions();
            if positions.len() != tf as usize {
                break;
            }
            n += 1;
            cur.advance();
        }
        if let Some(e) = cur.error.take() {
            return Err(e);
        }
        if n != count || cur.pos != self.data.len() {
            return Err(FormatError::corrupt("posting list length disagrees with doc_count"));
        }
        Ok(())
    }
}

impl<'a> PostingList for DiskPostingList<'a> {
    type Cursor<'c>
        = DiskCursor<'a>
    where
        Self: 'c;

    fn doc_freq(&self) -> u32 {
        self.doc_freq
    }

    fn cursor(&self) -> DiskCursor<'a> {
        DiskCursor::new(self.data)
    }
}

#[derive(Debug, Clone, Copy)]
struct Current {
    doc: DocId,
    tf: u32,
    pos_start: usize,
    pos_end: usize,
}

/// Lazy decoder over one posting list. Doc headers are decoded on `advance`;
/// positions only when asked, into a buffer owned by the cursor.
pub struct DiskCursor<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u32,
    cur: Option<Current>,
    positions: Vec<Position>,
    decoded: bool,
    error: Option<FormatError>,
}

impl<'a> DiskCursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut c = Self { data, pos: 0, remaining: 0, cur: None, positions: Vec::new(), decoded: false, error: None };
        match read_u32(data, &mut c.pos) {
            Ok(n) => c.remaining = n,
            Err(e) => c.fail(e),
        }
        c.load_next();
        c
    }

    fn fail(&mut self, e: FormatError) {
        log::error!("corrupt posting list: {e}");
        self.error = Some(e);
        self.cur = None;
        self.remaining = 0;
    }

    fn load_next(&mut self) {
        if self.remaining == 0 {
            self.cur = None;
            return;
        }
        match self.decode_header() {
            Ok(c) => {
                self.cur = Some(c);
                self.remaining -= 1;
                self.decoded = false;
            }
            Err(e) => self.fail(e),
        }
    }

    fn decode_header(&mut self) -> Result<Current> {
        let delta = read_u32(self.data, &mut self.pos)?;
        let doc = match self.cur {
            None => delta,
            Some(c) => {
                if delta == 0 {
                    return Err(FormatError::corrupt("zero doc delta"));
                }
                c.doc.checked_add(delta).ok_or_else(|| FormatError::corrupt("doc id overflow"))?
            }
        };
        let tf = read_u32(self.data, &mut self.pos)?;
        if tf == 0 {
            return Err(FormatError::corrupt("zero term frequency"));
        }
        let pos_bytes = read_u32(self.data, &mut self.pos)? as usize;
        let pos_start = self.pos;
        let pos_end = pos_start.checked_add(pos_bytes).filter(|&e| e <= self.data.len()).ok_or(
            FormatError::Truncated { at: pos_start, needed: pos_bytes },
        )?;
        self.pos = pos_end;
        Ok(Current { doc, tf, pos_start, pos_end })
    }

    fn decode_positions(&mut self) -> Result<()> {
        let Some(c) = self.cur else { return Ok(()) };
        self.positions.clear();
        let slice = &self.data[..c.pos_end];
        let mut p = c.pos_start;
        let mut prev: Option<Position> = None;
        for _ in 0..c.tf {
            let d = read_u32(slice, &mut p)?;
            let v = match prev {
                None => d,
                Some(q) => {
                    if d == 0 {
                        return Err(FormatError::corrupt("zero position delta"));
                    }
                    q.checked_add(d).ok_or_else(|| FormatError::corrupt("position overflow"))?
                }
            };
            self.positions.push(v);
            prev = Some(v);
        }
        if p != c.pos_end {
            return Err(FormatError::corrupt("pos_bytes disagrees with position varints"));
        }
        Ok(())
    }
}

impl PostingCursor for DiskCursor<'_> {
    fn doc(&self) -> Option<DocId> {
        self.cur.map(|c| c.doc)
    }

    fn advance(&mut self) -> Option<DocId> {
        if self.cur.is_some() {
            self.load_next();
        }
        self.doc()
    }

    fn seek(&mut self, target: DocId) -> Option<DocId> {
        while let Some(c) = self.cur {
            if c.doc >= target {
                break;
            }
            self.load_next();
        }
        self.doc()
    }

    fn positions(&mut self) -> &[Position] {
        if !self.decoded {
            if let Err(e) = self.decode_positions() {
                self.positions.clear();
                self.fail(e);
            }
            self.decoded = true;
        }
        &self.positions
    }

    fn term_freq(&mut self) -> u32 {
        self.cur.map_or(0, |c| c.tf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Lists = Vec<(DocId, Vec<Position>)>;

    fn build(terms: &[(&str, Lists)]) -> Vec<u8> {
        let mut sorted: Vec<(&str, Lists)> = terms.to_vec();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let (bytes, summary) = write(Vec::new(), &sorted).unwrap();
        assert_eq!(summary.len as usize, bytes.len());
        bytes
    }

    fn collect(list: DiskPostingList<'_>) -> Lists {
        let mut c = list.cursor();
        let mut out = vec![];
        while let Some(d) = c.doc() {
            out.push((d, c.positions().to_vec()));
            c.advance();
        }
        out
    }

    #[test]
    fn postings_roundtrip_and_laziness() {
        let list: Lists = vec![(3, vec![0, 5, 6]), (4, vec![2]), (100, vec![7, 1000, 100_000])];
        let mut out = Vec::new();
        let tokens = encode_postings(list.iter().map(|(d, p)| (*d, p.as_slice())), &mut out, &mut Vec::new()).unwrap();
        assert_eq!(tokens, 7);
        let dl = DiskPostingList { data: &out, doc_freq: 3 };
        dl.verify().unwrap();
        assert_eq!(collect(dl), list);

        let mut c = dl.cursor();
        assert_eq!(c.doc(), Some(3));
        assert_eq!(c.term_freq(), 3);
        assert!(!c.decoded);
        assert_eq!(c.seek(50), Some(100)); // skipped doc 4 without decoding its positions
        assert!(!c.decoded);
        assert_eq!(c.positions(), &[7, 1000, 100_000]);
        assert_eq!(c.seek(100), Some(100));
        assert_eq!(c.advance(), None);
        assert_eq!(c.seek(0), None);
        assert_eq!(c.term_freq(), 0);
    }

    #[test]
    fn encoder_refuses_invariant_violations() {
        let mut out = Vec::new();
        let mut s = Vec::new();
        let dup_pos: Lists = vec![(1, vec![5, 5])];
        assert_eq!(
            encode_postings(dup_pos.iter().map(|(d, p)| (*d, p.as_slice())), &mut out, &mut s),
            Err(FormatError::Invariant("positions must strictly increase"))
        );
        let dup_doc: Lists = vec![(1, vec![0]), (1, vec![1])];
        assert!(matches!(encode_postings(dup_doc.iter().map(|(d, p)| (*d, p.as_slice())), &mut out, &mut s), Err(FormatError::Invariant(_))));
        let empty: Lists = vec![(1, vec![])];
        assert!(matches!(encode_postings(empty.iter().map(|(d, p)| (*d, p.as_slice())), &mut out, &mut s), Err(FormatError::Invariant(_))));
    }

    #[test]
    fn decoder_rejects_zero_deltas() {
        // doc_count=1, doc=1, tf=2, pos_bytes=2, positions 5, +0
        let bad = [1u8, 1, 2, 2, 5, 0];
        let dl = DiskPostingList { data: &bad, doc_freq: 1 };
        assert!(matches!(dl.verify(), Err(FormatError::Corrupt(_))));
        let mut c = dl.cursor();
        assert_eq!(c.positions(), &[] as &[u32]); // failure is logged and yields nothing
        assert!(c.error.is_some());

        // doc_count=2, doc=1, tf=1, pb=1, pos 0, then doc delta 0
        let bad2 = [2u8, 1, 1, 1, 0, 0, 1, 1, 0];
        let mut c = DiskCursor::new(&bad2);
        assert_eq!(c.doc(), Some(1));
        assert_eq!(c.advance(), None); // second header is corrupt
        assert!(c.error.is_some());
    }

    #[test]
    fn empty_index() {
        let bytes = build(&[]);
        let v = IdxView::parse(&bytes).unwrap();
        v.verify().unwrap();
        assert_eq!(v.num_terms(), 0);
        assert!(v.lookup("anything").is_none());
        assert!(v.terms().try_next().unwrap().is_none());
    }

    #[test]
    fn lookup_across_block_boundaries() {
        for n in [1usize, 2, 63, 64, 65, 127, 128, 129, 300] {
            let terms: Vec<(String, Lists)> = (0..n).map(|i| (format!("t{i:05}"), vec![(i as u32, vec![i as u32])])).collect();
            let refs: Vec<(&str, Lists)> = terms.iter().map(|(t, l)| (t.as_str(), l.clone())).collect();
            let bytes = build(&refs);
            let v = IdxView::parse(&bytes).unwrap();
            v.verify().unwrap();
            assert_eq!(v.num_terms() as usize, n);
            assert_eq!(v.toc().num_blocks as usize, n.div_ceil(64));
            for (t, l) in &terms {
                let found = v.lookup(t).unwrap_or_else(|| panic!("n={n} term {t}"));
                assert_eq!(found.doc_freq(), 1);
                assert_eq!(collect(found), *l);
            }
            assert!(v.lookup("s").is_none()); // before everything
            assert!(v.lookup("t00000a").is_none()); // between terms
            assert!(v.lookup("u").is_none()); // after everything
            assert!(v.lookup("").is_none());
            // Iterate everything in order.
            let mut it = v.terms();
            let mut seen = 0;
            while let Some((term, df, _)) = it.try_next().unwrap() {
                assert_eq!(term, terms[seen].0);
                assert_eq!(df, 1);
                seen += 1;
            }
            assert_eq!(seen, n);
        }
    }

    #[test]
    fn front_coding_with_shared_prefixes_and_unicode() {
        let terms: Vec<(&str, Lists)> = vec![
            ("a", vec![(1, vec![1])]),
            ("ab", vec![(1, vec![2])]),
            ("abc", vec![(2, vec![0])]),
            ("abd", vec![(3, vec![0, 1])]),
            ("b", vec![(1, vec![3])]),
            ("café", vec![(4, vec![0])]),
            ("cafés", vec![(4, vec![1])]),
            ("日本", vec![(5, vec![0])]),
            ("日本語", vec![(5, vec![1])]),
        ];
        let bytes = build(&terms);
        let v = IdxView::parse(&bytes).unwrap();
        v.verify().unwrap();
        for (t, l) in &terms {
            assert_eq!(collect(v.lookup(t).unwrap()), *l, "{t}");
        }
        assert!(v.lookup("abcd").is_none());
        assert!(v.lookup("caf").is_none());
        assert!(v.lookup("日").is_none());
    }

    #[test]
    fn overlong_terms_are_dropped() {
        let long = "x".repeat(MAX_TERM_BYTES + 1);
        let terms: Vec<(&str, Lists)> = vec![("short", vec![(1, vec![0])]), (long.as_str(), vec![(1, vec![1])])];
        let bytes = build(&terms);
        let v = IdxView::parse(&bytes).unwrap();
        v.verify().unwrap();
        assert_eq!(v.num_terms(), 1);
        assert!(v.lookup(&long).is_none());
        assert!(v.lookup("short").is_some());
    }

    #[test]
    fn corruption_is_caught_by_verify() {
        let terms: Vec<(&str, Lists)> = vec![("alpha", vec![(1, vec![0, 3])]), ("beta", vec![(2, vec![1])])];
        let bytes = build(&terms);
        let mut flipped = bytes.clone();
        flipped[20] ^= 0x01; // inside postings
        let v = IdxView::parse(&flipped).unwrap();
        assert!(matches!(v.verify(), Err(FormatError::Checksum { .. })));

        let mut toc_bad = bytes.clone();
        let toc_at = bytes.len() - 16 - TOC_LEN;
        toc_bad[toc_at + 56] ^= 1; // num_blocks
        assert!(matches!(IdxView::parse(&toc_bad), Err(FormatError::Corrupt(_))));
    }
}
