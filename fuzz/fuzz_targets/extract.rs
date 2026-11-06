//! Fuzz the full extraction pipeline: arbitrary bytes + a
//! content-type string → DonSift, then the v5 view layer over the
//! extracted document. This is the broadest net: it covers the block
//! segmenter, junk filters, focus BM25, rendering, pagination, every
//! dedicated extractor (reddit, HN, feed, jsdata) that claims the
//! input, and the projections web_fetch exposes (toc, section, focus,
//! must_contain) including their parser.
//!
//! Every v2.5 daemon-abort panic class (charset multi-byte, js_unescape,
//! paginate overflow, debug slice) lives under this target.

#![no_main]

use donsetch::extract::view::{Term, View};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Derive a content-type from the first bytes so the fuzzer
    // explores both the HTML path and the passthrough path.
    let ct = match data.first() {
        Some(b'h') => "text/html; charset=utf-8",
        Some(b'p') => "application/pdf",
        Some(b'x') => "text/xml",
        Some(b'j') => "application/json",
        _ => "",
    };
    let body = &data[data.len().min(1)..];
    let url = "https://fuzz.example.com/page";

    // Exercise a spread of option combinations deterministically
    // derived from the input tail, so each corpus entry explores a
    // different feature path (focus, links, offsets, hostile max_chars).
    let tail = data.get(data.len().saturating_sub(8)..).unwrap_or(&[]);
    let mut opts = donsetch::extract::ExtractOptions::default();
    if tail.first() == Some(&b'f') {
        opts.focus = Some("target topic".to_string());
    }
    if tail.get(1) == Some(&b'm') {
        opts.focus = Some("/t.rg.t/.{0,64}/".to_string());
    }
    if tail.get(3) == Some(&b'l') {
        opts.include_links = true;
    }
    if let Some(n) = tail.get(4) {
        opts.offset = (*n as usize) << 20; // hostile offsets
        opts.max_chars = Some((*n as usize) << 24); // hostile max_chars
    }

    let extracted = donsetch::extract::extract(body, ct, url, &opts);

    // The views project the whole document afterwards; fuzz them over
    // whatever came out (an error has no document to project).
    if let Ok(ex) = extracted {
        let mut view = View {
            max_chars: 16_000,
            ..Default::default()
        };
        if tail.get(5) == Some(&b't') {
            view.toc = true;
        }
        if tail.get(6) == Some(&b's') {
            view.section = Some("intro".to_string());
        }
        if tail.get(7) == Some(&b'f') {
            view.focus = Some("target topic".to_string());
        }
        if let Some(raw) = tail.get(2).map(|b| format!("/{}/", *b as char)) {
            if let Ok(term) = Term::parse(&raw) {
                if tail.get(5) != Some(&b't') {
                    view.must_contain.push(term);
                }
            }
        }
        let incomplete = ex.partial.as_ref().map(|p| p.reason);
        let _ = donsetch::extract::view::render(&ex.markdown, &view, incomplete);
    }

    // Wall detection over the same bytes — DOM smart detection is
    // on every fetch's hot path.
    let _ = donsetch::detect::walls::detect_dom_smart(body);
    let _ = donsetch::detect::walls::visible_text_count(body);
});
