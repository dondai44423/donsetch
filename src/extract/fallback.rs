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
    collect_fallback_text(body, &mut paragraphs, &mut current, 0);
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

// Non-content tags only. header/footer/nav/aside deliberately
// NOT skipped: on SPA profile pages (instagram, twitter) the main
// content lives inside them, and the fallback is the LAST resort
// - prefer over-collecting (some boilerplate) over failing with
// "no real content" on a page that renders perfectly (live case:
// instagram's 972-visible-char profile page, 45 collected).
const SKIP_FALLBACK_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
    "form", "button", "input", "select", "textarea", "option",
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

/// Recursion cap for the fallback walker. The two primary walkers
/// (blocks::walk, inline::render) already cap depth; this last-resort
/// walker did not, and it runs unconditionally — before the length
/// gate — on the tokio worker's 2 MiB stack for exactly the deep,
/// thin pages that reach it (blocks::walk stops at 300 and hands off
/// as thin). A crafted nest of tens of thousands of elements, well
/// under the body caps, overflowed that stack: a SIGSEGV that
/// panic=abort turns into a one-request remote abort.
const MAX_DEPTH: usize = 300;

fn collect_fallback_text(
    el: scraper::ElementRef,
    paragraphs: &mut Vec<String>,
    current: &mut String,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let text = t.text.trim();
                if !text.is_empty() {
                    if !current.is_empty() && !current.ends_with(' ') && !current.ends_with('\n') {
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
                    collect_fallback_text(child_el, paragraphs, &mut heading, depth + 1);
                    if !heading.trim().is_empty() {
                        paragraphs.push(format!("{} {}", "#".repeat(level), heading.trim()));
                    }
                    continue;
                }
                // A br: one = a line break inside the current
                // paragraph; two in a row = the paragraph break.
                // Matches browser rendering and the main converter
                // (issue #227 class): the old behavior flushed at
                // every br, splitting prose at every line break.
                if name == "br" {
                    if current.ends_with('\n') {
                        if !current.trim().is_empty() {
                            paragraphs.push(std::mem::take(current).trim().to_string());
                        }
                    } else {
                        current.push('\n');
                    }
                    continue;
                }
                // Block elements: flush, recurse, flush
                if PARAGRAPH_BREAK_TAGS.contains(&name) {
                    if !current.trim().is_empty() {
                        paragraphs.push(std::mem::take(current).trim().to_string());
                    }
                    let mut inner = String::new();
                    collect_fallback_text(child_el, paragraphs, &mut inner, depth + 1);
                    if !inner.trim().is_empty() {
                        paragraphs.push(inner.trim().to_string());
                    }
                } else {
                    // Inline: recurse without flush
                    collect_fallback_text(child_el, paragraphs, current, depth + 1);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    // The primary walkers cap recursion depth; this last-resort one
    // did not, and it runs unconditionally on the tokio worker's
    // 2 MiB stack for exactly the deep, thin pages that reach it. A
    // crafted 20k-deep nest overflowed that stack: a SIGSEGV that
    // panic=abort turns into a one-request remote abort. Mirrors the
    // #145 MathML cap test — a bounded 1 MiB thread plus a deep nest.
    // Uncapped this crashes the test process; capped it completes.
    #[test]
    fn deep_nesting_is_capped_not_a_stack_overflow() {
        let depth = 20_000;
        let mut html = String::from("<html><body>");
        for _ in 0..depth {
            html.push_str("<blockquote>");
        }
        html.push_str("deep");
        for _ in 0..depth {
            html.push_str("</blockquote>");
        }
        html.push_str("</body></html>");
        let done = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let doc = scraper::Html::parse_document(&html);
                let sel = scraper::Selector::parse("body").unwrap();
                let body = doc.select(&sel).next().unwrap();
                let mut paragraphs = Vec::new();
                let mut current = String::new();
                collect_fallback_text(body, &mut paragraphs, &mut current, 0);
                true
            })
            .unwrap()
            .join()
            .expect("the capped walk must complete on a 1 MiB stack");
        assert!(done);
    }
}

#[cfg(test)]
mod fallback_live {
    use super::*;

    /// LIVE receipt: instagram's rendered profile page (dumped by
    /// the ghost debug path on this box). Skips silently when the
    /// dump is absent so gates stay green elsewhere. Discriminating:
    /// the old skip-list dropped <header> entirely, so a page with
    /// 972 visible chars collected 45.
    #[test]
    fn instagram_profile_dump_extracts_profile_content() {
        let Ok(entries) = std::fs::read_dir("/tmp/fresh2/ghost-debug") else {
            return;
        };
        let Some(path) = entries
            .flatten()
            .map(|e| e.path())
            .find(|p| p.to_string_lossy().contains("instagram"))
        else {
            return;
        };
        let html = std::fs::read_to_string(&path).unwrap();
        let doc = scraper::Html::parse_document(&html);
        let body_sel = scraper::Selector::parse("body").unwrap();
        let mut paragraphs = Vec::new();
        let mut current = String::new();
        collect_fallback_text(
            doc.select(&body_sel).next().unwrap(),
            &mut paragraphs,
            &mut current,
            0,
        );
        if !current.trim().is_empty() {
            paragraphs.push(current.trim().to_string());
        }
        let all = paragraphs.join("\n");
        assert!(
            all.contains("679M followers") || all.contains("cristiano") || all.len() >= 200,
            "the profile card content must be collected (got {} chars: {:?})",
            all.len(),
            &all[..all.len().min(160)]
        );
    }
}
