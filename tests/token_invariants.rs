//! Token-efficiency invariants as a CI gate (v3 foundation #3).
//! Offline: real saved pages (tests/fixtures/corpus/), full DonSift
//! pipeline : the same claims bench/tokens.py makes live, asserted
//! deterministically on every build.

use donsetch::extract::{self, ExtractOptions};

fn corpus(name: &str) -> Vec<u8> {
    let p = format!(
        "{}/tests/fixtures/corpus/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {p}: {e}"))
}

fn opts_with(f: impl FnOnce(&mut ExtractOptions)) -> ExtractOptions {
    let mut o = ExtractOptions::default();
    f(&mut o);
    o
}

#[test]
fn focus_cuts_at_least_40_percent_on_a_long_page() {
    let body = corpus("wiki-rust.html");
    let full = extract::extract(
        &body,
        "text/html",
        "https://fixture.invalid/wiki/Rust_(programming_language)",
        &opts_with(|o| o.max_chars = Some(80_000)),
    )
    .unwrap();
    let focused = extract::extract(
        &body,
        "text/html",
        "https://fixture.invalid/wiki/Rust_(programming_language)",
        &opts_with(|o| {
            o.max_chars = Some(80_000);
            o.focus = Some("ownership borrow checker lifetime".into());
        }),
    )
    .unwrap();
    let saving = 1.0 - (focused.markdown.len() as f64 / full.markdown.len() as f64);
    assert!(
        saving >= 0.40,
        "focus saving {saving:.0}% < 40% (full {} chars, focused {})",
        full.markdown.len(),
        focused.markdown.len()
    );
}

#[test]
fn toc_costs_under_5_percent_of_the_full_page() {
    let body = corpus("wiki-rust.html");
    let full = extract::extract(
        &body,
        "text/html",
        "https://fixture.invalid/wiki/Rust_(programming_language)",
        &opts_with(|o| o.max_chars = Some(200_000)),
    )
    .unwrap();
    let toc = extract::extract(
        &body,
        "text/html",
        "https://fixture.invalid/wiki/Rust_(programming_language)",
        &opts_with(|o| {
            o.max_chars = Some(200_000);
            o.toc = true;
            o.section = Some("TOC".into());
        }),
    )
    .unwrap();
    // The toc-only view (section=TOC) must be a small fraction.
    assert!(
        toc.markdown.len() as f64 / full.markdown.len() as f64 <= 0.05,
        "toc output {} chars vs full {} : >5%",
        toc.markdown.len(),
        full.markdown.len()
    );
}

#[test]
fn probe_output_stays_tiny() {
    let body = corpus("wiki-rust.html");
    let probe = extract::extract(
        &body,
        "text/html",
        "https://fixture.invalid/wiki/Rust_(programming_language)",
        &opts_with(|o| o.must_contain = Some("borrow checker".into())),
    )
    .unwrap();
    // Verdict + up to 3 context excerpts (±90 chars each): the
    // invariant is "tiny vs the page", not an arbitrary constant.
    assert!(
        probe.markdown.len() <= 800,
        "probe output {} chars > 800",
        probe.markdown.len()
    );
    assert!(
        (probe.markdown.len() as f64 / probe.total_chars as f64) <= 0.02,
        "probe output {} is >2% of the {}-char page",
        probe.markdown.len(),
        probe.total_chars
    );
    assert!(
        probe.markdown.to_lowercase().contains("match"),
        "probe must carry the verdict"
    );
}

#[test]
fn mcp_instructions_stay_cheap() {
    // The handshake blurb is the one string an agent pays for in
    // every session, whether or not it ever calls us : so it is a
    // token invariant like any response size. Generated live, not
    // read from the fixture: a fixture is only as fresh as its
    // last blessing, so measuring it would pass on stale text and
    // report the bloat one run too late.
    let text = donsetch::mcp::tools::instructions();

    // chars/4 : the estimator the rest of the codebase uses.
    let tokens = text.len() / 4;
    assert!(
        tokens <= 165,
        "instructions cost ~{tokens} tokens (>165) : resident in every session"
    );
    // The 150 cap covered the 5-tool contract; web_screenshot (issue
    // #171) added the sixth tool line and the ceiling follows it.
}

#[test]
fn split_mcp_contract_stays_under_3000_estimated_tokens() {
    // chars/4 is the repository-wide offline estimator. Exact tokenizer
    // measurements belong in the external evaluation report. The
    // web_memory addition (v4 phase 5.2) spent part of the old
    // 2500 headroom; the ceiling tracks the 5-tool contract.
    let bytes = donsetch::mcp::tools::list().to_string().len();
    let estimated_tokens = bytes.div_ceil(4);
    assert!(
        estimated_tokens <= 3_000,
        "split MCP schema costs ~{estimated_tokens} tokens (>3000)"
    );
}

#[test]
fn links_on_renders_real_links() {
    // Wikipedia article body: hundreds of interwiki links. With
    // links=true the pipeline must render them as markdown links
    // (handle rewriting is the MCP layer's job on top).
    let body = corpus("wiki-rust.html");
    let ex = extract::extract(
        &body,
        "text/html",
        "https://fixture.invalid/wiki/Rust_(programming_language)",
        &opts_with(|o| {
            o.include_links = true;
            o.max_chars = Some(40_000);
        }),
    )
    .unwrap();
    let link_count = ex.markdown.matches("](http").count();
    assert!(
        link_count >= 30,
        "expected a link-heavy render, got {link_count} links"
    );
}
