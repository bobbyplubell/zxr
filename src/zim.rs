//! Minimal read-only ZIM container parser.
//!
//! Just enough to locate the embedded Xapian fulltext index blob
//! (`X/fulltext/xapian`) inside a ZIM v6 file and hand back its location.
//!
//! ZIM format reference: <https://wiki.openzim.org/wiki/ZIM_file_format>
//! All multi-byte integers are little-endian.

use memmap2::Mmap;
use std::fs::File;
use std::io;
use std::path::Path;

const ZIM_MAGIC: u32 = 0x044D_495A; // "ZIM\x04"

/// Directory-entry mimetype sentinels.
const MIME_REDIRECT: u16 = 0xffff;
const MIME_LINKTARGET: u16 = 0xfffe;
const MIME_DELETED: u16 = 0xfffd;

/// Parsed ZIM header (the fields we care about).
#[derive(Debug, Clone)]
pub struct Header {
    pub major: u16,
    pub minor: u16,
    pub entry_count: u32,
    pub cluster_count: u32,
    pub url_ptr_pos: u64,
    pub title_ptr_pos: u64,
    pub cluster_ptr_pos: u64,
    pub mime_list_pos: u64,
    pub main_page: u32,
    pub checksum_pos: u64,
}

/// A located blob inside the ZIM: where its bytes live in the file.
#[derive(Debug, Clone)]
pub struct BlobLocation {
    /// Cluster index that holds the blob.
    pub cluster: u32,
    /// Blob index within the cluster.
    pub blob: u32,
    /// Compression type byte of the cluster (low nibble): 1/0 = none, 4 = xz, 5 = zstd.
    pub compression: u8,
    /// True if the cluster uses 64-bit offsets ("extended").
    pub extended: bool,
    /// Absolute file offset of the blob's raw bytes, *if* the cluster is uncompressed.
    /// For compressed clusters this is None and you must use `read_blob`.
    pub file_offset: Option<u64>,
    /// Length of the blob in bytes.
    pub length: u64,
}

pub struct Zim {
    mmap: Mmap,
    pub header: Header,
    pub mime_types: Vec<String>,
}

/// little-endian readers over a byte slice.
#[inline]
fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
#[inline]
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
#[inline]
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Read a NUL-terminated string starting at `off`; returns (string, offset-after-NUL).
fn read_cstr(b: &[u8], off: usize) -> (&str, usize) {
    let start = off;
    let mut i = off;
    while i < b.len() && b[i] != 0 {
        i += 1;
    }
    let s = std::str::from_utf8(&b[start..i]).unwrap_or("");
    (s, i + 1)
}

impl Zim {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Zim> {
        let file = File::open(path)?;
        // SAFETY: file is opened read-only; we treat the mapping as immutable.
        let mmap = unsafe { Mmap::map(&file)? };
        let b = &mmap[..];

        let magic = rd_u32(b, 0);
        if magic != ZIM_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("not a ZIM file (magic={magic:#010x})"),
            ));
        }
        let header = Header {
            major: rd_u16(b, 4),
            minor: rd_u16(b, 6),
            // 8..24 = uuid
            entry_count: rd_u32(b, 24),
            cluster_count: rd_u32(b, 28),
            url_ptr_pos: rd_u64(b, 32),
            title_ptr_pos: rd_u64(b, 40),
            cluster_ptr_pos: rd_u64(b, 48),
            mime_list_pos: rd_u64(b, 56),
            main_page: rd_u32(b, 64),
            // 68..72 = layout_page
            checksum_pos: rd_u64(b, 72),
        };

        // MIME type list: NUL-terminated strings, ended by an empty string.
        let mut mime_types = Vec::new();
        let mut off = header.mime_list_pos as usize;
        loop {
            let (s, next) = read_cstr(b, off);
            if s.is_empty() {
                break;
            }
            mime_types.push(s.to_string());
            off = next;
        }

        Ok(Zim {
            mmap,
            header,
            mime_types,
        })
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        &self.mmap[..]
    }

    /// Offset of the i-th directory entry (via the URL pointer list).
    fn dirent_offset(&self, i: u32) -> u64 {
        let b = self.bytes();
        let ptr = self.header.url_ptr_pos as usize + (i as usize) * 8;
        rd_u64(b, ptr)
    }

    /// Read the (namespace, url) sort key of the i-th entry without allocating the title.
    fn entry_key(&self, i: u32) -> (u8, &str) {
        let b = self.bytes();
        let off = self.dirent_offset(i) as usize;
        let namespace = b[off + 3];
        // url starts at offset 16 for content entries, 12 for redirect entries.
        let mime = rd_u16(b, off);
        let url_off = if mime == MIME_REDIRECT { off + 12 } else { off + 16 };
        let (url, _) = read_cstr(b, url_off);
        (namespace, url)
    }

    /// Binary search the URL pointer list for (namespace, url).
    /// Entries are sorted by (namespace, url) byte order.
    pub fn find_entry(&self, namespace: u8, url: &str) -> Option<u32> {
        let mut lo = 0i64;
        let mut hi = self.header.entry_count as i64 - 1;
        while lo <= hi {
            let mid = ((lo + hi) / 2) as u32;
            let (ns, u) = self.entry_key(mid);
            let ord = ns.cmp(&namespace).then_with(|| u.as_bytes().cmp(url.as_bytes()));
            match ord {
                std::cmp::Ordering::Less => lo = mid as i64 + 1,
                std::cmp::Ordering::Greater => hi = mid as i64 - 1,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Resolve a content entry index to (cluster, blob). Follows nothing; errors on redirect.
    fn entry_cluster_blob(&self, i: u32) -> io::Result<(u32, u32)> {
        let b = self.bytes();
        let off = self.dirent_offset(i) as usize;
        let mime = rd_u16(b, off);
        if mime == MIME_REDIRECT || mime == MIME_LINKTARGET || mime == MIME_DELETED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("entry {i} is not a content entry (mime={mime:#06x})"),
            ));
        }
        let cluster = rd_u32(b, off + 8);
        let blob = rd_u32(b, off + 12);
        Ok((cluster, blob))
    }

    /// Locate a blob's bytes (offset+len) given its cluster and blob index.
    pub fn locate_blob(&self, cluster: u32, blob: u32) -> io::Result<BlobLocation> {
        let b = self.bytes();
        let cptr = self.header.cluster_ptr_pos as usize + (cluster as usize) * 8;
        let cluster_off = rd_u64(b, cptr) as usize;

        let info = b[cluster_off];
        let compression = info & 0x0f;
        let extended = (info & 0x10) != 0;
        let data_start = cluster_off + 1; // offset array begins right after the info byte

        if compression == 0 || compression == 1 {
            // Uncompressed: offsets index directly into the file.
            let (start, end) = if extended {
                let o0 = rd_u64(b, data_start + (blob as usize) * 8);
                let o1 = rd_u64(b, data_start + (blob as usize + 1) * 8);
                (o0, o1)
            } else {
                let o0 = rd_u32(b, data_start + (blob as usize) * 4) as u64;
                let o1 = rd_u32(b, data_start + (blob as usize + 1) * 4) as u64;
                (o0, o1)
            };
            Ok(BlobLocation {
                cluster,
                blob,
                compression,
                extended,
                file_offset: Some(data_start as u64 + start),
                length: end - start,
            })
        } else {
            // Compressed cluster: caller must decompress. We still record metadata.
            Ok(BlobLocation {
                cluster,
                blob,
                compression,
                extended,
                file_offset: None,
                length: 0,
            })
        }
    }

    /// Convenience: locate the Xapian fulltext index blob (namespace 'X', "fulltext/xapian").
    /// Returns the blob location plus the entry index.
    pub fn find_fulltext_index(&self) -> io::Result<(u32, BlobLocation)> {
        // Modern libzim stores it at namespace 'X', url "fulltext/xapian".
        let idx = self
            .find_entry(b'X', "fulltext/xapian")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no X/fulltext/xapian entry"))?;
        let (cluster, blob) = self.entry_cluster_blob(idx)?;
        let loc = self.locate_blob(cluster, blob)?;
        Ok((idx, loc))
    }

    /// Borrow the raw mmap bytes (for the glass reader to read in-place).
    pub fn raw(&self) -> &[u8] {
        self.bytes()
    }

    // -- Article / subresource serving + title-prefix search ---------------
    //
    // These build on the container primitives above (`find_entry`,
    // `dirent_offset`, `locate_blob`) to turn the archive into a content
    // source for a viewer: resolve `(namespace, url)` to decompressed bytes +
    // MIME (following redirects), serve the main page, and binary-search the
    // by-title ordering for a prefix. Ported from the legacy `zim-reader`
    // crate; adapted to this crate's mmap-backed primitives + its existing
    // `zstd`/`flate2` decompressors (no extra decode stack).

    /// The MIME type string for a content entry's mime id, if known.
    pub fn mime_type(&self, id: u16) -> Option<&str> {
        self.mime_types.get(id as usize).map(String::as_str)
    }

    /// Parse the directory entry at `index` into a [`DirEntry`].
    fn dir_entry(&self, index: u32) -> Option<DirEntry> {
        if index >= self.header.entry_count {
            return None;
        }
        let b = self.bytes();
        let off = self.dirent_offset(index) as usize;
        let mime = rd_u16(b, off);
        let namespace = b[off + 3];
        if mime == MIME_REDIRECT {
            let redirect = rd_u32(b, off + 8);
            let (url, _) = read_cstr(b, off + 12);
            Some(DirEntry { mime, namespace, url: url.to_string(), content: None, redirect: Some(redirect) })
        } else if mime == MIME_LINKTARGET || mime == MIME_DELETED {
            let (url, _) = read_cstr(b, off + 8);
            Some(DirEntry { mime, namespace, url: url.to_string(), content: None, redirect: None })
        } else {
            let cluster = rd_u32(b, off + 8);
            let blob = rd_u32(b, off + 12);
            let (url, _) = read_cstr(b, off + 16);
            Some(DirEntry { mime, namespace, url: url.to_string(), content: Some((cluster, blob)), redirect: None })
        }
    }

    /// The display title of directory entry `index` (the `title` zstring that
    /// follows the `url`; empty title means "same as url" per the ZIM spec).
    fn dir_title(&self, index: u32) -> Option<String> {
        if index >= self.header.entry_count {
            return None;
        }
        let b = self.bytes();
        let off = self.dirent_offset(index) as usize;
        let mime = rd_u16(b, off);
        // url starts at 16 for content entries, 12 for redirects, 8 otherwise.
        let url_off = match mime {
            MIME_REDIRECT => off + 12,
            MIME_LINKTARGET | MIME_DELETED => off + 8,
            _ => off + 16,
        };
        let (url, after_url) = read_cstr(b, url_off);
        let (title, _) = read_cstr(b, after_url);
        Some(if title.is_empty() { url.to_string() } else { title.to_string() })
    }

    /// Follow a redirect chain (bounded) to the terminal entry.
    fn follow_redirects(&self, mut entry: DirEntry) -> Option<DirEntry> {
        for _ in 0..=MAX_REDIRECT_DEPTH {
            if entry.mime != MIME_REDIRECT {
                return Some(entry);
            }
            let target = entry.redirect?;
            entry = self.dir_entry(target)?;
        }
        None
    }

    /// Read a content entry's blob bytes (cluster, blob), decompressing the
    /// cluster if needed (uncompressed + zstd; xz/extended unsupported).
    fn read_blob(&self, cluster: u32, blob: u32) -> Option<Vec<u8>> {
        let loc = self.locate_blob(cluster, blob).ok()?;
        if loc.extended {
            // 64-bit-offset (>4 GiB) clusters are not handled.
            return None;
        }
        if let Some(off) = loc.file_offset {
            // Uncompressed: bytes index directly into the mmap.
            let b = self.bytes();
            let start = off as usize;
            let end = start + loc.length as usize;
            return b.get(start..end).map(<[u8]>::to_vec);
        }
        // Compressed cluster: decompress the whole body, then slice the blob
        // out of the in-memory offset table.
        match loc.compression {
            5 => self.read_blob_zstd(cluster, blob),
            _ => None, // 4 == xz/lzma2 (no C-free decoder); others unknown.
        }
    }

    /// Decompress a zstd cluster and slice out blob `blob` via its 32-bit
    /// offset table. The compressed body runs from just past the cluster's
    /// info byte to the next cluster's start (or the checksum position for the
    /// last cluster).
    fn read_blob_zstd(&self, cluster: u32, blob: u32) -> Option<Vec<u8>> {
        let b = self.bytes();
        let cptr = self.header.cluster_ptr_pos as usize + (cluster as usize) * 8;
        let cluster_off = rd_u64(b, cptr) as usize;
        let body_start = cluster_off + 1; // skip the info byte
        let body_end = if cluster + 1 < self.header.cluster_count {
            let nptr = self.header.cluster_ptr_pos as usize + ((cluster + 1) as usize) * 8;
            rd_u64(b, nptr) as usize
        } else {
            self.header.checksum_pos as usize
        };
        let body = b.get(body_start..body_end)?;
        let decompressed = zstd::decode_all(body).ok()?;

        // 32-bit blob offset table at the start of the decompressed body.
        let first = rd_u32(&decompressed, 0) as usize;
        let offset_count = first / 4;
        let n = blob as usize;
        if n + 1 >= offset_count {
            return None;
        }
        let start = rd_u32(&decompressed, n * 4) as usize;
        let end = rd_u32(&decompressed, (n + 1) * 4) as usize;
        decompressed.get(start..end).map(<[u8]>::to_vec)
    }

    /// Resolve `(namespace, url)` to a content entry's decompressed bytes,
    /// following redirects. `None` when the entry is missing or unreadable.
    pub fn article_by_url(&self, namespace: u8, url: &str) -> Option<Vec<u8>> {
        let idx = self.find_entry(namespace, url)?;
        let entry = self.follow_redirects(self.dir_entry(idx)?)?;
        let (cluster, blob) = entry.content?;
        self.read_blob(cluster, blob)
    }

    /// Resolve a subresource by `(namespace, url)`, following redirects,
    /// returning its bytes **and** the entry's MIME type string. This is what
    /// an in-archive resource provider needs to serve CSS / images: the body
    /// bytes plus the MIME so the renderer knows how to interpret them.
    /// Returns `None` when no entry matches (a missing subresource degrades
    /// gracefully rather than erroring). The MIME reported is that of the
    /// final (post-redirect) content entry.
    pub fn get_by_url(&self, namespace: u8, url: &str) -> Option<(Vec<u8>, String)> {
        let idx = self.find_entry(namespace, url)?;
        let entry = self.follow_redirects(self.dir_entry(idx)?)?;
        let mime = self.mime_type(entry.mime).unwrap_or("").to_string();
        let (cluster, blob) = entry.content?;
        let bytes = self.read_blob(cluster, blob)?;
        Some((bytes, mime))
    }

    /// Resolve the archive's declared main page to its decompressed bytes.
    pub fn main_page(&self) -> Option<Vec<u8>> {
        if self.header.main_page == u32::MAX {
            return None;
        }
        let entry = self.follow_redirects(self.dir_entry(self.header.main_page)?)?;
        let (cluster, blob) = entry.content?;
        self.read_blob(cluster, blob)
    }

    /// Case-insensitive **prefix** search over the archive's by-title
    /// ordering, returning up to `limit` `(title, url)` pairs for entries
    /// whose title starts with `prefix`.
    ///
    /// Backed by a binary search over the sorted title pointer list, so it
    /// touches only `O(log N + limit)` directory entries — it scales to
    /// archives with tens of millions of entries without iterating them all.
    /// Two backing layouts are supported transparently:
    ///
    /// - **Header pointer list** (older archives): the header
    ///   `title_ptr_pos` field points at a packed `u32` array of
    ///   `entry_count` directory-entry indices, sorted by `(namespace,
    ///   title)`.
    /// - **Listing entry** (spec ≥6): the header field is `u64::MAX`; the
    ///   array is instead the content of the `X/listing/titleOrdered/v1`
    ///   entry, which we load on demand.
    ///
    /// Redirect entries are followed so a hit always names a real content URL;
    /// entries with no resolvable content are skipped.
    pub fn title_search(&self, prefix: &str, limit: usize) -> Vec<(String, String)> {
        let index = self.title_index();
        let slots = index.len();
        if limit == 0 || slots == 0 {
            return Vec::new();
        }
        let needle = prefix.to_lowercase();
        let start = self.title_lower_bound(&index, &needle);

        let mut out = Vec::new();
        let mut i = start;
        while i < slots && out.len() < limit {
            let Some(dir_idx) = self.title_ptr(&index, i) else {
                i += 1;
                continue;
            };
            let Some(title) = self.dir_title(dir_idx) else {
                i += 1;
                continue;
            };
            let lower = title.to_lowercase();
            if !lower.starts_with(&needle) {
                // Sorted by (namespace, title): once we've moved strictly past
                // the prefix we're done. A namespace boundary can interleave a
                // non-matching title, so only stop once `lower > needle`.
                if lower.as_str() > needle.as_str() {
                    break;
                }
                i += 1;
                continue;
            }
            // Only surface entries that resolve (through redirects) to real
            // content, naming the underlying content url.
            if let Some(entry) = self.dir_entry(dir_idx)
                && let Some(target) = self.follow_redirects(entry)
                && target.content.is_some()
            {
                out.push((title, target.url));
            }
            i += 1;
        }
        out
    }

    /// First title slot whose lowercased title is `>= needle` (lower-bound
    /// binary search; one directory-entry read per probe).
    fn title_lower_bound(&self, index: &TitleIndex, needle: &str) -> u32 {
        let mut lo = 0u32;
        let mut hi = index.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            // On a read miss, treat the slot as `>= needle` so the search
            // converges; the forward scan re-validates each hit.
            let before = self
                .title_ptr(index, mid)
                .and_then(|d| self.dir_title(d))
                .is_some_and(|t| t.to_lowercase().as_str() < needle);
            if before {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Resolve where this archive's by-title ordering lives (header pointer
    /// list, or the modern `X/listing/titleOrdered/v1` listing entry).
    fn title_index(&self) -> TitleIndex {
        let title_ptr_pos = self.header.title_ptr_pos;
        let needed = self.header.entry_count as u64 * 4;
        if title_ptr_pos != u64::MAX
            && title_ptr_pos != 0
            && title_ptr_pos.saturating_add(needed) <= self.header.checksum_pos
        {
            return TitleIndex::HeaderPtr { pos: title_ptr_pos, count: self.header.entry_count };
        }
        // Modern layout: the title order is the content of the
        // `X/listing/titleOrdered/v1` entry — a packed array of u32 indices.
        if let Some(bytes) = self.article_by_url(b'X', "listing/titleOrdered/v1") {
            let ptrs: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            if !ptrs.is_empty() {
                return TitleIndex::Listing(ptrs);
            }
        }
        TitleIndex::None
    }

    /// The directory-entry index stored at title slot `i`, from whichever
    /// backing layout this archive uses. For the header layout this reads one
    /// `u32` from the mmap; for the listing layout it indexes the in-memory
    /// array.
    fn title_ptr(&self, index: &TitleIndex, i: u32) -> Option<u32> {
        match index {
            TitleIndex::HeaderPtr { pos, count } => {
                if i >= *count {
                    return None;
                }
                let b = self.bytes();
                let off = *pos as usize + (i as usize) * 4;
                b.get(off..off + 4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            }
            TitleIndex::Listing(p) => p.get(i as usize).copied(),
            TitleIndex::None => None,
        }
    }
}

/// A parsed directory entry (the fields the serving path cares about).
#[derive(Clone)]
struct DirEntry {
    mime: u16,
    #[allow(dead_code)]
    namespace: u8,
    url: String,
    /// For content entries: (cluster index, blob index).
    content: Option<(u32, u32)>,
    /// For redirect entries: the target directory-entry index.
    redirect: Option<u32>,
}

/// Max redirect hops before we declare a loop.
const MAX_REDIRECT_DEPTH: u32 = 16;

/// Where the archive's by-title ordering lives. The title ordering is an array
/// of directory-entry indices sorted by `(namespace, title)`; it backs the
/// binary-search title lookup. See [`Zim::title_search`].
enum TitleIndex {
    /// Header-resident packed u32 pointer array (read from the mmap on demand).
    HeaderPtr { pos: u64, count: u32 },
    /// In-memory pointer array decoded from the `titleOrdered` listing entry.
    Listing(Vec<u32>),
    /// No title ordering available — title search returns nothing.
    None,
}

impl TitleIndex {
    /// Number of slots in the title ordering.
    const fn len(&self) -> u32 {
        match self {
            TitleIndex::HeaderPtr { count, .. } => *count,
            TitleIndex::Listing(p) => p.len() as u32,
            TitleIndex::None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // --- Synthetic archive builder -------------------------------------
    //
    // Hand-assembles a minimal but spec-valid uncompressed ZIM (with a
    // header-resident title pointer list) so the serving + title-prefix
    // lookups can be exercised without a real multi-GiB archive. Each content
    // entry gets its own single-blob uncompressed cluster. The archive is
    // written to a temp file and opened via the normal mmap path.

    struct TestEntry {
        namespace: u8,
        url: &'static str,
        title: &'static str,
        mime: u16,
        body: &'static [u8],
    }

    struct TestRedirect {
        namespace: u8,
        url: &'static str,
        title: &'static str,
        /// Index (into the content-entry list) of the redirect target.
        target_content: usize,
    }

    fn push_zstring(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }

    /// Build an uncompressed single-blob cluster body for `data`.
    fn build_cluster(data: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        c.push(0u8); // info byte: comp=0 (uncompressed), not extended.
        let first = 8u32; // 2 offsets * 4 bytes.
        c.extend_from_slice(&first.to_le_bytes());
        c.extend_from_slice(&(first + data.len() as u32).to_le_bytes());
        c.extend_from_slice(data);
        c
    }

    /// Assemble a complete in-memory ZIM. The title pointer list is built
    /// sorted by `(namespace, title)` as the spec requires.
    fn build_archive(
        mime_types: &[&str],
        content: &[TestEntry],
        redirects: &[TestRedirect],
        main_page_content: usize,
    ) -> Vec<u8> {
        let n_content = content.len();
        let n_redir = redirects.len();
        let entry_count = (n_content + n_redir) as u32;
        let cluster_count = n_content as u32;

        let mut mime_blob = Vec::new();
        for m in mime_types {
            push_zstring(&mut mime_blob, m);
        }
        mime_blob.push(0); // empty terminator.

        // Logical entries sorted by (namespace, url) — the dir-entry index
        // space the URL pointer list (and binary search) walks.
        enum Logical<'a> {
            Content { e: &'a TestEntry, cluster: u32 },
            Redirect { r: &'a TestRedirect },
        }
        let mut logical: Vec<(u8, &str, Logical)> = Vec::new();
        for (ci, e) in content.iter().enumerate() {
            logical.push((e.namespace, e.url, Logical::Content { e, cluster: ci as u32 }));
        }
        for r in redirects {
            logical.push((r.namespace, r.url, Logical::Redirect { r }));
        }
        logical.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));

        let mut index_of_url: std::collections::HashMap<(u8, &str), u32> =
            std::collections::HashMap::new();
        for (i, (ns, url, _)) in logical.iter().enumerate() {
            index_of_url.insert((*ns, *url), i as u32);
        }

        let mut entry_bodies: Vec<Vec<u8>> = Vec::new();
        for (ns, _url, item) in &logical {
            let mut b = Vec::new();
            match item {
                Logical::Content { e, cluster } => {
                    b.extend_from_slice(&e.mime.to_le_bytes());
                    b.push(0); // parameter len
                    b.push(*ns);
                    b.extend_from_slice(&0u32.to_le_bytes()); // revision
                    b.extend_from_slice(&cluster.to_le_bytes());
                    b.extend_from_slice(&0u32.to_le_bytes()); // blob 0
                    push_zstring(&mut b, e.url);
                    push_zstring(&mut b, e.title);
                }
                Logical::Redirect { r } => {
                    let target = index_of_url[&(
                        content[r.target_content].namespace,
                        content[r.target_content].url,
                    )];
                    b.extend_from_slice(&MIME_REDIRECT.to_le_bytes());
                    b.push(0);
                    b.push(*ns);
                    b.extend_from_slice(&0u32.to_le_bytes()); // revision
                    b.extend_from_slice(&target.to_le_bytes()); // redirect idx
                    push_zstring(&mut b, r.url);
                    push_zstring(&mut b, r.title);
                }
            }
            entry_bodies.push(b);
        }

        let clusters: Vec<Vec<u8>> =
            content.iter().map(|e| build_cluster(e.body)).collect();

        let header_len = 80u64;
        let mime_pos = header_len;
        let url_ptr_pos = mime_pos + mime_blob.len() as u64;
        let title_ptr_pos = url_ptr_pos + entry_count as u64 * 8;
        let cluster_ptr_pos = title_ptr_pos + entry_count as u64 * 4;
        let entries_pos = cluster_ptr_pos + cluster_count as u64 * 8;

        let mut entry_offsets = Vec::new();
        let mut cur = entries_pos;
        for b in &entry_bodies {
            entry_offsets.push(cur);
            cur += b.len() as u64;
        }
        let mut cluster_offsets = Vec::new();
        for c in &clusters {
            cluster_offsets.push(cur);
            cur += c.len() as u64;
        }
        let checksum_pos = cur;

        // Title pointer list: url-sorted dir indices, ordered by (ns, title).
        let mut all: Vec<(u8, String, u32)> = Vec::new();
        for e in content {
            all.push((e.namespace, e.title.to_string(), index_of_url[&(e.namespace, e.url)]));
        }
        for r in redirects {
            all.push((r.namespace, r.title.to_string(), index_of_url[&(r.namespace, r.url)]));
        }
        all.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

        let main_page_idx = index_of_url[&(
            content[main_page_content].namespace,
            content[main_page_content].url,
        )];

        let mut out = vec![0u8; 80];
        out[0..4].copy_from_slice(&ZIM_MAGIC.to_le_bytes());
        out[24..28].copy_from_slice(&entry_count.to_le_bytes());
        out[28..32].copy_from_slice(&cluster_count.to_le_bytes());
        out[32..40].copy_from_slice(&url_ptr_pos.to_le_bytes());
        out[40..48].copy_from_slice(&title_ptr_pos.to_le_bytes());
        out[48..56].copy_from_slice(&cluster_ptr_pos.to_le_bytes());
        out[56..64].copy_from_slice(&mime_pos.to_le_bytes());
        out[64..68].copy_from_slice(&main_page_idx.to_le_bytes());
        out[68..72].copy_from_slice(&0xffff_ffffu32.to_le_bytes()); // layout page
        out[72..80].copy_from_slice(&checksum_pos.to_le_bytes());

        out.extend_from_slice(&mime_blob);
        for off in &entry_offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        for (_, _, idx) in &all {
            out.extend_from_slice(&idx.to_le_bytes());
        }
        for off in &cluster_offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        for b in &entry_bodies {
            out.extend_from_slice(b);
        }
        for c in &clusters {
            out.extend_from_slice(c);
        }
        out.extend_from_slice(&[0u8; 16]); // checksum
        out
    }

    /// Write `bytes` to a temp file and open it as a `Zim` (exercises the real
    /// mmap path). Returns the opened archive (the temp file is kept alive for
    /// the test's duration via the returned guard).
    fn open_bytes(bytes: &[u8]) -> (Zim, tempfile::NamedTempFile) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        let z = Zim::open(f.path()).unwrap();
        (z, f)
    }

    fn sample_archive() -> (Zim, tempfile::NamedTempFile) {
        let content = [
            TestEntry { namespace: b'C', url: "Apple", title: "Apple", mime: 0, body: b"<html>apple</html>" },
            TestEntry { namespace: b'C', url: "Apricot", title: "Apricot", mime: 0, body: b"<html>apricot</html>" },
            TestEntry { namespace: b'C', url: "Banana", title: "Banana", mime: 0, body: b"<html>banana</html>" },
            TestEntry { namespace: b'C', url: "style.css", title: "style.css", mime: 1, body: b"body{color:red}" },
        ];
        let redirects = [TestRedirect {
            namespace: b'C',
            url: "Apple_fruit",
            title: "Apple (fruit)",
            target_content: 0,
        }];
        let bytes = build_archive(&["text/html", "text/css"], &content, &redirects, 0);
        open_bytes(&bytes)
    }

    #[test]
    fn synthetic_archive_main_page() {
        let (a, _f) = sample_archive();
        assert_eq!(a.header.entry_count, 5);
        assert_eq!(a.main_page().unwrap(), b"<html>apple</html>");
    }

    #[test]
    fn get_by_url_resolves_content_and_mime() {
        let (a, _f) = sample_archive();
        let (bytes, mime) = a.get_by_url(b'C', "style.css").unwrap();
        assert_eq!(bytes, b"body{color:red}");
        assert_eq!(mime, "text/css");

        let (html, mime2) = a.get_by_url(b'C', "Banana").unwrap();
        assert_eq!(html, b"<html>banana</html>");
        assert_eq!(mime2, "text/html");

        assert!(a.get_by_url(b'C', "Nonexistent").is_none());
        assert!(a.get_by_url(b'I', "style.css").is_none());
    }

    #[test]
    fn get_by_url_follows_redirect() {
        let (a, _f) = sample_archive();
        let (bytes, mime) = a.get_by_url(b'C', "Apple_fruit").unwrap();
        assert_eq!(bytes, b"<html>apple</html>");
        assert_eq!(mime, "text/html");
    }

    #[test]
    fn article_by_url_follows_redirect() {
        let (a, _f) = sample_archive();
        assert_eq!(a.article_by_url(b'C', "Apple_fruit").unwrap(), b"<html>apple</html>");
        assert_eq!(a.article_by_url(b'C', "Apricot").unwrap(), b"<html>apricot</html>");
        assert!(a.article_by_url(b'C', "Nope").is_none());
    }

    #[test]
    fn title_search_prefix_and_order() {
        let (a, _f) = sample_archive();
        let hits = a.title_search("Ap", 10);
        let titles: Vec<&str> = hits.iter().map(|(t, _)| t.as_str()).collect();
        assert!(titles.contains(&"Apple"), "{titles:?}");
        assert!(titles.contains(&"Apricot"), "{titles:?}");
        // Redirect surfaces its underlying content url ("Apple").
        let redirect_hit = hits.iter().find(|(t, _)| t == "Apple (fruit)");
        assert_eq!(redirect_hit.map(|(_, u)| u.as_str()), Some("Apple"));

        // Sorted (title-index) order.
        let sorted: Vec<String> = titles.iter().map(|t| t.to_lowercase()).collect();
        let mut expected = sorted.clone();
        expected.sort();
        assert_eq!(sorted, expected, "title_search must preserve sorted order");

        // Case-insensitive.
        assert!(!a.title_search("ap", 10).is_empty());
        assert!(!a.title_search("BAN", 10).is_empty());
    }

    #[test]
    fn title_search_limit_and_miss() {
        let (a, _f) = sample_archive();
        assert_eq!(a.title_search("Ap", 1).len(), 1);
        assert!(a.title_search("Zebra", 10).is_empty());
        assert!(a.title_search("Ap", 0).is_empty());
    }
}
