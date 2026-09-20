//! JSON-in-script content miner (JS-data).
//!
//! Modern SPAs (Next.js, Nuxt, Remix, React App, YouTube, Medium)
//! render client-side, but they embed their full content as a JSON
//! blob assigned to a JS global or inside a
//! `<script type="application/json">`/ld+json tag. Tier 1 can't run
//! the JS, but it CAN parse that blob : turning an empty SPA shell
//! into real content without a browser. This is the single biggest
//! tier-1 unlock: YouTube, GitHub, and every Next.js site drops
//! its data in the HTML.
//!
//! Strategy:
//!   1. Find candidate JSON blobs (known globals + typed script tags).
//!   2. Parse each as JSON (tolerant).
//!   3. Recursively walk, collecting content-bearing strings,
//!      scored by path key + prose-like shape.
//!   4. Filter / dedupe / rank, render as clean markdown.
//!   5. Return `None` when no meaningful content is recovered.
//!
//! Runs only as a rescue: when the normal pipeline produced a thin
//! shell, this mines the embedded data and, if richer, wins.

use scraper::Html;
use serde_json::Value;

use super::{ContentKind, ExtractOptions, Extracted};

/// Known JS globals that hold a JSON document value. Some sites
/// assign with `var NAME = ...`, others `window.NAME = ...`.
const KNOWN_GLOBALS: &[&str] = &[
    "ytInitialData",
    "ytInitialPlayerResponse",
    "__NEXT_DATA__",
    "nextData",
    "__NUXT__",
    "window.__NUXT__",
    "__APOLLO_STATE__",
    "__INITIAL_STATE__",
    "__PRELOADED_STATE__",
    "__ASYNC_LOADING_STATE__",
    "__REMARKS_STATE__",
    "__STATIC_ERRORS__",
    "window.__INITIAL_DATA__",
    "preloadState",
    "initialState",
    "appSettings",
    "window.APPLICATION_STATE",
    "window.BOOTCAMP_DATA",
    "window.PAYPAL_CHOOSE_JS__",
    "window.__myCodeIgniterData",
    "window.__DIFFICULTY_DATA__",
];

/// Path fragments that strongly mark real content (boost score).
const GOOD_KEYS: &[&str] = &[
    "title",
    "name",
    "headline",
    "children",
    "markup",
    "description",
    "shortdescription",
    "overview",
    "summary",
    "abstract",
    "readme",
    "body",
    "content",
    "text",
    "plaintext",
    "markdown",
    "articlebody",
    "articletext",
    "subtitle",
    "excerpt",
    "intro",
    "introduction",
    "conclusion",
    "details",
    "transcript",
    "lyrics",
    "caption",
    "quote",
    "review",
    "snippet",
    "catchline",
    "surtitle",
    "ebyline",
    "about",
    "bio",
    "profile_bio",
    "answer",
    "question",
    "explanation",
    "tagline",
    "message",
    "descriptiontext",
    "repositorydescription",
    "objectives",
    "requirements",
    "responsibilities",
    "qualifications",
];

/// Path fragments that mark configuration / identity noise (penalty).
const BAD_KEYS: &[&str] = &[
    "id",
    "guid",
    "url",
    "uri",
    "href",
    "src",
    "srcset",
    "permalink",
    "thumbnail",
    "avatar",
    "icon",
    "logo",
    "image",
    "imageurl",
    "background",
    "bgimage",
    "css",
    "javascript",
    "bundle",
    "chunk",
    "token",
    "secret",
    "apikey",
    "accesstoken",
    "csrf",
    "xsrf",
    "signature",
    "fingerprint",
    "endpoint",
    "api",
    "query",
    "mutation",
    "fragment",
    "relay",
    "typename",
    "__typename",
    "props",
    "style",
    "trackingparams",
    "clicktracking",
    "continuation",
    "ctoken",
    "clicktrackingparams",
    "attributes",
    "badges",
    "chips",
    "emoji",
    "emoticons",
    "timestamp",
    "timesec",
    "lengthseconds",
    "duration",
    "starttime",
    "endtime",
    "expiry",
    "date",
    "datetime",
    "uuid",
    "videoid",
    "playlistid",
    "channelid",
    "thumbnailoverlay",
    "tracingvector",
    "cornernedges",
    "accessibility",
    "arialabel",
    "tooltip",
    "button",
    "likes",
    "dislike",
    "subscribe",
    "notification",
    "settings",
    "mnemonic",
    "hotkeydialog",
    "topbar",
    "leftcontrols",
    "playeroverlay",
    "chipcloud",
    "teaser",
    "carousel",
    "shelf",
    "compact",
    "lockup",
    "reel",
    "short",
    "grid",
    "menu",
    "dropdown",
    "watchnext",
    "relatedvideos",
    "secondaryvideo",
    "suggestion",
    "recommendation",
];

/// Content-bearing key names treated as "title-like" (rendered as a
/// heading when present).
const TITLE_KEYS: &[&str] = &["title", "name", "headline", "namewithowner", "fullname"];

/// Layout-independent selectors for JSON globals / typed scripts.
struct Blob {
    /// JSON text (still raw, may be HTML-entity-encoded by the DOM).
    raw: String,
}

/// Entry point. Returns `Some` only when meaningful content was
/// recovered from embedded JSON.
pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() || opts.toc {
        return None;
    }

    let blobs = find_blobs(html);
    if blobs.is_empty() {
        return None;
    }

    let mut items: Vec<Item> = Vec::new();
    let mut order: usize = 0;
    for blob in &blobs {
        let text = unescape(&blob.raw);
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            walk(&value, "", &mut items, &mut order, 0);
        }
    }
    if items.is_empty() {
        return None;
    }

    // Score each candidate; generous lower bound : prose with any
    // content key already earned points. Config noise is penalized
    // below the threshold.
    let mut kept: Vec<Item> = items
        .into_iter()
        .filter(|i| i.score >= 2.0 && i.text.trim().chars().count() >= 25)
        .collect();
    if kept.is_empty() {
        return None;
    }

    // Dedupe near-identical strings (title appears in several
    // blobs). Keep best score + longest.
    dedupe(&mut kept);

    // Reject trivial blobs that only yielded a couple fragments.
    let total_len: usize = kept.iter().map(|i| i.text.len()).sum();
    if total_len < 400 {
        return None;
    }

    // Render markdown.
    let (md, title) = render(&kept, url);

    let total = md.len();
    let (slice, next) = paginate(&md, opts);
    Some(Extracted {
        markdown: slice,
        title,
        byline: None,
        published: None,
        site: extract_site(url),
        total_chars: total,
        next_offset: next,
        blocks_total: kept.len(),
        blocks_shown: kept.len(),
        tokens_est: total / 4,
        thin: total < 800,
        content_kind: ContentKind::Page,
        lang: guess_lang(&md),
        quality: 0.7,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
    })
}

// ── Blob discovery ─────────────────────────────────────────────

/// Find JSON blobs: known global assignments and typed script tags.
fn find_blobs(html: &str) -> Vec<Blob> {
    let mut out: Vec<Blob> = Vec::new();

    // 1. Known JS globals: `var NAME = <json>` or `window.NAME = <json>`.
    //
    // A failed value scan runs to the end of the document (no brace
    // after the marker, or one that never closes), and the marker is
    // page text: a page repeating `window.__NEXT_DATA__ = ` with no
    // value made every occurrence rescan the rest, N × the document.
    // A real page has a handful of markers, each scanned once, so a
    // budget of a few document lengths covers it; past it the
    // remaining markers are left alone.
    let mut budget = html.len().saturating_mul(SCAN_BUDGET_MULTIPLIER);
    'keys: for key in KNOWN_GLOBALS {
        let search = format!("{key} = ");
        let mut from = 0;
        while let Some(idx) = html[from..].find(&search) {
            let start = from + idx + search.len();
            let (found, scanned) = extract_js_value_counted(&html[start..]);
            budget = budget.saturating_sub(scanned);
            if let Some((raw, consumed)) = found {
                if !out.iter().any(|b: &Blob| b.raw == raw) {
                    out.push(Blob { raw });
                }
                from = start + consumed;
            } else if budget == 0 {
                break 'keys;
            } else {
                // Advance to the next CHAR boundary (a replacement
                // char is 3 bytes; a naive +1 slices mid-char), and
                // never past the end (start can be html.len()).
                let mut n = start + 1;
                while n < html.len() && !html.is_char_boundary(n) {
                    n += 1;
                }
                from = n.min(html.len());
            }
        }
    }

    // 2. Typed script tags carrying raw JSON.
    for ty in ["application/json", "application/ld+json"] {
        for raw in find_typed_bodies(html, ty) {
            let raw: String = raw.to_string();
            if !out.iter().any(|b: &Blob| b.raw == raw) {
                out.push(Blob { raw });
            }
        }
    }

    // 3. GitHub-style `data-target="react-*.embeddedData"` scripts
    // (carry the repo payload + rendered README, no type attr).
    for raw in find_data_target(html, "embeddedData") {
        let raw: String = raw.to_string();
        if !out.iter().any(|b: &Blob| b.raw == raw) {
            out.push(Blob { raw });
        }
    }

    // 4. Modern Next.js (>=13): `self.__next_f.push([k,"..."])`
    // streaming-RSC flight frames. These carry the FULL rendered
    // page (paragraphs under `children`) : the single biggest
    // modern-SPA unlock.
    for frame in find_next_f(html) {
        // Frame is `[k]:"value"` : strip the key prefix so the
        // remainder is a standalone JSON array/object.
        let json = strip_frame_key(&frame);
        if !json.is_empty()
            && json.starts_with(['[', '{'])
            && !out.iter().any(|b: &Blob| b.raw == json)
        {
            out.push(Blob { raw: json });
        }
    }

    out
}

/// Collect decoded `self.__next_f.push([k, "..."])` string values.
fn find_next_f(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "self.__next_f.push([";
    let mut from = 0usize;
    while from < html.len() {
        let Some(rel) = html[from..].find(needle) else {
            break;
        };
        let body_start = from + rel + needle.len();
        let rest = &html[body_start..];
        // `[k,"` : skip to the opening quote.
        let Some(q) = rest.find('"') else { break };
        // Scan the JS string literal (\" and \\ are escapes).
        let bytes = rest.as_bytes();
        let mut esc = false;
        let mut i = q + 1;
        let end = loop {
            if i >= bytes.len() {
                break rest.len();
            }
            let b = bytes[i];
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                break i;
            }
            i += 1;
        };
        let inner = &rest[q + 1..end];
        let decoded = js_unescape(inner);
        if decoded.len() > 8 {
            out.push(decoded);
        }
        from = body_start + end + 1;
    }
    out
}

/// Decode a JavaScript string literal: `\uXXXX`, `\xXX`, `\\`,
/// `\"`, `\n`, `\r`, `\t`, `\/`, `\b`, `\f`, `\0`.
fn js_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'\\' {
            // Copy the full UTF-8 char.
            let ch_len = utf8_len(b[i]);
            let end = (i + ch_len).min(b.len());
            out.push_str(&s[i..end]);
            i = end;
            continue;
        }
        // Escape sequence.
        let j = i + 1;
        if j >= b.len() {
            break;
        }
        match b[j] {
            b'u' => {
                let hex = s.get(j + 1..j + 5);
                if let Some(h) = hex
                    && let Ok(cp) = u32::from_str_radix(h, 16)
                {
                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    i = j + 5;
                } else {
                    out.push('\\');
                    i = j;
                }
            }
            b'x' => {
                let hex = s.get(j + 1..j + 3);
                if let Some(h) = hex
                    && let Ok(cp) = u32::from_str_radix(h, 16)
                {
                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    i = j + 3;
                } else {
                    out.push('\\');
                    i = j;
                }
            }
            b'n' => {
                out.push('\n');
                i = j + 1;
            }
            b'r' => {
                out.push('\r');
                i = j + 1;
            }
            b't' => {
                out.push('\t');
                i = j + 1;
            }
            b'b' => {
                out.push('\u{8}');
                i = j + 1;
            }
            b'f' => {
                out.push('\u{c}');
                i = j + 1;
            }
            b'/' => {
                out.push('/');
                i = j + 1;
            }
            _ => {
                // Unknown escape. If the byte after the backslash is
                // a multi-byte UTF-8 lead, advancing to j+1 would land
                // mid-character and the next `&s[i..end]` slice would
                // panic on a hostile page (`\é` inside a flight
                // frame) : copy the full char instead.
                let ch_len = utf8_len(b[j]);
                let end = (j + ch_len).min(b.len());
                if ch_len == 1 {
                    out.push(b[j] as char);
                    i = j + 1;
                } else {
                    out.push_str(&s[j..end]);
                    i = end;
                }
            }
        }
    }
    out
}

fn utf8_len(b0: u8) -> usize {
    if b0 < 0x80 {
        1
    } else if b0 >> 5 == 0b110 {
        2
    } else if b0 >> 4 == 0b1110 {
        3
    } else if b0 >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Strip `[k]:` / `k:` key prefix when the rest is JSON.
fn strip_frame_key(frame: &str) -> String {
    let t = frame.trim();
    let Some(idx) = t.find(':') else {
        return t.to_string();
    };
    let rest = t[idx + 1..].trim();
    if rest.starts_with(['[', '{']) {
        rest.to_string()
    } else {
        t.to_string()
    }
}

/// Find `<script data-target="...embeddedData">` bodies.
fn find_data_target<'a>(html: &'a str, suffix: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let needle = "data-target=\"";
    let mut from = 0usize;
    while from < html.len() {
        let Some(lt) = html[from..].find("<script") else {
            break;
        };
        let tag_start = from + lt;
        let Some(tag_end_rel) = html[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_end_rel;
        let tag = &html[tag_start..tag_end];
        if tag.contains(needle) && tag.contains(suffix) {
            let body_start = tag_end + 1;
            let rest = &html[body_start..];
            if let Some(cs) = rest.find("</script>") {
                let raw = &rest[..cs];
                if !raw.trim().is_empty() {
                    out.push(raw);
                }
            }
        }
        from = tag_end + 1;
    }
    out
}

/// Find `<script ... type="TYP">` open tags and return each
/// raw JSON body (trimmed).
fn find_typed_bodies<'a>(html: &'a str, ty: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let needle = format!(r#"type="{ty}""#);
    let mut from = 0usize;
    while from < html.len() {
        let Some(lt) = html[from..].find("<script") else {
            break;
        };
        let tag_start = from + lt;
        let Some(tag_end_rel) = html[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_end_rel;
        if html[tag_start..tag_end].contains(&needle) {
            let body_start = tag_end + 1;
            let rest = &html[body_start..];
            if let Some(cs) = rest.find("</script>") {
                let raw = &rest[..cs];
                if !raw.trim().is_empty() {
                    out.push(raw);
                }
            }
        }
        from = tag_end + 1;
    }
    out
}

/// Extract a balanced JS object/array starting at `s`. Handles
/// strings, escapes, and nested brackets. Returns the JSON-slice.
/// Returns (raw_json, consumed) where `consumed` is the byte
/// position in `s` AFTER the closing bracket. The caller needs
/// this to advance past the value, not into it: using the raw
/// string's length alone misses bytes between the search match
/// and the opening bracket, which can land mid-character.
/// Bytes the global-assignment scan may examine per document, as a
/// multiple of the document length.
const SCAN_BUDGET_MULTIPLIER: usize = 4;

/// The first balanced `{…}`/`[…]` after the marker, with the number
/// of bytes examined so the caller can bound the total work over
/// many markers.
fn extract_js_value_counted(s: &str) -> (Option<(String, usize)>, usize) {
    let bytes = s.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' || b == b'[' {
            start = Some(i);
            break;
        }
    }
    let Some(start) = start else {
        return (None, bytes.len());
    };
    let open = bytes[start];
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let close = if open == b'{' { b'}' } else { b']' };
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && b == close {
                    let raw = &s[start..=i];
                    return (Some((raw.to_string(), i + 1)), i + 1);
                }
            }
            _ => {}
        }
    }
    (None, bytes.len())
}

// ── JSON walk ─────────────────────────────────────────────────

struct Item {
    text: String,
    score: f32,
    order: usize,
    title_like: bool,
}

fn walk(value: &Value, path: &str, out: &mut Vec<Item>, order: &mut usize, depth: usize) {
    if depth > 24 {
        return;
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let np = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                walk(v, &np, out, order, depth + 1);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter().take(400) {
                walk(v, path, out, order, depth + 1);
            }
        }
        Value::String(s) => {
            let (score, title_like) = score_string(s, path);
            if score >= 1.5 {
                let ord = *order;
                *order += 1;
                out.push(Item {
                    text: s.clone(),
                    score,
                    order: ord,
                    title_like,
                });
            }
        }
        _ => {}
    }
}

/// Score a string as page content by shape + path.
fn score_string(s: &str, path: &str) -> (f32, bool) {
    let t = s.trim();
    if t.is_empty() {
        return (0.0, false);
    }
    // Hard noise structures: CSS/JS/JSON braces, stylesheet blocks,
    // `key=value` fragments, url(…) / rgb(…) tokens. Real prose
    // (even code snippets) is free of these.
    if t.contains(['{', '}']) {
        return (0.0, false);
    }
    if t.contains("@media")
        || t.contains(":root")
        || t.contains("@font-face")
        || t.contains("url(")
        || t.contains("px;")
        || t.contains("rgb(")
        || t.contains("var(--")
    {
        return (0.0, false);
    }
    let low = t.to_lowercase();
    if (low.contains("noopener") || low.contains("noreferrer") || low.contains("nofollow"))
        && !t.contains(' ')
    {
        return (0.0, false);
    }
    if looks_like_attr(t) {
        return (0.0, false);
    }
    let lower = path.to_lowercase();

    // Hard rejects: pure URLs, image-ish data, hex/base64 blobs.
    if t.contains("://") || t.starts_with("data:") {
        return (0.0, false);
    }
    let len = t.chars().count();
    // Long single-token blobs (base64, opaque IDs, tracking tokens) :
    // reject unless the path is a real content key AND it reads
    // as prose (has spaces).
    if !t.contains(' ') && len >= 40 {
        return (0.0, false);
    }
    let letters: usize = t.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let tot: usize = t.chars().count();
    if letters == 0 || (letters as f32 / tot.max(1) as f32) < 0.25 {
        return (0.0, false); // numeric/emoji/data
    }

    let mut score: f32 = 0.0;
    let mut title_like = false;

    // Strong signal: path hits a content key.
    let mut key_hit = false;
    for key in GOOD_KEYS {
        if lower.contains(key) {
            score += 2.0;
            key_hit = true;
            if TITLE_KEYS.contains(key) {
                title_like = true;
            }
            break;
        }
    }
    for key in BAD_KEYS {
        if lower.contains(key) {
            score -= 3.0;
            break;
        }
    }

    // Shape signals (secondary : only matter with a key hit).
    if t.contains(' ') {
        score += 1.0;
    }
    if t.contains(['.', ',', '!', '?', ':']) {
        score += 0.5;
    }
    let len = t.chars().count();
    if len >= 120 {
        score += 1.0;
    } else if len >= 60 {
        score += 0.5;
    }

    // YouTube/structured-data sections earn a bonus (clean metadata).
    if lower.contains("microformat") || lower.contains("primaryinfo") {
        score += 0.5;
    }

    // Without a real content-key hit, only very long prose-shaped
    // strings survive : short buttons/aria labels stay out.
    if !key_hit {
        // Requires an actual sentence-shaped string: >= 60 chars,
        // words separated by spaces, with sentence punctuation.
        if !(len >= 60 && t.contains(' ') && (t.contains('.') || t.contains('?')))
            || t.split_whitespace().count() < 6
        {
            return (0.0, false);
        }
    }

    (score.max(0.0), title_like)
}

/// `key=value[_]key=value` attribute fragments (viewports, meta
/// charset) : no prose. Only fires when before the first space the
/// token is a bare `key=value` pair.
fn looks_like_attr(t: &str) -> bool {
    let first = t
        .split(|c: char| c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    let Some(eq) = first.find('=') else {
        return false;
    };
    eq >= 2
        && first[eq + 1..].len() >= 2
        && first[..eq]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn dedupe(items: &mut Vec<Item>) {
    let normed: Vec<String> = items
        .iter()
        .map(|i| {
            i.text
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .to_lowercase()
        })
        .collect();
    let mut seen: Vec<String> = Vec::new();
    let mut keep = vec![true; items.len()];
    for (i, norm) in normed.iter().enumerate() {
        if norm.is_empty() || seen.iter().any(|s| s == norm) {
            keep[i] = false;
        } else {
            seen.push(norm.clone());
        }
    }
    let mut idx = 0;
    items.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
    items.sort_by_key(|i| i.order);
}

// ── Render ─────────────────────────────────────────────────────

fn render(items: &[Item], url: &str) -> (String, Option<String>) {
    let mut md = String::new();
    let mut title: Option<String> = None;

    // Prefer a short, title-like fragment for the heading.
    for it in items {
        if it.title_like && it.text.chars().count() <= 200 {
            let clean = strip_html(&it.text);
            if !clean.is_empty() {
                title = Some(clean.clone());
                md.push_str(&format!("# {clean}\n\n"));
                break;
            }
        }
    }
    if title.is_none() {
        // Fallback: derive from URL path.
        if let Ok(u) = url::Url::parse(url) {
            let last = u
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or_default();
            if !last.is_empty() && last != "/" {
                let t = last.replace(['-', '+', '_'], " ");
                title = Some(t.clone());
                md.push_str(&format!("# {t}\n\n"));
            }
        }
    }

    let mut emitted = 0usize;
    for it in items {
        if it.title_like && title.is_some() {
            continue; // heading already shown
        }
        let clean = strip_html(&it.text);
        let clean = clean.trim();
        if clean.chars().count() < 25 {
            continue;
        }
        md.push_str(clean);
        md.push_str("\n\n");
        emitted += 1;
        if emitted >= 240 || md.len() > 60_000 {
            break;
        }
    }

    (md, title)
}

/// Strip HTML tags from a string (embedded HTML in JSON).
fn strip_html(s: &str) -> String {
    if !s.contains('<') || !s.contains('>') {
        return s.to_string();
    }
    let doc = Html::parse_fragment(s);
    let text: String = doc
        .select(&scraper::Selector::parse("body").unwrap())
        .next()
        .map(|b| b.text().collect())
        .unwrap_or_default();
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

/// HTML-entity-unescape the raw JSON (DOM encodes it).
fn unescape(raw: &str) -> String {
    // The blob may contain &quot; &amp; &lt; &gt; &#39; : decode the
    // common ones (serde_json decodes the rest).
    let mut s = raw.to_string();
    for (from, to) in [
        ("&quot;", "\""),
        ("&#34;", "\""),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&apos;", "'"),
    ] {
        s = s.replace(from, to);
    }
    s
}

fn extract_site(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
}

fn guess_lang(md: &str) -> String {
    // Light heuristic: if it contains CJK, say so; else "en".
    if md.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
        "zh".into()
    } else if md.chars().any(|c| ('\u{ac00}'..='\u{d7af}').contains(&c)) {
        "ko".into()
    } else if md.chars().any(|c| ('\u{3040}'..='\u{30ff}').contains(&c)) {
        "ja".into()
    } else {
        "en".into()
    }
}

/// Apply caller's max_chars/offset (shared shape with DonSift).
fn paginate(full: &str, opts: &ExtractOptions) -> (String, Option<usize>) {
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let offset = opts.offset;
    let chars: Vec<char> = full.chars().collect();
    if offset >= chars.len() {
        return (String::new(), None);
    }
    let end = offset.saturating_add(max).min(chars.len());
    let slice: String = chars[offset..end].iter().collect();
    let next = if end < chars.len() { Some(end) } else { None };
    (slice, next)
}

#[cfg(test)]
mod tests {
    /// Fuzzer find (CI, 2026-08-22): a global-assignment match at
    /// the END of input advanced `from` past the string and/or
    /// mid-replacement-char : `html[from..]` panicked. The advance
    /// now floors to the next char boundary, clamped to len.
    #[test]
    fn global_match_at_end_of_input_no_panic() {
        // crash-652047c9: 0x97 lead byte (→ U+FFFD) + "[ndow.__NUXT__ = "
        let html = String::from_utf8_lossy(&[
            0x97, b'[', b'n', b'd', b'o', b'w', b'.', b'_', b'_', b'N', b'U', b'X', b'T', b'_',
            b'_', b' ', b'=', b' ',
        ])
        .into_owned();
        let _ = find_blobs(&html);
    }

    // Every marker with no value behind it rescanned the rest of the
    // document: 512 KiB of them was minutes.
    #[test]
    fn repeated_valueless_markers_are_bounded_by_the_scan_budget() {
        let html = "window.__NEXT_DATA__ = ".repeat(512 * 1024 / 23);
        let started = std::time::Instant::now();
        let blobs = find_blobs(&html);
        assert!(blobs.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
        // The same markers with a value at the end: the value is
        // still found, once, before the budget matters.
        let html = format!("{}{{\"a\":1}}", "window.__NEXT_DATA__ = ".repeat(1000));
        let blobs = find_blobs(&html);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].raw, "{\"a\":1}");
    }

    #[test]
    fn global_match_inside_multibyte_no_panic() {
        let html = "x\u{FFFD}__NUXT__ = \u{FFFD}tail";
        let _ = find_blobs(html);
    }

    /// Fuzzer find (CI, 2026-08-25): `__NUXT__ = ` followed by
    /// invalid UTF-8 (→ U+FFFD) then `c<>[]`. The `[` and `]` form
    /// a valid JSON array, so `extract_js_value` returns "[]" (2
    /// bytes). But `from = start + raw_len` = 37 + 2 = 39, which
    /// is inside the U+FFFD at bytes 37..40. The fix: advance by
    /// the consumed byte count (position after `]` in the slice),
    /// not the raw string's length.
    #[test]
    fn global_match_with_gap_before_bracket_no_panic() {
        let html = String::from_utf8_lossy(&[
            0x03, 0x00, 0x00, 0x09, b')', b'<', b'<', b'>', b'[', b'c', b'<', b'>', b'[', b']',
            0x09, b')', b'<', b'/', b'C', b'<', b'>', b'[', b']', b'/', 0x01, b'.', b'_', b'_',
            b'N', b'U', b'X', b'T', b'_', b'_', b' ', b'=', b' ', 0xff, 0x56, 0xbf, 0xbf, 0xff,
            0xff, 0xff, 0xff, 0x28, 0xc2, 0xff, 0x18, 0x8b, b'c', b'<', b'>', b'[', b']', 0x09,
            b')', 0xbf, 0xbf, 0xbf, b'<', b'/', b'C', 0xbf, b'<',
        ])
        .into_owned();
        let _ = find_blobs(&html);
    }

    use super::*;
    use crate::extract::ExtractOptions;

    fn opts() -> ExtractOptions {
        ExtractOptions {
            selector: None,
            toc: false,
            max_chars: None,
            offset: 0,
            focus: None,
            include_links: false,
            include_media: false,
            section: None,
            ..Default::default()
        }
    }

    #[test]
    fn js_unescape_handles_unicode_and_quotes() {
        assert_eq!(js_unescape("a\\u003cb\\u003e"), "a<b>");
        assert_eq!(js_unescape("say \\\"hi\\\""), "say \"hi\"");
        assert_eq!(js_unescape("line1\\nline2"), "line1\nline2");
        assert_eq!(js_unescape("\\\\ path"), "\\ path");
        assert_eq!(js_unescape("\\u00e9tude"), "étude");
        assert_eq!(js_unescape("plain"), "plain");
    }

    #[test]
    fn js_unescape_backslash_before_multibyte_char() {
        // Hostile/sloppy page: a literal backslash before a multi-byte
        // UTF-8 char. The catch-all escape arm used to advance to a
        // non-char-boundary index and panic on the next slice.
        assert_eq!(js_unescape("\\é"), "é");
        assert_eq!(js_unescape("\\末日乐园"), "末日乐园");
        assert_eq!(js_unescape("a\\éb"), "aéb");
        assert_eq!(js_unescape("\\🚀x"), "🚀x");
    }

    #[test]
    fn paginate_huge_max_chars_no_panic() {
        let full = "段落一\n\n段落二\n\n段落三".repeat(100);
        let o = ExtractOptions {
            max_chars: Some(usize::MAX - 5),
            offset: 1,
            ..opts()
        };
        let (slice, _) = paginate(&full, &o);
        assert!(
            !slice.is_empty(),
            "saturating add must not wrap end < start"
        );
    }

    #[test]
    fn find_next_f_collects_frames() {
        // Real frames are long JSON arrays (>{:?} chars) - short
        // fragments are intentionally dropped as micro-junk.
        let html = r#"<script>self.__next_f.push([1,"52:[\"$\",\"p\",null,{\"children\":\"Hello from the server\"}]"]);self.__next_f.push([2,"61:[\"$\",\"h2\",null,{\"children\":\"A second paragraph of text\"}]"]);</script>"#;
        let frames = find_next_f(html);
        assert_eq!(frames.len(), 2, "two push frames decoded");
        assert!(
            frames[0].contains("Hello from the server"),
            "frame 0: {}",
            frames[0]
        );
        assert!(
            frames[1].contains("A second paragraph"),
            "frame 1: {}",
            frames[1]
        );
    }

    #[test]
    fn extract_wins_from_next_f_rsc_frames() {
        // A Next.js SPA shell whose body is empty but whose RSC
        // flight frames carry the article prose (>= 400 chars so
        // the real threshold passes).
        let para = |n: usize| {
            format!(
                "This is body paragraph number {} of the article -- containing enough substantial prose across several sentences that it reads as genuine written content rather than a short label or fragment.",
                n
            )
        };
        let mut body = String::new();
        for i in 0..6 {
            body.push_str(&format!(
                r#"self.__next_f.push([{},"{}:[\"$\",\"p\",null,{{\"children\":\"{}\"}}]"]);"#,
                i + 1,
                i + 50,
                para(i)
            ));
        }
        let html = format!(
            r#"<html><head><title>My Next Page</title></head><body><div id="root"></div><script>{body}</script></body></html>"#
        );
        let res = extract(&html, "https://example.com/post/1", &opts());
        let e = res.expect("jsdata should extract RSC content");
        assert!(e.total_chars >= 400, "has body: {}", e.total_chars);
        let p0: String = para(0).chars().take(30).collect();
        let p5: String = para(5).chars().take(30).collect();
        assert!(e.markdown.contains(&p0), "first paragraph present");
        assert!(
            e.markdown.contains(&p5),
            "later paragraph present (ordering/dedupe intact)"
        );
    }

    #[test]
    fn extract_none_on_pure_shell_without_json() {
        // A plain static HTML body : jsdata must not claim it.
        let html = "<html><body><p>Just a normal server-side page, no embedded JSON anywhere in this document.</p></body></html>";
        assert!(extract(html, "https://example.com/x", &opts()).is_none());
    }

    #[test]
    fn extract_none_when_only_config_noise() {
        // Embedded JSON with only ids/urls : must not produce
        // "content". Ensure we don't fabricate prose from config.
        let html = r#"<script type="application/json">{"sessionId":"abc123","tracking":{"url":"/api/t","nonce":"deadbeefdecafbad"}}</script>"#;
        assert!(extract(html, "https://example.com/a", &opts()).is_none());
    }

    #[test]
    fn score_rejects_css_and_attrs() {
        assert_eq!(
            score_string(
                "@media (prefers-color-scheme:dark){body{color:#000}}",
                ".footer.style"
            ),
            (0.0, false)
        );
        assert_eq!(
            score_string("width=device-width,initial-scale=1.0", ".viewport"),
            (0.0, false)
        );
        // A real title under a title path scores positive + title_like.
        let (score, titley) = score_string("Making Navigations Instant in v0", ".flattened.title");
        assert!(score > 2.0, "title scores: {score}");
        assert!(titley, "title flagged");
    }

    #[test]
    fn dedupe_keeps_distinct_content() {
        let mut items = vec![
            Item {
                text: "short description".into(),
                score: 4.0,
                order: 0,
                title_like: true,
            },
            Item {
                text: "a genuinely different long body of text that should survive".into(),
                score: 5.0,
                order: 1,
                title_like: false,
            },
            Item {
                text: "short description".into(),
                score: 4.0,
                order: 2,
                title_like: true,
            },
        ];
        dedupe(&mut items);
        assert_eq!(items.len(), 2, "exact dup removed, distinct kept");
    }

    #[test]
    fn strip_frame_key_passes_through_plain_json() {
        assert_eq!(strip_frame_key("[\"$\",\"p\"]"), "[\"$\",\"p\"]");
        assert_eq!(strip_frame_key("12:{\"a\":1}"), "{\"a\":1}");
    }
}
