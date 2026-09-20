//! Feed extractor: RSS 2.0 / Atom / JSON Feed → structured
//! markdown. A feed URL returned as a raw XML blob (the v2.1
//! behavior : 25K chars of CDATA soup) is worthless to an agent;
//! this renders the feed the way a feed reader shows it: channel
//! header + items with title/link/date/summary.

use serde_json::Value;

use super::{ContentKind, ExtractOptions, Extracted};

/// Max items rendered before truncating (feeds can run to hundreds).
const MAX_ITEMS: usize = 60;

/// Per-item summary cap (chars). Feed descriptions are often the
/// full article; the agent can fetch the link for the rest.
const SUMMARY_CAP: usize = 400;

/// Sniff whether these bytes (with their Content-Type) are a feed.
pub fn is_feed(content_type: &str, body: &[u8]) -> bool {
    let ct = content_type.to_lowercase();
    if ct.contains("rss") || ct.contains("atom") || ct.contains("feed+json") {
        return true;
    }
    // Content-Type lies constantly (text/xml, text/plain, no
    // header at all): sniff the payload.
    let head = &body[..body.len().min(1024)];
    let head_l = String::from_utf8_lossy(head).to_lowercase();
    if head_l.trim_start().starts_with("<?xml")
        || head_l.trim_start().starts_with("<rss")
        || head_l.trim_start().starts_with("<feed")
    {
        let scan = &body[..body.len().min(16 * 1024)];
        let s = String::from_utf8_lossy(scan).to_lowercase();
        return s.contains("<rss") || s.contains("<feed");
    }
    // JSON Feed: {"version":"https://jsonfeed.org/...","items":[...]}
    if (ct.contains("json") || head_l.trim_start().starts_with("{"))
        && head_l.contains("jsonfeed.org")
    {
        return true;
    }
    false
}

pub fn extract(body: &[u8], url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        extract_json_feed(&text, url, opts)
    } else {
        extract_xml_feed(&text, url, opts)
    }
}

// ── XML (RSS 2.0 / Atom) ─────────────────────────────────────

fn extract_xml_feed(text: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    // HTML parsers mangle XML feeds two ways: `<link>` is a VOID
    // element in HTML (its child text vanishes) and `<![CDATA[..]]>`
    // becomes literal text. Preprocess both away : the markers
    // never appear legitimately inside feed text content.
    let text = text.replace("<![CDATA[", "").replace("]]>", "");
    let text = text
        .replace("<link>", "<rsslink>")
        .replace("</link>", "</rsslink>");
    // Unknown elements stay on the parser's stack until closed; a
    // feed nested thousands deep is the html5ever quadratic too.
    if super::nesting::max_nesting(&text) > super::nesting::MAX_NESTING {
        return None;
    }
    let doc = scraper::Html::parse_document(&text);

    // RSS: channel > title/rsslink/description, item > ...
    // Atom: feed > title, entry > ...
    let is_atom = doc
        .select(&scraper::Selector::parse("feed").ok()?)
        .next()
        .is_some();

    let channel_title = first_text(&doc, &["channel > title", "feed > title"])
        .or_else(|| first_text(&doc, &["title"]))?;
    let channel_desc = first_text(&doc, &["channel > description", "feed > subtitle"]);
    let site_link = first_attr(&doc, &["feed > link"], "href")
        .or_else(|| first_text(&doc, &["channel > rsslink"]));

    let item_sel = if is_atom { "entry" } else { "item" };
    let items_sel = scraper::Selector::parse(item_sel).ok()?;
    let items: Vec<scraper::ElementRef<'_>> = doc.select(&items_sel).collect();
    if items.is_empty() {
        return None; // XML but not a recognizable feed
    }

    let mut md = format!("# {channel_title}\n");
    if let Some(d) = &channel_desc
        && !d.is_empty()
    {
        md.push_str(&format!("> {}\n", d));
    }
    if let Some(l) = &site_link
        && !l.is_empty()
    {
        md.push_str(&format!("{l}\n"));
    }
    md.push_str(&format!("{url}\n\n"));

    let mut shown = 0usize;
    for item in items.iter().take(MAX_ITEMS) {
        let title = child_text(*item, &["title"]).unwrap_or_default();
        let link = child_attr(*item, &["link"], "href")
            .or_else(|| child_text(*item, &["rsslink"]))
            .or_else(|| child_text(*item, &["guid"]))
            .unwrap_or_default();
        let date =
            child_text(*item, &["pubdate", "published", "updated", "dc:date"]).unwrap_or_default();
        let summary_raw = child_text(
            *item,
            &["description", "summary", "content:encoded", "content"],
        )
        .unwrap_or_default();
        let summary = clean_html(&summary_raw);
        let summary: String = summary.chars().take(SUMMARY_CAP).collect();

        if !title.is_empty() {
            if link.is_empty() {
                md.push_str(&format!("## {title}\n"));
            } else {
                md.push_str(&format!("## [{title}]({link})\n"));
            }
        } else if !link.is_empty() {
            md.push_str(&format!("## {link}\n"));
        } else {
            continue;
        }
        let mut meta = String::new();
        if !date.is_empty() {
            meta.push_str(&date);
        }
        if !meta.is_empty() {
            md.push_str(&format!("{meta}\n"));
        }
        if !summary.is_empty() {
            md.push_str(&format!("{summary}\n"));
        }
        md.push('\n');
        shown += 1;
    }

    if items.len() > MAX_ITEMS {
        md.push_str(&format!(
            "*(feed truncated: {} items total, showing {MAX_ITEMS})*\n",
            items.len()
        ));
    }

    finish(md, Some(channel_title), items.len(), shown, opts)
}

// ── JSON Feed (jsonfeed.org) ──────────────────────────────────

fn extract_json_feed(text: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let items = v.get("items")?.as_array()?;
    if items.is_empty() {
        return None;
    }
    let channel_title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Feed")
        .to_string();
    let home = v.get("home_page_url").and_then(Value::as_str);

    let mut md = format!("# {channel_title}\n");
    if let Some(d) = v.get("description").and_then(Value::as_str)
        && !d.is_empty()
    {
        md.push_str(&format!("> {}\n", d));
    }
    if let Some(h) = home {
        md.push_str(&format!("{h}\n"));
    }
    md.push_str(&format!("{url}\n\n"));

    let mut shown = 0usize;
    for item in items.iter().take(MAX_ITEMS) {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let link = item
            .get("url")
            .or_else(|| item.get("external_url"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let date = item
            .get("date_published")
            .or_else(|| item.get("date_modified"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let summary_raw = item
            .get("summary")
            .or_else(|| item.get("content_text"))
            .or_else(|| item.get("content_html"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let summary = clean_html(summary_raw);
        let summary: String = summary.chars().take(SUMMARY_CAP).collect();

        if !title.is_empty() {
            if link.is_empty() {
                md.push_str(&format!("## {title}\n"));
            } else {
                md.push_str(&format!("## [{title}]({link})\n"));
            }
        } else if !link.is_empty() {
            md.push_str(&format!("## {link}\n"));
        } else {
            continue;
        }
        if !date.is_empty() {
            md.push_str(&format!("{date}\n"));
        }
        if !summary.is_empty() {
            md.push_str(&format!("{summary}\n"));
        }
        md.push('\n');
        shown += 1;
    }

    if items.len() > MAX_ITEMS {
        md.push_str(&format!(
            "*(feed truncated: {} items total, showing {MAX_ITEMS})*\n",
            items.len()
        ));
    }

    finish(md, Some(channel_title), items.len(), shown, opts)
}

// ── shared helpers ────────────────────────────────────────────

fn finish(
    md: String,
    title: Option<String>,
    total_items: usize,
    shown: usize,
    opts: &ExtractOptions,
) -> Option<Extracted> {
    let total = md.len();
    let max_chars = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max_chars);
    let tokens_est = slice.len() / 4;
    Some(Extracted {
        markdown: slice,
        title,
        byline: None,
        published: None,
        site: None,
        total_chars: total,
        next_offset: next,
        blocks_total: total_items,
        blocks_shown: shown,
        tokens_est,
        thin: shown == 0,
        content_kind: ContentKind::Listing,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
    })
}

/// First direct-child text of `el` matching any tag name.
fn child_text(el: scraper::ElementRef<'_>, names: &[&str]) -> Option<String> {
    for child in el.children() {
        if let Some(c) = scraper::ElementRef::wrap(child)
            && names.contains(&c.value().name())
        {
            let t: String = c.text().collect::<Vec<_>>().join("");
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Direct-child element matching any tag name → attribute value.
fn child_attr(el: scraper::ElementRef<'_>, names: &[&str], attr: &str) -> Option<String> {
    for child in el.children() {
        if let Some(c) = scraper::ElementRef::wrap(child)
            && names.contains(&c.value().name())
            && let Some(v) = c.value().attr(attr)
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

fn first_text(doc: &scraper::Html, sels: &[&str]) -> Option<String> {
    for s in sels {
        if let Ok(sel) = scraper::Selector::parse(s)
            && let Some(el) = doc.select(&sel).next()
        {
            let t: String = el.text().collect::<Vec<_>>().join("");
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn first_attr(doc: &scraper::Html, sels: &[&str], attr: &str) -> Option<String> {
    for s in sels {
        if let Ok(sel) = scraper::Selector::parse(s)
            && let Some(el) = doc.select(&sel).next()
            && let Some(v) = el.value().attr(attr)
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Feed summaries carry inline HTML : strip to visible text.
fn clean_html(s: &str) -> String {
    if !s.contains('<') {
        return s.trim().to_string();
    }
    let frag = scraper::Html::parse_fragment(s);
    let text: String = frag.root_element().text().collect::<Vec<_>>().join(" ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rss_by_content_type_and_sniff() {
        let body = br#"<?xml version="1.0"?><rss version="2.0"><channel></channel></rss>"#;
        assert!(is_feed("application/rss+xml", body));
        assert!(is_feed("text/xml", body));
        assert!(is_feed("", body));
        assert!(!is_feed("text/html", b"<html><body>hi</body></html>"));
    }

    #[test]
    fn detects_atom_and_json_feed() {
        let atom = br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>t</title></feed>"#;
        assert!(is_feed("application/atom+xml", atom));
        let jf = br#"{"version":"https://jsonfeed.org/version/1.1","title":"T","items":[]}"#;
        assert!(is_feed("application/json", jf));
    }

    #[test]
    fn extracts_rss_items_not_raw_xml() {
        let rss = r#"<?xml version="1.0" encoding="UTF-8"?>
        <rss version="2.0"><channel>
          <title><![CDATA[BBC News]]></title>
          <description><![CDATA[BBC News - News Front Page]]></description>
          <link>https://www.bbc.co.uk/news</link>
          <item>
            <title><![CDATA[Envoy meets Hamas leader]]></title>
            <description><![CDATA[The rare meeting comes a week after <b>talks</b> collapsed.]]></description>
            <link>https://www.bbc.co.uk/news/articles/xyz?at_medium=RSS</link>
            <pubDate>Sun, 16 Aug 2026 20:32:10 GMT</pubDate>
            <guid isPermaLink="false">https://www.bbc.co.uk/news/articles/xyz#xt=orss</guid>
          </item>
          <item>
            <title><![CDATA[Second headline]]></title>
            <description>Plain description.</description>
            <link>https://www.bbc.co.uk/news/articles/abc</link>
            <pubDate>Sun, 16 Aug 2026 19:00:00 GMT</pubDate>
          </item>
        </channel></rss>"#;
        let ex = extract(
            rss.as_bytes(),
            "https://feeds.example/rss.xml",
            &ExtractOptions::default(),
        )
        .expect("rss extracts");
        assert!(ex.markdown.contains("# BBC News"), "{}", ex.markdown);
        assert!(ex.markdown.contains("[Envoy meets Hamas leader]("));
        assert!(ex.markdown.contains("Sun, 16 Aug 2026"));
        // Inline HTML stripped from descriptions.
        assert!(ex.markdown.contains("talks collapsed"), "{}", ex.markdown);
        assert!(!ex.markdown.contains("<b>"));
        assert!(!ex.markdown.contains("<![CDATA"));
        assert!(!ex.markdown.contains("<?xml"));
        assert_eq!(ex.content_kind, ContentKind::Listing);
    }

    #[test]
    fn extracts_atom_entries() {
        let atom = r#"<?xml version="1.0"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Example Atom</title>
          <subtitle>subtitle here</subtitle>
          <link href="https://example.com/"/>
          <entry>
            <title>Atom Post</title>
            <link href="https://example.com/atom-post"/>
            <published>2026-08-01T10:00:00Z</published>
            <summary>Atom summary text.</summary>
          </entry>
        </feed>"#;
        let ex = extract(
            atom.as_bytes(),
            "https://example.com/feed.atom",
            &ExtractOptions::default(),
        )
        .expect("atom extracts");
        assert!(
            ex.markdown
                .contains("[Atom Post](https://example.com/atom-post)")
        );
        assert!(ex.markdown.contains("2026-08-01"));
    }

    #[test]
    fn extracts_json_feed() {
        let jf = r#"{"version":"https://jsonfeed.org/version/1.1","title":"My Feed","home_page_url":"https://example.com","items":[{"id":"1","title":"Item One","url":"https://example.com/1","date_published":"2026-08-15T00:00:00Z","content_text":"Item one body text."}]}"#;
        let ex = extract(
            jf.as_bytes(),
            "https://example.com/feed.json",
            &ExtractOptions::default(),
        )
        .expect("json feed extracts");
        assert!(ex.markdown.contains("# My Feed"));
        assert!(ex.markdown.contains("[Item One](https://example.com/1)"));
        assert!(ex.markdown.contains("Item one body text."));
    }

    #[test]
    fn empty_channel_is_none() {
        let rss = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Empty</title></channel></rss>"#;
        assert!(extract(rss.as_bytes(), "https://x/rss", &ExtractOptions::default()).is_none());
    }
}
