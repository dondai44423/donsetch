//! Raw-text fallback for DonSift: when the block-based pipeline
//! fails on a complex DOM but the page has real visible text, this
//! strips tags and renders paragraphs + headings as markdown so
//! "found DOM but failed to extract" cannot return nothing.

use scraper::{Html, Node};

use super::ContentKind;
use super::metadata;
use super::paginate;
use super::{ExtractOptions, Extracted};

/// Raw text fallback: strip tags and return visible text as
/// markdown paragraphs. Used when DonSift's block-based extraction
/// pipeline fails on complex DOMs. Preserves heading
/// structure (h1-h6 → # ## ###) and paragraph breaks. Skips
/// script/style/nav/footer/header/aside/form elements.
///
/// Returns None when there's < 200 chars of visible text : the
/// page is genuinely empty (JS shell or block page).
// Shared fallback thresholds (E19: duplicated between extract() and
// text_fallback, guaranteed to drift).
/// A page below this much extracted text (and no focus query) triggers
/// the raw-text fallback pass.
pub const FALLBACK_MIN_TEXT: usize = 200;
/// A fallback page below this much real text is classified thin
/// (agent signal: this looks like a JS shell).
pub const FALLBACK_THIN_TEXT: usize = 800;

pub fn text_fallback(
    html_text: &str,
    meta: &metadata::Meta,
    url: &str,
    opts: &ExtractOptions,
    max_chars: usize,
) -> Option<Extracted> {
    let doc = Html::parse_document(html_text);
    let body_sel = scraper::Selector::parse("body").ok()?;
    let body = doc.select(&body_sel).next()?;

    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    collect_fallback_text(body, &mut paragraphs, &mut current);
    if !current.trim().is_empty() {
        paragraphs.push(current.trim().to_string());
    }

    // Filter whitespace-only and single-char paragraphs
    let paragraphs: Vec<String> = paragraphs
        .into_iter()
        .filter(|p| p.len() > 1 && p.chars().any(|c| !c.is_whitespace()))
        .collect();

    let total_text: usize = paragraphs.iter().map(|p| p.len()).sum();
    if total_text < 200 {
        return None;
    }

    let mut full = String::new();
    if let Some(t) = &meta.title {
        full.push_str(&format!("# {t}\n\n"));
    }
    full.push_str(&format!("{url}\n\n"));
    full.push_str(&paragraphs.join("\n\n"));

    let (slice, next) = paginate(&full, opts.offset, max_chars);
    let blocks_total = paragraphs.len();
    let tokens_est = slice.len() / 4;

    // thin=true when < 800 chars: a JS shell with 300 chars of
    // visible text (script filenames, noscript messages, meta
    // descriptions) is NOT real content. The MCP layer must
    // escalate to ghost. Only pages with >= 800 chars of real
    // visible text are non-thin : those are genuinely complex
    // DOMs where block extraction failed but text is real.
    Some(Extracted {
        markdown: slice,
        title: meta.title.clone(),
        byline: meta.byline.clone(),
        published: meta.published.clone(),
        site: meta.site.clone(),
        total_chars: full.len(),
        next_offset: next,
        blocks_total,
        blocks_shown: blocks_total,
        tokens_est,
        thin: total_text < FALLBACK_THIN_TEXT,
        content_kind: ContentKind::Page,
        lang: "unknown".to_string(),
        quality: 0.3, // lower quality than block-based extraction
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
    })
}

const SKIP_FALLBACK_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed", "nav",
    "aside", "footer", "header", "form", "button", "input", "select", "textarea", "option",
];

const PARAGRAPH_BREAK_TAGS: &[&str] = &[
    "p",
    "br",
    "li",
    "tr",
    "blockquote",
    "pre",
    "dt",
    "dd",
    "figcaption",
];

fn heading_level(tag: &str) -> Option<usize> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn collect_fallback_text(
    el: scraper::ElementRef,
    paragraphs: &mut Vec<String>,
    current: &mut String,
) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let text = t.text.trim();
                if !text.is_empty() {
                    if !current.is_empty() && !current.ends_with(' ') {
                        current.push(' ');
                    }
                    current.push_str(text);
                }
            }
            Node::Element(e) => {
                let name = e.name();
                if SKIP_FALLBACK_TAGS.contains(&name) {
                    continue;
                }
                let Some(child_el) = scraper::ElementRef::wrap(child) else {
                    continue;
                };
                // Headings: flush, prefix with markdown, recurse
                if let Some(level) = heading_level(name) {
                    if !current.trim().is_empty() {
                        paragraphs.push(std::mem::take(current).trim().to_string());
                    }
                    let mut heading = String::new();
                    collect_fallback_text(child_el, paragraphs, &mut heading);
                    if !heading.trim().is_empty() {
                        paragraphs.push(format!("{} {}", "#".repeat(level), heading.trim()));
                    }
                    continue;
                }
                // Block elements: flush, recurse, flush
                if PARAGRAPH_BREAK_TAGS.contains(&name) {
                    if !current.trim().is_empty() {
                        paragraphs.push(std::mem::take(current).trim().to_string());
                    }
                    let mut inner = String::new();
                    collect_fallback_text(child_el, paragraphs, &mut inner);
                    if !inner.trim().is_empty() {
                        paragraphs.push(inner.trim().to_string());
                    }
                } else {
                    // Inline: recurse without flush
                    collect_fallback_text(child_el, paragraphs, current);
                }
            }
            _ => {}
        }
    }
}
