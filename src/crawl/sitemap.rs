//! Sitemap + robots.txt discovery engine.
//!
//! Phase-1 of the two-phase crawl surface: read the site's
//! published URL inventory BEFORE crawling. A 10K-page site
//! costs 2 requests here (robots.txt + sitemap index) instead of
//! 10K. Streaming byte-scanners — no DOM, tolerant of the
//! malformed XML sitemaps actually ship.

use super::PageFetcher;

/// robots.txt: sitemap directives + `*` Disallow rules.
/// We obey robots by default — it is both polite AND the
/// fastest signal of what the site WANTS crawled (`Allow`
/// paths + sitemap lists).
#[derive(Default, Debug, Clone)]
pub struct Robots {
    pub sitemaps: Vec<String>,
    /// `Disallow:` prefixes for agent `*`. Longest-match wins.
    pub disallow: Vec<String>,
    /// `Allow:` prefixes (override Disallow on longest match).
    pub allow: Vec<String>,
    /// Site-declared request delay seconds, if any.
    pub crawl_delay: Option<f64>,
}

impl Robots {
    pub fn parse(body: &str, base_host: &str) -> Self {
        let mut r = Robots::default();
        let mut in_star_group = false;
        let mut seen_any_group = false;
        for raw in body.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let k = k.trim().to_lowercase();
            let v = v.trim();
            match k.as_str() {
                "user-agent" => {
                    // A new group starts. Track only `*` groups
                    // (and exact us if we had a UA string; we act
                    // as a generic crawler).
                    in_star_group = v == "*" || v.to_lowercase().contains("donsetch");
                    seen_any_group = true;
                }
                "disallow" if in_star_group && !v.is_empty() => {
                    r.disallow.push(v.to_string());
                }
                "allow" if in_star_group && !v.is_empty() => {
                    r.allow.push(v.to_string());
                }
                "crawl-delay" if in_star_group => {
                    r.crawl_delay = v.parse::<f64>().ok();
                }
                "sitemap" => {
                    // Sitemap directives apply outside groups.
                    if v.starts_with("http") {
                        r.sitemaps.push(v.to_string());
                    } else if v.starts_with('/') {
                        r.sitemaps.push(format!("https://{base_host}{v}"));
                    }
                }
                _ => {}
            }
        }
        // Some sites emit rules with NO user-agent group; treat as
        // implicit `*` when we saw no groups at all.
        if !seen_any_group {
            for raw in body.lines() {
                let line = raw.split('#').next().unwrap_or("").trim();
                let Some((k, v)) = line.split_once(':') else {
                    continue;
                };
                if k.trim().eq_ignore_ascii_case("disallow") && !v.trim().is_empty() {
                    r.disallow.push(v.trim().to_string());
                }
            }
        }
        r
    }

    /// Longest-match rule evaluation. Allow beats Disallow at
    /// equal length (RFC 9309).
    pub fn allowed(&self, path: &str) -> bool {
        let mut best_dis = 0usize;
        let mut best_allow = 0usize;
        for d in &self.disallow {
            if path.starts_with(d.as_str()) && d.len() > best_dis {
                best_dis = d.len();
            }
        }
        for a in &self.allow {
            if path.starts_with(a.as_str()) && a.len() > best_allow {
                best_allow = a.len();
            }
        }
        best_allow >= best_dis
    }
}

/// One sitemap URL entry.
#[derive(Debug, Clone)]
pub struct SitemapEntry {
    pub loc: String,
    pub lastmod: Option<String>,
    /// Sitemap-declared priority (0.0–1.0). Used to seed
    /// the frontier relevance score.
    pub priority: Option<f32>,
    /// True when the entry came from a `<sitemap>` index block
    /// (a CHILD sitemap to fetch), false for `<url>` (a page).
    pub is_index: bool,
}

/// Streaming sitemap parser: finds `<url>`/`<sitemap>` blocks
/// and lifts `<loc>` + `<lastmod>` from each. Tolerates stray
/// namespace prefixes and junk between tags.
pub fn parse_sitemap(xml: &str, out: &mut Vec<SitemapEntry>, cap: usize) {
    let b = xml.as_bytes();
    let mut pos = 0usize;
    while out.len() < cap {
        let Some((tag_off, close)) = next_block_open(b, pos) else {
            break;
        };
        let Some(end) = find_from(b, close.as_bytes(), tag_off) else {
            break;
        };
        let block = &xml[tag_off..end];
        if let Some(loc) = extract_tag(block, "loc") {
            let lastmod = extract_tag(block, "lastmod");
            let priority = extract_tag(block, "priority").and_then(|s| s.parse::<f32>().ok());
            let is_index = close == "</sitemap>";
            out.push(SitemapEntry {
                loc,
                lastmod,
                priority,
                is_index,
            });
        }
        pos = end + close.len();
    }
}

/// Find the next `<url>` or `<sitemap>` open tag — skipping the
/// `<urlset>`/`<sitemapindex>` containers. Returns
/// (content_offset, close_tag). Tag-name based: immune to
/// prefix collisions like `<urlset>` matching `<url`.
fn next_block_open(b: &[u8], from: usize) -> Option<(usize, String)> {
    let mut pos = from;
    loop {
        let i = find_from(b, b"<", pos)?;
        let name_start = i + 1;
        if name_start >= b.len() {
            return None;
        }
        let name_end = b[name_start..]
            .iter()
            .position(|&c| !c.is_ascii_alphabetic())
            .map(|p| p + name_start)?;
        let name = &b[name_start..name_end];
        let (is_block, close) = match name {
            b"url" => (true, "</url>"),
            b"sitemap" => (true, "</sitemap>"),
            _ => (false, ""),
        };
        let gt = find_from(b, b">", name_end)? + 1;
        pos = gt;
        if is_block {
            return Some((gt, close.to_string()));
        }
    }
}

fn find_from(b: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Extract the text of the first `<tag>` in `block`, trimming.
fn extract_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let s = block.find(&open)?;
    let after = block[s + open.len()..].find('>')? + s + open.len() + 1;
    let e = block[after..].find(&close)? + after;
    let v = block[after..e].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Decompress a gzip'd sitemap body (many sites ship .xml.gz).
pub fn maybe_gunzip(body: &[u8]) -> Vec<u8> {
    if body.len() > 2 && body[0] == 0x1f && body[1] == 0x8b {
        use std::io::Read;
        // Same 64 MiB cap as the fetch decompressor — a malicious
        // .xml.gz sitemap must not OOM the daemon via unbounded
        // decompression.
        const MAX_SITEMAP_DECOMPRESSED: usize = 64 << 20;
        let mut out = Vec::new();
        let dec = flate2::read::GzDecoder::new(body);
        let mut limited = dec.take((MAX_SITEMAP_DECOMPRESSED + 1) as u64);
        if limited.read_to_end(&mut out).is_ok() {
            if out.len() > MAX_SITEMAP_DECOMPRESSED {
                // Bomb: return the raw bytes; XML parse of gzip
                // garbage fails honestly downstream.
                return body.to_vec();
            }
            return out;
        }
    }
    body.to_vec()
}

/// Sitemap discovery: robots.txt first for directives, then
/// the conventional /sitemap.xml fallback. Returns parsed
/// entries (map phase) and the robots rules. Runs through the
/// injected PageFetcher (real or mock).
pub async fn discover(fetch: &PageFetcher, host: &str, cap: usize) -> (Robots, Vec<SitemapEntry>) {
    let robots_url = format!("https://{host}/robots.txt");
    let mut robots = Robots::default();
    let page = fetch(robots_url, "direct".to_string(), None).await;
    if page.status == 200 {
        robots = Robots::parse(&String::from_utf8_lossy(&maybe_gunzip(&page.body)), host);
    }

    // Sitemap candidates: robots directives first.
    let mut queue: Vec<String> = robots.sitemaps.clone();
    if queue.is_empty() {
        // Multiple conventional locations — many sites use non-standard
        // sitemap paths (WordPress /wp-sitemap.xml, Yoast /sitemap_index.xml).
        queue.extend([
            format!("https://{host}/sitemap.xml"),
            format!("https://{host}/sitemap_index.xml"),
            format!("https://{host}/sitemap-index.xml"),
            format!("https://{host}/wp-sitemap.xml"),
            format!("https://{host}/sitemaps.xml"),
            format!("https://{host}/sitemap.txt"),
        ]);
    }

    let mut entries = Vec::new();
    let mut fetched = 0usize;
    let mut first = true;
    while !queue.is_empty() && entries.len() < cap && fetched < 32 {
        if first {
            // Wave 1: the highest-confidence candidate alone.
            // Robots-declared sitemaps and /sitemap.xml cover the
            // large majority of sites — one request, exactly like
            // the serial v1 loop's best case.
            first = false;
            let loc = queue.remove(0);
            fetched += 1;
            if let Some(text) = fetch_sitemap_text(fetch, &loc).await {
                absorb(text, &mut queue, &mut entries);
            }
            continue;
        }
        // Wave 2+: remaining candidates IN PARALLEL (bounded 8).
        // Sitemap-less sites used to pay every candidate as a
        // serial 404 round-trip (~1-3s of pure latency); now the
        // whole miss-set resolves in one round. Child sitemap
        // indexes discovered later are also waved — they are
        // metadata probes, not page fetches, and the governor's
        // page-fetch pacing is untouched.
        let wave: Vec<String> = queue.drain(..queue.len().min(8)).collect();
        fetched += wave.len();
        let futs = wave.iter().map(|loc| fetch_sitemap_text(fetch, loc));
        let texts = futures_util::future::join_all(futs).await;
        for text in texts {
            if entries.len() >= cap {
                break;
            }
            if let Some(text) = text {
                absorb(text, &mut queue, &mut entries);
            }
        }
    }
    (robots, entries)
}

/// Fetch one sitemap candidate and decode it to text.
/// None = non-200, binary, or undecodable.
async fn fetch_sitemap_text(fetch: &PageFetcher, loc: &str) -> Option<String> {
    let page = fetch(loc.to_string(), "direct".to_string(), None).await;
    if page.status != 200 {
        return None;
    }
    let body = maybe_gunzip(&page.body);
    String::from_utf8(body).ok()
}

/// Parse one sitemap body: child indexes go back to the queue,
/// page entries join the map.
fn absorb(text: String, queue: &mut Vec<String>, entries: &mut Vec<SitemapEntry>) {
    let mut here = Vec::new();
    if text.trim_start().starts_with("<") {
        // XML sitemap.
        parse_sitemap(&text, &mut here, 10_000);
    } else {
        // Plain-text sitemap: one URL per line (doc.rust-lang
        // publishes sitemap.txt).
        for line in text.lines().take(10_000) {
            let u = line.trim();
            if u.starts_with("http") {
                here.push(SitemapEntry {
                    loc: u.to_string(),
                    lastmod: None,
                    priority: None,
                    is_index: false,
                });
            }
        }
    }
    // Child sitemaps recurse; pages go straight to the map.
    for e in here {
        if e.is_index {
            if queue.len() < 128 {
                queue.push(e.loc);
            }
        } else {
            entries.push(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_star_group_disallow_sitemap() {
        let body = "User-agent: Googlebot\nDisallow: /g\n\nUser-agent: *\nDisallow: /admin\nDisallow: /private\nCrawl-delay: 2\nSitemap: https://ex.com/sitemap.xml\n";
        let r = Robots::parse(body, "ex.com");
        assert_eq!(r.sitemaps, vec!["https://ex.com/sitemap.xml"]);
        assert!(!r.allowed("/admin/x"));
        assert!(!r.allowed("/private"));
        assert!(r.allowed("/ok"));
        assert_eq!(r.crawl_delay, Some(2.0));
    }

    #[test]
    fn robots_allow_beats_disallow_on_longest() {
        let body = "User-agent: *\nDisallow: /a\nAllow: /a/b\n";
        let r = Robots::parse(body, "ex.com");
        assert!(r.allowed("/a/b/c"));
        assert!(!r.allowed("/a/x"));
    }

    #[test]
    fn sitemap_urlset_parses() {
        let xml = r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
<url><loc>https://ex.com/a</loc><lastmod>2026-01-01</lastmod></url>
<url><loc>https://ex.com/b</loc></url>
</urlset>"#;
        let mut out = Vec::new();
        parse_sitemap(xml, &mut out, 100);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].loc, "https://ex.com/a");
        assert_eq!(out[0].lastmod.as_deref(), Some("2026-01-01"));
        assert!(out[1].lastmod.is_none());
    }

    #[test]
    fn sitemap_index_children() {
        let xml = r#"<sitemapindex xmlns="x">
<sitemap><loc>https://ex.com/sm-a.xml</loc></sitemap>
<sitemap><loc>https://ex.com/sm-b.xml</loc></sitemap>
</sitemapindex>"#;
        let mut out = Vec::new();
        parse_sitemap(xml, &mut out, 100);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].loc, "https://ex.com/sm-a.xml");
    }

    #[test]
    fn sitemap_malformed_tolerates() {
        let xml = "<urlset><url><loc>https://ex.com/a</loc>"/* truncated */;
        let mut out = Vec::new();
        parse_sitemap(xml, &mut out, 100);
        assert!(out.is_empty()); // graceful, no panic
    }

    #[test]
    fn gunzip_passthrough_plain() {
        let plain = b"<urlset/>";
        assert_eq!(maybe_gunzip(plain), plain);
    }
}
