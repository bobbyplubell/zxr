//! `zxr` — a pure-Rust reader for [openzim](https://wiki.openzim.org/wiki/ZIM_file_format)
//! `.zim` archives with **full-text body search** over the embedded Xapian
//! "glass" index (BM25 ranking).
//!
//! Two complementary capabilities:
//!
//! - **Container + article serving** ([`zim::Zim`]): open an archive, resolve
//!   articles / subresources by `(namespace, url)` following redirects, serve
//!   their decompressed bytes + MIME, and binary-search the title index for a
//!   prefix (instant typeahead). See [`zim`].
//! - **Full-text search** ([`search::Searcher`]): BM25-ranked body search over
//!   the embedded glass index ([`glass`]), located via
//!   [`zim::Zim::find_fulltext_index`].
//!
//! The crate is a standalone library (a thin CLI lives in `main.rs`); it has no
//! dependency on any host application.

pub mod glass;
pub mod search;
pub mod zim;
