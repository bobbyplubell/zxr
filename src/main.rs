use zxr::{glass, search, zim};

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
    let mut extract_title: Option<String> = None;
    let mut out_dir = "wiki-extract".to_string();
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
            // `--extract <Title> [--out DIR]`: dump one article's HTML + its
            // referenced CSS to DIR (default ./wiki-extract), rewriting the
            // stylesheet hrefs to the local files. A self-contained offline
            // sample for testing an HTML/CSS renderer.
            "--extract" => {
                i += 1;
                extract_title = args.get(i).cloned();
            }
            "--out" => {
                i += 1;
                if i < args.len() {
                    out_dir = args[i].clone();
                }
            }
            "--dump-term" => {
                i += 1;
                let term = args.get(i).cloned().unwrap_or_default();
                return dump_term(&zim_path, &term);
            }
            other => query_parts.push(other.to_string()),
        }
        i += 1;
    }

    if let Some(title) = extract_title {
        return cmd_extract(&zim_path, &title, &out_dir);
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

/// Extract one article's HTML + its referenced CSS into `out_dir`, rewriting
/// the `<link rel=stylesheet>` hrefs to the local files. A self-contained
/// offline sample for testing an HTML/CSS renderer.
fn cmd_extract(zim_path: &str, title: &str, out_dir: &str) -> std::io::Result<()> {
    use std::fs;
    use std::path::Path;

    let z = zim::Zim::open(zim_path)?;

    // Resolve the title → content url via the title index (exact match
    // preferred, else the first prefix hit).
    let hits = z.title_search(title, 50);
    let url = hits
        .iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(title))
        .or_else(|| hits.first())
        .map(|(_, u)| u.clone())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no article matching '{title}'"),
            )
        })?;

    let html_bytes = z
        .article_by_url(b'C', &url)
        .or_else(|| z.article_by_url(b'A', &url))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("article not found: {url}"),
            )
        })?;
    let mut html = String::from_utf8_lossy(&html_bytes).into_owned();

    let out = Path::new(out_dir);
    fs::create_dir_all(out)?;

    // Pull each referenced stylesheet out of the archive, write it locally,
    // and rewrite its href in the HTML to point at the local file.
    let mut css_n = 0;
    for href in stylesheet_hrefs(&html) {
        let (ns, entry) = resolve_href(&href);
        if let Some((bytes, _mime)) = fetch_entry(&z, ns, &entry) {
            let fname = format!("style-{css_n}.css");
            fs::write(out.join(&fname), &bytes)?;
            html = html.replace(&href, &fname);
            eprintln!("  css: {fname}  ({} B)  <- {href}", bytes.len());
            css_n += 1;
        } else {
            eprintln!("  css: UNRESOLVED  <- {href}");
        }
    }

    fs::write(out.join("article.html"), html.as_bytes())?;
    eprintln!(
        "wrote {out_dir}/article.html ({} B) + {css_n} stylesheet(s)  [article url: {url}]",
        html.len()
    );
    Ok(())
}

/// `href` of every `<link ... rel="stylesheet" ...>` in the HTML (crude tag
/// scan — fine for the well-formed markup ZIM articles carry).
fn stylesheet_hrefs(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<link") {
        let start = pos + rel;
        let end = lower[start..].find('>').map_or(html.len(), |e| start + e);
        if lower[start..end].contains("stylesheet")
            && let Some(h) = attr_value(&html[start..end], "href")
        {
            out.push(h);
        }
        pos = end;
    }
    out
}

/// Pull a quoted (or bare) attribute value out of a start tag.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{attr}=");
    let at = lower.find(&key)? + key.len();
    let rest = &tag[at..];
    match rest.chars().next()? {
        q @ ('"' | '\'') => {
            let body = &rest[1..];
            body.find(q).map(|e| body[..e].to_string())
        }
        _ => {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        }
    }
}

/// Resolve a relative ZIM href (`../-/style.css`, `./_mw_/x.css`, …) into a
/// `(namespace, entry-url)` pair. A leading `../<NS>/` names the ZIM
/// namespace; everything else defaults to content (`C`).
fn resolve_href(href: &str) -> (u8, String) {
    let h = href.split(['#', '?']).next().unwrap_or(href).trim();
    if let Some(rest) = h.strip_prefix("../") {
        if let Some((ns, entry)) = rest.split_once('/')
            && ns.len() == 1
        {
            return (ns.as_bytes()[0], entry.to_string());
        }
        return (b'C', rest.to_string());
    }
    (b'C', h.trim_start_matches("./").trim_start_matches('/').to_string())
}

/// Fetch an entry, trying the resolved namespace then the common ZIM
/// namespaces so both legacy (`A`/`-`/`I`) and modern (`C`/`M`) archives work.
fn fetch_entry(z: &zim::Zim, ns: u8, url: &str) -> Option<(Vec<u8>, String)> {
    [ns, b'C', b'-', b'I', b'M', b'A']
        .into_iter()
        .find_map(|n| z.get_by_url(n, url))
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
