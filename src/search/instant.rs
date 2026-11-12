//! SERP instant layer (v4 C5): byte-derived featured snippets,
//! instant answers, and knowledge panels.
//!
//! Rules (edge ledger):
//! - Never invent text. Copy only what the SERP HTML already contains.
//! - Empty when absent.
//! - Never merged into the organic list as if organic.
//! - Source URL always present and never a SERP self-link.

use scraper::{Html, Selector};

use super::engines::{Hit, is_serp_url};

/// One instant answer extracted from a SERP body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstantAnswer {
    /// "featured" | "answer" | "knowledge"
    pub kind: &'static str,
    pub title: String,
    /// Body text, byte-derived from the page (already whitespace-collapsed).
    pub text: String,
    /// Source URL. Always http(s), never a SERP URL.
    pub url: String,
    /// Engine that produced the SERP.
    pub engine: String,
}

impl InstantAnswer {
    /// Apply the same URL, title and exclusion rules as organic evidence.
    pub(crate) fn permitted(&self, facts: &super::query::QueryFacts) -> bool {
        super::contract::veto_lane(
            facts,
            &Hit {
                title: self.title.clone(),
                url: self.url.clone(),
                snippet: self.text.clone(),
                rank: 0,
                published: None,
            },
        )
        .is_none()
    }
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("static selector")
}

fn text(el: scraper::ElementRef) -> String {
    // Visible text only: a script/style subtree in a SERP cell is
    // source, never snippet text (#288's class, search side).
    crate::extract::inline::visible_text(el)
}

fn first_http_href(root: scraper::ElementRef) -> Option<String> {
    let a = sel("a[href]");
    for el in root.select(&a) {
        let href = el.value().attr("href").unwrap_or("");
        if href.starts_with("http") && !is_serp_url(href) && !href.contains("bing.com/ck/a") {
            return Some(href.to_string());
        }
    }
    None
}

fn finish(
    kind: &'static str,
    engine: &str,
    title: String,
    body: String,
    url: String,
) -> Option<InstantAnswer> {
    let text = body.trim().to_string();
    let title = title.trim().to_string();
    if text.is_empty() || text.len() < 20 {
        return None;
    }
    // Cap: instant answers are a decision aid, not a second page.
    let text: String = text.chars().take(480).collect();
    if !url.starts_with("http") || is_serp_url(&url) {
        return None;
    }
    Some(InstantAnswer {
        kind,
        title,
        text,
        url,
        engine: engine.to_string(),
    })
}

/// Extract an instant answer from a SERP body. `None` when the page
/// has none (the common case for most queries).
pub fn parse_instant(engine: &str, html: &str) -> Option<InstantAnswer> {
    let doc = Html::parse_document(html);
    parse_instant_doc(engine, &doc)
}

/// Same as `parse_instant`, on an already-parsed document. The
/// engine fan-out parses the SERP once and feeds hits + instant
/// from the same DOM (html5ever is not free on a 3MB page).
pub fn parse_instant_doc(engine: &str, doc: &Html) -> Option<InstantAnswer> {
    match engine {
        "bing" => parse_bing_instant(doc, engine),
        "brave" => parse_brave_instant(doc, engine),
        "yahoo" => parse_yahoo_instant(doc, engine),
        "google" => parse_google_instant(doc, engine),
        "mojeek" => parse_mojeek_instant(doc, engine),
        _ => None,
    }
}

fn parse_bing_instant(doc: &Html, engine: &str) -> Option<InstantAnswer> {
    // Featured snippet / answer pole. Layered: b_pole (answers),
    // b_ans (generic answer), focus answer.
    for css in [
        "#b_pole .b_ans",
        "#b_pole",
        "li.b_ans",
        ".b_focusAnswer",
        "#np_featured_snippets",
        ".b_ans",
    ] {
        let Ok(s) = Selector::parse(css) else {
            continue;
        };
        let Some(block) = doc.select(&s).next() else {
            continue;
        };
        let body = block
            .select(&sel(".b_caption, .b_focusText, .b_snippet, p"))
            .map(text)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if body.trim().len() < 20 {
            continue;
        }
        let title = block
            .select(&sel("h2, .b_title, .b_focusTextSourceTitle"))
            .map(text)
            .find(|t| !t.is_empty())
            .unwrap_or_default();
        let url = first_http_href(block).unwrap_or_default();
        let kind = if css.contains("pole") || css.contains("b_ans") {
            "answer"
        } else {
            "featured"
        };
        if let Some(a) = finish(kind, engine, title, body, url) {
            return Some(a);
        }
    }
    None
}

fn parse_brave_instant(doc: &Html, engine: &str) -> Option<InstantAnswer> {
    for css in [
        ".featured-snippet",
        "#featured-snippet",
        "[data-type=\"web\"].answer",
        ".answer-box",
        ".infobox",
    ] {
        let Ok(s) = Selector::parse(css) else {
            continue;
        };
        let Some(block) = doc.select(&s).next() else {
            continue;
        };
        let body = block
            .select(&sel(".generic-snippet, .snippet-content, p, .description"))
            .map(text)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if body.trim().len() < 20 {
            continue;
        }
        let title = block
            .select(&sel(".title, h2, h3"))
            .map(text)
            .find(|t| !t.is_empty())
            .unwrap_or_default();
        let url = first_http_href(block).unwrap_or_default();
        let kind = if css.contains("infobox") {
            "knowledge"
        } else {
            "featured"
        };
        if let Some(a) = finish(kind, engine, title, body, url) {
            return Some(a);
        }
    }
    None
}

fn parse_google_instant(doc: &Html, engine: &str) -> Option<InstantAnswer> {
    for css in [
        ".IZ6rdc",
        ".hgKElc",
        ".LGOjhe",
        "#kp-wp-tab-overview .wDYxhc",
        ".kp-blk",
        ".V3FYCf",
    ] {
        let Ok(s) = Selector::parse(css) else {
            continue;
        };
        let Some(block) = doc.select(&s).next() else {
            continue;
        };
        let body = text(block);
        if body.trim().len() < 20 {
            continue;
        }
        // Google featured snippets often have no separate title;
        // knowledge panels do.
        let title = block
            .select(&sel("[data-attrid='title'], .qrShPb, h2, h3"))
            .map(text)
            .find(|t| !t.is_empty())
            .unwrap_or_default();
        // Prefer the C-block source link; fall back to any http.
        let url = block
            .select(&sel("a[href]"))
            .filter_map(|a| a.value().attr("href").map(String::from))
            .find(|h| h.starts_with("http") && !h.contains("google.com"))
            .or_else(|| first_http_href(block))
            .unwrap_or_default();
        let kind = if css.contains("kp-") || css.contains("V3FYCf") {
            "knowledge"
        } else {
            "featured"
        };
        if let Some(a) = finish(kind, engine, title, body, url) {
            return Some(a);
        }
    }
    None
}

fn parse_yahoo_instant(doc: &Html, engine: &str) -> Option<InstantAnswer> {
    for css in [".algoTop", "#main .algo:first-child", ".compText"] {
        let Ok(s) = Selector::parse(css) else {
            continue;
        };
        let Some(block) = doc.select(&s).next() else {
            continue;
        };
        // Only treat the TOP block as instant if it looks like a
        // definition/answer, not a normal result. Heuristic: long
        // snippet + title that is not "More results".
        let body = block
            .select(&sel(".compText, .compText a, p"))
            .map(text)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let body = if body.trim().len() >= 20 {
            body
        } else {
            text(block)
        };
        if body.len() < 40 {
            continue;
        }
        let title = block
            .select(&sel("h3.title, h3"))
            .map(text)
            .find(|t| !t.is_empty() && !t.to_lowercase().contains("more results"))
            .unwrap_or_default();
        let url = first_http_href(block).unwrap_or_default();
        if let Some(a) = finish("featured", engine, title, body, url) {
            return Some(a);
        }
    }
    None
}

fn parse_mojeek_instant(doc: &Html, engine: &str) -> Option<InstantAnswer> {
    // Mojeek rarely ships featured snippets; only accept a clear
    // answer box so we never promote a normal organic hit.
    let Ok(s) = Selector::parse(".answer, .featured-snippet, #answer") else {
        return None;
    };
    let block = doc.select(&s).next()?;
    let body = text(block);
    let url = first_http_href(block).unwrap_or_default();
    finish("answer", engine, String::new(), body, url)
}

/// Coerce a top organic hit into the outcome's instant slot is
/// NEVER done. This helper only exists so call sites that already
/// have hits can check whether an engine's first hit was pulled
/// from an answer block we also captured as organic (dedup).
pub fn same_source(instant: &InstantAnswer, hits: &[Hit]) -> bool {
    let key = crate::search::rank::norm_key(&instant.url);
    hits.iter()
        .any(|h| crate::search::rank::norm_key(&h.url) == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bing_featured_snippet_is_byte_derived_and_sourced() {
        let html = r#"
        <html><body>
        <div id="b_pole">
          <div class="b_ans">
            <h2>Rust ownership</h2>
            <div class="b_caption"><p>Ownership is Rust's most unique feature, and it enables memory safety without a garbage collector.</p></div>
            <a href="https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html">Source</a>
          </div>
        </div>
        <ol id="b_results">
          <li class="b_algo"><h2><a href="https://example.com/a">Other</a></h2></li>
        </ol>
        </body></html>
        "#;
        let a = parse_instant("bing", html).expect("instant");
        assert_eq!(a.kind, "answer");
        assert!(a.text.contains("memory safety"), "{}", a.text);
        assert_eq!(
            a.url,
            "https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html"
        );
        assert_eq!(a.engine, "bing");
        let query = super::super::query::QueryFacts::new;
        assert!(a.permitted(&query(
            "rust ownership site:doc.rust-lang.org intitle:ownership"
        )));
        assert!(!a.permitted(&query("rust ownership site:example.com")));
        assert!(!a.permitted(&query("rust ownership intitle:borrowing")));
        assert!(!a.permitted(&query("rust ownership -collector")));
        assert!(!a.permitted(&query("rust ownership filetype:pdf")));
        let mut reference = a.clone();
        reference.url = "https://en.wikipedia.org/wiki/Tokio".into();
        assert!(!reference.permitted(&query("tokio::spawn")));
    }

    #[test]
    fn empty_serp_yields_none() {
        let html = r#"<html><body><ol id="b_results"></ol></body></html>"#;
        assert!(parse_instant("bing", html).is_none());
    }

    #[test]
    fn never_returns_serp_self_link() {
        let html = r#"
        <div id="b_pole"><div class="b_ans">
          <div class="b_caption"><p>This is a long enough answer body to pass the minimum length gate for tests.</p></div>
          <a href="https://www.bing.com/search?q=rust">self</a>
        </div></div>
        "#;
        // No valid source URL → None (never invent, never SERP link).
        assert!(parse_instant("bing", html).is_none());
    }

    #[test]
    fn brave_featured_snippet() {
        let html = r#"
        <div class="featured-snippet">
          <div class="title">HTTP 429</div>
          <div class="generic-snippet">The 429 status code indicates the user has sent too many requests in a given amount of time.</div>
          <a href="https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/429">MDN</a>
        </div>
        "#;
        let a = parse_instant("brave", html).expect("brave instant");
        assert_eq!(a.kind, "featured");
        assert!(a.text.contains("too many requests"));
        assert!(a.url.starts_with("https://developer.mozilla.org"));
    }

    #[test]
    fn short_body_rejected() {
        let html = r#"
        <div id="b_pole"><div class="b_ans">
          <div class="b_caption"><p>too short</p></div>
          <a href="https://example.com/x">x</a>
        </div></div>
        "#;
        assert!(parse_instant("bing", html).is_none());
    }
}

#[cfg(test)]
mod visible_text_guard_tests {
    use super::*;

    // #288's class, search side: a script/style subtree inside a SERP
    // cell is source, never snippet text.
    #[test]
    fn snippets_never_carry_script_text() {
        let doc = Html::parse_fragment(
            "<div>Result title<style>.r { color: red; }</style> and body</div>",
        );
        let root = doc.root_element();
        let el = root
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .next()
            .unwrap();
        let t = text(el);
        assert!(!t.contains("color: red"), "{t}");
        assert!(t.contains("Result title"), "{t}");
        assert!(t.contains("and body"), "{t}");
    }
}
