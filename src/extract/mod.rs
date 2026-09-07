//! DonSift : the extraction engine of DonSeTch.
//!
//! HTML bytes in → agent-native markdown out. Block model, not
//! article text: typed blocks with heading breadcrumbs power BM25
//! focus, stable pagination, and token-war rendering policies.
//!
//! Pipeline: decode charset → parse once (no mutation) → metadata →
//! scope (selector or density-scored main) → segment blocks →
//! focus filter → render markdown → paginate.

pub mod blocks;
pub mod charset;
pub mod fallback;
pub mod feed;
pub mod focus;
pub mod hn;
pub mod inline;
pub mod jsdata;
pub mod junk;
pub mod language;
pub mod math;
pub mod metadata;
pub mod reddit;
pub mod render;
pub mod score;

#[cfg(test)]
mod tests;

use scraper::Html;

#[derive(Default, Clone)]
pub struct ExtractOptions {
    /// BM25 relevance query: keep only blocks matching, with context.
    pub focus: Option<String>,
    /// CSS selector: extract only from matching subtrees.
    pub selector: Option<String>,
    /// Max chars of markdown to return (default 16_000).
    pub max_chars: Option<usize>,
    /// Resume offset into the (post-focus) markdown.
    pub offset: usize,
    /// Keep [text](url) links; default strips to text (token saver).
    pub include_links: bool,
    /// Keep ![alt](src) media lines; default drops them.
    pub include_media: bool,
    /// Outline only: heading tree, no body text. Lets an
    /// agent read structure first, then target a section.
    pub toc: bool,
    /// Scope to one heading section (substring, case-
    /// insensitive). Pairs with toc.
    pub section: Option<String>,
    /// Probe (v3): verification query : when set, the extraction
    /// runs in full but the output is reduced to hit/no-hit plus
    /// short context excerpts around matches. The page never
    /// enters the agent's context wholesale. Case-insensitive
    /// substring; `/re/` syntax = regex.
    pub must_contain: Option<String>,
}

impl ExtractOptions {
    /// A regex probe: `must_contain` wrapped in slashes.
    pub fn probe_is_regex(&self) -> bool {
        self.must_contain.as_deref().is_some_and(|m| {
            if !m.starts_with('/') || m.len() < 3 {
                return false;
            }
            let inner = &m[1..];
            match inner.rfind('/') {
                Some(pos) if pos >= 1 => inner[pos + 1..].chars().all(char::is_alphabetic),
                _ => false,
            }
        })
    }

    /// The probe pattern with regex slashes and trailing flags stripped.
    pub fn probe_pattern(&self) -> &str {
        match &self.must_contain {
            Some(m) if self.probe_is_regex() => {
                let inner = &m[1..];
                let at = inner.rfind('/').unwrap_or(inner.len());
                &inner[..at]
            }
            Some(m) => m,
            None => "",
        }
    }
}

pub struct Extracted {
    pub markdown: String,
    pub title: Option<String>,
    // byline/published/site are rendered into the markdown
    // frontmatter; the MCP layer also reads them directly.
    #[allow(dead_code)]
    pub byline: Option<String>,
    #[allow(dead_code)]
    pub published: Option<String>,
    #[allow(dead_code)]
    pub site: Option<String>,
    /// Full markdown length after focus, before pagination.
    pub total_chars: usize,
    pub next_offset: Option<usize>,
    pub blocks_total: usize,
    pub blocks_shown: usize,
    /// Rough token estimate (chars / 4) of the returned markdown.
    pub tokens_est: usize,
    /// True when the page was large but almost no content
    /// extracted : a JS shell. Tier 2's job.
    pub thin: bool,
    /// Best-guess content kind from block composition.
    /// Conservative: only non-Page when confident.
    pub content_kind: ContentKind,
    /// Detected language (BCP-47 code: "en", "zh", "ja", etc.).
    #[allow(dead_code)]
    pub lang: String,
    /// Quality score 0.0..1.0 : content density, metadata
    /// completeness, structure diversity. Helps agents
    /// decide if content is trustworthy.
    #[allow(dead_code)]
    pub quality: f32,
    /// PDF only: per-page extraction stats (chars, ocr flag,
    /// confidence). Block merging intentionally flows paragraphs
    /// across page breaks for reading continuity : page
    /// boundaries are preserved HERE instead.
    pub pdf_pages: Option<Vec<crate::pdf::PageMeta>>,
    /// Content-area images (alt, src) capped at 12 : collected
    /// whether or not media lines are rendered, so image-text OCR
    /// (v3) can run on demand.
    pub images: Vec<(String, String)>,
    /// Knowledge fingerprint (v3): sha256 of the FULL pre-
    /// pagination markdown. Stable across max_chars settings :
    /// re-fetches compare apples to apples.
    pub fingerprint: Option<String>,
    /// Domain-intelligence adapter that produced this result
    /// (v3): the honest `via=adapter:...` label. `None` = the
    /// generic DonSift pipeline.
    pub via: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Article,
    Listing,
    Forum,
    Docs,
    Table,
    Page, // unsure
}

#[derive(Debug)]
pub enum ExtractError {
    BadSelector(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::BadSelector(s) => write!(f, "invalid CSS selector: {s}"),
        }
    }
}

/// Quality score 0.0..1.0 : content density, metadata
/// completeness, structure diversity, and language
/// coherence. Helps agents decide if the extracted
/// content is trustworthy enough to quote.
fn quality_score(
    meta: &metadata::Meta,
    kept: &[&blocks::Block],
    blocks_total: usize,
    raw_len: usize,
    lang_info: &language::LanguageInfo,
) -> f32 {
    let mut score = 0.0;

    // 1. Content density: extracted chars vs raw HTML bytes.
    let text_len: usize = kept.iter().map(|b| b.chars()).sum();
    if raw_len > 0 {
        let density = text_len as f64 / raw_len as f64;
        score += (density * 4.0).min(1.0) * 0.25;
    }

    // 2. Metadata completeness.
    if meta.title.is_some() {
        score += 0.1;
    }
    if meta.byline.is_some() {
        score += 0.05;
    }
    if meta.published.is_some() {
        score += 0.05;
    }
    if meta.site.is_some() {
        score += 0.05;
    }

    // 3. Structure diversity: headings + paragraphs + lists.
    let mut headings = 0;
    let mut paras = 0;
    let mut lists = 0;
    let mut code = 0;
    let mut tables = 0;
    let mut quotes = 0;
    for b in kept {
        match b {
            blocks::Block::Heading { .. } => headings += 1,
            blocks::Block::Para { .. } => paras += 1,
            blocks::Block::List { .. } => lists += 1,
            blocks::Block::Code { .. } => code += 1,
            blocks::Block::Table { .. } => tables += 1,
            blocks::Block::Quote { .. } => quotes += 1,
            _ => {}
        }
    }
    let structure_types = [
        headings > 0,
        paras > 0,
        lists > 0,
        code > 0,
        tables > 0,
        quotes > 0,
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    score += (structure_types as f64 / 6.0) * 0.2;

    // 4. Block count health: enough blocks to be real content.
    if blocks_total >= 5 {
        score += 0.1;
    }
    if blocks_total >= 20 {
        score += 0.05;
    }

    // 5. Language detected (not unknown).
    if lang_info.code != "unknown" && lang_info.code != "und" {
        score += 0.05;
    }

    // 6. Text volume: actual prose exists.
    if text_len > 500 {
        score += 0.1;
    }
    if text_len > 2000 {
        score += 0.05;
    }

    score.min(1.0) as f32
}

/// Classify content from block composition.
/// Conservative : Page when nothing is dominant.
fn classify(blocks: &[&blocks::Block]) -> ContentKind {
    let mut code = 0usize;
    let mut tables = 0usize;
    let mut lists = 0usize;
    let mut list_items = 0usize;
    let mut list_chars = 0usize;
    let mut quotes = 0usize;
    let mut para_chars = 0usize;
    let mut paras = 0usize;
    let mut headings = 0usize;
    for b in blocks {
        match b {
            blocks::Block::Code { .. } => code += 1,
            blocks::Block::Table { .. } => tables += 1,
            blocks::Block::List { items, .. } => {
                lists += 1;
                list_items += items.len();
                list_chars += items.iter().map(|i| i.len()).sum::<usize>();
            }
            blocks::Block::Quote { .. } => quotes += 1,
            blocks::Block::Para { md, .. } => {
                paras += 1;
                para_chars += md.len();
            }
            blocks::Block::Heading { .. } => headings += 1,
            _ => {}
        }
    }
    if code >= 3 {
        return ContentKind::Docs;
    }
    // Article = heading-STRUCTURED prose: several
    // headings, substantial paragraphs between them.
    // Char mass lies (reference lists outweigh prose).
    if headings >= 3 && paras >= 5 && para_chars / paras.max(1) > 150 {
        return ContentKind::Article;
    }
    if tables >= 2 && tables >= paras {
        return ContentKind::Table;
    }
    if quotes >= 5 {
        return ContentKind::Forum;
    }
    if lists >= 3 && list_items >= 12 && list_chars > paras * 200 {
        return ContentKind::Listing;
    }
    if paras >= 3 && para_chars / paras.max(1) > 200 {
        return ContentKind::Article;
    }
    ContentKind::Page
}

/// Extract agent-ready markdown from a fetched body.
///
/// `content_type` is the raw Content-Type header value (may be
/// empty). Non-HTML bodies pass through (truncated by max_chars).
pub fn extract(
    body: &[u8],
    content_type: &str,
    url: &str,
    opts: &ExtractOptions,
) -> Result<Extracted, ExtractError> {
    let max_chars = opts.max_chars.unwrap_or(16_000).max(200);

    // Non-HTML passthrough (json/text/xml): no extraction lies.
    let ct = content_type.to_lowercase();
    let is_pdf = body.len() >= 5 && body.starts_with(b"%PDF-") || ct.contains("pdf");

    // Feeds (RSS/Atom/JSON Feed): structured rendering, never a
    // raw XML blob. Checked BEFORE passthrough : feed content
    // types (text/xml, application/rss+xml…) never say "html".
    if !is_pdf
        && feed::is_feed(&ct, body)
        && let Some(ex) = feed::extract(body, url, opts)
    {
        return Ok(ex);
    }

    // Domain-intelligence adapters (v3): JSON payloads from the
    // registry's URL rewrites (reddit .json, package APIs) render
    // structured : BEFORE the non-HTML passthrough so adapter JSON
    // never dumps raw.
    if !is_pdf && let Some(ex) = crate::adapters::extract_json(body, &ct, url, opts) {
        return Ok(ex);
    }

    // A "text/plain" body that is actually HTML (misconfigured
    // servers, raw git URLs): parse it as HTML, not as literal
    // text full of angle brackets.
    let plain_is_html =
        !ct.is_empty() && !ct.contains("html") && !is_pdf && body_starts_with_html(body);

    if !ct.is_empty() && !ct.contains("html") && !is_pdf && !plain_is_html {
        // Probe applies to non-HTML bodies too. A must_contain on a
        // text/plain or json/xml response returns MATCH/NO-MATCH plus
        // excerpts like the HTML path, never the full document.
        let text = String::from_utf8_lossy(body);
        if let Some(pattern) = &opts.must_contain
            && !pattern.trim().is_empty()
        {
            let md = probe_render(&text, opts.probe_pattern(), opts.probe_is_regex());
            return Ok(Extracted {
                tokens_est: md.len() / 4,
                total_chars: text.len(),
                markdown: md,
                title: None,
                byline: None,
                published: None,
                site: None,
                next_offset: None,
                thin: false,
                content_kind: ContentKind::Page,
                blocks_total: 1,
                blocks_shown: 1,
                lang: "unknown".to_string(),
                quality: 1.0,
                pdf_pages: None,
                images: vec![],
                fingerprint: None,
                via: None,
            });
        }
        let (slice, next) = paginate(&text, opts.offset, max_chars);

        return Ok(Extracted {
            tokens_est: slice.len() / 4,
            total_chars: text.len(),
            markdown: slice,
            title: None,
            byline: None,
            published: None,
            site: None,
            next_offset: next,
            blocks_total: 0,
            blocks_shown: 0,
            thin: false,
            content_kind: ContentKind::Page,
            lang: "unknown".to_string(),
            quality: 0.0,
            pdf_pages: None,
            images: Vec::new(),
            fingerprint: None,
            via: None,
        });
    }

    // --- PDF: DonSheet parses bytes into the same block model. ---
    if is_pdf {
        match crate::pdf::parse(body) {
            Ok(parsed) => {
                return downstream(
                    &parsed.meta,
                    parsed.blocks,
                    body.len(),
                    false,
                    false,
                    parsed.notes,
                    parsed.lang_info,
                    Some(parsed.pages_meta),
                    url,
                    opts,
                    max_chars,
                );
            }
            Err(crate::pdf::PdfFailure::Encrypted) => {
                return Ok(empty_pdf(
                    url,
                    "encrypted document : a password is required; could not extract text",
                ));
            }
            Err(crate::pdf::PdfFailure::Corrupt(msg)) => {
                return Ok(empty_pdf(url, &format!("{msg}; could not extract text")));
            }
            Err(crate::pdf::PdfFailure::TooLarge(n)) => {
                return Ok(empty_pdf(
                    url,
                    &format!(
                        "document too large ({:.0} MB > limit); could not extract text",
                        n as f64 / 1_048_576.0
                    ),
                ));
            }
            Err(crate::pdf::PdfFailure::NotPdf) => {
                return Ok(empty_pdf(
                    url,
                    "bytes do not decode as a PDF; could not extract text",
                ));
            }
        }
    }

    // --- HTML upstream ---
    let html_text = charset::decode(body, &ct);
    let raw_len = body.len();

    // Reddit dedicated extractor: bypasses DonSift for
    // old.reddit.com, produces compact structured output.
    // Returns None for non-reddit or unrecognized pages.
    if let Some(extracted) = reddit::extract(&html_text, url, opts) {
        return Ok(extracted);
    }

    // Hacker News dedicated extractor: the comment tree is a
    // table layout the generic pipeline mangles. Full comment
    // text with authors and reply depth.
    if let Some(extracted) = hn::extract(&html_text, url, opts) {
        return Ok(extracted);
    }

    // Domain-intelligence adapters (v3): GitHub / Stack Exchange /
    // Wikipedia infobox / docs-framework pages restructured.
    if let Some(extracted) = crate::adapters::extract_html(&html_text, url, opts) {
        return Ok(extracted);
    }

    let doc = Html::parse_document(&html_text);
    let base = metadata::base_url(&doc).unwrap_or_else(|| url.to_string());
    let meta = metadata::metadata(&doc);
    let lang_info = language::detect(&doc);

    // A large page that yields almost nothing is a JS
    // shell (Medium, SPAs) : flag it for tier 2 routing.
    let thin_flag = raw_len > 50_000;

    // Skeleton/SPA loading detection. Only use aria-busy :
    // the word "skeleton" appears in CSS class names on
    // fully-hydrated pages (Amazon, React apps), making it
    // a false-positive. aria-busy is a reliable loading
    // signal set by the browser, not CSS.
    // ASCII-case-insensitive scan (L9): was a whole-document
    // to_lowercase() allocation just to count three markers.
    let has_skeletons = count_ascii_ci(&html_text, "aria-busy=\"true\"", 3) >= 3;

    // Scope: explicit selector or scored main-content detection.
    let roots: Vec<scraper::ElementRef<'_>> = if let Some(sel) = &opts.selector {
        let parsed =
            scraper::Selector::parse(sel).map_err(|_| ExtractError::BadSelector(sel.clone()))?;
        doc.select(&parsed).collect()
    } else {
        score::find_main(&doc).into_iter().collect()
    };

    // Segment into typed blocks.
    let mut all_blocks = Vec::new();
    for root in &roots {
        blocks::segment(*root, &base, opts, &mut all_blocks);
    }

    let extracted = downstream(
        &meta,
        all_blocks,
        raw_len,
        thin_flag,
        has_skeletons,
        Vec::new(),
        lang_info,
        None,
        url,
        opts,
        max_chars,
    )?;

    // A TOC is a complete projection, not a failed short extraction. Once
    // downstream has built the outline, content-oriented rescue paths must
    // not replace it with the page body.
    if opts.toc {
        return Ok(extracted);
    }

    // JSON-in-script rescue: SPAs (Next.js/React/YouTube) embed
    // their content as a JS-assigned JSON blob. DonSift sees an
    // empty shell, but the data is sitting in the HTML. When
    // DonSift came up thin, mine the embedded JSON : if it's
    // richer, it wins. This is the tier-1 unlock for the SPA
    // class of sites.
    // When focus is active, short content is intentional
    // (the agent asked for a slice), not a shell signal. Only
    // apply the < 600 threshold when focus is not set.
    if (extracted.thin || (extracted.total_chars < 600 && opts.focus.is_none()))
        && let Some(mut js) = jsdata::extract(&html_text, url, opts)
        && js.total_chars > extracted.total_chars
    {
        // The fallback wins on text, but the scoped segment's
        // image list is still the best image list.
        if js.images.is_empty() {
            js.images = extracted.images.clone();
        }
        return Ok(js);
    }

    // Raw text fallback: when block extraction fails (returns thin)
    // but the DOM has real visible text, strip tags and return text.
    // This makes "found DOM but failed to extract content" IMPOSSIBLE
    // when the DOM has real content. Less structured than block-based
    // extraction (no proper paragraphs/lists/tables), but infinitely
    // better than returning nothing. The fallback preserves heading
    // structure (h1-h6 → markdown headings) and paragraph breaks.
    // When focus is active, short content is intentional (the agent
    // asked for a filtered slice), not a sign of extraction failure.
    let needs_fallback = extracted.thin
        || (extracted.total_chars < fallback::FALLBACK_MIN_TEXT && opts.focus.is_none());
    if needs_fallback
        && let Some(mut fb) = fallback::text_fallback(&html_text, &meta, url, opts, max_chars)
    {
        // Comic/gallery pages go thin and fall back here; the
        // scoped Media blocks (the actual content images) must
        // survive the handoff for on-demand OCR.
        if fb.images.is_empty() {
            fb.images = extracted.images.clone();
        }
        return Ok(fb);
    }
    Ok(extracted)
}

/// Does the body start (after BOM/whitespace) with an HTML
/// doctype or `<html` tag? Used to catch HTML served as
/// text/plain.
fn body_starts_with_html(body: &[u8]) -> bool {
    let mut s = body;
    if s.starts_with(&[0xEF, 0xBB, 0xBF]) {
        s = &s[3..];
    }
    let s = String::from_utf8_lossy(&s[..s.len().min(256)]);
    let t = s.trim_start().to_lowercase();
    t.starts_with("<!doctype html") || t.starts_with("<html")
}

/// Honest stub for PDFs that could not be parsed.
fn empty_pdf(url: &str, reason: &str) -> Extracted {
    let md = format!("{url}\n\n*[pdf: {reason}]*\n");
    Extracted {
        tokens_est: md.len() / 4,
        total_chars: md.len(),
        markdown: md,
        title: None,
        byline: None,
        published: None,
        site: None,
        next_offset: None,
        blocks_total: 0,
        blocks_shown: 0,
        thin: false,
        content_kind: ContentKind::Page,
        lang: "unknown".to_string(),
        quality: 0.0,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
    }
}

/// Shared downstream: TOC → section scope → focus BM25 → render →
/// trust signals → pagination → quality. Identical for HTML and PDF;
/// PDF upstream produces blocks + meta + notes + language and hands
/// them here.
#[allow(clippy::too_many_arguments)]
fn downstream(
    meta: &crate::extract::metadata::Meta,
    mut all_blocks: Vec<blocks::Block>,
    raw_len: usize,
    thin_flag: bool,
    has_skeletons: bool,
    notes: Vec<String>,
    lang_info: language::LanguageInfo,
    pdf_pages: Option<Vec<crate::pdf::PageMeta>>,
    url: &str,
    opts: &ExtractOptions,
    max_chars: usize,
) -> Result<Extracted, ExtractError> {
    // TOC mode: heading tree only, with stable section IDs and
    // per-section size labels so the agent can target and budget
    // before reading anything.
    if opts.toc {
        let mut md = String::new();
        if let Some(t) = &meta.title {
            md.push_str(&format!("# {t}\n\n"));
        }
        let mut shown = 0usize;
        let sizes = section_sizes(&all_blocks);
        for b in &all_blocks {
            if let blocks::Block::Heading { level, text, .. } = b {
                let indent = "  ".repeat((*level as usize).saturating_sub(1));
                let size = sizes.get(shown).copied().unwrap_or(0);
                md.push_str(&format!(
                    "{indent}- [s{}] {} · {}\n",
                    shown + 1,
                    text,
                    size_label(size)
                ));
                shown += 1;
            }
        }
        if shown == 0 {
            md.push_str("*(no headings : flat page)*\n");
        } else {
            md.push_str("\n*fetch with section=\"sN\" (or a heading name) to read that part*\n");
        }
        return Ok(Extracted {
            tokens_est: md.len() / 4,
            total_chars: md.len(),
            markdown: md,
            title: meta.title.clone(),
            byline: meta.byline.clone(),
            published: meta.published.clone(),
            site: meta.site.clone(),
            next_offset: None,
            blocks_total: all_blocks.len(),
            blocks_shown: shown,
            thin: false,
            content_kind: ContentKind::Page,
            lang: lang_info.code.clone(),
            quality: 0.0,
            pdf_pages: None,
            images: Vec::new(),
            fingerprint: None,
            via: None,
        });
    }

    // Section scope: keep blocks under a matching heading.
    // v3: "sN" (case-insensitive) targets the Nth heading exactly
    // as the toc numbered it; anything else is a substring match
    // on heading text.
    let mut section_missed = false;
    let mut section_hit = false;
    if let Some(sec) = &opts.section {
        let needle = sec.to_lowercase();
        let by_id: Option<usize> = needle
            .strip_prefix('s')
            .filter(|_| needle.len() >= 2 && needle[1..].chars().all(|c| c.is_ascii_digit()))
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n.saturating_sub(1));
        let mut heading_idx = 0usize;
        let mut in_section = false;
        let mut section_level = 0u8;
        let mut kept_idx: Vec<usize> = Vec::new();
        for (i, b) in all_blocks.iter().enumerate() {
            if let blocks::Block::Heading { level, text, .. } = b {
                if in_section && *level <= section_level {
                    // Section ends at the next heading
                    // of same-or-higher level.
                    in_section = false;
                }
                if !in_section {
                    let matched = match by_id {
                        Some(id) => heading_idx == id,
                        None => text.to_lowercase().contains(&needle),
                    };
                    if matched {
                        in_section = true;
                        section_level = *level;
                    }
                }
                heading_idx += 1;
            }
            if in_section {
                kept_idx.push(i);
            }
        }
        if !kept_idx.is_empty() {
            section_hit = true;
            all_blocks = kept_idx
                .into_iter()
                .map(|i| all_blocks[i].clone())
                .collect();
        } else {
            // No match → full page, but SIGNAL it.
            section_missed = true;
        }
    }

    // Code block fission: split large code blocks into sub-blocks
    // so focus can score and select relevant parts of structured
    // content (JSON schemas, large code listings) instead of
    // treating the entire block as one monolithic document.
    if opts.focus.is_some() {
        all_blocks = focus::expand_code_blocks(all_blocks);
    }

    let blocks_total = all_blocks.len();

    // Content images for on-demand OCR (v3) : capped, order kept.
    let images: Vec<(String, String)> = all_blocks
        .iter()
        .filter_map(|b| match b {
            blocks::Block::Media { alt, src, .. } => Some((alt.clone(), src.clone())),
            _ => None,
        })
        .take(12)
        .collect();

    // Focus: section-aware block filter (Section Gravity).
    // fell_back = query matched nothing → full content returned,
    // MUST be signaled.
    let (kept, focus_fell_back): (Vec<&blocks::Block>, bool) = match &opts.focus {
        Some(q) => focus::filter_semantic(&all_blocks, q, &lang_info),
        None => (all_blocks.iter().collect(), false),
    };
    let blocks_shown = kept.len();

    // Dropped-content manifest (v3): when focus removed blocks,
    // one accounting line so omission is audited, never silent.
    // None when nothing was dropped or focus fell back (that case
    // gets its own full-content notice).
    let dropped_note = if opts.focus.is_some() && !focus_fell_back && kept.len() < blocks_total {
        dropped_manifest(&all_blocks, &kept)
    } else {
        None
    };

    // Render markdown (frontmatter + blocks) then paginate.
    let mut full = render::render(meta, url, &kept, opts);

    // Engine notes first (PDF scan flags etc.) : they frame any
    // other trust signal that follows.
    for note in &notes {
        full = format!("*[pdf: {note}]*\n\n{full}");
    }

    // Agent-trust signals, inline in the content:
    // - focus miss → agent must not quote wrong content
    // - empty page → silence looks like a bug
    if focus_fell_back {
        if let Some(q) = &opts.focus {
            full = format!("*[focus \"{q}\": no matches : showing full content]*\n\n{full}");
        }
    } else if section_missed {
        if let Some(s) = &opts.section {
            full = format!("*[section \"{s}\": not found : showing full content]*\n\n{full}");
        }
    } else if full.trim().is_empty() || (blocks_total == 0 && meta.title.is_none()) {
        full = format!("{url}\n\n*(no extractable content)*\n");
    }

    // The manifest goes after the fell-back/missed notices (which
    // are rarer and more important) but before any body content.
    if let Some(m) = dropped_note {
        full = format!("*[{m}]*\n\n{full}");
    }

    // JS-shell warning: agent must know the content
    // below is likely incomplete. On tier=auto the MCP
    // fetch escalates to the browser itself : this note
    // only surfaces on an explicit tier=1 request.
    //
    // Thinness: the extraction yield is the truth. Any page over
    // 5KB that yields < 800 chars is a shell (the 27KB-challenge-
    // page-with-250-chars class : three boilerplate blocks used
    // to pass as non-thin). Zero blocks or a >50KB page with
    // almost nothing are shells at any size. Skeleton markers
    // stay a secondary signal for borderline yields.
    //
    // Content density: a page > 20KB that yields < 5% of its
    // raw size as text, with < 3000 chars total, is a JS shell.
    // SPAs that server-render their layout (navigation, sidebar,
    // footer) produce enough boilerplate text to pass the < 800
    // char threshold, but the main content is client-rendered.
    // Measured: artstation 0.9% density, pixiv 2.1% (both > 20KB).
    // Real pages: 15-40%+ density. bilibili at 6% with 1476 chars
    // is NOT flagged (above the 5% density threshold, and above
    // 3000 chars is not required because density already says
    // it's real content).
    // A matched section is intentionally small : the agent asked for
    // exactly this slice. Shell detection must not fire on it (a
    // small section on a 400KB page used to escalate to ghost and
    // return the full page instead of the section).
    let density = if raw_len > 0 {
        full.len() as f64 / raw_len as f64
    } else {
        1.0
    };
    let is_shell = raw_len > 20_000 && density < 0.05 && full.len() < 3_000;
    // A PDF can legitimately contain only a few words. It is already parsed
    // from a complete binary document, so HTML shell heuristics must never
    // label a short PDF as a JS-rendered page.
    let is_pdf = pdf_pages.is_some();
    let thin = !is_pdf
        && !section_hit
        && ((full.len() < 800 && (thin_flag || raw_len > 5_000 || blocks_total == 0))
            || (thin_flag && has_skeletons && full.len() < 4000)
            || is_shell);
    if thin {
        full = format!(
            "*[note: large page rendered almost no content : likely JS-rendered (SPA). Content below may be a shell; use tier=auto to render with a real browser.]*\n\n{full}"
        );
    }
    let (slice, next) = paginate(&full, opts.offset, max_chars);
    let tokens_est = slice.len() / 4;

    let content_kind = classify(&kept);
    let quality = quality_score(meta, &kept, blocks_total, raw_len, &lang_info);

    // Probe mode (v3): the page resolved fully (tiers, walls and
    // all) but the agent only asked a verification question. The
    // output collapses to a verdict + short context excerpts :
    // the page itself never enters the agent's context.
    if let Some(pattern) = &opts.must_contain
        && !pattern.trim().is_empty()
    {
        let md = probe_render(&full, opts.probe_pattern(), opts.probe_is_regex());
        return Ok(Extracted {
            tokens_est: md.len() / 4,
            total_chars: full.len(),
            markdown: md,
            next_offset: None,
            thin: false,
            content_kind: ContentKind::Page,
            title: meta.title.clone(),
            byline: meta.byline.clone(),
            published: meta.published.clone(),
            site: meta.site.clone(),
            blocks_total,
            blocks_shown,
            lang: lang_info.code.clone(),
            quality,
            pdf_pages,
            images,
            fingerprint: None,
            via: None,
        });
    }

    Ok(Extracted {
        markdown: slice,
        title: meta.title.clone(),
        byline: meta.byline.clone(),
        published: meta.published.clone(),
        site: meta.site.clone(),
        total_chars: full.len(),
        next_offset: next,
        blocks_total,
        blocks_shown,
        tokens_est,
        thin,
        content_kind,
        lang: lang_info.code.clone(),
        quality,
        pdf_pages,
        images,
        fingerprint: Some(crate::pages::history::PageHistory::fingerprint(&full)),
        via: None,
    })
}

/// Probe rendering (v3): verdict + up to three context excerpts
/// around matches, whitespace-collapsed to one line each. The
/// honest answer to "does this page mention X?" at ~3 lines of
/// output instead of 16k chars.
pub fn probe_render(text: &str, pattern: &str, is_regex: bool) -> String {
    let total = text.chars().count();
    let report = |hits: Option<Vec<(usize, usize)>>| -> String {
        match hits {
            None => format!("probe: invalid regex /{pattern}/ : use a plain substring instead"),
            Some(v) if v.is_empty() => {
                format!("probe: NO MATCH for {pattern:?} in {total} chars of content")
            }
            Some(v) => {
                let mut out = format!(
                    "probe: MATCH : {} hit{} for {pattern:?}\n",
                    v.len(),
                    if v.len() == 1 { "" } else { "s" }
                );
                for (i, (s, e)) in v.iter().take(3).enumerate() {
                    let ctx = context_around(text, *s, *e, 90);
                    out.push_str(&format!("[{}] {ctx}\n", i + 1));
                }
                if v.len() > 3 {
                    out.push_str(&format!("(+{} more)\n", v.len() - 3));
                }
                out
            }
        }
    };

    // Both modes go through one case-insensitive regex so every hit
    // is a byte offset into `text` itself. The old substring path
    // searched a separately lowercased copy, whose offsets drift
    // from the real text as soon as case folding changes a byte
    // length ('İ' -> "i̇"); and both paths then fed those BYTE
    // offsets to a context window that treated them as CHAR indices,
    // so on any page with non-ASCII text before the hit the excerpt
    // landed after the match and did not contain it.
    let source = if is_regex {
        std::borrow::Cow::Borrowed(pattern)
    } else {
        std::borrow::Cow::Owned(regex::escape(pattern))
    };
    match regex::RegexBuilder::new(&source)
        .case_insensitive(true)
        .size_limit(1 << 20)
        .build()
    {
        Ok(re) => {
            let hits: Vec<(usize, usize)> = re
                .find_iter(text)
                .map(|m| (m.start(), m.end()))
                .take(50)
                .collect();
            report(Some(hits))
        }
        // A literal can only fail to build by blowing the size
        // limit (a multi-KB pattern under (?i) expands into Unicode
        // classes); it is never "invalid", so search it as bytes
        // instead. ASCII case-insensitive is the honest fallback:
        // non-ASCII bytes must match exactly, which also keeps every
        // hit on a char boundary.
        Err(_) if !is_regex => report(Some(literal_hits(text, pattern))),
        Err(_) => report(None),
    }
}

/// Byte-window search for a literal `needle` in `text`, ASCII
/// case-insensitive. Non-ASCII bytes compare exactly, so a match
/// start is always a char boundary (a UTF-8 lead byte or ASCII, never
/// a continuation byte) and `start + needle.len()` is too.
fn literal_hits(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let (hay, nb) = (text.as_bytes(), needle.as_bytes());
    if nb.is_empty() || nb.len() > hay.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut from = 0;
    while from + nb.len() <= hay.len() {
        match hay[from..]
            .windows(nb.len())
            .position(|w| w.eq_ignore_ascii_case(nb))
        {
            Some(pos) => {
                let start = from + pos;
                hits.push((start, start + nb.len()));
                from = start + nb.len();
                if hits.len() >= 50 {
                    break;
                }
            }
            None => break,
        }
    }
    hits
}

/// One-line context window around a match, ellipsized and
/// whitespace-collapsed. `start`/`end` are BYTE offsets into `text`
/// (char boundaries, as regex matches are); `pad` is in chars.
fn context_around(text: &str, start: usize, end: usize, pad: usize) -> String {
    let byte_lo = if pad == 0 {
        start
    } else {
        text[..start]
            .char_indices()
            .rev()
            .nth(pad - 1)
            .map(|(b, _)| b)
            .unwrap_or(0)
    };
    let byte_hi = text[end..]
        .char_indices()
        .nth(pad)
        .map(|(b, _)| end + b)
        .unwrap_or(text.len());
    let window = &text[byte_lo..byte_hi];
    let collapsed: String = window.split_whitespace().collect::<Vec<_>>().join(" ");
    let prefix = if byte_lo > 0 { "…" } else { "" };
    let suffix = if byte_hi < text.len() { "…" } else { "" };
    format!("{prefix}{collapsed}{suffix}")
}

/// One-line accounting of what the focus filter dropped: block
/// count, word estimate, and up to four section names the dropped
/// content lived under. Omission must be audited, never silent :
/// but the audit itself must never cost more than one line.
fn dropped_manifest(all: &[blocks::Block], kept: &[&blocks::Block]) -> Option<String> {
    let kept_ptrs: std::collections::HashSet<usize> = kept
        .iter()
        .map(|b| std::ptr::from_ref::<blocks::Block>(b) as usize)
        .collect();
    let mut dropped_blocks = 0usize;
    let mut dropped_chars = 0usize;
    let mut sections: Vec<&str> = Vec::new();
    for b in all {
        if kept_ptrs.contains(&(std::ptr::from_ref(b) as usize)) {
            continue;
        }
        dropped_blocks += 1;
        dropped_chars += b.chars();
        if let Some(name) = b.path().last()
            && !name.is_empty()
            && name.len() <= 32
            && !sections.contains(&name.as_str())
        {
            sections.push(name);
        }
    }
    if dropped_blocks == 0 {
        return None;
    }
    let words = dropped_chars / 6;
    let words_fmt = if words >= 1000 {
        format!("{:.1}k", words as f64 / 1000.0)
    } else {
        words.to_string()
    };
    let mut line = format!("dropped by focus: {dropped_blocks} blocks (~{words_fmt} words)");
    if !sections.is_empty() {
        let names: Vec<&str> = sections.iter().take(4).copied().collect();
        line.push_str(&format!(" : {}", names.join(", ")));
    }
    // Hard cap: the audit line must never outweigh what it audits.
    if line.chars().count() > 160 {
        line = line.chars().take(157).collect::<String>() + "…";
    }
    Some(line)
}

/// Compact char-count label for toc lines: "1.2k", "840".
fn size_label(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k", chars as f64 / 1000.0)
    } else {
        chars.to_string()
    }
}

/// Per-heading content size, in heading order: the heading's own
/// chars plus every following block until the next heading of
/// same-or-higher level (subsections included). Powers the toc's
/// size labels : the agent's fetch-cost signal before it reads.
fn section_sizes(all: &[blocks::Block]) -> Vec<usize> {
    let mut sizes = Vec::new();
    let mut i = 0;
    while i < all.len() {
        if let blocks::Block::Heading { level, .. } = &all[i] {
            let mut size = all[i].chars();
            let mut j = i + 1;
            while j < all.len() {
                if let blocks::Block::Heading { level: l2, .. } = &all[j]
                    && *l2 <= *level
                {
                    break;
                }
                size += all[j].chars();
                j += 1;
            }
            sizes.push(size);
        }
        i += 1;
    }
    sizes
}

/// Char-budget slice at a UTF-8 boundary, preferring a block
/// boundary ("\n\n") near the cut. Returns (slice, next_offset).
fn paginate(text: &str, offset: usize, max_chars: usize) -> (String, Option<usize>) {
    if offset >= text.len() {
        return (String::new(), None);
    }
    let mut start = ceil_char_boundary(text, offset);
    // Seek forward to the next block boundary ("\n\n") when
    // resuming. Agents who resume at offset=N should start at a
    // clean paragraph/heading boundary, not mid-sentence.
    if offset > 0 {
        // Floor the window end to a char boundary : a mid-char
        // offset into CJK text makes start+500 mid-character and
        // the slice below would panic.
        let mut search_end = start.saturating_add(500).min(text.len());
        while !text.is_char_boundary(search_end) {
            search_end -= 1;
        }
        if let Some(pos) = text[start..search_end].find("\n\n") {
            start = start + pos + 2; // skip past the "\n\n"
        }
    }
    // saturating: max_chars comes from tool args; a hostile/huge
    // value must not wrap end below start (slice panic).
    let mut end = start.saturating_add(max_chars).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        // Prefer a block boundary within the last quarter of the window.
        let window_start = ceil_char_boundary(text, start + (end - start) * 3 / 4);
        if let Some(pos) = text[window_start..end].rfind("\n\n") {
            end = window_start + pos;
        }
    }
    let next = if end < text.len() { Some(end) } else { None };
    let mut slice = text[start..end].to_string();
    // In-content truncation marker: agents read content,
    // not metadata : the resume instruction must be IN
    // the markdown.
    if let Some(n) = next {
        slice.push_str(&format!("\n\n*[truncated : continue with offset={n}]*"));
    }
    (slice, next)
}

/// Shared pagination for dedicated extractors (reddit, hn).
pub fn paginate_public(text: &str, offset: usize, max_chars: usize) -> (String, Option<usize>) {
    paginate(text, offset, max_chars)
}

fn ceil_char_boundary(text: &str, mut i: usize) -> usize {
    if i >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Count case-insensitive ASCII occurrences of `needle` in `hay`,
/// stopping at `max` (the detectors only compare against a small
/// threshold). No allocation: was a whole-document to_lowercase() just
/// to count three `aria-busy` markers (L9).
fn count_ascii_ci(hay: &str, needle: &str, max: usize) -> usize {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return 0;
    }
    let first_low = n[0].to_ascii_lowercase();
    let first_high = n[0].to_ascii_uppercase();
    let mut count = 0usize;
    let mut i = 0usize;
    while i + n.len() <= h.len() && count < max {
        if (h[i] == first_low || h[i] == first_high) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            count += 1;
            i += n.len();
        } else {
            i += 1;
        }
    }
    count
}
