//! Read-only reader for the Xapian "glass" backend, single-file variant
//! (as embedded in ZIM files).
//!
//! Two distinct encodings live in a glass database:
//!   * **Packed integers** (`unpack_uint`): LSB-first base-128 varint (LEB128),
//!     used in the version file and inside B-tree tags. Endian-independent.
//!   * **Block-structural integers**: fixed-width **big-endian** (the block
//!     header R/L/M/T/D fields and the directory offsets).
//!
//! References (in the cloned xapian-core tree):
//!   * backends/glass/glass_version.cc  — version file + RootInfo layout
//!   * backends/glass/glass_table.{h,cc} — block header + B-tree
//!   * common/pack.h                     — unpack_uint / unpack_string
//!   * common/wordaccess.h               — big-endian block fields

use std::io;

// ---------------------------------------------------------------------------
// LEB128 varint (Xapian pack_uint / unpack_uint)
// ---------------------------------------------------------------------------

/// Decode an LSB-first base-128 varint. Advances `*pos`. Returns None on overflow/EOD.
pub fn unpack_uint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0u64;
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            return None;
        }
        let byte = buf[*pos];
        *pos += 1;
        if shift >= 64 {
            // would overflow
            if byte < 128 {
                return Some(result);
            }
            continue;
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte < 128 {
            return Some(result);
        }
        shift += 7;
    }
}

/// Decode a "sort preserving" unsigned integer (pack_uint_preserving_sort).
pub fn unpack_sortable(buf: &[u8], pos: &mut usize) -> Option<u64> {
    if *pos >= buf.len() {
        return None;
    }
    let len_byte = buf[*pos];
    *pos += 1;
    if len_byte < 0x80 {
        if *pos >= buf.len() {
            return None;
        }
        let lo = buf[*pos];
        *pos += 1;
        return Some(((len_byte as u64) << 8) | lo as u64);
    }
    if len_byte == 0xff {
        return None;
    }
    // count leading set bits (from 0x40 downwards) -> number of trailing bytes
    let mut len = 2usize;
    let mut m = 0x40u8;
    while len_byte & m != 0 {
        len += 1;
        m >>= 1;
    }
    let mask = 0xffu16 << (9 - len);
    let mut r = (len_byte & !(mask as u8)) as u64;
    for _ in 0..len {
        if *pos >= buf.len() {
            return None;
        }
        r = (r << 8) | buf[*pos] as u64;
        *pos += 1;
    }
    Some(r)
}

/// Encode a "sort preserving" unsigned integer (pack_uint_preserving_sort).
pub fn pack_sortable(value: u64) -> Vec<u8> {
    if value < 0x8000 {
        return vec![(value >> 8) as u8, (value & 0xff) as u8];
    }
    // len = total number of bytes (3..=9).
    let mut len = 3usize;
    let mut x = value >> 22;
    while x != 0 {
        len += 1;
        x >>= 7;
    }
    let mut buf = vec![0u8; len];
    let mut v = value;
    for i in 1..len {
        buf[len - i] = (v & 0xff) as u8;
        v >>= 8;
    }
    let mask = (0xffu16 << (10 - len)) as u8; // low byte: top (len-1) bits set
    buf[0] = (v as u8) | mask;
    buf
}

/// Decode a length-prefixed string (pack_string = unpack_uint(len) + bytes).
pub fn unpack_string<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = unpack_uint(buf, pos)? as usize;
    if *pos + len > buf.len() {
        return None;
    }
    let s = &buf[*pos..*pos + len];
    *pos += len;
    Some(s)
}

// ---------------------------------------------------------------------------
// Version file (single-file: the blob start is the version magic)
// ---------------------------------------------------------------------------

const GLASS_MAGIC: &[u8] = b"\x0f\x0dXapian Glass";
/// DATE_TO_VERSION(2016,3,14) = ((2016-2014)<<9 | 3<<5 | 14) = 1134.
const GLASS_FORMAT_VERSION: u16 = 1134;

/// One table's root info (decoded from the version file).
#[derive(Debug, Clone)]
pub struct RootInfo {
    pub root: u64,
    pub level: u32,
    pub num_entries: u64,
    pub root_is_fake: bool,
    pub sequential: bool,
    pub blocksize: u32,
    pub compress_min: u32,
    /// Serialised freelist (we don't need it for read-only traversal).
    pub fl_serialised_len: usize,
}

impl RootInfo {
    fn parse(buf: &[u8], pos: &mut usize) -> Option<RootInfo> {
        let root = unpack_uint(buf, pos)?;
        let val = unpack_uint(buf, pos)?;
        let num_entries = unpack_uint(buf, pos)?;
        let b = unpack_uint(buf, pos)?;
        let mut compress_min = unpack_uint(buf, pos)? as u32;
        let fl = unpack_string(buf, pos)?;

        let level = (val >> 2) as u32;
        let sequential = (val & 0x02) != 0;
        let root_is_fake = (val & 0x01) != 0;
        let blocksize = (b << 11) as u32;
        if compress_min == 4 {
            compress_min = 8; // COMPRESS_MIN default
        }
        Some(RootInfo {
            root,
            level,
            num_entries,
            root_is_fake,
            sequential,
            blocksize,
            compress_min,
            fl_serialised_len: fl.len(),
        })
    }
}

/// Table indices (Glass::table_type order).
pub const POSTLIST: usize = 0;
pub const DOCDATA: usize = 1;
pub const TERMLIST: usize = 2;
pub const POSITION: usize = 3;
pub const SPELLING: usize = 4;
pub const SYNONYM: usize = 5;
pub const TABLE_NAMES: [&str; 6] = [
    "postlist", "docdata", "termlist", "position", "spelling", "synonym",
];

#[derive(Debug, Clone)]
pub struct Version {
    pub format_version: u16,
    pub uuid: [u8; 16],
    pub rev: u64,
    pub roots: Vec<RootInfo>,
    // database stats
    pub doccount: u64,
    pub last_docid: u64,
    pub doclen_lbound: u64,
    pub wdf_ubound: u64,
    pub doclen_ubound: u64,
    pub oldest_changeset: u64,
    pub total_doclen: u64,
    pub spelling_wordfreq_ubound: u64,
}

impl Version {
    /// Parse the version data starting at `data` (== the blob bytes from offset 0).
    pub fn parse(data: &[u8]) -> io::Result<Version> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());

        if data.len() < 16 || &data[0..GLASS_MAGIC.len()] != GLASS_MAGIC {
            return Err(bad("glass magic mismatch"));
        }
        let format_version = u16::from_be_bytes([data[14], data[15]]);
        if format_version != GLASS_FORMAT_VERSION {
            return Err(bad("unsupported glass format version"));
        }

        let mut pos = 16usize;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&data[pos..pos + 16]);
        pos += 16;

        let rev = unpack_uint(data, &mut pos).ok_or_else(|| bad("bad rev"))?;

        let mut roots = Vec::with_capacity(6);
        for _ in 0..6 {
            roots.push(RootInfo::parse(data, &mut pos).ok_or_else(|| bad("bad root info"))?);
        }

        // Database stats (see GlassVersion::unserialise_stats).
        let doccount = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats doccount"))?;
        let mut last_docid = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats last_docid"))?;
        let doclen_lbound = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats dl_lbound"))?;
        let wdf_ubound = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats wdf_ubound"))?;
        let mut doclen_ubound = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats dl_ubound"))?;
        let oldest_changeset = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats oldest"))?;
        let total_doclen = unpack_uint(data, &mut pos).ok_or_else(|| bad("stats total_dl"))?;
        let spelling_wordfreq_ubound =
            unpack_uint(data, &mut pos).ok_or_else(|| bad("stats spell_ub"))?;

        // Stored deltas (see unserialise_stats).
        last_docid += doccount;
        doclen_ubound += wdf_ubound;

        Ok(Version {
            format_version,
            uuid,
            rev,
            roots,
            doccount,
            last_docid,
            doclen_lbound,
            wdf_ubound,
            doclen_ubound,
            oldest_changeset,
            total_doclen,
            spelling_wordfreq_ubound,
        })
    }
}

// ---------------------------------------------------------------------------
// Block-level access
// ---------------------------------------------------------------------------

pub const DIR_START: usize = 11;
pub const LEVEL_FREELIST: u8 = 254;

/// A borrowed B-tree block.
#[derive(Clone, Copy)]
pub struct Block<'a> {
    pub data: &'a [u8],
}

impl<'a> Block<'a> {
    #[inline]
    pub fn revision(&self) -> u32 {
        u32::from_be_bytes(self.data[0..4].try_into().unwrap())
    }
    #[inline]
    pub fn level(&self) -> u8 {
        self.data[4]
    }
    #[inline]
    pub fn max_free(&self) -> usize {
        u16::from_be_bytes(self.data[5..7].try_into().unwrap()) as usize
    }
    #[inline]
    pub fn total_free(&self) -> usize {
        u16::from_be_bytes(self.data[7..9].try_into().unwrap()) as usize
    }
    #[inline]
    pub fn dir_end(&self) -> usize {
        u16::from_be_bytes(self.data[9..11].try_into().unwrap()) as usize
    }
    /// Number of directory entries (each entry is a 2-byte big-endian offset).
    #[inline]
    pub fn dir_count(&self) -> usize {
        (self.dir_end() - DIR_START) / 2
    }
    pub fn is_freelist(&self) -> bool {
        self.level() == LEVEL_FREELIST
    }
}

/// A single table within the (single-file) glass database.
pub struct Table<'a> {
    /// The whole mmap'd file (or blob); table reads index into here.
    file: &'a [u8],
    /// Absolute byte offset where this single-file DB's block space begins.
    base: usize,
    pub block_size: usize,
    pub root: u64,
    pub level: u32,
    pub num_entries: u64,
    pub sequential: bool,
    pub root_is_fake: bool,
    pub compress_min: u32,
    pub name: &'static str,
}

impl<'a> Table<'a> {
    pub const fn new(file: &'a [u8], base: usize, info: &RootInfo, name: &'static str) -> Table<'a> {
        Table {
            file,
            base,
            block_size: info.blocksize as usize,
            root: info.root,
            level: info.level,
            num_entries: info.num_entries,
            sequential: info.sequential,
            root_is_fake: info.root_is_fake,
            compress_min: info.compress_min,
            name,
        }
    }

    /// Read block `n` (file offset = base + n*block_size).
    pub fn block(&self, n: u64) -> io::Result<Block<'a>> {
        let start = self.base + (n as usize) * self.block_size;
        let end = start + self.block_size;
        if end > self.file.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("block {n} out of range for table {}", self.name),
            ));
        }
        Ok(Block {
            data: &self.file[start..end],
        })
    }

    /// Decode a term's postlist. Returns (termfreq, collfreq, postings[(docid,wdf)]).
    /// `term` empty => the doclen list (special key, no separator before did).
    pub fn read_postlist(&self, term: &[u8]) -> io::Result<Option<Postlist>> {
        // Build the first-chunk key.
        let prefix: Vec<u8> = if term.is_empty() {
            vec![0x00, 0xe0]
        } else {
            // pack_string_preserving_sort(term, last=true): escape internal NULs.
            escape_nuls(term)
        };
        let mut cur = self.new_cursor()?;
        if !self.find(&mut cur, &prefix)? {
            return Ok(None);
        }
        let tag = self.read_tag(&mut cur)?;

        let mut postings: Vec<(u64, u32)> = Vec::new();
        let mut pos = 0usize;
        let termfreq = unpack_uint(&tag, &mut pos).ok_or_else(eod)?;
        let collfreq = unpack_uint(&tag, &mut pos).ok_or_else(eod)?;
        let mut did = unpack_uint(&tag, &mut pos).ok_or_else(eod)? + 1;
        let mut is_last = decode_chunk_body(&tag, &mut pos, did, &mut postings)?;

        // Continuation chunks via cursor.next(); first_did parsed from the key.
        let dbg = std::env::var("ZXR_DEBUG").is_ok();
        while !is_last {
            if !self.next_default(&mut cur, 0)? {
                if dbg { eprintln!("  [stop: next_default end of table]"); }
                break;
            }
            let blk = cur.levels[0].block.unwrap();
            let key = leaf_item(&blk, cur.levels[0].c as usize).key.to_vec();
            // Verify same postlist and parse first-did from the key suffix.
            let sep = if term.is_empty() { 0 } else { 1 }; // NUL separator for real terms
            if key.len() < prefix.len() + sep || &key[..prefix.len()] != &prefix[..] {
                if dbg { eprintln!("  [stop: prefix mismatch, key={:?}]", String::from_utf8_lossy(&key)); }
                break;
            }
            let mut kp = prefix.len();
            if sep == 1 {
                if key[kp] != 0 {
                    if dbg { eprintln!("  [stop: sep byte != 0, key={:02x?}]", key); }
                    break;
                }
                kp += 1;
            }
            let chunk_first_did = unpack_sortable(&key, &mut kp).ok_or_else(eod)?;
            let tag = self.read_tag(&mut cur)?;
            did = chunk_first_did;
            let mut p = 0usize;
            is_last = decode_chunk_body(&tag, &mut p, did, &mut postings)?;
        }

        Ok(Some(Postlist { termfreq, collfreq, postings }))
    }

    /// Look up an exact key. Returns the (decompressed) tag bytes, or None.
    pub fn get_exact_entry(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if key.is_empty() || key.len() > 255 || self.root_is_fake {
            return Ok(None);
        }
        let mut cur = self.new_cursor()?;
        if !self.find(&mut cur, key)? {
            return Ok(None);
        }
        Ok(Some(self.read_tag(&mut cur)?))
    }

    /// Position a cursor at the first (leftmost) entry in the table.
    pub fn cursor_to_first(&self) -> io::Result<Cursor<'a>> {
        let mut cur = self.new_cursor()?;
        for j in (1..=self.level as usize).rev() {
            cur.levels[j].c = DIR_START as i32;
            let blk = cur.levels[j].block.unwrap();
            let child = bitem_block_given_by(&blk, DIR_START);
            cur.levels[j - 1].block = Some(self.block(child)?);
            cur.levels[j - 1].n = child;
        }
        cur.levels[0].c = DIR_START as i32;
        Ok(cur)
    }

    /// Debug: dump up to `limit` (key, tag_len, compressed, first, last, component) tuples.
    pub fn dump_keys(&self, limit: usize) -> io::Result<Vec<(Vec<u8>, usize, bool, bool, bool, i32)>> {
        let mut out = Vec::new();
        if self.root_is_fake {
            return Ok(out);
        }
        let mut cur = self.cursor_to_first()?;
        loop {
            let blk = cur.levels[0].block.unwrap();
            let item = leaf_item(&blk, cur.levels[0].c as usize);
            out.push((
                item.key.to_vec(),
                item.chunk.len(),
                item.compressed,
                item.first,
                item.last,
                item.component_of,
            ));
            if out.len() >= limit {
                break;
            }
            if !self.next_default(&mut cur, 0)? {
                break;
            }
        }
        Ok(out)
    }

    /// Dump up to `limit` keys starting at the first key >= `start`.
    pub fn dump_keys_from(&self, start: &[u8], limit: usize) -> io::Result<Vec<(Vec<u8>, usize)>> {
        let mut out = Vec::new();
        if self.root_is_fake {
            return Ok(out);
        }
        let mut cur = self.new_cursor()?;
        let exact = self.find(&mut cur, start)?;
        // find positions at last key <= start. If not exact (or before-first), step to >= start.
        if !exact {
            if cur.levels[0].c < DIR_START as i32 {
                cur.levels[0].c = DIR_START as i32;
            } else if !self.next_default(&mut cur, 0)? {
                return Ok(out);
            }
        }
        loop {
            let blk = cur.levels[0].block.unwrap();
            let item = leaf_item(&blk, cur.levels[0].c as usize);
            if item.first {
                out.push((item.key.to_vec(), item.chunk.len()));
                if out.len() >= limit {
                    break;
                }
            }
            if !self.next_default(&mut cur, 0)? {
                break;
            }
        }
        Ok(out)
    }

    fn new_cursor(&self) -> io::Result<Cursor<'a>> {
        let mut levels = Vec::with_capacity(self.level as usize + 1);
        for _ in 0..=self.level {
            levels.push(CursorLevel { n: u64::MAX, block: None, c: -1 });
        }
        // Load the root into the top level.
        let lv = self.level as usize;
        levels[lv].block = Some(self.block(self.root)?);
        levels[lv].n = self.root;
        Ok(Cursor { levels })
    }

    /// Descend from root to the leaf for `key`. Returns true on exact match.
    /// On return, cur.levels[0].c is the directory slot of the last key <= search key.
    fn find(&self, cur: &mut Cursor<'a>, key: &[u8]) -> io::Result<bool> {
        // Branch levels: level..1
        for j in (1..=self.level as usize).rev() {
            let blk = cur.levels[j].block.unwrap();
            let c = find_in_branch(&blk, key, -1);
            cur.levels[j].c = c;
            let child = bitem_block_given_by(&blk, c as usize);
            // load child into level j-1
            if cur.levels[j - 1].n != child {
                cur.levels[j - 1].block = Some(self.block(child)?);
                cur.levels[j - 1].n = child;
            }
        }
        let leaf = cur.levels[0].block.unwrap();
        let (c, exact) = find_in_leaf(&leaf, key, -1);
        cur.levels[0].c = c;
        Ok(exact)
    }

    /// Read (and decompress if needed) the tag the cursor's leaf currently points at.
    fn read_tag(&self, cur: &mut Cursor<'a>) -> io::Result<Vec<u8>> {
        let leaf = cur.levels[0].block.unwrap();
        let first = leaf_item(&leaf, cur.levels[0].c as usize);
        let compressed = first.compressed;
        let mut raw: Vec<u8> = Vec::new();

        loop {
            let blk = cur.levels[0].block.unwrap();
            let item = leaf_item(&blk, cur.levels[0].c as usize);
            raw.extend_from_slice(item.chunk);
            if item.last {
                break;
            }
            if !self.next_default(cur, 0)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected end of table reading tag continuation",
                ));
            }
        }

        if compressed {
            decompress_deflate(&raw)
        } else {
            Ok(raw)
        }
    }

    /// Advance the cursor at level `j` to the next item (non-sequential variant).
    fn next_default(&self, cur: &mut Cursor<'a>, j: usize) -> io::Result<bool> {
        let blk = cur.levels[j].block.unwrap();
        let mut c = cur.levels[j].c + 2; // += D2
        if c >= blk.dir_end() as i32 {
            if j == self.level as usize {
                return Ok(false);
            }
            if !self.next_default(cur, j + 1)? {
                return Ok(false);
            }
            c = DIR_START as i32;
        }
        cur.levels[j].c = c;
        if j > 0 {
            let parent = cur.levels[j].block.unwrap();
            let child = bitem_block_given_by(&parent, c as usize);
            if cur.levels[j - 1].n != child {
                cur.levels[j - 1].block = Some(self.block(child)?);
                cur.levels[j - 1].n = child;
            }
            cur.levels[j - 1].c = DIR_START as i32; // will be overwritten on deeper descent
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Cursor + item parsing
// ---------------------------------------------------------------------------

/// A decoded postlist.
pub struct Postlist {
    pub termfreq: u64,
    pub collfreq: u64,
    pub postings: Vec<(u64, u32)>,
}

fn eod() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "unexpected end of postlist data")
}

/// pack_string_preserving_sort(term, last=true): internal NUL -> NUL,0xff. No trailing NUL.
fn escape_nuls(term: &[u8]) -> Vec<u8> {
    if !term.contains(&0) {
        return term.to_vec();
    }
    let mut out = Vec::with_capacity(term.len());
    for &b in term {
        out.push(b);
        if b == 0 {
            out.push(0xff);
        }
    }
    out
}

/// Decode a standard chunk body starting at `*pos`, given the first docid in the chunk.
/// Appends (docid, wdf) postings. Returns is_last_chunk.
fn decode_chunk_body(
    tag: &[u8],
    pos: &mut usize,
    first_did: u64,
    postings: &mut Vec<(u64, u32)>,
) -> io::Result<bool> {
    // is_last_chunk: 1-byte bool encoded as ASCII '0'/'1' (see unpack_bool).
    if *pos >= tag.len() {
        return Err(eod());
    }
    let is_last = tag[*pos] == b'1';
    *pos += 1;
    let _increase_to_last = unpack_uint(tag, pos).ok_or_else(eod)?;
    let mut did = first_did;
    // First entry's wdf.
    let wdf = unpack_uint(tag, pos).ok_or_else(eod)? as u32;
    postings.push((did, wdf));
    // Remaining entries: (did_increase, wdf) until end of tag.
    while *pos < tag.len() {
        let inc = unpack_uint(tag, pos).ok_or_else(eod)?;
        did += inc + 1;
        let wdf = unpack_uint(tag, pos).ok_or_else(eod)? as u32;
        postings.push((did, wdf));
    }
    Ok(is_last)
}

struct CursorLevel<'a> {
    n: u64,
    block: Option<Block<'a>>,
    c: i32,
}

pub struct Cursor<'a> {
    levels: Vec<CursorLevel<'a>>,
}

// Item flag bits (top 3 bits of the first byte of the big-endian I field).
const I_COMPRESSED_BIT: u8 = 0x80;
const I_LAST_BIT: u8 = 0x40;
const I_FIRST_BIT: u8 = 0x20;
const ITEM_SIZE_MASK: usize = 0x1fff;

/// Read a 2-byte big-endian directory pointer at directory slot `c`.
#[inline]
fn dir_ptr(block: &Block, c: usize) -> usize {
    u16::from_be_bytes([block.data[c], block.data[c + 1]]) as usize
}

/// A parsed leaf item (key + this chunk's tag bytes + flags).
struct LeafItemView<'a> {
    key: &'a [u8],
    component_of: i32,
    compressed: bool,
    first: bool,
    last: bool,
    chunk: &'a [u8],
}

fn leaf_item<'a>(block: &Block<'a>, c: usize) -> LeafItemView<'a> {
    let off = dir_ptr(block, c);
    let ip = &block.data[off..];
    let flags = ip[0];
    let i = u16::from_be_bytes([ip[0], ip[1]]) as usize;
    let size = (i & ITEM_SIZE_MASK) + 3;
    let key_len = ip[2] as usize;
    let key = &ip[3..3 + key_len];
    let first = (flags & I_FIRST_BIT) != 0;
    let last = (flags & I_LAST_BIT) != 0;
    let compressed = (flags & I_COMPRESSED_BIT) != 0;
    let mut cd = 3 + key_len; // I2 + K1 + key
    let component_of = if first {
        1
    } else {
        let x = u16::from_be_bytes([ip[cd], ip[cd + 1]]) as i32;
        cd += 2; // X2
        x
    };
    let chunk = &ip[cd..size];
    LeafItemView { key, component_of, compressed, first, last, chunk }
}

/// Branch item key bytes + component, and the child block number.
fn bitem_key<'a>(block: &Block<'a>, c: usize) -> (&'a [u8], i32) {
    let off = dir_ptr(block, c);
    let ip = &block.data[off..];
    let key_len = ip[4] as usize; // after 4-byte block number
    let key = &ip[5..5 + key_len];
    let comp = u16::from_be_bytes([ip[5 + key_len], ip[6 + key_len]]) as i32;
    (key, comp)
}

fn bitem_block_given_by(block: &Block, c: usize) -> u64 {
    let off = dir_ptr(block, c);
    let ip = &block.data[off..];
    u32::from_be_bytes([ip[0], ip[1], ip[2], ip[3]]) as u64
}

/// compare(search_key, item_key): byte order, then length, then component_of.
/// The search key always has component_of == 1.
#[inline]
fn compare_key(search: &[u8], item_key: &[u8], item_component: i32) -> i32 {
    let k = search.len().min(item_key.len());
    let c = search[..k].cmp(&item_key[..k]);
    match c {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => {
            let d = search.len() as i32 - item_key.len() as i32;
            if d != 0 {
                d
            } else {
                1 - item_component // search component_of is 1
            }
        }
    }
}

/// Returns the directory slot of the last branch key <= search key.
fn find_in_branch(block: &Block, key: &[u8], _hint: i32) -> i32 {
    let mut i = DIR_START as i32;
    let mut j = block.dir_end() as i32;
    while j - i > 2 {
        let mid = i + ((j - i) / 4) * 2; // mid, aligned to D2
        let (ik, ic) = bitem_key(block, mid as usize);
        // compare(item=key, BItem) ; r<0 means key precedes branch item
        let r = compare_key(key, ik, ic);
        if r < 0 {
            j = mid;
        } else {
            i = mid;
            if r == 0 {
                break;
            }
        }
    }
    i
}

/// Returns (directory slot of last leaf key <= search key, exact match flag).
fn find_in_leaf(block: &Block, key: &[u8], _hint: i32) -> (i32, bool) {
    let mut i = DIR_START as i32 - 2; // can be "before first"
    let mut j = block.dir_end() as i32;
    let mut exact = false;
    while j - i > 2 {
        let mid = i + ((j - i) / 4) * 2;
        let item = leaf_item(block, mid as usize);
        let r = compare_key(key, item.key, item.component_of);
        if r < 0 {
            j = mid;
        } else {
            i = mid;
            if r == 0 {
                exact = true;
                break;
            }
        }
    }
    (i, exact)
}

/// Raw DEFLATE decompression (glass uses inflateInit2(-15), i.e. no zlib header).
fn decompress_deflate(raw: &[u8]) -> io::Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    let mut dec = DeflateDecoder::new(raw);
    dec.read_to_end(&mut out)?;
    Ok(out)
}
