//! Tool spec table : the single source of truth for the three
//! agent tools (fetch, search, crawl).
//!
//! Both frontends are GENERATED from this table:
//!
//! - MCP: `mcp_schema()` builds the tools/list JSON schema.
//! - CLI: `cli_command()` builds the clap subcommand, and
//!   `matches_to_json()` converts parsed argv back into the
//!   exact JSON args the MCP dispatcher receives.
//!
//! Maintenance rule: adding, removing, or changing a parameter
//! happens HERE, once. Both interfaces update together. The tool
//! functions in `mcp/server.rs` hold all logic; the adapters
//! (MCP stdio loop, CLI renderer) hold none.
//!
//! Defaults are NOT duplicated into clap: unset flags are simply
//! absent from the generated JSON, so the core's own defaults
//! remain the single default source.

use clap::{Arg, ArgAction};
use serde_json::{Value, json};

// ── Types ────────────────────────────────────────────────────

/// Parameter value kind. Drives both the JSON schema type and
/// the clap value parser.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// JSON string; CLI `--flag <value>`.
    Str,
    /// JSON integer; CLI `--flag <N>` (usize-parsed).
    Usize,
    /// JSON string OR array of strings; CLI `--flag <value>` /
    /// positional bulk. Schema type: ["string", "array"].
    StrOrList,
    /// JSON string from a fixed set; CLI validated choices.
    Enum(&'static [&'static str]),
    /// JSON array of strings; CLI repeatable + comma-splittable.
    StrList,
    /// Bounded JSON array of strings; CLI repeatable + comma-splittable.
    StrListMax(usize),
    /// JSON boolean; CLI flag whose presence sets `true`.
    SetTrue,
    /// JSON boolean; CLI flag whose presence sets `false`
    /// (negating flags like --any-host for same_host).
    SetFalse,
    /// Ordered browser action objects; CLI takes one JSON array string.
    ActionList,
}

/// How the parameter appears on the CLI.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    /// `--flag value` style option.
    Flag,
    /// Single positional argument (`<url>` for crawl).
    PositionalSingle,
    /// Variadic positional joined with spaces (`<query>...`).
    PositionalJoined,
    /// Variadic positional where the first value fills the JSON
    /// param and the rest are handled by the CLI adapter
    /// (`<url> [more-urls...]` bulk fetch).
    PositionalBulk,
}

pub struct ParamSpec {
    /// JSON argument name (MCP schema property).
    pub name: &'static str,
    /// CLI long flag (`--focus`). Ignored for positionals.
    pub flag: &'static str,
    pub kind: ParamKind,
    pub cli: CliKind,
    pub required: bool,
    /// Complete human-facing help used by the CLI.
    pub help: &'static str,
    /// Optional compact model-facing help. `None` means the CLI help is
    /// already concise enough to serve both audiences.
    pub mcp_help: Option<&'static str>,
}

pub struct ToolSpec {
    /// MCP tool name (`web_fetch`).
    pub name: &'static str,
    /// CLI subcommand (`fetch`).
    pub cli_cmd: &'static str,
    /// One-liner : `donsetch --help` listing AND the MCP
    /// `instructions` blurb sent at initialize.
    pub summary: &'static str,
    /// Full human-facing description used by CLI long help.
    pub description: &'static str,
    /// Compact model-facing contract. Field-specific instructions stay on the
    /// corresponding parameters instead of being repeated here.
    pub mcp_description: &'static str,
    pub params: &'static [ParamSpec],
    /// Copy-pasteable CLI examples, shown in `--help` epilog.
    pub examples: &'static [&'static str],
}

// ── web_fetch ────────────────────────────────────────────────

const FETCH_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "url",
        flag: "",
        kind: ParamKind::StrOrList,
        cli: CliKind::PositionalBulk,
        required: true,
        help: "URL to fetch: http(s) URL, a handle (L/S from earlier results), or an array of up to 12 for one parallel batch call.",
        mcp_help: Some(
            "One http(s) URL, an S/L handle returned earlier, or an array of up to 12 URLs or handles for one batch read.",
        ),
    },
    ParamSpec {
        name: "budget_tokens",
        flag: "budget-tokens",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Batch mode (array url): total output token budget shared across all results (200-500k). DonSeTch allocates it across pages by size : small pages stay whole, big ones get sliced with a resume note. Without it each page uses max_chars independently.",
        mcp_help: Some(
            "Shared output-token limit for a URL batch (200-500000). Omit for one URL. Truncated members return continuation information.",
        ),
    },
    ParamSpec {
        name: "focus",
        flag: "focus",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Relevance query : returns ONLY blocks that score against it, cutting tokens 50-80% on long pages. Scoring is BM25 keyword matching; a cross-encoder pass also catches blocks that use different words, but only on pages of 80 blocks or fewer AND only when the rerank model is already cached from prior search use (plain BM25 otherwise, never a download mid-fetch). If nothing matches, returns the full page with a notice. ALWAYS set when you know what you're looking for : #1 token saver.",
        mcp_help: Some(
            "Topic or question for returning only relevant passages. Set when the requested evidence is known; omit only when the whole page is needed. A no-match response is labeled and falls back to the full page.",
        ),
    },
    ParamSpec {
        name: "max_chars",
        flag: "max-chars",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max markdown chars (default 16000). Truncated pages include next_offset for resumption.",
        mcp_help: None,
    },
    ParamSpec {
        name: "offset",
        flag: "offset",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Resume from a previous response's next_offset to continue a truncated page.",
        mcp_help: Some(
            "Continue from a previous next_offset. Reuse the same URL and any focus, section, selector or rendering fields from that fetch.",
        ),
    },
    ParamSpec {
        name: "section",
        flag: "section",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Heading name (substring, case-insensitive) : return only that section. Use after toc to target a specific part.",
        mcp_help: None,
    },
    ParamSpec {
        name: "toc",
        flag: "toc",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "true = heading outline only, no body text. Read structure first, then target with section or focus.",
        mcp_help: Some(
            "Return only the heading outline. Use as a standalone first read, then make a new fetch with section or focus.",
        ),
    },
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time budget for this call in ms (500-600000). On expiry: honest deadline error + next_action : never a silent hang. Batch mode: per-URL budget.",
        mcp_help: None,
    },
    ParamSpec {
        name: "archive",
        flag: "archive",
        kind: ParamKind::Enum(&["auto", "only", "off"]),
        cli: CliKind::Flag,
        required: false,
        help: "Dead-page recovery via the keyless Wayback Machine. auto (default): on hard failure (404/paywall/unsolvable wall) serve the nearest archived snapshot, clearly labeled with its date. only: skip the live fetch, go straight to the archive. off: never.",
        mcp_help: Some(
            "Archived-page policy: auto (default) falls back when the live page is unavailable; only skips the live page; off disables archive recovery.",
        ),
    },
    ParamSpec {
        name: "since_last",
        flag: "since-last",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Change check instead of full read: if the page is unchanged since your last fetch, output collapses to a one-line verdict; if changed, you get the section-level delta (added/removed/changed). Refetch without it for full content. Monitoring/re-verification at ~zero tokens.",
        mcp_help: Some(
            "Standalone change check for a previously fetched URL. Returns an unchanged verdict or changed sections instead of the full page. Do not combine with reading filters or rendering fields.",
        ),
    },
    ParamSpec {
        name: "stitch",
        flag: "stitch",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Multi-page articles: follow rel=next pagination and return the WHOLE article in one call (up to 6 parts / 48k chars) with *(part N)* markers, instead of re-fetching page by page. Same-host only.",
        mcp_help: Some(
            "Read all parts of a paginated article in one call (up to 6 parts / 48000 chars). Enable only when the requested source spans article pages.",
        ),
    },
    ParamSpec {
        name: "image_text",
        flag: "image-text",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "OCR the page's content images (up to 4) and append an 'image text' section. For infographics/comics/screenshots whose meaning IS the image. Costs extra fetch+compute : only when image content matters.",
        mcp_help: Some(
            "Extract words from up to 4 content images. Enable only when requested evidence is inside an infographic, screenshot, comic or other image.",
        ),
    },
    ParamSpec {
        name: "must_contain",
        flag: "must-contain",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Probe mode: verify the page mentions a string/pattern WITHOUT loading it into context. Output = MATCH/NO-MATCH verdict + up to 3 short context excerpts. Case-insensitive substring, or /regex/ (e.g. \"/CVE-2026-\\d+/\"). Full fetch still happens (tiers, walls, PDFs) : only the output collapses. For verification questions, not reading.",
        mcp_help: Some(
            "Case-insensitive string or /regex/ presence check. Returns MATCH/NO-MATCH and up to 3 excerpts instead of the full page; use for verification, not general reading.",
        ),
    },
    ParamSpec {
        name: "selector",
        flag: "selector",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "CSS selector : extract only from matching elements. Narrows scope precisely.",
        mcp_help: Some(
            "CSS selector for extracting only matching elements. Set only when the selector is known; focus may filter passages inside that region.",
        ),
    },
    ParamSpec {
        name: "tier",
        flag: "tier",
        kind: ParamKind::Enum(&["auto", "1", "2"]),
        cli: CliKind::Flag,
        required: false,
        help: "auto (default, always use for real work): HTTP first, auto-escalates to headless browser on bot-walls/JS-shells, auto-detects and parses PDFs. \"1\" (testing): HTTP only, no browser : fails on JS sites. \"2\" (testing): browser directly : slower, skips HTTP entirely.",
        mcp_help: None,
    },
    ParamSpec {
        name: "links",
        flag: "links",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Include [text](url) link URLs. Default false : saves ~30% tokens. Enable only when you need the URLs.",
        mcp_help: Some(
            "Include destination URLs found inside page content. The fetched page's own citation URL is always returned separately; enable only when outbound targets are requested evidence.",
        ),
    },
    ParamSpec {
        name: "media",
        flag: "media",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Include image alt text and sources. Default false.",
        mcp_help: Some(
            "Include image alt text and source URLs. Use image_text instead when words inside an image must be read.",
        ),
    },
    ParamSpec {
        name: "actions",
        flag: "actions",
        kind: ParamKind::ActionList,
        cli: CliKind::Flag,
        required: false,
        help: "Browser steps to run BEFORE extraction : page control inside fetch: [{\"do\":\"click\",\"selector\":\"#load-more\"},{\"do\":\"type\",\"selector\":\"input[q]\",\"text\":\"query\"},{\"do\":\"press\",\"key\":\"Enter\"},{\"do\":\"wait_text\",\"text\":\"results\"}]. Steps: wait {ms}, wait_selector {selector,timeout_ms}, wait_text {text,timeout_ms}, click {selector OR text}, hover, type {selector?,text}, press {key: Enter|Tab|Escape|Backspace|ArrowDown|...}, scroll {to: top|bottom|down | px}. Max 16 steps. Actions run in the headless browser (tier auto/2, never 1); after them the page is extracted normally : focus/section/toc still apply. First failing step aborts honestly with per-step results in structuredContent.actions; fix that step and re-run.",
        mcp_help: Some(
            "Ordered page interactions before reading (1-16 objects). Each object requires do. Required fields: wait uses ms; wait_selector uses selector; wait_text uses text; click/hover uses selector or text; type uses text and optional selector; press uses key; scroll uses to or px. The first failed step stops and is reported.",
        ),
    },
    ParamSpec {
        name: "shot",
        flag: "shot",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "File path : saves a PNG screenshot when blocked by interactive captcha. Only fires on captcha walls; not a general screenshot tool.",
        mcp_help: None,
    },
];

// ── web_search ───────────────────────────────────────────────

const SEARCH_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "query",
        flag: "",
        kind: ParamKind::Str,
        cli: CliKind::PositionalJoined,
        required: true,
        help: "Search query.",
        mcp_help: Some(
            "Primary search formulation. Include required entities, dates, versions and exclusions; do not include a guessed answer.",
        ),
    },
    ParamSpec {
        name: "query_variants",
        flag: "query-variant",
        kind: ParamKind::StrListMax(2),
        cli: CliKind::Flag,
        required: false,
        help: "Optional alternate formulations of the same information need (max 2). DonSeTch searches the base query and variants in parallel and returns one clearly separated result set per query. Use for ambiguous, multilingual, exploratory, or hard-to-recall searches; never put a guessed answer in a variant.",
        mcp_help: Some(
            "Optional array; maximum 2 alternate formulations for the same unmet requirement. Preserve entities and hard constraints; vary vocabulary, terminology, language or answer-bearing phrasing. Omit when no distinct retrieval angle exists.",
        ),
    },
    ParamSpec {
        name: "max_results",
        flag: "max-results",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max results (default 7, max 12). The most relevant results almost always live in the top 7. Increase only when results are weak.",
        mcp_help: Some(
            "Maximum ranked candidates to return (default 7, max 12). Increase only when the returned candidate set is insufficient.",
        ),
    },
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time budget in ms (500-600000). Engines have their own timeouts; this caps the whole call. On expiry: honest deadline error.",
        mcp_help: None,
    },
    ParamSpec {
        name: "intent",
        flag: "intent",
        kind: ParamKind::Enum(&["auto", "web", "code", "paper", "news", "entity"]),
        cli: CliKind::Flag,
        required: false,
        help: "auto (default) detects from query. code: adds GitHub, HN, StackExchange, MDN verticals. paper: adds Scholar, arXiv. news: adds Google News, HN. entity: adds Wikipedia. web: general only.",
        mcp_help: None,
    },
];

// ── web_crawl ────────────────────────────────────────────────

/// `web_answer` params.
const ANSWER_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "query",
        flag: "",
        kind: ParamKind::Str,
        cli: CliKind::PositionalJoined,
        required: true,
        help: "Factual question to collect cited evidence for.",
        mcp_help: None,
    },
    ParamSpec {
        name: "budget_tokens",
        flag: "budget-tokens",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Token budget for the whole evidence pack (200-8000, default 2000).",
        mcp_help: Some("Token budget for the whole evidence pack (200-8000, default 2000)."),
    },
    ParamSpec {
        name: "max_pages",
        flag: "max-pages",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max pages to read evidence from (1-5, default 3).",
        mcp_help: Some("Max pages to read evidence from (1-5, default 3)."),
    },
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time budget for this call in ms (500-600000). On expiry: honest deadline error + next_action : never a silent hang.",
        mcp_help: None,
    },
];

const CRAWL_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "url",
        flag: "",
        kind: ParamKind::Str,
        cli: CliKind::PositionalSingle,
        required: true,
        help: "Seed http(s) URL to crawl from.",
        mcp_help: None,
    },
    ParamSpec {
        name: "mode",
        flag: "mode",
        kind: ParamKind::Enum(&["full", "map", "content"]),
        cli: CliKind::Flag,
        required: false,
        help: "full (default): sitemap map + content. map: URL inventory only (very cheap). content: skip sitemap, BFS from seed.",
        mcp_help: Some(
            "full (default) discovers pages and reads content; map returns only the URL inventory; content follows links from the seed when no usable site map exists.",
        ),
    },
    ParamSpec {
        name: "focus",
        flag: "topic",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Relevance query : ranks the frontier by BM25-lite keyword scoring over link text + URL path (site-wide IDF from the map inventory when one exists), then crawls only matching pages. No semantic matching before fetch; a link sharing no token with the query is never enqueued. Fetched pages are then focus-filtered as in web_fetch. Essential for large sites; without it the crawl burns budget on noise.",
        mcp_help: Some(
            "Topic or question for selecting relevant pages and passages. Set when the request targets one subject; omit only when the whole site scope is relevant.",
        ),
    },
    ParamSpec {
        name: "max_pages",
        flag: "max-pages",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max pages to fetch+extract (default 10, cap 200).",
        mcp_help: None,
    },
    ParamSpec {
        name: "max_depth",
        flag: "max-depth",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max link depth from seed (default 2). 0 = seed only.",
        mcp_help: None,
    },
    ParamSpec {
        name: "max_total_chars",
        flag: "max-chars",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Total extracted-char budget across all pages (default 60000, range 4000-500000).",
        mcp_help: None,
    },
    ParamSpec {
        name: "per_page_max",
        flag: "per-page-max",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max markdown chars per page (default 8000, range 400-40000).",
        mcp_help: None,
    },
    ParamSpec {
        name: "include_paths",
        flag: "include",
        kind: ParamKind::StrList,
        cli: CliKind::Flag,
        required: false,
        help: "Path globs to include (e.g. [\"/docs/*\"]). Empty = all.",
        mcp_help: None,
    },
    ParamSpec {
        name: "exclude_paths",
        flag: "exclude",
        kind: ParamKind::StrList,
        cli: CliKind::Flag,
        required: false,
        help: "Path globs to exclude (e.g. [\"*/tags/*\", \"*/archive/*\"]).",
        mcp_help: None,
    },
    ParamSpec {
        name: "same_host",
        flag: "any-host",
        kind: ParamKind::SetFalse,
        cli: CliKind::Flag,
        required: false,
        help: "Stay on seed's host (default true). false = follow cross-domain links.",
        mcp_help: None,
    },
    ParamSpec {
        name: "respect_robots",
        flag: "no-robots",
        kind: ParamKind::SetFalse,
        cli: CliKind::Flag,
        required: false,
        help: "Obey robots.txt Disallow + crawl-delay (default true).",
        mcp_help: None,
    },
    ParamSpec {
        name: "deadline_s",
        flag: "deadline",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard crawl deadline in seconds (default 120, range 5-600). Partial results return after.",
        mcp_help: None,
    },
    ParamSpec {
        name: "since_last",
        flag: "since-last",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Delta crawl: pages are re-checked against fingerprint memory; only new/changed pages are returned. Monitoring and re-crawls at a fraction of the cost.",
        mcp_help: Some(
            "Return only pages new or changed since the last crawl of this site. Use for monitoring or a repeated crawl, not a first visit.",
        ),
    },
    ParamSpec {
        name: "dataset",
        flag: "dataset",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Dataset mode: output one JSON object per page (JSON Lines) instead of markdown.",
        mcp_help: Some(
            "Dataset mode: return JSON Lines (one JSON object per page) instead of markdown.",
        ),
    },
    ParamSpec {
        name: "resume",
        flag: "resume",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Resume token from a previous response to continue a stopped crawl. Valid for 30 min.",
        mcp_help: Some(
            "Opaque token returned by a stopped crawl. Send it instead of url to continue saved progress; do not invent or alter it. A new crawl requires url and omits resume.",
        ),
    },
];

/// Local web memory (v4 phase 5.2): semantic search over pages this
/// machine already fetched, stored on-device only.
const MEMORY_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "query",
        flag: "query",
        kind: ParamKind::Str,
        cli: CliKind::PositionalJoined,
        required: true,
        help: "Semantic query over pages this machine already fetched.",
        mcp_help: Some(
            "Natural-language query over the local page memory. Describe what you are looking for; hits come from pages this machine fetched before.",
        ),
    },
    ParamSpec {
        name: "limit",
        flag: "limit",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max hits, default 6.",
        mcp_help: Some("Maximum hits to return, 1..=50. Default 6."),
    },
];

// ── The table ────────────────────────────────────────────────

const SCREENSHOT_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "url",
        flag: "url",
        kind: ParamKind::Str,
        cli: CliKind::PositionalJoined,
        required: true,
        help: "Page to render and capture.",
        mcp_help: Some("The URL to capture."),
    },
    ParamSpec {
        name: "full_page",
        flag: "full-page",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Capture the full scrollable page, not just the viewport.",
        mcp_help: Some("Capture the full page height, not just the viewport."),
    },
    ParamSpec {
        name: "wait_ms",
        flag: "wait-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Extra settle time after load, up to 5000 ms. Default 600.",
        mcp_help: Some("Extra milliseconds to wait after load, max 5000."),
    },
];

pub static TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "web_fetch",
        cli_cmd: "fetch",
        summary: "Fetch a URL as clean markdown (auto bot-wall bypass, PDF, JS render)",
        description: "Fetch one URL (or a batch) as clean markdown : use when you have a specific URL to read. To find URLs use web_search; for whole sites use web_crawl.\n\nURL forms: a URL · an L-handle from earlier fetch output ([text](LxK7mP2q) → fetch LxK7mP2q) · an S-handle from search (fetch the S-handle shown next to a result) · an array of up to 12 for ONE parallel batch call (share a budget with budget_tokens).\n\nPick the CHEAPEST reading mode for the job:\n- Verification question (\"does it mention X?\") → must_contain=\"X\" (or /regex/) : returns MATCH/NO-MATCH + ≤3 excerpts, ~60 tokens.\n- Don't know where it is in a long page → toc=true (outline with section ids+sizes) → section=\"s3\" or section=\"heading text\" for just that part.\n- Know the topic → focus=\"query\" : only relevant blocks, 50-80% cheaper.\n- Just reading → default full page.\n\nRe-checking a page you fetched before: since_last=true → one-line unchanged verdict, or the section-level diff if it changed (~30 tokens). structuredContent.changed carries the verdict on every fetch.\n\nMulti-page articles (rel=next chains): stitch=true returns the whole article in one call (≤6 parts, *(part N)* markers).\n\nDead links: archive=auto (default) serves the nearest Wayback snapshot, honestly labeled with its age; archive=only skips the live web.\n\nReliability: PDFs (even scanned, ≤100MB) auto-parsed; bot walls auto-escalate to a headless browser, solve, and hand back to fast HTTP; known-walled sites that return decoy content to plain HTTP get an equivalence check (decoy_suspected flag). JS-only pages need actions=[{click|type|press|scroll|wait,...}] : deterministic wait_selector/wait_text beats blind sleeps. image_text=true OCRs content images (infographics/comics).\n\nTime control: deadline_ms caps any fetch (honest deadline error, never a hang). Send _meta.progressToken for per-URL progress on batches. Long output: structuredContent.next_offset → call again with offset.\n\nDomain intelligence: reddit threads/listings, npm/PyPI/crates.io/Go/RubyGems pages, GitHub issues/releases/commits, Stack Overflow, Wikipedia infoboxes and docs sites are auto-restructured from each site's best source : no special params, it just returns clean structure. Operators can add more sites with local JSON adapters (`donsetch adapters`).\n\nResponse: content[0].text is one canonical source document. structuredContent contains only actionable model state such as url, content_ok, content_kind, thin, changed, next_offset, PDF summary, stitch count, cloak warning and error code/next_action. Transport tier, timing, quality, adapter and escalation diagnostics live in _meta.",
        mcp_description: "Read one URL or a deliberate URL batch as source markdown. Use search to discover URLs and crawl for multiple pages from one site. Prefer the smallest view that can supply the required evidence, and continue only from returned continuation state. Do not repeat a successful read unless it was thin, truncated, or failed. Automatic acquisition may use HTTP, a browser, an adapter, PDF extraction, or an archive. Treat content_ok=false, thin=true, or a stable error code as unresolved; follow next_action or choose another source. Cite the returned source URL.",
        params: FETCH_PARAMS,
        examples: &[
            "donsetch fetch https://example.com/article",
            "donsetch fetch https://long-docs-page --focus \"error handling\"",
            "donsetch fetch https://long-docs-page --offset 16000",
            "donsetch fetch https://a.com/x https://b.com/y   # bulk fetch",
            "donsetch fetch https://site.com/search --actions '[{\"do\":\"type\",\"selector\":\"input[q]\",\"text\":\"rust async\"},{\"do\":\"press\",\"key\":\"Enter\"},{\"do\":\"wait_text\",\"text\":\"results\"}]'",
        ],
    },
    ToolSpec {
        name: "web_search",
        cli_cmd: "search",
        summary: "Web search : 5 keyless engines merged + reranked, or your API keys",
        description: "Web search : returns ranked URLs + titles + snippets. Use to decide WHAT to fetch (web_fetch reads content; this never does).\n\nOne query is the normal path. For an ambiguous, multilingual, exploratory, or hard-to-recall information need, add up to two query_variants: all searches run in parallel and come back as clearly separated result sets, with no automatic rewriting or guessed answers.\n\nEach result is one compact evidence row: fetch handle (or raw URL), title, host, focused snippet, and a browser-cost warning only when relevant. Rank already represents DonSeTch's scoring decision, so per-engine scores and timing are not repeated in model context. Weak or degraded retrieval remains explicitly labeled. Multi-query mode keeps one clearly labeled ranked section per formulation.\n\nEngines: 10+ keyless backends fused by cross-engine consensus + local semantic reranking (automatic). Verticals via intent: GitHub, Wikipedia, HN, Scholar, news, StackExchange, MDN. BYOK: providers configured via `donsetch keys` (Tavily/Exa/Serper/TinyFish/Parallel/BrightData) take over automatically.\n\ndeadline_ms caps the whole call (honest deadline error, never a hang).\n\nResponse: content[0].text is the ranked evidence list. structuredContent contains weak plus rank, URL and optional fetch handle for each result; multi-query mode keeps those lists separate. Engine health, score, cache, provider, reranker and timing diagnostics live in _meta.\n\nAfter search: fetch the best result via its S-handle : enrichment pre-fetches top results, so the next fetch is near-instant.",
        mcp_description: "Discover ranked candidate sources. Use this before fetch when you do not already have a URL; it returns titles and snippets, not page contents. A snippet supports only claims it states explicitly. Treat weak or degraded results as incomplete, and fetch a candidate when its full text is required. Search handles resolve directly in fetch; cite source URLs from the result metadata accompanying the ranked list.",
        params: SEARCH_PARAMS,
        examples: &[
            "donsetch search rust async trait objects",
            "donsetch search \"exact phrase\" --intent code",
            "donsetch search site:github.com tokio --max-results 10",
            "donsetch search \"rust async trait patterns\" --query-variant \"async fn in trait rust\"",
        ],
    },
    ToolSpec {
        name: "web_answer",
        cli_cmd: "answer",
        summary: "Evidence pack for a factual question : search + read top pages, passages with sources",
        description: "One-call research for a factual question : DonSeTch searches the web, reads the top pages, and returns a compact pack of relevant passages, each tied to its source URL with a freshness stamp.\n\nNo prose answer is invented: you get verbatim cited evidence to reason over. Dead or walled pages are skipped and reported, never silently dropped. Duplicate mirrors are collapsed.\n\nbudget_tokens caps the whole pack; max_pages caps how many sources are read; deadline_ms caps the wall clock (honest deadline error, never a hang).\n\nResponse: content[0].text is the markdown evidence pack. structuredContent contains query, empty, truncated, tokens_est, per-source citations and skipped pages with reasons. Search and per-page timing diagnostics live in _meta.",
        mcp_description: "Factual question -> cited evidence in one call: searches the web, reads top pages, returns verbatim passages tied to source URLs with freshness stamps. No prose answer invented; you synthesize. Dead/walled pages are skipped and reported.",
        params: ANSWER_PARAMS,
        examples: &[
            "donsetch answer \"when did Rust 1.0 release\"",
            "donsetch answer \"capital of Bhutan\" --budget-tokens 1000",
            "donsetch answer \"tokio select macro\" --max-pages 4",
        ],
    },
    ToolSpec {
        name: "web_crawl",
        cli_cmd: "crawl",
        summary: "Crawl a site into markdown (sitemap-aware, focus-ranked, resumable)",
        description: "Crawl a site from a seed : for multi-page extraction (docs, API refs, wikis). Single page → web_fetch; finding sites → web_search.\n\nTwo-phase: sitemap discovery (cheap URL inventory) first, then focus-ranked page fetching with adaptive per-host pacing. Docs sites (mkdocs/docusaurus/sphinx/antora) get their nav as the site map automatically.\n\nModes: full (default) = map + content · map = URL inventory only, very cheap : see what a site has before committing · content = BFS from seed, no sitemap (use when sitemap is missing). PDF pages auto-parsed, not skipped. dataset=true emits JSON Lines (one object per page).\n\nBudgets: focus (topic) ranks the frontier by BM25-lite link-text/URL-path keyword scoring and crawls only matches : set it whenever you have a topic. max_pages / max_total_chars / deadline_s cap the run; resume tokens continue across calls. since_last=true skips pages unchanged since your last crawl of the site (fingerprint memory : returns only what moved). Send _meta.progressToken for live per-page progress (\"12 pages, 34 queued\"); cancellation stops gracefully and keeps the resume token.\n\nResponse: content[0].text is one linear site-evidence document. structuredContent contains seed, completion, page URLs, stop reason and any resume/next action. Map, queue, skip, score, quality, crawl-delay and timing diagnostics live in _meta. FrontierEmpty is complete; MaxPages, CharBudget, DepthLimit, Deadline, ThrottledOut and Cancelled are incomplete.",
        mcp_description: "Read multiple pages from one known site. Use search to discover a site and fetch for one page. Scope the crawl to the requested evidence and set explicit budgets. FrontierEmpty means complete; budget, deadline, throttle, cancellation, or depth stops are incomplete and may return a resume token. Cite the returned page URLs. dataset=true returns JSONL (one object per page).",
        params: CRAWL_PARAMS,
        examples: &[
            "donsetch crawl https://docs.site.com --topic \"authentication\"",
            "donsetch crawl https://docs.site.com --mode map",
            "donsetch crawl https://docs.site.com --max-pages 25 --deadline 300",
            "donsetch crawl https://docs.site.com --dataset > site.jsonl",
        ],
    },
    ToolSpec {
        name: "web_memory",
        cli_cmd: "memory",
        summary: "Search pages this machine already fetched (local, on-device)",
        description: "One-call semantic search over your local page history. DonSeTch keeps a bounded on-device index of pages it fetched (URL, title, markdown digest) with all-MiniLM-L6-v2 embeddings computed locally by the vendored onnxruntime. Nothing leaves the box: the model runs here, the index lives under the cache dir, no cloud call.\n\nHits are ranked by cosine similarity (score 0..1). Kill switch: DONSETCH_NO_WEB_MEMORY (when set, web_memory returns nothing and ingest is disabled). The index is capped at 4000 rows (DONSETCH_WEB_MEMORY_CAP) with oldest-first eviction.",
        mcp_description: "Search pages this machine fetched before with a natural-language query. Results come from a bounded local index that DonSeTch maintains itself; the model runs on-device. Ranked hits (url, title, snippet, score). To read beyond the snippet fetch the URL with web_fetch.",
        params: MEMORY_PARAMS,
        examples: &[
            "donsetch memory \"quic congestion control\"",
            "donsetch memory \"the alt-svc cache\" --limit 10",
        ],
    },
    ToolSpec {
        name: "web_screenshot",
        cli_cmd: "screenshot",
        summary: "Open a URL in a real browser, return the page as PNG",
        description: "A rendered PNG of a page: DonSeTch opens the URL in the same tier-2 browser it already keeps for challenge walls, waits for the load, and returns the capture as PNG bytes. Deeper truth for any page that lies without JavaScript, and a visual receipt for scripts and logins. The capture is in-process only (nothing posted anywhere); the caller decides whether the pixels are worth their tokens. full_page asks the browser to capture beyond the viewport.",
        mcp_description: "Capture a URL as a rendered PNG in a real browser. full_page captures the full page height; wait_ms adds settle time after load (max 5000, default 600). Private/loopback URLs are blocked like web_fetch.",
        params: SCREENSHOT_PARAMS,
        examples: &[
            "donsetch screenshot https://example.com",
            "donsetch screenshot https://example.com --full-page",
            "donsetch screenshot https://example.com --wait-ms 2500",
            "donsetch screenshot https://example.com --out page.png",
        ],
    },
];

/// Look up a tool spec by CLI subcommand name.
pub fn by_cli_cmd(cmd: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.cli_cmd == cmd)
}

// ── MCP schema generation ────────────────────────────────────

/// Build the tools/list entry for one tool. Output is identical
/// in shape to the historical hand-written schema (pinned by the
/// golden fixture test).
pub fn mcp_schema(tool: &ToolSpec) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for p in tool.params {
        let mut schema = serde_json::Map::new();
        let ty = match p.kind {
            ParamKind::Str | ParamKind::Enum(_) => "string",
            ParamKind::StrOrList => {
                schema.insert(
                    "anyOf".into(),
                    json!([
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]),
                );
                schema.insert("description".into(), json!(p.mcp_help.unwrap_or(p.help)));
                props.insert(p.name.into(), Value::Object(schema));
                if p.required {
                    required.push(json!(p.name));
                }
                continue;
            }
            ParamKind::Usize => "integer",
            ParamKind::StrList | ParamKind::StrListMax(_) | ParamKind::ActionList => "array",
            ParamKind::SetTrue | ParamKind::SetFalse => "boolean",
        };
        schema.insert("type".into(), json!(ty));
        if let ParamKind::Enum(variants) = p.kind {
            schema.insert("enum".into(), json!(variants));
        }
        if matches!(p.kind, ParamKind::StrList | ParamKind::StrListMax(_)) {
            schema.insert("items".into(), json!({ "type": "string" }));
        }
        if let ParamKind::StrListMax(max) = p.kind {
            schema.insert("maxItems".into(), json!(max));
        }
        if p.kind == ParamKind::ActionList {
            schema.insert(
                "items".into(),
                json!({
                    "type": "object",
                    "properties": {
                        "do": {
                            "type": "string",
                            "enum": [
                                "wait", "wait_selector", "wait_text", "click", "hover",
                                "type", "press", "scroll"
                            ]
                        },
                        "selector": { "type": "string" },
                        "text": { "type": "string" },
                        "key": { "type": "string" },
                        "to": {
                            "type": "string",
                            "enum": ["top", "bottom", "down", "px"]
                        },
                        "px": { "type": "integer" },
                        "ms": { "type": "integer" },
                        "timeout_ms": { "type": "integer" }
                    },
                    "required": ["do"],
                    "additionalProperties": false
                }),
            );
            schema.insert("minItems".into(), json!(1));
            schema.insert("maxItems".into(), json!(16));
        }
        schema.insert("description".into(), json!(p.mcp_help.unwrap_or(p.help)));
        props.insert(p.name.into(), Value::Object(schema));
        if p.required {
            required.push(json!(p.name));
        }
    }
    json!({
        "name": tool.name,
        "description": tool.mcp_description,
        "inputSchema": {
            "type": "object",
            "properties": Value::Object(props),
            "required": required,
        }
    })
}

// ── CLI generation ───────────────────────────────────────────

/// Build the clap subcommand for one tool. `--json` and
/// `--quiet` are CLI-adapter flags (not MCP params), appended
/// to every tool command.
pub fn cli_command(tool: &ToolSpec) -> clap::Command {
    // CLI --help speaks to a human in a terminal; the full agent
    // contract stays in the MCP description field. Same facts,
    // different voice.
    let cli_long = match tool.cli_cmd {
        "fetch" => {
            "Fetch one URL (or several) as clean markdown. \
Redirects, bot walls and JS-shells escalate automatically through two \
tiers; the output tells you what happened at every step. \
Use --focus to narrow the content, --must-contain for a cheap \
probe, and a result handle from a search result (donsetch fetch \
@10hd73d) to fetch a result directly. \
--browser-actions sends clicks/typing to a headless browser for \
sites that need interaction. \
--archive serves a Wayback snapshot when the live page is dead."
        }
        "search" => {
            "Search the web across 5 keyless engines, merged with \
consensus ranking (no API keys needed). \
Every result carries a short handle: donsetch fetch @<handle> \
fetches it directly. \
Use --max-results to cap the list and --intent to pick a vertical \
(web|news|code|docs|pdf|paper|hashun); the default auto-detects. \
--json gives the full result envelope (titles, snippets, scores, \
engine health)."
        }
        "crawl" => {
            "Crawl a site: sitemap-aware, budgeted, focused. \
The default --mode content walks the same domain from the seed with \
a best-first frontier, scoring pages for the --topic you give. \
--mode map is a cheap sitemap-shape inventory. \
Returns pages with per-page notes; long crawls emit a resume token \
you can pass with --resume to continue later."
        }
        _ => tool.summary,
    };
    let mut cmd = clap::Command::new(tool.cli_cmd)
        .about(tool.summary)
        .long_about(cli_long)
        .after_help(format!(
            "EXAMPLES:\n{}",
            tool.examples
                .iter()
                .map(|e| format!("  {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    for p in tool.params {
        cmd = cmd.arg(cli_arg(p));
    }
    cmd.arg(
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .help("Print the full JSON envelope on stdout (content + all metadata)."),
    )
    .arg(
        Arg::new("quiet")
            .long("quiet")
            .short('q')
            .action(ArgAction::SetTrue)
            .help("Suppress the stderr stats line."),
    )
}

fn cli_arg(p: &ParamSpec) -> Arg {
    let arg = Arg::new(p.name).help(p.help);
    match p.cli {
        CliKind::PositionalSingle => arg.required(p.required),
        CliKind::PositionalJoined | CliKind::PositionalBulk => {
            arg.required(p.required).num_args(1..)
        }
        CliKind::Flag => {
            let arg = arg.long(p.flag);
            match p.kind {
                ParamKind::Str | ParamKind::StrOrList => arg.value_name("VALUE"),
                ParamKind::Usize => arg.value_name("N").value_parser(clap::value_parser!(usize)),
                ParamKind::Enum(variants) => {
                    arg.value_name("VALUE")
                        .value_parser(clap::builder::PossibleValuesParser::new(
                            variants.iter().copied(),
                        ))
                }
                ParamKind::ActionList => arg.value_name("JSON"),
                ParamKind::StrList => arg
                    .value_name("GLOB")
                    .action(ArgAction::Append)
                    .value_delimiter(','),
                ParamKind::StrListMax(_) => arg
                    .value_name("VALUE")
                    .action(ArgAction::Append)
                    .value_delimiter(','),
                ParamKind::SetTrue => arg.action(ArgAction::SetTrue),
                ParamKind::SetFalse => arg.action(ArgAction::SetFalse),
            }
        }
    }
}

/// Convert parsed CLI matches into the exact JSON args Value
/// the MCP dispatcher receives. Unset flags are omitted : the
/// core applies its own defaults (single default source).
pub fn matches_to_json(tool: &ToolSpec, m: &clap::ArgMatches) -> Value {
    let mut map = serde_json::Map::new();
    for p in tool.params {
        match p.cli {
            CliKind::PositionalSingle | CliKind::PositionalBulk => {
                if let Some(v) = m.get_one::<String>(p.name) {
                    map.insert(p.name.into(), json!(v));
                }
            }
            CliKind::PositionalJoined => {
                let words: Vec<&str> = m
                    .get_many::<String>(p.name)
                    .map(|v| v.map(String::as_str).collect())
                    .unwrap_or_default();
                if !words.is_empty() {
                    map.insert(p.name.into(), json!(words.join(" ")));
                }
            }
            CliKind::Flag => match p.kind {
                ParamKind::Str | ParamKind::StrOrList | ParamKind::Enum(_) => {
                    if let Some(v) = m.get_one::<String>(p.name) {
                        map.insert(p.name.into(), json!(v));
                    }
                }
                ParamKind::ActionList => {
                    if let Some(v) = m.get_one::<String>(p.name)
                        && let Ok(parsed) = serde_json::from_str::<Value>(v)
                    {
                        map.insert(p.name.into(), parsed);
                    }
                }
                ParamKind::Usize => {
                    if let Some(v) = m.get_one::<usize>(p.name) {
                        map.insert(p.name.into(), json!(v));
                    }
                }
                ParamKind::StrList | ParamKind::StrListMax(_) => {
                    let items: Vec<&str> = m
                        .get_many::<String>(p.name)
                        .map(|v| v.map(String::as_str).collect())
                        .unwrap_or_default();
                    if !items.is_empty() {
                        map.insert(p.name.into(), json!(items));
                    }
                }
                ParamKind::SetTrue => {
                    if m.get_flag(p.name) {
                        map.insert(p.name.into(), json!(true));
                    }
                }
                ParamKind::SetFalse => {
                    if !m.get_flag(p.name) {
                        map.insert(p.name.into(), json!(false));
                    }
                }
            },
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch_tool() -> &'static ToolSpec {
        TOOLS
            .iter()
            .find(|tool| tool.name == "web_fetch")
            .expect("web_fetch spec")
    }

    fn search_tool() -> &'static ToolSpec {
        TOOLS
            .iter()
            .find(|tool| tool.name == "web_search")
            .expect("web_search spec")
    }

    fn crawl_tool() -> &'static ToolSpec {
        TOOLS
            .iter()
            .find(|tool| tool.name == "web_crawl")
            .expect("web_crawl spec")
    }

    #[test]
    fn search_variants_use_a_strict_array_schema() {
        let schema = mcp_schema(search_tool());
        let variants = &schema["inputSchema"]["properties"]["query_variants"];
        assert_eq!(variants["type"], "array");
        assert_eq!(variants["items"]["type"], "string");
        assert_eq!(variants["maxItems"], 2);
        assert!(
            !schema["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("query_variants"))
        );
    }

    #[test]
    fn cli_collects_repeated_search_variants() {
        let tool = search_tool();
        let matches = cli_command(tool)
            .try_get_matches_from([
                "search",
                "rust async trait patterns",
                "--query-variant",
                "rust async trait objects",
                "--query-variant",
                "async fn in trait rust",
            ])
            .expect("valid search CLI");
        let args = matches_to_json(tool, &matches);
        assert_eq!(args["query"], "rust async trait patterns");
        assert_eq!(
            args["query_variants"],
            json!(["rust async trait objects", "async fn in trait rust"])
        );
    }

    #[test]
    fn mcp_contract_is_compact_and_cli_help_stays_usable() {
        for tool in TOOLS {
            let schema = mcp_schema(tool);
            assert_eq!(schema["description"], tool.mcp_description);
            assert!(
                tool.mcp_description.len() < tool.description.len(),
                "{} model description should be shorter than the agent contract",
                tool.name
            );
        }

        // The CLI long-help is written for a human in a terminal: it
        // must stay discoverable (the main knobs named) without
        // borrowing the agent-facing contract text.
        let long_help = cli_command(fetch_tool()).render_long_help().to_string();
        assert!(long_help.contains("--focus"), "{long_help}");
        assert!(long_help.contains("handle"), "{long_help}");
        let long_search = cli_command(search_tool()).render_long_help().to_string();
        assert!(long_search.contains("--intent"), "{long_search}");
        assert!(long_search.contains("handle"), "{long_search}");
        let long_crawl = cli_command(crawl_tool()).render_long_help().to_string();
        assert!(long_crawl.contains("--resume"), "{long_crawl}");
    }

    #[test]
    fn action_field_describes_the_runtime_shape() {
        let schema = mcp_schema(fetch_tool());
        let actions = &schema["inputSchema"]["properties"]["actions"];
        assert_eq!(actions["type"], "array");
        assert_eq!(actions["minItems"], 1);
        assert_eq!(actions["maxItems"], 16);
        assert_eq!(actions["items"]["required"], json!(["do"]));
        assert_eq!(actions["items"]["additionalProperties"], false);
        assert_eq!(
            actions["items"]["properties"]["do"]["enum"],
            json!([
                "wait",
                "wait_selector",
                "wait_text",
                "click",
                "hover",
                "type",
                "press",
                "scroll"
            ])
        );
    }
}
