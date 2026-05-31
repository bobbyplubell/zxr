//! BM25 ranked search over a glass database embedded in a ZIM file.
//!
//! Implements Xapian's default BM25 weighting (k1=1, k2=0, k3=1, b=0.5,
//! min_normlen=0.5), matching weight/bm25weight.cc, with OP_OR query semantics.

use crate::glass::{self, Table, Version};
use rust_stemmers::{Algorithm, Stemmer};
use std::collections::HashMap;
use std::io;

// Xapian BM25 default parameters.
const K1: f64 = 1.0;
const K3: f64 = 1.0;
const B: f64 = 0.5;
const MIN_NORMLEN: f64 = 0.5;

pub struct Searcher<'a> {
    raw: &'a [u8],
    base: usize,
    pub version: Version,
    avgdl: f64,
    /// doclen indexed by docid (index 0 unused). Built lazily.
    doclens: Vec<u32>,
}

pub struct Hit {
    pub docid: u64,
    pub score: f64,
    pub path: String,
}

impl<'a> Searcher<'a> {
    /// Build a searcher; decodes the doclen list up front (needed for BM25).
    pub fn new(raw: &'a [u8], base: usize, version: Version) -> io::Result<Searcher<'a>> {
        let avgdl = if version.doccount > 0 {
            version.total_doclen as f64 / version.doccount as f64
        } else {
            0.0
        };
        let postlist = Table::new(raw, base, &version.roots[glass::POSTLIST], "postlist");

        // Decode the doclen list (term == "") into a flat vector.
        let mut doclens = vec![0u32; (version.last_docid + 1) as usize];
        if let Some(pl) = postlist.read_postlist(b"")? {
            for (did, dl) in pl.postings {
                if (did as usize) < doclens.len() {
                    doclens[did as usize] = dl;
                }
            }
        }

        Ok(Searcher { raw, base, version, avgdl, doclens })
    }

    fn postlist_table(&self) -> Table<'a> {
        Table::new(self.raw, self.base, &self.version.roots[glass::POSTLIST], "postlist")
    }
    fn docdata_table(&self) -> Table<'a> {
        Table::new(self.raw, self.base, &self.version.roots[glass::DOCDATA], "docdata")
    }

    /// BM25 termweight (idf component, already multiplied by (k1+1) and the k3/wqf factor).
    fn termweight(&self, df: u64, wqf: u32) -> f64 {
        let n = self.version.doccount as f64;
        let mut tw = (n - df as f64 + 0.5) / (df as f64 + 0.5);
        if tw < 2.0 {
            tw = tw * 0.5 + 1.0;
        }
        let mut w = tw.ln();
        // k3 factor for within-query frequency.
        let wqf = wqf as f64;
        w *= (K3 + 1.0) * wqf / (K3 + wqf);
        w *= K1 + 1.0;
        w
    }

    /// Run an OR query, returning the top `k` hits by BM25 score.
    pub fn search(&self, query: &str, k: usize) -> io::Result<Vec<Hit>> {
        // Tokenise + stem (the index stores Porter2/English stems, no prefix).
        let stemmer = Stemmer::create(Algorithm::English);
        let mut wqf: HashMap<String, u32> = HashMap::new();
        for tok in tokenize(query) {
            let stem = stemmer.stem(&tok).into_owned();
            *wqf.entry(stem).or_insert(0) += 1;
        }

        let postlist = self.postlist_table();
        let mut scores: HashMap<u64, f64> = HashMap::new();

        for (term, qf) in &wqf {
            let pl = match postlist.read_postlist(term.as_bytes())? {
                Some(pl) if pl.termfreq > 0 => pl,
                _ => continue,
            };
            let tw = self.termweight(pl.termfreq, *qf);
            for (did, wdf) in pl.postings {
                let dl = *self.doclens.get(did as usize).unwrap_or(&0) as f64;
                let normlen = (dl / self.avgdl).max(MIN_NORMLEN);
                let denom = K1 * (normlen * B + (1.0 - B)) + wdf as f64;
                let contrib = tw * (wdf as f64 / denom);
                *scores.entry(did).or_insert(0.0) += contrib;
            }
        }

        // Top-k by score (descending), tie-break by docid ascending.
        let mut hits: Vec<(u64, f64)> = scores.into_iter().collect();
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        hits.truncate(k);

        // Resolve paths from docdata.
        let docdata = self.docdata_table();
        let mut out = Vec::with_capacity(hits.len());
        for (did, score) in hits {
            let path = match docdata.get_exact_entry(&glass::pack_sortable(did))? {
                Some(tag) => String::from_utf8_lossy(&tag).into_owned(),
                None => String::from("<no docdata>"),
            };
            out.push(Hit { docid: did, score, path });
        }
        Ok(out)
    }
}

/// Lowercase + split on non-alphanumeric. Mirrors how lowercase word terms were indexed.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}
