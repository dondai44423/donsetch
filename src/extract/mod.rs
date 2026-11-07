//! DonSift : the extraction engine of DonSeTch.
//!
//! HTML bytes in → agent-native markdown out. Block model, not
//! article text: typed blocks with heading breadcrumbs power crawl's
//! BM25 focus, stable pagination, and token-war rendering policies.
//! `web_fetch` views the extracted markdown through [`view`].
//!
//! Pipeline: decode charset → parse once (no mutation) → metadata →
//! density-scored main → segment blocks → focus filter → render
//! markdown → paginate.

mod amount;
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
pub mod nesting;
pub mod quality;
pub mod reddit;
pub mod render;
pub mod score;
pub mod view;

#[cfg(test)]
mod tests;

use scraper::Html;

/// How to extract a document. `web_fetch`'s views (outline, section, focus
/// passages, must_contain) project the extracted markdown afterwards, in
/// [`view`]; crawl narrows each page with `focus` here.
#[derive(Default, Clone)]
pub struct ExtractOptions {
    /// BM25 relevance query: keep only blocks matching, with context.
    pub focus: Option<String>,
    /// Max chars of markdown to return (default 16_000).
    pub max_chars: Option<usize>,
    /// Resume offset into the (post-focus) markdown.
    pub offset: usize,
    /// Keep [text](url) links; default strips to text (token saver).
    pub include_links: bool,
    /// Keep links as their real addresses. The default hands the caller
    /// short handles in their place, once the document is rendered.
    pub link_urls: bool,
    /// The fetch layer verified this body as content: bot-wall markers in it
    /// (an interstitial's words, a vendor script) are not a wall. Login walls
    /// and paywalls are still reported.
    pub verified: bool,
}

#[derive(Clone, serde::Serialize)]
pub struct PartialContent {
    pub reason: &'static str,
    pub items_found: usize,
    pub items_total: Option<usize>,
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
    /// Focus deliberately omitted source blocks, rows or unordered items.
    pub focus_omitted: bool,
    /// Rough token estimate (chars / 4) of the returned markdown.
    pub tokens_est: usize,
    /// True when the page was large but almost no content
    /// extracted : a JS shell.
    pub thin: bool,
    /// Best-guess content kind from block composition.
    /// Conservative: only non-Page when confident.
    pub content_kind: ContentKind,
    /// Where `lang` came from: "html_lang" or "meta" (a declaration, which
    /// can be wrong), or None when the page declared nothing. Without a
    /// declaration `lang` is "unknown" rather than a guess from the script.
    pub lang_source: Option<&'static str>,
    /// Dominant writing script actually observed in the text, independent of
    /// any declaration (drives tokenization: CJK, Devanagari, Latin).
    pub script: Option<language::Script>,
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
    /// Source-side omission, independent of output pagination or read filters.
    pub partial: Option<PartialContent>,
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
    Wall(crate::detect::walls::Verdict),
    /// The body was refused (nesting gate) or the parse did not finish off the worker (budget, or the task died).
    Failed(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Wall(v) => write!(f, "access wall: {v:?}"),
            ExtractError::Failed(s) => write!(f, "{s}"),
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
/// Budget for one PDF parse off the async runtime.
pub const PDF_EXTRACT_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);

/// `extract`, with a PDF body parsed on the blocking pool under
/// `PDF_EXTRACT_BUDGET`. pdfium rasterizes and lays out every page
/// synchronously; run inline on a tokio worker that pinned the
/// worker for the whole parse, the caller's deadline could not fire
/// (the future never yields inside it), and every other tool call
/// scheduled there waited. The crawl already did this; the fetch
/// tool called `extract` inline at every site. HTML goes straight
/// through: it is fast and already bounded by the walk caps.
pub async fn extract_off_worker(
    body: &[u8],
    ct: &str,
    url: &str,
    opts: &ExtractOptions,
) -> Result<Extracted, ExtractError> {
    if !is_pdf_body(body, ct) {
        return extract(body, ct, url, opts);
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return extract(body, ct, url, opts);
    };
    let body = body.to_vec();
    let ct = ct.to_string();
    let url = url.to_string();
    let opts = opts.clone();
    let task = handle.spawn_blocking(move || extract(&body, &ct, &url, &opts));
    match tokio::time::timeout(PDF_EXTRACT_BUDGET, task).await {
        Ok(Ok(r)) => r,
        Ok(Err(join_err)) => Err(ExtractError::Failed(format!(
            "extract task failed: {join_err}"
        ))),
        Err(_) => Err(ExtractError::Failed(format!(
            "PDF extraction timed out after {}s",
            PDF_EXTRACT_BUDGET.as_secs()
        ))),
    }
}

fn is_pdf_body(body: &[u8], ct: &str) -> bool {
    body.len() >= 5 && body.starts_with(b"%PDF-") || ct.to_ascii_lowercase().contains("pdf")
}

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

    if !is_pdf
        && !opts.verified
        && body_starts_with_html(body)
        && let Some(vendor) = crate::detect::walls::detect_interstitial(body)
    {
        return Err(ExtractError::Wall(
            crate::detect::walls::Verdict::Challenge(vendor),
        ));
    }

    if !is_pdf && (ct.contains("xml") || body.starts_with(b"<?xml")) {
        let text = charset::decode(body, content_type);
        let mut entries = Vec::new();
        crate::crawl::sitemap::parse_sitemap(&text, &mut entries, 10_000);
        if !entries.is_empty() {
            let inventory = entries
                .iter()
                .map(|e| format!("- {}\n", e.loc))
                .collect::<String>();
            let mut ex = extract(inventory.as_bytes(), "text/plain; charset=utf-8", url, opts)?;
            ex.via = Some("format:sitemap");
            ex.content_kind = ContentKind::Listing;
            return Ok(ex);
        }
    }

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
        let text = String::from_utf8_lossy(body);
        if let Some(v) = crate::detect::walls::text_wall(&text)
            && !(opts.verified && bot_wall(v))
        {
            return Err(ExtractError::Wall(v));
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
            blocks_total: 1,
            blocks_shown: 1,
            focus_omitted: false,
            thin: false,
            content_kind: ContentKind::Page,
            lang: "unknown".to_string(),

            lang_source: None,
            script: None,
            quality: 0.0,
            pdf_pages: None,
            images: Vec::new(),
            fingerprint: None,
            via: Some("format:text"),
            partial: None,
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
                    None,
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
    if nesting::max_nesting(&html_text) > nesting::MAX_NESTING {
        return Err(ExtractError::Failed(format!(
            "markup nested deeper than {} levels: not a document",
            nesting::MAX_NESTING
        )));
    }

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
    let mut meta = metadata::metadata(&doc);
    if url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .is_some_and(|host| host.ends_with(".wikipedia.org"))
        && meta.byline.as_deref() == Some("Contributors to Wikimedia projects")
    {
        meta.byline = None;
        meta.published = None;
    }
    let lang_info = language::detect(&doc);

    // A large page that yields almost nothing is a JS
    // shell (Medium, SPAs) : flag it.
    let thin_flag = raw_len > 50_000;

    // Skeleton/SPA loading detection. Only use aria-busy :
    // the word "skeleton" appears in CSS class names on
    // fully-hydrated pages (Amazon, React apps), making it
    // a false-positive. aria-busy is a reliable loading
    // signal set by the browser, not CSS.
    // ASCII-case-insensitive scan (L9): was a whole-document
    // to_lowercase() allocation just to count three markers.
    let has_skeletons = count_ascii_ci(&html_text, "aria-busy=\"true\"", 3) >= 3;

    let roots: Vec<scraper::ElementRef<'_>> = score::find_main(&doc).into_iter().collect();

    // Segment into typed blocks.
    let mut all_blocks = Vec::new();
    for root in &roots {
        blocks::segment(*root, &base, opts, &mut all_blocks);
    }

    let neutral_identity = if opts.include_links {
        let neutral = ExtractOptions::default();
        let mut neutral_blocks = Vec::new();
        for root in &roots {
            blocks::segment(*root, &base, &neutral, &mut neutral_blocks);
        }
        Some(render::render(
            &meta,
            url,
            &neutral_blocks.iter().collect::<Vec<_>>(),
            &neutral,
        ))
    } else {
        None
    };

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
        neutral_identity,
    )?;

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
        focus_omitted: false,
        thin: false,
        content_kind: ContentKind::Page,
        lang: "unknown".to_string(),

        lang_source: None,
        script: None,
        quality: 0.0,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
        partial: None,
    }
}

fn bot_wall(verdict: crate::detect::walls::Verdict) -> bool {
    use crate::detect::walls::Verdict;
    matches!(verdict, Verdict::Challenge(_) | Verdict::Blocked)
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
    neutral_identity: Option<String>,
) -> Result<Extracted, ExtractError> {
    let identity = neutral_identity.unwrap_or_else(|| {
        render::render(
            meta,
            url,
            &all_blocks.iter().collect::<Vec<_>>(),
            &ExtractOptions::default(),
        )
    });
    if pdf_pages.is_none()
        && let Some(v) = crate::detect::walls::text_wall(&identity)
        && !(opts.verified && bot_wall(v))
    {
        return Err(ExtractError::Wall(v));
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
    let mut blocks_shown = kept.len();

    // Dropped-content manifest (v3): when focus removed blocks,
    // one accounting line so omission is audited, never silent.
    // None when nothing was dropped or focus fell back (that case
    // gets its own full-content notice).
    let dropped_note = if opts.focus.is_some() && !focus_fell_back && kept.len() < blocks_total {
        dropped_manifest(&all_blocks, &kept)
    } else {
        None
    };

    // Refine tables/lists only after section selection: the selected rows
    // carry their headers and complete nested items, plus an omission count.
    let refined = opts
        .focus
        .as_ref()
        .filter(|_| !focus_fell_back)
        .map(|query| focus::refine_structured(&all_blocks, &kept, query, &lang_info));
    let mut focus_omitted =
        dropped_note.is_some() || refined.as_ref().is_some_and(|(_, note)| note.is_some());
    let mut kept: Vec<&blocks::Block> = match &refined {
        Some((blocks, _)) => blocks.iter().collect(),
        None => kept,
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
    } else if full.trim().is_empty() || (blocks_total == 0 && meta.title.is_none()) {
        full = format!("{url}\n\n*(no extractable content)*\n");
    }

    // The manifest goes after the fell-back/missed notices (which
    // are rarer and more important) but before any body content.
    if let Some(m) = dropped_note {
        full = format!("*[{m}]*\n\n{full}");
    }

    if let Some((_, Some(note))) = &refined {
        full = format!("*[{note}]*\n\n{full}");
    }

    // Broad matches can cost more than a plain read once context and the
    // omission manifest are included. Preserve explicit miss/engine notes.
    let neutral = ExtractOptions {
        include_links: opts.include_links,
        ..Default::default()
    };
    let unfocused = if opts.focus.is_some() && opts.include_links {
        std::borrow::Cow::Owned(render::render(
            meta,
            url,
            &all_blocks.iter().collect::<Vec<_>>(),
            &neutral,
        ))
    } else {
        std::borrow::Cow::Borrowed(identity.as_str())
    };
    if opts.focus.is_some()
        && !focus_fell_back
        && notes.is_empty()
        && full.len().saturating_mul(5) > unfocused.len().saturating_mul(4)
    {
        full = format!(
            "*[focus: less than 20% saving : showing full content]*\n\n{}",
            unfocused
        );
        blocks_shown = blocks_total;
        focus_omitted = false;
        kept = all_blocks.iter().collect();
    }
    // JS-shell warning: the agent must know the content
    // below is likely incomplete : the local engine could
    // not hydrate this page. Thin reads keep the note;
    // the fetch tool fails this class honestly instead.
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
    // Shell detection measures source yield, never the deliberately narrowed
    // focus projection. Otherwise a successful focus would read as a shell.
    let source_len = identity.len();
    let density = if raw_len > 0 {
        source_len as f64 / raw_len as f64
    } else {
        1.0
    };
    let is_shell = raw_len > 20_000 && density < 0.05 && source_len < 3_000;
    // A PDF can legitimately contain only a few words. It is already parsed
    // from a complete binary document, so HTML shell heuristics must never
    // label a short PDF as a JS-rendered page.
    let is_pdf = pdf_pages.is_some();
    let thin = !is_pdf
        && ((source_len < 800 && (thin_flag || raw_len > 5_000 || blocks_total == 0))
            || (thin_flag && has_skeletons && source_len < 4000)
            || is_shell);
    if thin {
        full = format!(
            "*[note: large page rendered almost no content : likely JS-rendered (SPA). Content below may be a shell.]*\n\n{full}"
        );
    }
    let (slice, next) = paginate(&full, opts.offset, max_chars);
    let tokens_est = slice.len() / 4;

    let content_kind = classify(&kept);
    let quality = quality_score(meta, &kept, blocks_total, raw_len, &lang_info);

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
        focus_omitted,
        tokens_est,
        thin,
        content_kind,
        lang: lang_info.code.clone(),

        lang_source: lang_info.source,
        script: Some(lang_info.script),
        quality,
        pdf_pages,
        images,
        fingerprint: Some(crate::pages::history::PageHistory::fingerprint(&identity)),
        via: None,
        partial: None,
    })
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

/// Char-budget slice at a UTF-8 boundary, preferring a block
/// boundary ("\n\n") near the cut. Returns (slice, next_offset).
fn paginate(text: &str, offset: usize, max_chars: usize) -> (String, Option<usize>) {
    if offset >= text.len() {
        return (String::new(), None);
    }
    // Resume exactly where the previous slice ended. Seeking forward to a
    // paragraph boundary silently discarded the remainder of long blocks.
    let start = ceil_char_boundary(text, offset);
    // saturating: max_chars comes from tool args; a hostile/huge
    // value must not wrap end below start (slice panic).
    let mut end = start.saturating_add(max_chars).min(text.len());
    if end < text.len() {
        let marker_size = format!("\n\n*[truncated : continue with offset={}]*", text.len()).len();
        // Tiny internal budgets cannot hold the instruction plus a UTF-8
        // character. Keep the hard cap and expose continuation in metadata.
        if max_chars >= marker_size + 4 {
            end = start
                .saturating_add(max_chars - marker_size)
                .min(text.len());
        }
    }
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
        let marker = format!("\n\n*[truncated : continue with offset={n}]*");
        if slice.len() + marker.len() <= max_chars {
            slice.push_str(&marker);
        }
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
