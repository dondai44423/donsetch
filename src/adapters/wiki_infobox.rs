//! Wikipedia infobox adapter: the summary table agents actually
//! want (born/died/founded/area/status) becomes a clean field
//! list at the top of the article. The generic pipeline drops
//! infoboxes or scatters them; here they are first-class facts
//! with the FULL article body still following.

use scraper::{ElementRef, Html, Selector};

use crate::extract::{ContentKind, ExtractOptions, Extracted};

pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() {
        return None;
    }
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    // *.wikipedia.org only : the REST/mobile mirrors differ.
    if !host.ends_with(".wikipedia.org") {
        return None;
    }
    // Article pages only (/wiki/<title>, not /wiki/Special:...).
    let title = u.path().strip_prefix("/wiki/")?;
    if title.is_empty() || title.contains(':') || u.path().contains("Special:") {
        return None;
    }

    let doc = Html::parse_document(html);
    let box_sel = Selector::parse("table.infobox").ok()?;
    let infobox = doc.select(&box_sel).next()?;

    // Field rows: <tr><th scope=row>Field</th><td>Value</td></tr>.
    let row_sel = Selector::parse("tr").unwrap();
    let th_sel = Selector::parse("th").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let mut fields: Vec<(String, String)> = Vec::new();
    for row in infobox.select(&row_sel) {
        let (Some(th), Some(td)) = (row.select(&th_sel).next(), row.select(&td_sel).next()) else {
            continue;
        };
        // Row header must be scoped as a row field (not a header
        // row like "Personal information").
        if th.value().attr("scope") != Some("row") {
            continue;
        }
        let key = plain_text(th);
        let val = field_value(td);
        if key.is_empty() || val.is_empty() || fields.len() >= 24 {
            continue;
        }
        // Skip nav-noise fields and image captions.
        if val.starts_with('[') && val.ends_with(']') {
            continue;
        }
        fields.push((key, val));
    }
    if fields.len() < 3 {
        return None; // not a real infobox (stub or navbox)
    }

    let mut md = String::from("## Infobox\n\n");
    md.push_str("| field | value |\n| --- | --- |\n");
    for (k, v) in &fields {
        md.push_str(&format!("| {k} | {v} |\n"));
    }
    md.push_str("\n*(article body below)*\n\n");

    // Hand the rest to the generic pipeline, then prepend the
    // infobox. Reuse extract by calling the generic path with the
    // infobox REMOVED from the html : simplest correct approach:
    // re-serialize is heavy; instead extract the article body
    // directly from #mw-content-text via inline rendering of
    // block children.
    // The MAIN parser-output container carries the content-dir
    // class (mw-content-ltr); tiny sibling mw-parser-output divs
    // (page-status indicators) would otherwise win .next().
    let body_sel =
        Selector::parse("div.mw-content-ltr.mw-parser-output, div.mw-content-rtl.mw-parser-output")
            .ok()?;
    let body = doc.select(&body_sel).next()?;
    let mut body_opts = opts.clone();
    body_opts.max_chars = None; // paginate once, below, after prepending
    let body_md = render_blocks(body, url, &body_opts);

    let mut full = md;
    full.push_str(&body_md);
    let total = full.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&full, opts.offset, max);
    Some(Extracted {
        markdown: slice,
        title: doc
            .select(&Selector::parse("h1.firstHeading").unwrap())
            .next()
            .map(plain_text),
        byline: None,
        published: None,
        site: Some("wikipedia".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: fields.len(),
        blocks_shown: fields.len(),
        tokens_est: total / 4,
        thin: false,
        content_kind: ContentKind::Article,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:wikipedia-infobox"),
    })
}

/// Field value: text + links made readable ("Berlin, Germany"),
/// lists joined with "; ", references stripped.
fn field_value(td: ElementRef) -> String {
    let mut out = String::new();
    for child in td.children() {
        match child.value() {
            scraper::Node::Text(t) => out.push_str(&t.text),
            scraper::Node::Element(el) => {
                let c = match ElementRef::wrap(child) {
                    Some(c) => c,
                    None => continue,
                };
                match el.name() {
                    "sup" => {} // citations [1][2]
                    "style" | "script" | "link" | "noscript" => {}
                    "br" => out.push_str(" · "),
                    "ul" => {
                        let items: Vec<String> = c
                            .select(&Selector::parse("li").unwrap())
                            .map(plain_text)
                            .filter(|i| !i.is_empty())
                            .collect();
                        out.push_str(&items.join("; "));
                    }
                    _ => out.push_str(&plain_text(c)),
                }
            }
            _ => {}
        }
    }
    // Collapse whitespace runs, drop trailing separators.
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(['·', ' '])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

fn plain_text(el: ElementRef) -> String {
    el.text().collect::<String>().trim().to_string()
}

/// Page furniture, checked on the element itself (selecting
/// descendants never matches the root of the query).
fn is_furniture(el: ElementRef) -> bool {
    match el.value().name() {
        "style" | "script" | "sup" => {
            el.value().name() != "sup" || el.value().classes().any(|c| c == "reference")
        }
        "span" | "div" => el.value().classes().any(|c| c == "mw-editsection"),
        "table" => el.value().classes().any(|c| {
            matches!(
                c,
                "infobox" | "navbox" | "vertical-navbox" | "metadata" | "ambox" | "sistersitebox"
            )
        }),
        _ => false,
    }
}

/// Minimal block walk over the article body: headings, paragraphs,
/// lists, tables, code : furniture skipped at the element level.
fn render_blocks(root: ElementRef, url: &str, opts: &ExtractOptions) -> String {
    let mut out = String::new();
    let mut opts = opts.clone();
    opts.include_links = true; // interwiki links are the point of wiki
    for child in root.children() {
        let Some(el) = ElementRef::wrap(child) else {
            continue;
        };
        if is_furniture(el) {
            continue;
        }
        match el.value().name() {
            "h2" | "h3" | "h4" => {
                let t = plain_text(el);
                if !t.is_empty() {
                    let level = el.value().name().as_bytes()[1] - b'0';
                    out.push_str(&format!("{} {t}\n\n", "#".repeat(level as usize)));
                }
            }
            "p" => {
                let (m, _) = crate::extract::inline::markdown(el, url, &opts);
                if !m.trim().is_empty() {
                    out.push_str(m.trim());
                    out.push_str("\n\n");
                }
            }
            "ul" | "ol" => {
                let (items, _) = crate::extract::blocks::list_items(el, url, &opts, 0);
                if !items.is_empty() {
                    crate::extract::render::push_list(&mut out, &items, el.value().name() == "ol");
                }
            }
            "pre" => {
                let code = plain_text(el);
                if !code.is_empty() {
                    let fence = crate::extract::render::code_fence(&code);
                    out.push_str(&format!("{fence}\n{code}\n{fence}\n\n"));
                }
            }
            "table" => {
                // Regular data tables: first 12 rows as pipe table.
                render_wiki_table(el, url, &opts, &mut out);
            }
            "div" | "section" => {
                // Recurse one level (gallery, columns…).
                let inner = render_blocks(el, url, &opts);
                if !inner.is_empty() {
                    out.push_str(&inner);
                }
            }
            _ => {}
        }
    }
    out
}

fn render_wiki_table(table: ElementRef, url: &str, opts: &ExtractOptions, out: &mut String) {
    let row_sel = Selector::parse("tr").unwrap();
    let cell_sel = Selector::parse("th, td").unwrap();
    let mut rows = 0;
    for row in table.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|c| {
                let (m, _) = crate::extract::inline::markdown(c, url, opts);
                m.trim().to_string()
            })
            .collect();
        if cells.is_empty() {
            continue;
        }
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
        rows += 1;
        if rows == 1 {
            out.push_str(&format!("|{} \n", "| --- ".repeat(cells.len())));
        }
        if rows >= 12 {
            out.push_str("*(table truncated)*\n");
            break;
        }
    }
    if rows > 0 {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const WIKI: &str = r#"<html><body><div id="content">
      <h1 class="firstHeading">Rust (programming language)</h1>
      <div class="mw-content-ltr mw-parser-output">
        <table class="infobox">
          <tr><th colspan="2">Rust</th></tr>
          <tr><th scope="row">Paradigm</th><td>Multi-paradigm<br>concurrent</td></tr>
          <tr><th scope="row">Designed by</th><td>Graydon Hoare</td></tr>
          <tr><th scope="row">First appeared</th><td>2015<sup class="reference">[1]</sup></td></tr>
          <tr><th scope="row">License</th><td>MIT <br> Apache</td></tr>
        </table>
        <p><b>Rust</b> is a multi-paradigm systems programming language.</p>
        <h2>History</h2>
        <p>Rust began as a personal project in 2006.</p>
      </div>
    </div></body></html>"#;

    #[test]
    fn infobox_becomes_fields() {
        let ex = extract(
            WIKI,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            &opts(),
        )
        .unwrap();
        assert_eq!(ex.via, Some("adapter:wikipedia-infobox"));
        assert!(ex.markdown.contains("## Infobox"));
        assert!(
            ex.markdown
                .contains("| Paradigm | Multi-paradigm · concurrent |")
        );
        assert!(ex.markdown.contains("| Designed by | Graydon Hoare |"));
        // Reference marker stripped.
        assert!(ex.markdown.contains("| First appeared | 2015 |"));
        // Body still present after the infobox.
        assert!(ex.markdown.contains("## History"));
        assert!(ex.markdown.contains("personal project in 2006"));
    }

    #[test]
    fn non_article_pages_rejected() {
        assert!(
            extract(
                WIKI,
                "https://en.wikipedia.org/wiki/Special:Search",
                &opts()
            )
            .is_none()
        );
        assert!(extract(WIKI, "https://example.com/wiki/Rust", &opts()).is_none());
        // No infobox → no adapter.
        let plain = "<html><body><div class='mw-parser-output'><p>text</p></div></body></html>";
        assert!(extract(plain, "https://en.wikipedia.org/wiki/Plain", &opts()).is_none());
    }

    #[test]
    fn pre_containing_a_fence_gets_a_longer_fence() {
        // e.g. the Markdown article's own syntax example.
        let html = WIKI.replace(
            "<h2>History</h2>",
            "<pre>```python\nprint(1)\n```</pre><h2>History</h2>",
        );
        let ex = extract(&html, "https://en.wikipedia.org/wiki/Markdown", &opts()).unwrap();
        assert!(
            ex.markdown
                .contains("````\n```python\nprint(1)\n```\n````\n"),
            "{}",
            ex.markdown
        );
    }

    // `el.select("li")` matched nested items too, so each one
    // appeared fused into its parent's line and again as its own
    // top-level bullet (and nested numbering restarted the count).
    #[test]
    fn nested_list_items_are_indented_not_duplicated() {
        let html = WIKI.replace(
            "<h2>History</h2>",
            "<ul><li>Outer one<ul><li>Inner a</li><li>Inner b</li></ul></li><li>Outer two</li></ul>\
             <ol><li>First<ol><li>Sub</li></ol></li><li>Second</li></ol><h2>History</h2>",
        );
        let ex = extract(&html, "https://en.wikipedia.org/wiki/Lists", &opts()).unwrap();
        let md = &ex.markdown;
        assert_eq!(md.matches("Inner a").count(), 1, "{md}");
        assert!(
            md.contains("- Outer one\n  - Inner a\n  - Inner b\n- Outer two\n"),
            "{md}"
        );
        assert!(md.contains("1. First\n  - Sub\n2. Second\n"), "{md}");
    }
}
