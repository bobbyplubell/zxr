# zxr — native-Rust read-only Xapian (glass) reader for ZIM full-text search

Searches the Xapian full-text index embedded inside a Kiwix **ZIM** file (e.g. a
Wikipedia dump) with **no C/C++ dependency** — a from-scratch Rust reimplementation
of the read path of Xapian's *glass* backend plus a BM25 matcher.

```
cargo build --release
./target/release/zxr --zim /path/to/wikipedia.zim albert einstein
./target/release/zxr --zim /path/to/wikipedia.zim --info        # index stats
```

## What it does

1. **Parses the ZIM container** (`zim.rs`) — v6 header, MIME list, URL pointer list
   (binary search), directory entries, cluster pointer list — to locate the
   `X/fulltext/xapian` blob. For Wikipedia ZIMs that blob is stored **uncompressed**,
   so the glass DB is read in place via `mmap` at the blob's file offset (no 8 GB copy).
2. **Reads the single-file glass database** (`glass.rs`):
   - version header (magic `\x0f\x0d"Xapian Glass"`, format 1134 = 2016-03-14), UUID,
     revision, per-table `RootInfo`, and DB stats (doccount, total_doclen, bounds);
   - the glass **B-tree** read path: block navigation, binary-chop `find` down branch
     levels to a leaf, multi-chunk tag stitching, raw-DEFLATE decompression of tags;
   - **postlist** decoding: per-term `(docid, wdf)` postings across chunks, plus the
     doclen postlist (term `""`).
3. **Ranks** (`search.rs`): query words are lowercased and **Porter2/English-stemmed**
   (the index stores stems, no prefix — `delve -s english` agrees), then scored with
   Xapian's default **BM25** (k1=1, k2=0, k3=1, b=0.5, min_normlen=0.5), OR semantics,
   top-K, resolving each hit's article path from the `docdata` table.

   Note: this is plain BM25-OR. Kiwix itself layers on partial-match, phrase and
   title-boosting; matching that exactly is future work.

## Format notes (reverse-engineered from xapian-core)

| Encoding | Where | Detail |
|---|---|---|
| Block header integers | B-tree blocks | **big-endian** (`wordaccess.h` byteswaps on LE). `REVISION` u32@0, `LEVEL` u8@4, `MAX_FREE` u16@5, `TOTAL_FREE` u16@7, `DIR_END` u16@9, dir@11; freelist level=254 |
| Directory pointers | B-tree blocks | 2-byte big-endian offsets |
| `unpack_uint` | version file, tags | LSB-first base-128 varint (LEB128) |
| `pack_uint_preserving_sort` | continuation/docdata keys | length-tagged big-endian sortable int |
| bool (e.g. is_last_chunk) | postlist chunks | **ASCII `'0'`/`'1'`** (0x30/0x31), not 0/1 |
| Tag compression | docdata etc. | raw DEFLATE (`inflateInit2(-15)`), `compress_min` byte threshold |
| Term key | postlist | the term bytes (NUL-escaped); continuation = term + `\0` + sortable(first_did); doclen list key = `\x00\xe0` |

This ZIM's index has only the **postlist** and **docdata** tables (no termlist /
positions / spelling / synonym) — search-only, no phrase queries. Document values:
slot 0 = title, 1 = wordcount, 2 = geo.position; docdata = article `fullPath`.

## Validation against xapian-delve 1.4.30

Verified **byte-identical** to the C++ oracle on the live Wikipedia ZIM
(8,452,797 docs):

- header/stats: UUID, doccount, avgdl 973.131, doclen bounds [1, 174066], last_docid, rev;
- per-term `termfreq` / `collfreq` / `wdf_max`;
- **full posting lists** — `docid + wdf + doclen` for every posting (einstein: 19,794;
  water: 741,283 spanning hundreds of chunks) matched exactly;
- docdata paths.

## Layout

- `zim.rs` — ZIM container parser
- `glass.rs` — glass version header, B-tree read path, postlist decoder, varint/sortable codecs
- `search.rs` — BM25 matcher
- `main.rs` — CLI (`--zim`, `--info`, `--dump-term`, search)
