//! Main-content detection: bottom-up text-density scoring.
//!
//! One recursive pass: every element returns its subtree stats
//! plus the best candidate inside it. O(n), no maps, no mutation.

use scraper::{ElementRef, Html, Node};

const MAX_DEPTH: usize = 300;

#[derive(Default, Clone, Copy)]
struct Stats {
    text_len: usize,
    link_text_len: usize,
    punct: usize,
    paras: usize,
}

pub fn find_main<'a>(doc: &'a Html) -> Option<ElementRef<'a>> {
    let body = doc.select(&scraper::Selector::parse("body").ok()?).next()?;
    let (body_stats, best) = walk(body, 0);
    if crate::config::cfg().debug.extract {
        eprintln!(
            "[donsift] body stats: text={} link={} punct={} paras={}",
            body_stats.text_len, body_stats.link_text_len, body_stats.punct, body_stats.paras
        );
        if let Some((score, el)) = best {
            eprintln!(
                "[donsift] main = <{}> id={:?} class={:?} score={score:.0}",
                el.value().name(),
                el.value().id(),
                el.value().attr("class"),
            );
        }
    }
    best.map(|(_, el)| el).or(Some(body))
}

fn tag_prior(name: &str) -> f64 {
    match name {
        "article" | "main" => 150.0,
        "section" => 40.0,
        "figure" => 10.0,
        "div" => 0.0,
        "p" => -40.0,
        "ul" | "ol" => -100.0,
        "td" => -80.0,
        "table" => -120.0,
        "body" => -200.0,
        _ => -10.0,
    }
}

fn class_prior(el: &scraper::node::Element) -> f64 {
    if crate::extract::junk::is_positive(el) {
        400.0 // CMS content markers almost always mark the true article
    } else {
        0.0
    }
}

/// Returns (subtree stats, best candidate (score, element)).
fn walk<'a>(el: ElementRef<'a>, depth: usize) -> (Stats, Option<(f64, ElementRef<'a>)>) {
    if depth > MAX_DEPTH {
        return (Stats::default(), None);
    }
    let mut stats = Stats::default();
    let mut best: Option<(f64, ElementRef<'a>)> = None;

    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let text = t.text.trim();
                if !text.is_empty() {
                    stats.text_len += text.chars().count();
                    stats.punct += text
                        .chars()
                        .filter(|c| matches!(c, ',' | '.' | ';' | ':' | '!' | '?'))
                        .count();
                }
            }
            Node::Element(_) => {
                let Some(child_el) = ElementRef::wrap(child) else {
                    continue;
                };
                // Image alt text is content text: pages whose
                // meaning is carried by images (comics,
                // infographics) must still scope to the image
                // container, not lose it to a text-y sidebar.
                if child_el.value().name() == "img"
                    && let Some(alt) = child_el.value().attr("alt")
                {
                    stats.text_len += alt.chars().count().min(400);
                }
                if crate::extract::junk::skip(child_el) {
                    continue;
                }
                // Structural-region IDs (footer/bottom/sidebar/…):
                // never main content, at any size.
                if crate::extract::junk::structural_negative(child_el.value()) {
                    continue;
                }
                // Class-negative subtrees: skip only when small.
                // Real junk is small; a "negative" class on a
                // page-sized container is a false positive.
                if crate::extract::junk::negative(child_el)
                    && crate::extract::junk::text_size(child_el, 400) < 400
                {
                    continue;
                }
                let (cs, cb) = walk(child_el, depth + 1);
                stats.text_len += cs.text_len;
                stats.link_text_len += cs.link_text_len;
                stats.punct += cs.punct;
                stats.paras += cs.paras;
                if let Some((score, cand)) = cb
                    && best.is_none_or(|(b, _)| score > b)
                {
                    best = Some((score, cand));
                }
            }
            _ => {}
        }
    }

    let name = el.value().name();
    if name == "p" {
        stats.paras += 1;
    }
    if name == "a" {
        stats.link_text_len = stats.text_len;
    }

    // Candidate scoring. A positive-marked node must beat
    // ancestor wrappers that merely contain it.
    if stats.text_len >= 140 {
        let link_density = stats.link_text_len as f64 / (stats.text_len.max(1) as f64);
        // Link density discounts EVERYTHING, not just raw text:
        // a sidebar of link lists used to win on punctuation and
        // paragraph counts inside the link labels themselves
        // (xkcd: #bottom "Comics I enjoy: ..." outranked the comic).
        // Content containers keep ~full weight; nav/sidebar
        // collapse to ~0.
        let content_factor = (1.0 - link_density).clamp(0.0, 1.0).powi(2);
        let score = (stats.text_len as f64 + stats.punct as f64 * 15.0 + stats.paras as f64 * 40.0)
            * content_factor
            + tag_prior(name)
            + class_prior(el.value());
        // Body is always a wrapper : it includes nav, sidebar,
        // footer, and content. Without this penalty, large pages
        // (old.reddit with comments) score body higher than the
        // real content container, and extraction drowns in
        // boilerplate. 0.5× lets real content containers (with
        // positive classes) always win, while on simple pages
        // without containers, body still beats individual
        // paragraphs (its total score is higher than any one).
        let score = if name == "body" { score * 0.5 } else { score };
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, el));
        }
    }

    (stats, best)
}
