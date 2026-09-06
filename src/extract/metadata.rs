//! Page metadata: title, byline, published, site, base URL.

use scraper::Html;

pub struct Meta {
    pub title: Option<String>,
    pub byline: Option<String>,
    pub published: Option<String>,
    pub site: Option<String>,
    /// Meta description or og:description : a one-line summary
    /// agents can use to decide relevance without reading.
    pub description: Option<String>,
    /// Canonical URL from <link rel="canonical"> : the
    /// authoritative version of the page (dedup signal).
    #[allow(dead_code)]
    pub canonical: Option<String>,
}

pub fn base_url(doc: &Html) -> Option<String> {
    let sel = scraper::Selector::parse("base[href]").ok()?;
    doc.select(&sel)
        .next()
        .and_then(|b| b.value().attr("href"))
        .map(|s| s.to_string())
}

pub fn metadata(doc: &Html) -> Meta {
    let title = meta_attr(doc, "meta[property='og:title']", "content")
        .or_else(|| meta_attr(doc, "meta[name='twitter:title']", "content"))
        .or_else(|| {
            let sel = scraper::Selector::parse("title").ok()?;
            doc.select(&sel)
                .next()
                .map(|t| t.text().collect::<String>().trim().to_string())
        })
        .filter(|t| !t.is_empty());

    let byline = meta_attr(doc, "meta[name='author']", "content")
        .or_else(|| meta_attr(doc, "meta[property='article:author']", "content"))
        .or_else(|| json_ld_find(doc, "\"author\"", "\"name\""));

    let published = meta_attr(doc, "meta[property='article:published_time']", "content")
        .or_else(|| meta_attr(doc, "meta[name='date']", "content"))
        .or_else(|| meta_attr(doc, "meta[itemprop='datePublished']", "content"))
        .or_else(|| {
            let sel = scraper::Selector::parse("time[datetime]").ok()?;
            doc.select(&sel)
                .next()
                .and_then(|t| t.value().attr("datetime"))
                .map(|s| s.to_string())
        })
        .or_else(|| json_ld_find(doc, "\"datePublished\"", ""))
        .map(|d| d.chars().take(10).collect()); // date part only

    let site = meta_attr(doc, "meta[property='og:site_name']", "content")
        .or_else(|| meta_attr(doc, "meta[name='application-name']", "content"));

    let description = meta_attr(doc, "meta[name='description']", "content")
        .or_else(|| meta_attr(doc, "meta[property='og:description']", "content"))
        .or_else(|| meta_attr(doc, "meta[name='twitter:description']", "content"))
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());

    let canonical = scraper::Selector::parse("link[rel='canonical']")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .and_then(|l| l.value().attr("href"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Meta {
        title,
        byline,
        published,
        site,
        description,
        canonical,
    }
}

fn meta_attr(doc: &Html, selector: &str, attr: &str) -> Option<String> {
    let sel = scraper::Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|m| m.value().attr(attr))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Minimal hand-rolled JSON-LD value finder. JSON-LD in the
/// wild is malformed often enough that a real parser fails
/// more than a tolerant string search.
fn json_ld_find(doc: &Html, key: &str, subkey: &str) -> Option<String> {
    let sel = scraper::Selector::parse("script[type='application/ld+json']").ok()?;
    for script in doc.select(&sel) {
        let text: String = script.text().collect();
        let Some(ki) = text.find(key) else { continue };
        // The window end is a byte offset into text that is usually
        // raw UTF-8 (only Wikipedia-style output \u-escapes it), so
        // it must be pulled back onto a char boundary or the slice
        // panics on an ordinary non-English page.
        let start = ki + key.len();
        let end = text.floor_char_boundary(text.len().min(start + 300));
        let region = &text[start..end];
        let hay = if subkey.is_empty() {
            region
        } else {
            let Some(si) = region.find(subkey) else {
                continue;
            };
            &region[si + subkey.len()..]
        };
        // First "..." string after the key.
        let Some(q1) = hay.find('"') else { continue };
        let Some(q2) = hay[q1 + 1..].find('"') else {
            continue;
        };
        let val = &hay[q1 + 1..q1 + 1 + q2];
        if !val.is_empty() && val.len() < 200 {
            return Some(decode_unicode_escapes(val));
        }
    }
    None
}

/// Decode JSON \uXXXX escape sequences to actual characters.
/// Wikipedia and other sites encode non-ASCII author names
/// as Unicode escapes in JSON-LD. Without this, the byline
/// shows \u7ef4\u57fa instead of 维基.
fn decode_unicode_escapes(s: &str) -> String {
    if !s.contains("\\u") {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("\\u") {
        result.push_str(&rest[..pos]);
        rest = &rest[pos + 2..];
        // `get` (not a slice) : the 4 bytes after \u are only a
        // valid str if they're ASCII hex, and a multibyte char in
        // there would otherwise panic the slice.
        if let Some(hex) = rest.get(..4)
            && let Ok(code) = u32::from_str_radix(hex, 16)
            && let Some(ch) = char::from_u32(code)
        {
            result.push(ch);
            rest = &rest[4..];
            continue;
        }
        // Invalid escape : keep literal.
        result.push_str("\\u");
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // json_ld_find used to slice the ld+json text at a fixed +300
    // BYTE offset after the key. Most CMSs emit raw UTF-8 in ld+json
    // (only Wikipedia-style output \u-escapes it), so on an ordinary
    // non-English page that offset lands inside a multibyte char
    // about half the time -- a str-slice panic, which under this
    // crate's panic = "abort" release profile takes the whole MCP
    // daemon down on a plain fetch.
    #[test]
    fn json_ld_window_end_inside_multibyte_char_does_not_panic() {
        // One ASCII byte then 3-byte chars: byte 300 after the key is
        // never a char boundary (boundaries sit at 1 + 3k).
        let pad: String = std::iter::once('x')
            .chain(std::iter::repeat_n('年', 150))
            .collect();
        let html = format!(
            r#"<html><head><script type="application/ld+json">{{"author"{pad},"name":"X"}}</script></head><body></body></html>"#
        );
        let doc = Html::parse_document(&html);
        // Must not panic; the value is whatever the tolerant search
        // finds (or nothing) -- the assertion is that we get here.
        let _ = json_ld_find(&doc, "\"author\"", "\"name\"");
        let _ = json_ld_find(&doc, "\"datePublished\"", "");
    }

    // decode_unicode_escapes sliced `rest[..4]` after every `\u`,
    // which panics when a multibyte char sits inside those 4 bytes.
    #[test]
    fn unicode_escape_decoder_survives_multibyte_after_backslash_u() {
        // rest = "aëüller": a(1) ë(2) ü(2) -> byte 4 is inside 'ü'.
        assert_eq!(decode_unicode_escapes("Zo\\uaëüller"), "Zo\\uaëüller");
        // Still decodes real escapes.
        assert_eq!(decode_unicode_escapes("\\u7ef4\\u57fa"), "维基");
    }
}
