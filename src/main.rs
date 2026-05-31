mod glass;
mod search;
mod zim;

use std::env;
use std::time::Instant;

const DEFAULT_ZIM: &str = "../test-vault/wikipedia_en_all_nopic_2026-03.zim";

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    // Usage:
    //   zxr [--zim PATH] <query words...>
    //   zxr [--zim PATH] --info
    let mut zim_path = DEFAULT_ZIM.to_string();
    let mut info_only = false;
    let mut query_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--zim" => {
                i += 1;
                if i < args.len() {
                    zim_path = args[i].clone();
                }
            }
            "--info" => info_only = true,
            "--dump-term" => {
                i += 1;
                let term = args.get(i).cloned().unwrap_or_default();
                return dump_term(&zim_path, &term);
            }
            other => query_parts.push(other.to_string()),
        }
        i += 1;
    }
    let query = query_parts.join(" ");

    let t0 = Instant::now();
    let z = zim::Zim::open(&zim_path)?;
    let (_idx, loc) = z.find_fulltext_index()?;
    let off = loc
        .file_offset
        .expect("fulltext index is in a compressed cluster (unsupported)");
    let base = off as usize;
    let data = &z.raw()[base..(base + loc.length as usize).min(z.raw().len())];
    let version = glass::Version::parse(data)?;

    if info_only || query.is_empty() {
        print_info(&z, &loc, &version);
        if query.is_empty() && !info_only {
            eprintln!("\n(no query given — pass words to search, e.g. `zxr albert einstein`)");
        }
        return Ok(());
    }

    eprintln!(
        "index: {:.1} MiB glass db, {} docs, avgdl {:.1}",
        loc.length as f64 / (1024.0 * 1024.0),
        version.doccount,
        version.total_doclen as f64 / version.doccount.max(1) as f64,
    );
    let searcher = search::Searcher::new(z.raw(), base, version)?;
    eprintln!("loaded doclens in {:.2}s", t0.elapsed().as_secs_f64());

    let ts = Instant::now();
    let hits = searcher.search(&query, 10)?;
    let dt = ts.elapsed();

    println!("\nresults for: \"{query}\"  ({} hits, {:.0} ms)\n", hits.len(), dt.as_secs_f64() * 1000.0);
    for (rank, h) in hits.iter().enumerate() {
        let title = h.path.strip_prefix("C/").unwrap_or(&h.path).replace('_', " ");
        println!("  {:>2}. [{:.4}] {}", rank + 1, h.score, title);
        println!("      docid={} path={}", h.docid, h.path);
    }
    Ok(())
}

/// Dump all docids for a term, one per line (for diffing against xapian-delve).
fn dump_term(zim_path: &str, term: &str) -> std::io::Result<()> {
    let z = zim::Zim::open(zim_path)?;
    let (_idx, loc) = z.find_fulltext_index()?;
    let base = loc.file_offset.unwrap() as usize;
    let data = &z.raw()[base..(base + loc.length as usize).min(z.raw().len())];
    let version = glass::Version::parse(data)?;
    let pl = glass::Table::new(z.raw(), base, &version.roots[glass::POSTLIST], "postlist");
    // Decode the doclen list so we can emit "docid wdf doclen" like `delve -v -1`.
    let mut doclens = vec![0u32; (version.last_docid + 1) as usize];
    if let Some(dl) = pl.read_postlist(b"")? {
        for (did, l) in dl.postings {
            if (did as usize) < doclens.len() {
                doclens[did as usize] = l;
            }
        }
    }
    if let Some(p) = pl.read_postlist(term.as_bytes())? {
        eprintln!("termfreq={} collfreq={} decoded={}", p.termfreq, p.collfreq, p.postings.len());
        let mut out = String::new();
        for (did, wdf) in &p.postings {
            out.push_str(&format!("{} {} {}\n", did, wdf, doclens[*did as usize]));
        }
        print!("{out}");
    }
    Ok(())
}

fn print_info(z: &zim::Zim, loc: &zim::BlobLocation, v: &glass::Version) {
    println!("== ZIM ==");
    println!("  version {}.{}, {} entries, {} clusters", z.header.major, z.header.minor, z.header.entry_count, z.header.cluster_count);
    println!("  fulltext index: cluster {} blob {}, {} MiB, compression {}",
        loc.cluster, loc.blob, loc.length / (1024 * 1024), loc.compression);
    println!("== glass ==");
    println!("  format {}, rev {}, doccount {}, last_docid {}", v.format_version, v.rev, v.doccount, v.last_docid);
    println!("  total_doclen {}, avgdl {:.2}", v.total_doclen, v.total_doclen as f64 / v.doccount.max(1) as f64);
    for (i, r) in v.roots.iter().enumerate() {
        if r.num_entries > 0 {
            println!("  table {:8} root={} level={} entries={}", glass::TABLE_NAMES[i], r.root, r.level, r.num_entries);
        }
    }
}
