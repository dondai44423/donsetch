//! SERP parsers : one per engine, scraper-based, with
//! layered fallbacks. A parse that yields <3 hits counts
//! as engine failure (the health system hears about it).

use base64::Engine as _;
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, PAD_INDIFFERENT};
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
    // Visible text only: a script/style subtree in a SERP cell is
    // source, never snippet text (#288's class, search side).
    crate::extract::inline::visible_text(el)
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

/// base64url (RFC 4648 section 5), padded or not, decoded to UTF-8.
/// `None` when the input is not base64url or not UTF-8.
fn base64url_decode(s: &str) -> Option<String> {
    // Lenient on purpose, matching the hand-rolled decoder this
    // replaced: Bing's `u=` value is third-party data, seen both with
    // and without `=`, and nonzero unused bits in the last symbol do
    // not change the bytes a URL decodes to.
    const BING_URL_SAFE: GeneralPurpose = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        PAD_INDIFFERENT.with_decode_allow_trailing_bits(true),
    );
    let bytes = BING_URL_SAFE.decode(s).ok()?;
    String::from_utf8(bytes).ok()
}

/// Check if a URL is a search engine results page (SERP).
/// These should never appear as search results : they leak
/// through parsers when redirect decoding fails or when
/// broad selectors match pagination/header links.
pub(crate) fn is_serp_url(url: &str) -> bool {
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
    parse_with_instant(engine, html, "").0
}

/// One DOM parse feeds both the organic hit list and the C5
/// instant layer. html5ever on a multi-MB SERP is the expensive
/// part; doing it twice per engine was pure waste.
///
/// `page_url` is the SERP's own address: the shape fallback needs it to
/// resolve relative links and to know the page's host. Known selectors are
/// tried first (precise, cheap); when they yield nothing the document's shape
/// answers instead, so a restyle costs results, never the source.
pub fn parse_with_instant(
    engine: &str,
    html: &str,
    page_url: &str,
) -> (Vec<Hit>, Option<super::instant::InstantAnswer>) {
    let doc = Html::parse_document(html);
    let mut hits = match engine {
        "brave" => parse_brave(&doc),
        "google" => google_wml::parse(&doc),
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
    // Selector rot insurance: a parser that finds fewer than three results on a
    // page that visibly contains a list has met a restyle, not an empty SERP.
    if hits.len() < 3 && !page_url.is_empty() {
        let shaped = shape_extract(html, page_url);
        if shaped.len() > hits.len() {
            hits = shaped;
        }
    }
    let instant = super::instant::parse_instant_doc(engine, &doc);
    if hits.is_empty() && crate::config::cfg().debug.search {
        let dump = std::env::temp_dir().join(format!("donseek_debug_{engine}.html"));
        let dump = dump.to_string_lossy().into_owned();
        let _ = std::fs::write(&dump, html);
        crate::diag!(
            "[donseek] {engine}: 0 hits, dumped {len} bytes to {dump}",
            len = html.len()
        );
    }
    (hits, instant)
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
/// A sponsored result's click-through: Yahoo serves Bing's ads in its list, and
/// the link is Bing's `/aclick` redirect, not a page. It is not an answer to
/// anything the user asked, so it is dropped where the redirect is decoded.
fn is_ad_click(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.host_str()
            .is_some_and(|h| h == "bing.com" || h.ends_with(".bing.com"))
            && u.path().starts_with("/aclick")
    })
}

fn decode_yahoo(href: &str) -> String {
    // Yahoo's redirect host is `r.search.yahoo.com` on some layouts and
    // `rd.search.yahoo.com` on others; `contains("r.search.yahoo.com")` matched
    // only the first, so every `rd.` result came back as the redirect itself and
    // ranked as a page on yahoo.com. Match the suffix of the host instead.
    let host = href
        .split_once("://")
        .map_or(href, |(_, rest)| rest)
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    if !host.ends_with(".search.yahoo.com") && !href.contains("search.yahoo.com/search") {
        return href.to_string();
    }
    // Try RU= parameter (most common). Yahoo uses ; as a
    // separator in redirect URLs, so strip at ; or &.
    if let Some((_, ru)) = href.split_once("RU=") {
        let raw = ru.split(['&', ';']).next().unwrap_or(ru);
        if let Ok(decoded) = urlencoding_decode(raw)
            && decoded.starts_with("http")
        {
            return if is_ad_click(&decoded) {
                String::new()
            } else {
                strip_yahoo_tracking(&decoded)
            };
        }
    }
    // Try _url= parameter.
    if let Some((_, url)) = href.split_once("_url=") {
        let raw = url.split(['&', ';']).next().unwrap_or(url);
        if let Ok(decoded) = urlencoding_decode(raw)
            && decoded.starts_with("http")
        {
            return if is_ad_click(&decoded) {
                String::new()
            } else {
                strip_yahoo_tracking(&decoded)
            };
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
    serp_url_in(engine, query, None)
}

/// The same URL with the query's locale in the parameters the source honors:
/// Bing's `setlang`/`cc`, DuckDuckGo's `kl`, Yahoo's `vl`. Sources without a
/// locale parameter are unchanged.
pub fn serp_url_in(
    engine: &str,
    query: &str,
    locale: Option<super::locale::Locale>,
) -> Option<String> {
    let q = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    let mut url = match engine {
        "google" => Some(google_wml::url(query)),
        "brave" => Some(format!("https://search.brave.com/search?q={q}")),
        "bing" => Some(format!("https://www.bing.com/search?q={q}&count=15")),
        // DDG primary: lite endpoint (html endpoint serves CAPTCHA to proxy IPs).
        "ddg" => Some(format!("https://lite.duckduckgo.com/lite/?q={q}")),
        "ddg_lite" => Some(format!("https://lite.duckduckgo.com/lite/?q={q}")),
        "ddg_html" => Some(format!("https://html.duckduckgo.com/html/?q={q}")),
        "mojeek" => Some(format!("https://www.mojeek.com/search?q={q}")),
        "yahoo" => Some(format!("https://search.yahoo.com/search?p={q}")),
        // The same index as `bing`, through the one interface it publishes
        // as data rather than layout: RSS 2.0, ten items, no pagination and
        // no `site:` support. Used as bing's fallback, never as a 7th slot.
        "bing_rss" => Some(format!("https://www.bing.com/search?format=rss&q={q}")),
        _ => None,
    }?;
    if let Some(l) = locale {
        let extra = match engine {
            "bing" | "bing_rss" => format!("&setlang={}&cc={}", l.lang, l.region),
            "ddg" | "ddg_lite" | "ddg_html" => format!("&kl={}", l.kl()),
            "yahoo" => format!("&vl=lang_{}", l.lang),
            _ => String::new(),
        };
        url.push_str(&extra);
    }
    Some(url)
}

// ── Answer contracts: what a source replied, not just that it replied ────────

/// How a source publishes its answer. A structured format costs no DOM parse
/// and cannot rot with a restyle; HTML is the fallback, never the first choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Html,
    Rss,
}

pub fn format(engine: &str) -> Format {
    match engine {
        "bing_rss" => Format::Rss,
        _ => Format::Html,
    }
}

/// What came back, as a property of the *answer* rather than of the request.
///
/// `OffQuery` is the class this layer exists for: an HTTP 200 carrying a full
/// result list that has nothing to do with the query. Measured on this network:
/// bing's RSS answered 38/40 queries with ten items, and 24 of those answers
/// engaged no query term at all. A status-code health model scores that as 95%
/// healthy; this one scores it as the failure it is, and keeps the items out of
/// the merge so one bot-page cannot pollute a query's ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerClass {
    Content,
    /// Few items, or a mixed bag: the items are real, the answer is not full.
    Thin,
    /// A 200 with a result list that does not engage the query at all.
    OffQuery,
    Empty,
}

impl AnswerClass {
    /// Status string for the report; stable names, they reach the model.
    pub fn status(self) -> &'static str {
        match self {
            Self::Content => "ok",
            Self::Thin => "thin",
            Self::OffQuery => "off-query",
            Self::Empty => "empty-parse",
        }
    }

    /// Whether these items may enter the merge.
    pub fn contributes(self) -> bool {
        matches!(self, Self::Content | Self::Thin)
    }

    /// Whether this answer earns the source trust. A thin answer is real but
    /// unproven; an off-query one is evidence against the source.
    pub fn credits(self) -> bool {
        matches!(self, Self::Content)
    }
}

/// A first-party interface answered with items: its own ranking is the
/// contract, so the only failure it can commit here is returning nothing.
pub fn vertical_class(hits: &[Hit]) -> AnswerClass {
    if hits.is_empty() {
        AnswerClass::Empty
    } else {
        AnswerClass::Content
    }
}

// ── Shape extraction: the document's structure, never its class names ────────

/// Blocks containing more anchors than this are a list of *results*, not one.
const BLOCK_MAX_ANCHORS: usize = 3;
/// A block longer than this is a page section, not a result.
const BLOCK_MAX_CHARS: usize = 1600;
/// The climb stops once the block carries at least this much text.
const BLOCK_MIN_CHARS: usize = 60;
const MAX_SHAPED: usize = 20;

/// Extract results from any SERP by shape alone.
///
/// A result is a *block*: an element holding one primary outbound link, a
/// heading, and some text, sitting in a list of siblings that look the same.
/// Nothing here reads a class name, an id, or a tag order, which is the point:
/// the commonest way a scraped engine dies is a restyle, and a restyle cannot
/// change the document's shape without ceasing to be a search results page.
pub fn shape_extract(html: &str, page_url: &str) -> Vec<Hit> {
    use scraper::ElementRef;
    use std::collections::{HashMap, HashSet};

    let Ok(base) = url::Url::parse(page_url) else {
        return Vec::new();
    };
    let page_host = base
        .host_str()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let doc = Html::parse_document(html);
    let anchors = sel("a[href]");
    let headings = sel("h1, h2, h3, h4, h5, h6");
    // Text length is read for every ancestor of every anchor; on a 250 KB SERP
    // that is thousands of subtree walks. One pass per node, remembered.
    let mut len_cache: HashMap<ego_tree::NodeId, usize> = HashMap::new();
    let text_len = |el: ElementRef<'_>, cache: &mut HashMap<ego_tree::NodeId, usize>| -> usize {
        *cache
            .entry(el.id())
            .or_insert_with(|| text(el).chars().count())
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Hit> = Vec::new();
    for anchor in doc.select(&anchors) {
        if out.len() >= MAX_SHAPED {
            break;
        }
        let raw = anchor.value().attr("href").unwrap_or("");
        let url = resolve_result_url(raw, &base);
        let Ok(parsed) = url::Url::parse(&url) else {
            continue;
        };
        if !matches!(parsed.scheme(), "http" | "https") || is_serp_url(&url) {
            continue;
        }
        let host = parsed
            .host_str()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_ascii_lowercase();
        if host.is_empty()
            || host == page_host
            || host.ends_with(&format!(".{page_host}"))
            || page_host.ends_with(&format!(".{host}"))
        {
            continue;
        }
        let anchor_text = text(anchor);
        // The block: climb while the ancestor still looks like ONE result.
        let mut node = anchor;
        let mut block = anchor;
        for _ in 0..6 {
            let Some(parent) = node.parent().and_then(ElementRef::wrap) else {
                break;
            };
            if parent.select(&anchors).count() > BLOCK_MAX_ANCHORS
                || text_len(parent, &mut len_cache) > BLOCK_MAX_CHARS
            {
                break;
            }
            node = parent;
            block = parent;
            if text_len(block, &mut len_cache) >= BLOCK_MIN_CHARS {
                break;
            }
        }
        // Title: the block's own heading when it has one, else the link text.
        // Breadcrumbs and URL echoes are rejected the same way ranking does.
        let heading = block.select(&headings).next().map(text).unwrap_or_default();
        let title = if heading.chars().count() >= 8 && !heading.contains(" › ") {
            heading
        } else {
            anchor_text.clone()
        };
        if title.chars().count() < 8 || is_nav_text(&title) {
            continue;
        }
        let mut snippet = text(block);
        if let Some(pos) = snippet.find(&title) {
            snippet = snippet[pos + title.len()..].to_string();
        }
        let snippet = scrub_snippet(&snippet);
        let key = super::rank::norm_key(&url);
        if !seen.insert(key) {
            continue;
        }
        out.push(Hit {
            title,
            url,
            snippet,
            rank: out.len(),
            published: None,
        });
    }
    out
}

/// A result link as the source meant it: engine wrappers decoded, relatives
/// resolved. Every source must report the SAME url or consensus cannot match.
fn resolve_result_url(raw: &str, base: &url::Url) -> String {
    if raw.contains("uddg=") {
        return decode_ddg(raw);
    }
    if raw.contains("bing.com/ck/a") {
        return decode_bing(raw);
    }
    if raw.contains("r.search.yahoo.com") || raw.contains("search.yahoo.com/search") {
        return decode_yahoo(raw);
    }
    base.join(raw).map(|u| u.to_string()).unwrap_or_default()
}

/// Navigation and chrome that live in the same list as results.
fn is_nav_text(title: &str) -> bool {
    const NAV: &[&str] = &[
        "next",
        "previous",
        "more results",
        "load more",
        "sign in",
        "log in",
        "settings",
        "help",
        "privacy",
        "terms",
        "about",
        "feedback",
        "images",
        "videos",
        "news",
        "maps",
        "shopping",
    ];
    let t = title.trim().to_lowercase();
    NAV.iter()
        .any(|n| t == *n || t.starts_with(&format!("{n} ")))
}

/// Snippet text with the URL echoes and breadcrumbs every SERP block repeats.
fn scrub_snippet(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for token in raw.split_whitespace() {
        if token.starts_with("http://") || token.starts_with("https://") {
            continue;
        }
        if token == "›" || token.starts_with("›") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    let out = out.trim_matches(|c: char| c.is_whitespace() || "-–—|:·.,".contains(c));
    out.chars().take(400).collect()
}

// ── RSS: the structured interface ────────────────────────────────────────────

/// Parse an RSS 2.0 / Atom answer into hits. Small on purpose: the two formats
/// we consume carry four fields, and a hand-rolled scanner keeps `quick-xml`
/// out of the product's dependency set for forty lines of work.
pub fn parse_rss(body: &str) -> Vec<Hit> {
    let mut out = Vec::new();
    for item in rss_items(body) {
        let title = tag_text(item, &["title"]).unwrap_or_default();
        let url = tag_text(item, &["link"])
            .or_else(|| atom_link(item))
            .unwrap_or_default();
        let snippet = tag_text(item, &["description", "summary", "content"]).unwrap_or_default();
        let published = tag_text(item, &["pubDate", "published", "updated"])
            .and_then(|d| super::verticals::rss_date_to_iso(&d));
        if !url.starts_with("http") || title.trim().is_empty() {
            continue;
        }
        out.push(Hit {
            title: strip_markup(&title),
            url,
            // A feed description can be the whole article. Capping here is not
            // cosmetic: merge keeps the LONGEST snippet, and the cross-encoder
            // pads every pair in a batch to the longest one -- one 10 KB
            // description turned a 40 ms rerank into 40 CPU-seconds.
            snippet: strip_markup(&snippet)
                .chars()
                .take(MAX_RSS_SNIPPET)
                .collect(),
            rank: out.len(),
            published,
        });
    }
    out
}

/// A feed summary long enough to carry a claim, short enough to batch.
const MAX_RSS_SNIPPET: usize = 400;

fn rss_items(body: &str) -> Vec<&str> {
    let mut items = Vec::new();
    for tag in ["item", "entry"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut from = 0;
        while let Some(start) = body[from..].find(&open) {
            let start = from + start;
            let Some(end) = body[start..].find(&close) else {
                break;
            };
            items.push(&body[start..start + end]);
            from = start + end + close.len();
        }
    }
    items
}

/// Inner text of the first of `names` present in `item`, CDATA unwrapped and
/// entities decoded. Attribute values are ignored: neither format puts a
/// title there.
fn tag_text(item: &str, names: &[&str]) -> Option<String> {
    for name in names {
        let open = format!("<{name}");
        let Some(pos) = item.find(&open) else {
            continue;
        };
        let after = &item[pos + open.len()..];
        // `<title>` or `<title lang="en">`; anything else is a different tag
        // whose name merely starts the same way.
        let body = match after.find('>') {
            Some(gt) if !after[..gt].starts_with(|c: char| c.is_alphanumeric()) => &after[gt + 1..],
            Some(gt) if after[..gt].trim().is_empty() => &after[gt + 1..],
            Some(gt) if after[..gt].starts_with(' ') || after[..gt].contains('=') => {
                &after[gt + 1..]
            }
            _ => continue,
        };
        let close = format!("</{name}>");
        let Some(end) = body.find(&close) else {
            continue;
        };
        return Some(decode_xml(&body[..end]));
    }
    None
}

/// Atom's `<link href="..."/>` since Atom has no link element body.
fn atom_link(item: &str) -> Option<String> {
    let pos = item.find("<link")?;
    let after = &item[pos..];
    let gt = after.find('>')?;
    let tag = &after[..gt];
    let href = tag.split("href=\"").nth(1)?;
    Some(decode_xml(href.split('"').next()?))
}

fn decode_xml(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix("<![CDATA[")
        .and_then(|r| r.strip_suffix("]]>"))
        .unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';').filter(|s| *s <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Feeds carry HTML in their summaries; the agent gets text.
fn strip_markup(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    for c in raw.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_serp_url_targets_the_wml_endpoint() {
        assert!(serp_url("google", "rust").unwrap().contains("/wml/search?"));
        // The WML parser must not claim desktop-layout SERP HTML.
        let desktop = r#"<div class="g"><a href="https://example.org/"><h3>Title</h3></a><div class="VwiEFb">Snippet</div></div>"#;
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
    fn a_sponsored_click_through_is_not_a_result() {
        let ad = "https://r.search.yahoo.com/;_ylt=A;RU=https%3a%2f%2fwww.bing.com%2faclick%3fld%3De8wiz8%26u%3daHR0cHM;RK=2";
        assert_eq!(decode_yahoo(ad), "");
        // A real page on a Bing-owned host that is not an ad redirect stays.
        let page =
            "https://r.search.yahoo.com/;RU=https%3a%2f%2fwww.bing.com%2fsearch%3fq%3drust;RK=2";
        assert!(decode_yahoo(page).starts_with("https://www.bing.com/search"));
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
    fn bing_decodes_live_unpadded_redirects() {
        // `u=` values from a live SERP (2026-09-26): URL-safe alphabet,
        // `a1` prefix, `=` padding stripped, so the last group can be
        // 2, 3 or 4 symbols long. One case per tail length.
        let ck = |u: &str| format!("https://www.bing.com/ck/a?!&&p=abc&ptn=3&ver=2&u=a1{u}&ntb=1");
        for (u, url) in [
            (
                "aHR0cHM6Ly9kb2NzLnJzL2Jhc2U2NC9sYXRlc3QvYmFzZTY0Lw",
                "https://docs.rs/base64/latest/base64/",
            ),
            (
                "aHR0cHM6Ly9kb2NzLnJzL2NyYXRlL2Jhc2U2NC9sYXRlc3Q",
                "https://docs.rs/crate/base64/latest",
            ),
            (
                "aHR0cHM6Ly9naXRodWIuY29tL21hcnNoYWxscGllcmNlL3J1c3QtYmFzZTY0",
                "https://github.com/marshallpierce/rust-base64",
            ),
        ] {
            assert_eq!(decode_bing(&ck(u)), url, "{u}");
        }
    }

    #[test]
    fn bing_decode_uses_the_url_safe_alphabet() {
        // `-` and `_` are 62 and 63 in base64url; the standard `+`/`/`
        // alphabet would reject both.
        assert_eq!(
            base64url_decode("aHR0cHM6Ly9leGFtcGxlLmNvbS9hP2I9fn5-Pg").as_deref(),
            Some("https://example.com/a?b=~~~>")
        );
        // The vertical-nav stubs in tests/fixtures/bing-serp.html decode
        // to a relative path, which decode_bing then drops.
        assert_eq!(
            base64url_decode("L2ltYWdlcy9zZWFyY2g_cT10ZXN0K3F1ZXJ5JkZPUk09SERSU0My").as_deref(),
            Some("/images/search?q=test+query&FORM=HDRSC2")
        );
        assert_eq!(
            decode_bing(
                "https://www.bing.com/ck/a?!&&p=x&u=a1L2ltYWdlcy9zZWFyY2g_cT10ZXN0K3F1ZXJ5JkZPUk09SERSU0My&ntb=1"
            ),
            ""
        );
    }

    #[test]
    fn bing_decode_is_lenient_like_the_hand_rolled_decoder() {
        // Padded and unpadded forms decode alike.
        assert_eq!(base64url_decode("aHR0cA==").as_deref(), Some("http"));
        assert_eq!(base64url_decode("aHR0cA").as_deref(), Some("http"));
        // Nonzero unused bits in the last symbol (`B` instead of `A`)
        // are ignored rather than rejected.
        assert_eq!(base64url_decode("aHR0cB").as_deref(), Some("http"));
        // Standard-alphabet symbols and invalid UTF-8 are rejected, as
        // before.
        assert_eq!(base64url_decode("aHR0cA+/"), None);
        assert_eq!(base64url_decode("_w"), None);
        // A lone trailing symbol carries only 6 bits and no encoder
        // emits one. The old decoder guessed a byte from it ("htt" +
        // 'p' here); now the input is refused.
        assert_eq!(base64url_decode("aHR0c"), None);
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

#[cfg(test)]
mod visible_text_guard_tests {
    use super::*;

    // #288's class, search side: a script/style subtree inside a SERP
    // cell is source, never snippet text.
    #[test]
    fn snippets_never_carry_script_text() {
        let doc =
            Html::parse_fragment("<div>Result title<script>window.r=1;</script> and body</div>");
        let root = doc.root_element();
        let el = root
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .next()
            .unwrap();
        let t = text(el);
        assert!(!t.contains("window.r"), "{t}");
        assert!(t.contains("Result title"), "{t}");
        assert!(t.contains("and body"), "{t}");
    }
}

#[cfg(test)]
mod structured_answer_tests {
    use super::*;

    #[test]
    fn a_structured_answer_needs_no_selector() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0"><channel><title>Bing: rust tokio</title>
<item><title>spawn_blocking in tokio::task</title>
  <link>https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html</link>
  <description>Use &amp; enjoy spawn_blocking.</description>
  <pubDate>Thu, 31 Jul 2026 07:00:00 GMT</pubDate></item>
<item><title>No date here</title><link>https://example.com/b?x=1&amp;y=2</link>
  <description><![CDATA[Use <b>spawn_blocking</b> for blocking work.]]></description></item>
</channel></rss>"#;
        let hits = parse_rss(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "spawn_blocking in tokio::task");
        assert_eq!(hits[0].published.as_deref(), Some("2026-07-31"));
        assert_eq!(hits[1].url, "https://example.com/b?x=1&y=2");
        assert!(
            hits[1].snippet.contains("spawn_blocking") && !hits[1].snippet.contains('<'),
            "markup is stripped, entities decoded: {:?}",
            hits[1].snippet
        );
        assert!(hits[1].published.is_none());
    }

    fn shaped_serp(prefix: &str) -> String {
        let item = |slug: &str, title: &str, text: &str| {
            format!(
                r#"<div class="{prefix}-item"><h3 class="{prefix}-title">
                <a href="{slug}">{title}</a></h3><p class="{prefix}-text">{text}</p></div>"#
            )
        };
        format!(
            r#"<html><body><nav class="{prefix}-nav"><a href="/next">Next page</a>
            <a href="/about">About us</a></nav><main class="{prefix}-main"><div class="{prefix}-list">
            {a}{b}{c}</div></main></body></html>"#,
            a = item(
                "https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html",
                "spawn_blocking in tokio::task",
                "Use spawn_blocking for short-lived blocking operations."
            ),
            b = item(
                "https://users.rust-lang.org/t/spawn-blocking/85990",
                "spawn_blocking blocks the thread",
                "A forum thread about blocking calls in async code."
            ),
            c = item(
                "https://tokio.rs/tokio/tutorial/spawning",
                "Tokio tutorial: spawning",
                "How the runtime spawns work and blocking tasks."
            ),
        )
    }

    #[test]
    fn shape_reading_survives_a_restyle() {
        // Two documents with identical content and no shared class name: the
        // commonest way a scraped engine dies is a restyle, and shape reading
        // must not care. This is the whole point of the fallback.
        let base = "https://www.example-search.test/search?q=rust+tokio";
        let one = shape_extract(&shaped_serp("s"), base);
        let two = shape_extract(&shaped_serp("z9-kx"), base);
        assert_eq!(one.len(), 3, "three results: {one:#?}");
        assert_eq!(two.len(), 3, "three results: {two:#?}");
        let urls = |v: &[Hit]| v.iter().map(|h| h.url.clone()).collect::<Vec<_>>();
        assert_eq!(urls(&one), urls(&two));
        assert_eq!(one[0].title, "spawn_blocking in tokio::task");
        assert!(
            one[0].snippet.starts_with("Use spawn_blocking"),
            "snippet is the block's own text: {:?}",
            one[0].snippet
        );
        // Navigation on the SERP's own host is never a result.
        assert!(one.iter().all(|h| !h.url.contains("example-search.test")));
    }

    #[test]
    fn shape_reading_decodes_a_wrapped_result_url() {
        let body = format!(
            r#"<html><body><div><div><h3><a href="https://lite.duckduckgo.com/lite/?q=x&amp;uddg={}">spawn_blocking in tokio</a></h3>
            <p>Use it for blocking work.</p></div></div></body></html>"#,
            "https%3A%2F%2Fdocs.rs%2Ftokio%2Flatest%2Ftokio%2Ftask%2Ffn.spawn_blocking.html"
        );
        let hits = shape_extract(&body, "https://lite.duckduckgo.com/lite/?q=x");
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert_eq!(
            hits[0].url,
            "https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html"
        );
    }
}
