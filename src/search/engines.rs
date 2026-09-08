//! SERP parsers : one per engine, scraper-based, with
//! layered fallbacks. A parse that yields <3 hits counts
//! as engine failure (the health system hears about it).

use scraper::{Html, Selector};

pub mod google_wml;

/// One raw hit from one engine.
#[derive(Debug, Clone)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub rank: usize,
    /// ISO date when the source carries one (news vertical).
    pub published: Option<String>,
}

fn text(el: scraper::ElementRef) -> String {
    el.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("static selector")
}

/// DDG html endpoint wraps links in /l/?uddg= redirects.
/// Decode to the real URL : consensus matching depends on
/// every engine reporting the SAME url.
fn decode_ddg(href: &str) -> String {
    if let Some((_, q)) = href.split_once("uddg=") {
        let raw = q.split('&').next().unwrap_or(q);
        if let Ok(decoded) = urlencoding_decode(raw)
            && decoded.starts_with("http")
        {
            return decoded;
        }
    }
    href.to_string()
}

fn urlencoding_decode(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| ())?;
            let v = u8::from_str_radix(hex, 16).map_err(|_| ())?;
            out.push(v);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

/// Bing redirect links: /ck/a?...&u=a1aHR0cHM... (base64url
/// after the "a1" prefix).
fn decode_bing(href: &str) -> String {
    if href.contains("bing.com/ck/a")
        && let Some((_, u)) = href.split_once("&u=")
    {
        let u = u.split('&').next().unwrap_or(u);
        if let Some(b64) = u.strip_prefix("a1")
            && let Some(decoded) = base64url_decode(b64)
            && decoded.starts_with("http")
        {
            return decoded;
        }
    }
    // If this is a bing.com/ck/a stub we couldn't decode,
    // return empty : the is_serp_url filter and the
    // starts_with("http") check will drop it.
    if href.contains("bing.com/ck/a") {
        return String::new();
    }
    href.to_string()
}

fn base64url_decode(s: &str) -> Option<String> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut vals = Vec::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let pos = T.iter().position(|&t| t == c)?;
        vals.push(pos as u32);
    }
    let mut out = Vec::with_capacity(vals.len() * 6 / 8);
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (i, &v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// Check if a URL is a search engine results page (SERP).
/// These should never appear as search results : they leak
/// through parsers when redirect decoding fails or when
/// broad selectors match pagination/header links.
fn is_serp_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    let serp_patterns = [
        // General SERPs
        "search.yahoo.com/search",
        "r.search.yahoo.com/",
        "search.yahoo.com/yhs/search",
        "www.google.com/search",
        "google.com/search",
        "www.bing.com/search",
        "bing.com/search",
        "www.bing.com/ck/a",
        "bing.com/ck/a",
        "search.brave.com/search",
        "lite.duckduckgo.com/",
        "html.duckduckgo.com/",
        "duckduckgo.com/",
        "www.mojeek.com/search",
        "mojeek.com/search",
        // Vertical SERPs
        "www.google.com/scholar",
        "scholar.google.com/",
        "news.google.com/search",
    ];
    serp_patterns.iter().any(|p| lower.contains(p))
}

pub fn parse(engine: &str, html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let mut hits = match engine {
        "brave" => parse_brave(&doc),
        "google" => google_wml::parse(&doc),
        "google_ghost" => parse_google(&doc),
        "bing" => parse_bing(&doc),
        // DDG primary is now lite : the html endpoint serves a
        // CAPTCHA challenge to proxy IPs.  parse_ddg (html parser)
        // is kept for the ddg_html fallback engine.
        "ddg" => parse_ddg_lite(&doc),
        "ddg_lite" => parse_ddg_lite(&doc),
        "ddg_html" => parse_ddg(&doc),
        "mojeek" => parse_mojeek(&doc),
        "yahoo" => parse_yahoo(&doc),
        _ => Vec::new(),
    };
    // Filter out SERP URLs that leaked through parsers (e.g.
    // search.yahoo.com/search pagination links, undecoded
    // r.search.yahoo.com redirects, bing.com/ck/a stubs).
    hits.retain(|h| !is_serp_url(&h.url));
    if hits.is_empty() && std::env::var_os("DONSEEK_DEBUG").is_some() {
        let dump = std::env::temp_dir().join(format!("donseek_debug_{engine}.html"));
        let dump = dump.to_string_lossy().into_owned();
        let _ = std::fs::write(&dump, html);
        eprintln!(
            "[donseek] {engine}: 0 hits, dumped {len} bytes to {dump}",
            len = html.len()
        );
    }
    hits
}

/// Google URL unwrapping: Google wraps result URLs in
/// /url?q=REAL_URL&sa=U&ved=... : extract and decode the
/// real URL. Direct http(s) links pass through unchanged.
fn decode_google(href: &str) -> String {
    if let Some((_, q)) = href.split_once("/url?") {
        let q = q.split('&').next().unwrap_or(q);
        if let Some(raw) = q.strip_prefix("q=")
            && let Ok(decoded) = urlencoding_decode(raw)
            && decoded.starts_with("http")
        {
            return decoded;
        }
    }
    href.to_string()
}

/// Google SERP parser. Google's HTML changes frequently, so
/// this uses layered selectors: primary (div.g), fallback
/// (div[data-ved]), and a shotgun mode (any a with h3 sibling).
fn parse_google(doc: &Html) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Primary: div.g blocks (the classic Google result container).
    let g_blocks = sel("div.g, div.tF2Cxc");
    let link_sel = sel("a[href]");
    let h3 = sel("h3");
    let snip_sel = sel("span.aCOpRe, div[data-sncf], span.st, div.VwiEFb, div.IsZrdc");

    for block in doc.select(&g_blocks) {
        let Some(a) = block.select(&link_sel).next() else {
            continue;
        };
        let raw_href = a.value().attr("href").unwrap_or("");
        let url = decode_google(raw_href);
        if !url.starts_with("http") || url.contains("google.com/") {
            continue;
        }
        let key = url.split('&').next().unwrap_or(&url).to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        let title = block
            .select(&h3)
            .next()
            .map(text)
            .unwrap_or_else(|| text(a));
        if title.is_empty() {
            continue;
        }
        let snippet = block.select(&snip_sel).next().map(text).unwrap_or_default();
        hits.push(Hit {
            title,
            url,
            snippet,
            rank: hits.len(),
            published: None,
        });
    }

    if hits.len() >= 3 {
        return hits;
    }

    // Fallback: any div[data-ved] with a link + h3.
    let ved_blocks = sel("div[data-ved]");
    for block in doc.select(&ved_blocks) {
        let Some(a) = block.select(&link_sel).next() else {
            continue;
        };
        let raw_href = a.value().attr("href").unwrap_or("");
        let url = decode_google(raw_href);
        if !url.starts_with("http") || url.contains("google.com/") {
            continue;
        }
        let key = url.split('&').next().unwrap_or(&url).to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        let title = block.select(&h3).next().map(text);
        if title.is_none() {
            continue;
        }
        let title = title.unwrap();
        if title.is_empty() {
            continue;
        }
        let snippet = block.select(&snip_sel).next().map(text).unwrap_or_default();
        hits.push(Hit {
            title,
            url,
            snippet,
            rank: hits.len(),
            published: None,
        });
    }

    if hits.len() >= 3 {
        return hits;
    }

    // Shotgun: any <a> with an <h3> ancestor and an http href.
    for a in doc.select(&link_sel) {
        let raw_href = a.value().attr("href").unwrap_or("");
        let url = decode_google(raw_href);
        if !url.starts_with("http") || url.contains("google.com/") {
            continue;
        }
        let key = url.split('&').next().unwrap_or(&url).to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        // Check for h3 in the ancestor chain.
        let mut title = String::new();
        for ancestor in a.ancestors() {
            if let Some(el_ref) = scraper::ElementRef::wrap(ancestor)
                && let Some(h3_el) = el_ref.select(&h3).next()
            {
                title = text(h3_el);
                break;
            }
        }
        if title.is_empty() {
            continue;
        }
        hits.push(Hit {
            title,
            url,
            snippet: String::new(),
            rank: hits.len(),
            published: None,
        });
    }

    hits
}

fn parse_brave(doc: &Html) -> Vec<Hit> {
    let blocks = sel(r#"div[data-type="web"]"#);
    let link = sel("a[href]");
    let title = sel(".title");
    let snippet = sel(".generic-snippet");
    let mut hits = Vec::new();
    for (rank, block) in doc.select(&blocks).enumerate() {
        let Some(a) = block.select(&link).next() else {
            continue;
        };
        let url = a.value().attr("href").unwrap_or("").to_string();
        if !url.starts_with("http") {
            continue;
        }
        let t = a
            .select(&title)
            .next()
            .map(text)
            .or_else(|| block.select(&title).next().map(text))
            .unwrap_or_default();
        // Snippet: card text minus the title and breadcrumb.
        let full = block.select(&snippet).next().map(text).unwrap_or_default();
        let snip = full
            .replace(&t, "")
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if !t.is_empty() {
            hits.push(Hit {
                title: t,
                url,
                snippet: snip,
                rank,
                published: None,
            });
        }
    }
    hits
}

fn parse_bing(doc: &Html) -> Vec<Hit> {
    let items = sel("li.b_algo");
    // Keep the title link and the attribution link separate. A grouped
    // selector returns matches in document order, not selector-preference
    // order; Bing places `a.tilk` (site + breadcrumb) before `h2 a`, so the
    // old grouped selector silently used the breadcrumb as both URL and
    // title on every result.
    let title_link = sel("h2 a");
    let fallback_link = sel("a.tilk, a[data-h]");
    // Primary: .b_caption p; fallback: .b_lineclamp*, [data-text]
    let cap = sel(".b_caption p, .b_lineclamp2, .b_lineclamp3, .b_lineclamp4, p[data-text]");
    let h2 = sel("h2");
    let mut hits = Vec::new();
    for (rank, li) in doc.select(&items).enumerate() {
        let Some(a) = li
            .select(&title_link)
            .next()
            .or_else(|| li.select(&fallback_link).next())
            .or_else(|| li.select(&sel("a[href]")).next())
        else {
            continue;
        };
        let url = decode_bing(a.value().attr("href").unwrap_or(""));
        if !url.starts_with("http") {
            continue;
        }
        let title = text(a);
        // Fallback: if the link text is empty, try h2.
        let title = if title.is_empty() {
            li.select(&h2).next().map(text).unwrap_or_default()
        } else {
            title
        };
        let snippet = li.select(&cap).next().map(text).unwrap_or_default();
        if !title.is_empty() {
            hits.push(Hit {
                title,
                url,
                snippet,
                rank,
                published: None,
            });
        }
    }
    hits
}

fn parse_ddg(doc: &Html) -> Vec<Hit> {
    let links = sel("a.result__a");
    let snippets = sel("a.result__snippet, .result__snippet");
    let snippet_vec: Vec<String> = doc.select(&snippets).map(text).collect();
    let mut hits = Vec::new();
    for (rank, a) in doc.select(&links).enumerate() {
        let url = decode_ddg(a.value().attr("href").unwrap_or(""));
        if !url.starts_with("http") {
            continue;
        }
        let title = text(a);
        let snippet = snippet_vec.get(rank).cloned().unwrap_or_default();
        hits.push(Hit {
            title,
            url,
            snippet,
            rank,
            published: None,
        });
    }
    hits
}

fn parse_ddg_lite(doc: &Html) -> Vec<Hit> {
    // Lite: a table : result-link anchors then snippet tds.
    let links = sel("a.result-link");
    let snippets = sel("td.result-snippet");
    let snippet_vec: Vec<String> = doc.select(&snippets).map(text).collect();
    let mut hits = Vec::new();
    for (rank, a) in doc.select(&links).enumerate() {
        let url = decode_ddg(a.value().attr("href").unwrap_or(""));
        if !url.starts_with("http") {
            continue;
        }
        hits.push(Hit {
            title: text(a),
            url,
            snippet: snippet_vec.get(rank).cloned().unwrap_or_default(),
            rank,
            published: None,
        });
    }
    hits
}

fn parse_mojeek(doc: &Html) -> Vec<Hit> {
    // Mojeek: <li><a class="ob">breadcrumb</a>
    //         <h2><a class="title" href>real title</a></h2>
    //         <p class="s">snippet</p></li>
    let items = sel("ul.results-standard li");
    let link = sel("h2 a.title, h2 a");
    let cap = sel("p.s");
    let mut hits = Vec::new();
    for (rank, li) in doc.select(&items).enumerate() {
        let Some(a) = li.select(&link).next() else {
            continue;
        };
        let url = a.value().attr("href").unwrap_or("").to_string();
        if !url.starts_with("http") {
            continue;
        }
        let title = text(a);
        if title.is_empty() {
            continue;
        }
        hits.push(Hit {
            title,
            url,
            snippet: li.select(&cap).next().map(text).unwrap_or_default(),
            rank,
            published: None,
        });
    }
    hits
}

/// Yahoo redirect links: r.search.yahoo.com/...RU=REAL_URL
/// or r.search.yahoo.com/..._url=REAL_URL. Decode to the
/// real URL : consensus matching depends on every engine
/// reporting the SAME url.
///
/// Yahoo embeds tracking parameters (/RK=, /RS=, /RV=) inside
/// the URL-encoded RU= value, so they survive urlencoding_decode
/// as path suffixes on the real URL. We strip them.
fn decode_yahoo(href: &str) -> String {
    if !href.contains("r.search.yahoo.com") && !href.contains("search.yahoo.com/search") {
        return href.to_string();
    }
    // Try RU= parameter (most common). Yahoo uses ; as a
    // separator in redirect URLs, so strip at ; or &.
    if let Some((_, ru)) = href.split_once("RU=") {
        let raw = ru.split(['&', ';']).next().unwrap_or(ru);
        if let Ok(decoded) = urlencoding_decode(raw)
            && decoded.starts_with("http")
        {
            return strip_yahoo_tracking(&decoded);
        }
    }
    // Try _url= parameter.
    if let Some((_, url)) = href.split_once("_url=") {
        let raw = url.split(['&', ';']).next().unwrap_or(url);
        if let Ok(decoded) = urlencoding_decode(raw)
            && decoded.starts_with("http")
        {
            return strip_yahoo_tracking(&decoded);
        }
    }
    // Can't decode : return empty so the parser's `starts_with("http")`
    // check filters it out. Previously this returned the raw Yahoo
    // SERP URL, which leaked search.yahoo.com/search?p=... as a result.
    String::new()
}

/// Strip Yahoo tracking suffixes (/RK=, /RS=, /RV=) that
/// Yahoo embeds inside the URL-encoded RU= value. These are
/// not part of the real URL and would break consensus matching.
fn strip_yahoo_tracking(url: &str) -> String {
    for marker in ["/RK=", "/RS=", "/RV="] {
        if let Some(idx) = url.find(marker) {
            return url[..idx].to_string();
        }
    }
    url.to_string()
}

fn parse_yahoo(doc: &Html) -> Vec<Hit> {
    // Yahoo SERP selectors with fallbacks.
    let items = sel("div.dd.algo, li div.algo, div.algo, div.compTitle");
    let link = sel("h3.title a, h3 a, a.title, a[data-mat]");
    let cap = sel(".compText, .compText a, p");
    let h3 = sel("h3");
    let a_gen = sel("a[href]");
    let mut hits = Vec::new();
    for (rank, item) in doc.select(&items).enumerate() {
        let Some(a) = item
            .select(&link)
            .next()
            .or_else(|| item.select(&a_gen).next())
        else {
            continue;
        };
        let url = decode_yahoo(a.value().attr("href").unwrap_or(""));
        if !url.starts_with("http") {
            continue;
        }
        // Yahoo nests the breadcrumb and the h3 inside the same outer link.
        // `text(a)` therefore concatenates site name + URL + real title. Use
        // the dedicated heading first and retain the outer text only as a
        // fallback for older layouts without an h3.
        let heading = item.select(&h3).next().map(text).unwrap_or_default();
        let title = if heading.is_empty() { text(a) } else { heading };
        let snippet = item.select(&cap).next().map(text).unwrap_or_default();
        if !title.is_empty() {
            hits.push(Hit {
                title,
                url,
                snippet,
                rank,
                published: None,
            });
        }
    }
    hits
}

/// Engine URL builders.
pub fn serp_url(engine: &str, query: &str) -> Option<String> {
    let q = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    match engine {
        "google" => Some(google_wml::url(query)),
        "google_ghost" => Some(format!(
            "https://www.google.com/search?q={q}&hl=en&gl=us&num=15&ie=utf-8&oe=utf-8"
        )),
        "brave" => Some(format!("https://search.brave.com/search?q={q}")),
        "bing" => Some(format!("https://www.bing.com/search?q={q}&count=15")),
        // DDG primary: lite endpoint (html endpoint serves CAPTCHA to proxy IPs).
        "ddg" => Some(format!("https://lite.duckduckgo.com/lite/?q={q}")),
        "ddg_lite" => Some(format!("https://lite.duckduckgo.com/lite/?q={q}")),
        "ddg_html" => Some(format!("https://html.duckduckgo.com/html/?q={q}")),
        "mojeek" => Some(format!("https://www.mojeek.com/search?q={q}")),
        "yahoo" => Some(format!("https://search.yahoo.com/search?p={q}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_http_and_browser_keep_distinct_endpoints_and_parsers() {
        assert!(serp_url("google", "rust").unwrap().contains("/wml/search?"));
        assert!(
            serp_url("google_ghost", "rust")
                .unwrap()
                .contains("/search?")
        );
        assert!(!serp_url("google_ghost", "rust").unwrap().contains("/wml/"));
        let desktop = r#"<div class="g"><a href="https://example.org/"><h3>Title</h3></a><div class="VwiEFb">Snippet</div></div>"#;
        assert_eq!(parse("google_ghost", desktop).len(), 1);
        assert!(parse("google", desktop).is_empty());
    }

    #[test]
    fn serp_url_detection() {
        assert!(is_serp_url(
            "https://search.yahoo.com/search?p=rust+programming"
        ));
        assert!(is_serp_url("https://r.search.yahoo.com/RY=abc/RV=def"));
        assert!(is_serp_url("https://www.google.com/search?q=rust&hl=en"));
        assert!(is_serp_url("https://www.bing.com/search?q=rust"));
        assert!(is_serp_url("https://search.brave.com/search?q=rust"));
        assert!(is_serp_url("https://lite.duckduckgo.com/lite/?q=rust"));
        assert!(is_serp_url("https://www.mojeek.com/search?q=rust"));
    }

    #[test]
    fn real_urls_are_not_serp() {
        assert!(!is_serp_url(
            "https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html"
        ));
        assert!(!is_serp_url(
            "https://developer.mozilla.org/en-US/docs/Web/JavaScript"
        ));
        assert!(!is_serp_url("https://github.com/rust-lang/rust"));
        assert!(!is_serp_url(
            "https://stackoverflow.com/questions/2899/what-is-ownership"
        ));
        assert!(!is_serp_url(
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        ));
    }

    #[test]
    fn yahoo_decode_failure_returns_empty() {
        // A Yahoo SERP URL with no decodable redirect → empty,
        // not the raw SERP URL.
        assert_eq!(decode_yahoo("https://search.yahoo.com/search?p=rust"), "");
        // A real Yahoo redirect with RU= parameter → decoded.
        let decoded =
            decode_yahoo("https://r.search.yahoo.com/;_ylt=Awr;RU=https://example.com/page;RV=abc");
        assert_eq!(decoded, "https://example.com/page");
    }

    #[test]
    fn yahoo_tracking_stripped() {
        // Yahoo embeds /RK=2/RS=... inside the URL-encoded RU= value.
        // These tracking suffixes must be stripped for clean URLs.
        let decoded = decode_yahoo(
            "https://r.search.yahoo.com/;RU=https%3A%2F%2Fexample.com%2Fpage%2FRK%3D2%2FRS%3Dabc",
        );
        assert_eq!(decoded, "https://example.com/page");
    }

    #[test]
    fn bing_decode_failure_returns_empty() {
        // A Bing ck/a stub we can't decode → empty.
        assert_eq!(
            decode_bing("https://www.bing.com/ck/a?abcdef&u=a1invalid"),
            ""
        );
        // A non-Bing URL passes through unchanged.
        assert_eq!(
            decode_bing("https://example.com/page"),
            "https://example.com/page"
        );
    }

    #[test]
    fn parse_filters_serp_urls() {
        // Yahoo HTML with a SERP self-link mixed in with real results.
        let html = r#"
        <html><body>
        <div class="algo">
          <h3 class="title"><a href="https://r.search.yahoo.com/;RU=https://doc.rust-lang.org/book;RV=1">The Rust Book</a></h3>
          <div class="compText">A guide to Rust programming</div>
        </div>
        <div class="algo">
          <h3 class="title"><a href="https://search.yahoo.com/search?p=rust+ownership">More results for rust ownership</a></h3>
          <div class="compText">Search Yahoo</div>
        </div>
        </body></html>
        "#;
        let hits = parse("yahoo", html);
        assert_eq!(hits.len(), 1, "SERP URL should be filtered, got {hits:?}");
        assert_eq!(hits[0].url, "https://doc.rust-lang.org/book");
        assert_eq!(hits[0].title, "The Rust Book");
    }

    #[test]
    fn bing_prefers_result_heading_over_earlier_attribution_link() {
        let html = r#"
        <html><body><ol>
          <li class="b_algo">
            <div class="b_tpcn">
              <a class="tilk" href="https://www.bing.com/site-attribution">
                <span>rust-lang.org</span><cite>https://doc.rust-lang.org › book</cite>
              </a>
            </div>
            <h2><a href="https://doc.rust-lang.org/book/">The Rust Programming Language</a></h2>
            <div class="b_caption"><p>Learn Rust ownership and borrowing.</p></div>
          </li>
        </ol></body></html>
        "#;
        let hits = parse("bing", html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert_eq!(hits[0].url, "https://doc.rust-lang.org/book/");
        assert!(!hits[0].title.contains("https://"));
    }

    #[test]
    fn yahoo_extracts_heading_instead_of_outer_link_breadcrumb() {
        let html = r#"
        <html><body>
          <div class="compTitle options-toggle">
            <a data-matarget="algo" href="https://example.com/ownership">
              <div><span>Example</span>https://example.com › ownership</div>
              <h3 class="title"><span>Understanding Ownership</span></h3>
            </a>
            <div class="compText"><p>A focused explanation.</p></div>
          </div>
        </body></html>
        "#;
        let hits = parse("yahoo", html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Understanding Ownership");
        assert!(!hits[0].title.contains("https://"));
    }
}
