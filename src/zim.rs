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
}
