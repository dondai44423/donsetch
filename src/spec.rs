//! Tool spec table : the single source of truth for the four
//! agent tools (fetch, search, crawl, screenshot).
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

use crate::extract::view;
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
    /// JSON string OR array of at most N strings; CLI repeatable
    /// `--flag <value>` (one value sends a string) or positional bulk.
    StrOrListMax(usize),
    /// JSON string from a fixed set; CLI validated choices.
    Enum(&'static [&'static str]),
    /// JSON `true`/`false` or one string of a fixed set. The first string
    /// is the mode `true` means: on the CLI the bare flag and
    /// `--flag=<first>` both send `true`, and `--flag=<value>` sends any
    /// other string as itself.
    BoolOr(&'static [&'static str]),
    /// JSON array of strings; CLI repeatable + comma-splittable.
    StrList,
    /// Bounded JSON array of strings; CLI repeatable + comma-splittable.
    StrListMax(usize),
    /// JSON boolean; CLI flag whose presence sets `true`.
    SetTrue,
    /// JSON boolean; CLI flag whose presence sets `false`
    /// (negating flags like --any-host for same_host).
    SetFalse,
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
//
// Designed from the agent's seat: each option answers one question at the
// fewest tokens. must_contain (is it there?), focus (where is it?), toc and
// section (what is here, then one part), the page window (offset,
// max_chars), links, since_last, image_text and deadline_ms.

const FETCH_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "url",
        flag: "",
        kind: ParamKind::StrOrListMax(12),
        cli: CliKind::PositionalBulk,
        required: true,
        help: "The page to read: an http(s) URL, or a handle from earlier output (S-handles from search, L-handles from links). Several URLs read in parallel as one batch (up to 12); each page gets the same view, and max_chars is the batch total.",
        mcp_help: Some(
            "http(s) URL or an S/L handle from earlier output; an array of up to 12 reads them in parallel, each with the same view, max_chars shared across them.",
        ),
    },
    ParamSpec {
        name: "must_contain",
        flag: "must-contain",
        kind: ParamKind::StrOrListMax(view::MUST_CONTAIN_MAX_TERMS),
        cli: CliKind::Flag,
        required: false,
        help: "Check that the page says something, without reading it. A phrase matches case-insensitively, whatever the spacing, line breaks, quote style, dash style or markdown emphasis; /regex/ (flags imsx) matches a pattern. Repeat the flag for up to 8 terms. The answer is one line per term: found, how often, in which sections (sN, as toc numbers them) with short excerpts; or not found, with a hint when the phrase's words are on the page apart. The verdict is MATCH, PARTIAL, NO MATCH, or UNVERIFIED when part of the page could not be read (a missing term may be there). Searches the whole page, or only --section. Not with --focus, --toc, --offset or --since-last.",
        mcp_help: Some(
            "Presence check instead of reading: a phrase (case, spacing, quote/dash style and markdown emphasis ignored), a /regex/ (flags imsx), or a list of up to 8. Returns per term: found ×count, sections (sN) and excerpts; or not found, with a near-miss hint. Verdict match|partial|no_match|unverified (part of the page unread: absence proves nothing). Searches the whole page, or only section. Not with focus, toc, offset or since_last.",
        ),
    },
    ParamSpec {
        name: "focus",
        flag: "focus",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Return only the passages that answer a question or match keywords, best first, grouped under their section path (§sN · Heading › Sub). Paragraphs, list items, table rows (with the table header) and code are ranked separately; a section whose heading names the topic ranks all its passages. The header line counts the passages kept and names query words the page does not contain. No match returns the outline instead of the page. With --section, ranks inside that part.",
        mcp_help: Some(
            "Question or keywords: returns only matching passages, best first, under their §sN section path, and names query words absent from the page. A miss returns the outline, never the whole page. With section, ranks inside that part.",
        ),
    },
    ParamSpec {
        name: "toc",
        flag: "toc",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Return the heading outline instead of the text: each heading with its id ([s3]) and size, nested by level. Read it first on a long page, then fetch one part with --section s3. With --section, the outline of that part only.",
        mcp_help: Some(
            "Return the heading outline: [sN] ids with sizes, no body. With section, that part's outline only.",
        ),
    },
    ParamSpec {
        name: "section",
        flag: "section",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Read one part of the page: an id from --toc (s3), or heading text (an exact heading first, else the first heading containing it; other matches are named). The part runs to the next heading of the same or a higher level, subsections included. A name that matches no heading returns the outline.",
        mcp_help: Some(
            "One part: an sN id from toc, or heading text (exact first, else first containing it). Includes subsections; no match returns the outline.",
        ),
    },
    ParamSpec {
        name: "offset",
        flag: "offset",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Continue a response that was cut at max_chars: pass the next_offset it returned, with the same URL and the same view options.",
        mcp_help: Some(
            "Continue a cut-off response: its next_offset, with the same url and view arguments.",
        ),
    },
    ParamSpec {
        name: "max_chars",
        flag: "max-chars",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Most characters to return (200-1048576; default 16000, about 4000 tokens). A longer result ends with next_offset. For several URLs it is the total for the batch (default 48000): small pages stay whole and large ones are cut, each with its own resume note.",
        mcp_help: Some(
            "Most characters returned (default 16000, ~4 per token); longer results end with next_offset. For a url array it is the batch total (default 48000).",
        ),
    },
    ParamSpec {
        name: "links",
        flag: "links",
        kind: ParamKind::BoolOr(&["urls", "handles"]),
        cli: CliKind::Flag,
        required: false,
        help: "Keep the page's links. --links (or --links=urls) gives [text](https://...) with the real address, to cite a source, check where a link goes or follow it. --links=handles gives [text](handle) instead: a short L-handle that fetch accepts as its url, about half the tokens of the address. Off by default: link text only, which saves about a third of the tokens.",
        mcp_help: Some(
            "Keep links. true or \"urls\": [text](real URL), to cite, check or follow a link. \"handles\": [text](L-handle), about half the tokens; a handle passed as url follows it. Off: link text only (~30% fewer tokens).",
        ),
    },
    ParamSpec {
        name: "since_last",
        flag: "since-last",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Check a page you read before for changes: one line when it is unchanged, else only the sections that changed. Every read records the page, so any earlier fetch of the URL is the baseline; with none, you get the page itself. On its own: not with a view option.",
        mcp_help: Some(
            "Change check against your previous read of this url: an unchanged line, or only the changed sections; with no earlier read, the page itself. Not with view arguments.",
        ),
    },
    ParamSpec {
        name: "image_text",
        flag: "image-text",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Read the words in the page's first 4 content images (OCR) into an 'image text' section, which every view searches: charts, screenshots, scanned notices, comics. Slower; for pages whose evidence is an image.",
        mcp_help: Some(
            "OCR the first 4 content images into an 'image text' section that every view searches. Slower; only when the evidence is in an image.",
        ),
    },
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time limit for the call in milliseconds (500-600000). On expiry the answer is a deadline error naming what was tried, never a hang. For several URLs, the limit applies to each.",
        mcp_help: Some(
            "Hard time limit in ms (500-600000): a deadline error on expiry, never a hang. Per URL in a batch.",
        ),
    },
];

/// Arguments `web_fetch` no longer takes, with what replaces each. An agent
/// that learned one from an older schema is told, not just refused.
const REMOVED_FETCH_PARAMS: &[(&str, &str)] = &[
    (
        "archive",
        "archive was removed: web_fetch returns live page content only",
    ),
    (
        "tier",
        "tier was removed: every web_fetch runs the local engine, with no browser tier",
    ),
    (
        "actions",
        "actions was removed: every web_fetch runs the local engine, with no browser drive",
    ),
    ("shot", "shot was removed: use web_screenshot for captures"),
    (
        "budget_tokens",
        "budget_tokens was removed: max_chars is the total for a url array",
    ),
    (
        "mode",
        "mode was removed: set max_chars (about 4 characters per token)",
    ),
    (
        "stitch",
        "stitch was removed: fetch the next part named by next_page in [meta]",
    ),
    (
        "selector",
        "selector was removed: use section (a heading or an sN id from toc) or focus",
    ),
    (
        "media",
        "media was removed: image_text reads the words in images",
    ),
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
        help: "Max results (default 7, max 12). The answer is usually in the top 7; raise this only when the top results miss what you need.",
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
        help: "Hard time budget in ms (500-600000). Per-backend timeouts stay below it; this caps the whole call. On expiry: honest deadline error.",
        mcp_help: Some(
            "Hard time budget in ms (500-600000) for the whole call; per-backend timeouts stay below it. On expiry: an honest deadline error.",
        ),
    },
    ParamSpec {
        name: "read",
        flag: "read",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Read the top N results (max 3, default 0) inside the same deadline: each snippet becomes the passage of that page which answers the query, marked `page:`, and the page is warm for a follow-up fetch. A page that cannot be read keeps its snippet.",
        mcp_help: Some(
            "Read the top N results (max 3, default 0) within the call's deadline: their snippets become the page's own passage for the query (`page:`), and a follow-up web_fetch of them is served warm. Use when snippets are not enough to choose.",
        ),
    },
    ParamSpec {
        name: "intent",
        flag: "intent",
        kind: ParamKind::Enum(&["auto", "web", "code", "paper", "news", "entity"]),
        cli: CliKind::Flag,
        required: false,
        help: "auto (default) detects from query signals (code/tech tokens, paper terms, news words, who/what-is). Keyless mode adds that intent's first-party sources: code: GitHub, HN, StackExchange, MDN, and the crates.io, npm or PyPI record for a package name (`npm zod`, `cargo serde_json`); paper: OpenAlex, arXiv, Europe PMC for biomedical terms; news, or any query with a recent window (`this week`, `yesterday`): the Bing News wire, publisher feeds, GDELT, HN; entity: Wikipedia; web: general only. With a configured provider (donsetch keys) the provider replaces the fan-out: intent maps to its own filters and the keyless verticals do not join.",
        mcp_help: Some(
            "auto (default) detects from the query. Keyless mode adds first-party sources: code: + GitHub, HN, StackExchange, MDN, crates.io/npm/PyPI for a package name; paper: + OpenAlex, arXiv, Europe PMC (biomedical); news or a recent window ('this week'): + news wire, publisher feeds, HN; entity: + Wikipedia; web: general only. With a configured provider, intent selects that provider's filters instead (one provider; the keyless verticals do not join).",
        ),
    },
];

// ── web_crawl ────────────────────────────────────────────────

const CRAWL_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "url",
        flag: "",
        kind: ParamKind::Str,
        cli: CliKind::PositionalSingle,
        required: false,
        help: "Seed http(s) URL to crawl from; omit when continuing with resume.",
        mcp_help: None,
    },
    ParamSpec {
        name: "mode",
        flag: "mode",
        kind: ParamKind::Enum(&["full", "map", "content"]),
        cli: CliKind::Flag,
        required: false,
        help: "full (default): sitemap map + content. map: URL inventory only (very cheap: the site map, else the seed page's links). content: skip sitemap, BFS from seed.",
        mcp_help: Some(
            "full (default) discovers pages and reads content; map returns only the URL inventory (a site map when one exists, otherwise the seed page's links, with no page fetches); content follows links from the seed when no usable site map exists.",
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
        name: "map_limit",
        flag: "map-limit",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max URLs returned in the inventory (default 120, range 1-50000). A capped inventory is incomplete; narrow focus/include_paths for large sites.",
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
        help: "Total extracted-char budget across all pages (default 60000, range 1-500000).",
        mcp_help: None,
    },
    ParamSpec {
        name: "per_page_max",
        flag: "per-page-max",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max markdown chars per page (default 8000, range 1-40000).",
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
        help: "Hard crawl deadline in seconds (default 120, range 1-600). Partial results return after.",
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
            "Re-fetch pages and return only new or changed source fingerprints. Prior crawls or fetches supply per-URL baselines. Saves output, not verification requests; use for repeated reads.",
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
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time budget for this call in ms (500-600000, default 60000). On expiry: honest deadline error + next_action : never a silent hang.",
        mcp_help: Some(
            "Hard time budget in ms (500-600000, default 60000). On expiry: an honest deadline error with next_action, never a silent hang.",
        ),
    },
];

pub static TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "web_fetch",
        cli_cmd: "fetch",
        summary: "Read a URL as markdown, or only the part you need (in-process JS, PDF, honest failures)",
        description: "Read one URL, or up to 12 in parallel, as clean markdown: a web page (its scripts run in-process when it needs them), a PDF (scanned pages OCRed), a feed, or a JSON endpoint. Sites with a structured source (GitHub, Reddit, Stack Overflow, Wikipedia, npm/PyPI/crates.io and other registries, docs sites) come back from that source. Use web_search to find URLs and web_crawl for many pages of one site.\n\nViews, cheapest first. Every view reads the whole document and projects it, the same way for every kind of source:\n- must_contain: is it there? A phrase or /regex/, or up to 8 of them. Per term: found ×count, the sections it is in and excerpts; or not found, with a hint when a phrase's words are on the page apart. Verdict match, partial, no_match, or unverified when part of the page could not be read (a listing that loaded only some items, a document past the read limit), because then absence proves nothing. About 60 tokens instead of the page.\n- focus: where is it? Only the passages that answer the question, best first, grouped under their section path, each table row with its header. The header line names query words the page lacks. A miss returns the outline, never the page.\n- toc, then section: what is here, then one part. The outline numbers headings s1, s2, ... with sizes; section takes an id or heading text, subsections included; a miss returns the outline.\n- No view: the page.\nmax_chars caps the response (default 16000 characters); a cut-off response ends with next_offset, and the same call with offset continues it. For a url array max_chars is the batch total (default 48000). A paginated article names its next part in next_page.\n\nlinks keeps link targets: real URLs, or with \"handles\" short handles fetch accepts. since_last compares with your previous read of the URL and returns only what changed. image_text OCRs the first 4 content images into the document, so every view searches them. deadline_ms bounds the call.\n\nResponse: a leading [meta] JSON line, then the evidence. [meta] holds url (cite it), ok, content_ok, read_status (content, partial, thin, probe, focus, section, outline), content_complete, next_offset, next_page, the view's state (must_contain verdict and counts, focus passages kept of total, section id or not_found), changed, and on failure code, errorKind and next_action. Login walls, paywalls, bot walls and missing pages fail fast and name what stopped them. Transport and timing diagnostics live in _meta.",
        mcp_description: "Read a page, PDF or feed as markdown, or only the part that answers the question. Text starts with [meta] JSON, then evidence; cite [meta].url. ok:false, content_ok:false or thin:true is unresolved: follow next_action.\nCheapest view first:\n1. Is X there? must_contain: per-term found ×count with sections and excerpts, or not found. unverified = part of the page was unread, so absence proves nothing.\n2. Where is X? focus: matching passages only, best first, under their section path; a miss returns the outline.\n3. Long page: toc (sN ids, sizes), then section=\"sN\".\n4. No view: the page. A cut-off response has next_offset: repeat the call with offset. next_page names a paginated article's next part.\nWalls (login, paywall, bot) fail fast and say which. Sites with a structured source (GitHub, Reddit, Stack Overflow, Wikipedia, registries) come back from it.",
        params: FETCH_PARAMS,
        examples: &[
            "donsetch fetch https://example.com/article",
            "donsetch fetch https://example.com/changelog --must-contain \"breaking change\" --must-contain \"/CVE-\\d{4}-\\d+/\"",
            "donsetch fetch https://docs.example.com/guide --focus \"retry backoff limits\"",
            "donsetch fetch https://docs.example.com/guide --toc",
            "donsetch fetch https://docs.example.com/guide --section s4",
            "donsetch fetch https://a.example/x https://b.example/y --max-chars 20000",
        ],
    },
    ToolSpec {
        name: "web_search",
        cli_cmd: "search",
        summary: "Web search : keyless aggregation or optional configured providers",
        description: "Web search : returns ranked URLs + titles + snippets. Use to decide WHAT to fetch (web_fetch reads content; this never does).\n\nOne query is the normal path. For an ambiguous, multilingual, exploratory, or hard-to-recall information need, add up to two query_variants: all searches run in parallel and come back as clearly separated result sets, with no automatic rewriting or guessed answers.\n\nEach result is one compact evidence row: fetch handle (or raw URL), title, host, focused snippet, and a browser-cost warning only when relevant. Rank already represents DonSeTch's scoring decision, so per-engine scores and timing are not repeated in model context. A source census line names who answered and what every other source said (403, timeout, off-query): retrieval state, not a verdict on the list. Multi-query mode keeps one clearly labeled ranked section per formulation.\n\nDefault: keyless sources fused by cross-source consensus. Configured BYOK providers are preferred; keyless search remains the fallback. First-party interfaces join by answer shape: GitHub, Wikipedia, HN, OpenAlex, arXiv, Europe PMC, crates.io, npm, news, StackExchange, MDN. BYOK: providers configured via `donsetch keys` (Tavily/Exa/Serper/TinyFish/Parallel/BrightData) take over automatically (one provider; intent maps to its filters, and the keyless verticals do not join).\n\ndeadline_ms caps the whole call (honest deadline error, never a hang).\n\nResponse: content contains leading [meta] JSON and the ranked evidence list. State includes provider, a per-source census (id, items, status), plus rank, URL and optional fetch handle for each result; multi-query mode keeps those lists separate. Score, cache, provider, reranker and timing diagnostics live in _meta.\n\nAfter search: fetch the best result via its S-handle.",
        mcp_description: "Discover ranked URLs, titles and snippets; fetch a candidate for its full text. A snippet supports only what it states. A sources census names who answered and what every other source said, so an incomplete set reads as incomplete. Fetch handles resolve directly in web_fetch; citation URLs accompany the evidence in the leading [meta] JSON.",
        params: SEARCH_PARAMS,
        examples: &[
            "donsetch search rust async trait objects",
            "donsetch search \"exact phrase\" --intent code",
            "donsetch search site:github.com tokio --max-results 10",
            "donsetch search \"rust async trait patterns\" --query-variant \"async fn in trait rust\"",
        ],
    },
    ToolSpec {
        name: "web_crawl",
        cli_cmd: "crawl",
        summary: "Crawl a site into markdown (sitemap-aware, focus-ranked, resumable)",
        description: "Crawl a site from a seed : for multi-page extraction (docs, API refs, wikis). Single page → web_fetch; finding sites → web_search.\n\nTwo-phase: sitemap discovery (cheap URL inventory) first, then focus-ranked page fetching with adaptive per-host pacing. Docs sites (mkdocs/docusaurus/sphinx/antora) get their nav as the site map automatically.\n\nModes: full (default) = map + content · map = URL inventory only, very cheap : the site map when the site has one, else the seed page's links · content = BFS from seed, no sitemap (use when sitemap is missing). PDF pages auto-parsed, not skipped. dataset=true emits JSON Lines (one object per page).\n\nBudgets: focus (topic) ranks the frontier by BM25-lite link-text/URL-path keyword scoring and crawls only matches : set it whenever you have a topic. max_pages / max_total_chars / deadline_s cap the run; resume tokens continue across calls. since_last=true skips pages unchanged since your last crawl of the site (fingerprint memory : returns only what moved). Send _meta.progressToken for live per-page progress (\"12 pages, 34 queued\"). A cancelled call (MCP notifications/cancelled) stops the crawl and returns nothing: no pages and no resume token.\n\nResponse: content contains leading [meta] JSON and one site-evidence document. State includes seed, completion, page URLs, stop reason and any resume/next action. Map mode renders its URL inventory in the text body. Queue, skip, score, quality, crawl-delay and timing diagnostics live in _meta. FrontierEmpty is complete only when requested evidence was obtained without failed or excluded pages; MaxPages, CharBudget, DepthLimit, Deadline, ThrottledOut and Cancelled are incomplete.",
        mcp_description: "Read multiple pages of a known site within explicit budgets. Text starts with [meta] JSON containing completion, page URLs and resume state. FrontierEmpty is complete only if evidence was obtained without failed or excluded pages; budget, deadline, throttle, cancellation and depth stops are incomplete. Cite page URLs. dataset=true returns JSONL after the metadata line.",
        params: CRAWL_PARAMS,
        examples: &[
            "donsetch crawl https://docs.site.com --topic \"authentication\"",
            "donsetch crawl https://docs.site.com --mode map",
            "donsetch crawl https://docs.site.com --max-pages 25 --deadline 300",
            "donsetch crawl https://docs.site.com --dataset > site.jsonl",
        ],
    },
    ToolSpec {
        name: "web_screenshot",
        cli_cmd: "screenshot",
        summary: "Open a URL in a real browser, return the page as PNG",
        description: "A rendered PNG of a page: DonSeTch opens the URL in its browser, waits for the load, and returns the capture as PNG bytes. Deeper truth for any page that lies without JavaScript, and a visual receipt for scripts and logins. The capture is in-process only (nothing posted anywhere); the caller decides whether the pixels are worth their tokens. full_page asks the browser to capture beyond the viewport. The result carries the PNG inline plus [meta] JSON {path, bytes}; the path is server-local, under the screenshots dir of the cache root, and full-page captures can run several MB.",
        mcp_description: "Capture a URL as a rendered PNG in the browser. Returns an image content block and [meta] JSON with path and bytes. The path is server-local; use the inline image on remote clients. Full-page captures can be several MB. Private/loopback URLs are blocked like fetch. Failures return ok:false, code and next_action.",
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

// ── Argument validation ──────────────────────────────────────

/// Validate the argument object against parameter names, types, list limits
/// and enum variants. Absent or null fields retain handler defaults;
/// handlers enforce required fields, ranges and cross-field constraints.
pub fn check_args(tool: &ToolSpec, args: &Value) -> Result<(), String> {
    let object = args.as_object().ok_or("arguments must be an object")?;
    for name in object.keys() {
        if !tool.params.iter().any(|p| p.name == name) {
            // A removed surface gets a removal note, not a bare unknown:
            // an agent that learned `archive` from an older schema must
            // learn it is gone (live web only), not keep retrying it.
            if tool.name == "web_fetch"
                && let Some((_, note)) = REMOVED_FETCH_PARAMS.iter().find(|(n, _)| n == name)
            {
                return Err((*note).into());
            }
            return Err(format!(
                "unknown parameter {name:?}; use the current tools/list schema"
            ));
        }
    }
    for p in tool.params {
        // Optional nulls retain the handlers' established unset semantics.
        let value = match object.get(p.name) {
            None | Some(Value::Null) => continue,
            Some(v) => v,
        };
        if let ParamKind::Enum(variants) = p.kind {
            if value.as_str().is_some_and(|s| variants.contains(&s)) {
                continue;
            }
            let allowed = variants
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("{} must be one of {allowed}; got {value}", p.name));
        }
        let strings = |limit: usize| {
            value
                .as_array()
                .is_some_and(|items| items.len() <= limit && items.iter().all(Value::is_string))
        };
        let valid = match p.kind {
            ParamKind::Str => value.is_string(),
            ParamKind::Usize => value.as_u64().is_some_and(|n| usize::try_from(n).is_ok()),
            ParamKind::StrOrListMax(max) => value.is_string() || strings(max),
            ParamKind::StrList => strings(usize::MAX),
            ParamKind::StrListMax(max) => strings(max),
            ParamKind::SetTrue | ParamKind::SetFalse => value.is_boolean(),
            ParamKind::BoolOr(variants) => {
                value.is_boolean() || value.as_str().is_some_and(|s| variants.contains(&s))
            }
            ParamKind::Enum(_) => unreachable!("enums checked above"),
        };
        if !valid {
            return Err(format!(
                "{} has the wrong type or size; use the current tools/list schema",
                p.name
            ));
        }
    }
    Ok(())
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
            ParamKind::StrOrListMax(max) => {
                schema.insert(
                    "anyOf".into(),
                    json!([
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" }, "maxItems": max }
                    ]),
                );
                schema.insert("description".into(), json!(p.mcp_help.unwrap_or(p.help)));
                props.insert(p.name.into(), Value::Object(schema));
                if p.required {
                    required.push(json!(p.name));
                }
                continue;
            }
            ParamKind::BoolOr(variants) => {
                schema.insert(
                    "anyOf".into(),
                    json!([
                        { "type": "boolean" },
                        { "type": "string", "enum": variants }
                    ]),
                );
                schema.insert("description".into(), json!(p.mcp_help.unwrap_or(p.help)));
                props.insert(p.name.into(), Value::Object(schema));
                continue;
            }
            ParamKind::Usize => "integer",
            ParamKind::StrList | ParamKind::StrListMax(_) => "array",
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
            "additionalProperties": false,
        }
    })
}

// ── CLI generation ───────────────────────────────────────────

/// The CLI commands that take `--ignore-rules`: the tools that fetch
/// a caller's URL, which a local rule can deny.
pub const IGNORE_RULES_COMMANDS: &[&str] = &["fetch", "crawl", "screenshot"];

/// Build the clap subcommand for one tool. `--json` and
/// `--quiet` are CLI-adapter flags (not MCP params), appended
/// to every tool command; `--ignore-rules` is one too, appended
/// to the commands in [`IGNORE_RULES_COMMANDS`]. None of them is
/// in the tool-arguments JSON (`matches_to_json` reads only the
/// param table).
pub fn cli_command(tool: &ToolSpec) -> clap::Command {
    // CLI --help speaks to a human in a terminal; the full agent
    // contract stays in the MCP description field. Same facts,
    // different voice.
    let cli_long = match tool.cli_cmd {
        "fetch" => {
            "Read a URL (or several) as clean markdown, or only the part you need. \
--must-contain checks that the page says something without printing it; \
--focus prints only the passages that answer a question; \
--toc prints the outline and --section one part of it. \
Pass a URL from donsetch search to read that result. \
MCP search handles belong to their server session and cannot be reused \
in a separate CLI invocation."
        }
        "search" => {
            "Search the web without API keys: general engines, first-party \
sources (package registries, papers, encyclopedias, a dated news wire) and \
publisher feeds, merged by agreement between independent sources. \
Results include reusable URLs: pass one to donsetch fetch <url>. \
Session-local search handles are used only by the persistent MCP server. \
Use --max-results to cap the list and --intent to pick a vertical \
(web|news|code|paper|entity); the default auto-detects. \
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
    cmd = cmd
        .arg(
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
        );
    // The escape hatch stays out of the param table on purpose: the
    // MCP schema is built from that table, so a model never sees the
    // flag, and `check_args` refuses an `ignore_rules` argument as an
    // unknown parameter. main.rs applies it by switching `[rules]
    // mode` off before the config is installed.
    if IGNORE_RULES_COMMANDS.contains(&tool.cli_cmd) {
        cmd = cmd.arg(
            Arg::new("ignore_rules")
                .long("ignore-rules")
                .action(ArgAction::SetTrue)
                .help(
                    "Run as if the local DonSeTch rules ([rules] in donsetch.toml) were empty: \
                     no rule denies the URL. CLI only.",
                ),
        );
    }
    cmd
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
                ParamKind::Str => arg.value_name("VALUE"),
                ParamKind::StrOrListMax(_) => arg.value_name("VALUE").action(ArgAction::Append),
                ParamKind::Usize => arg.value_name("N").value_parser(clap::value_parser!(usize)),
                ParamKind::Enum(variants) => {
                    arg.value_name("VALUE")
                        .value_parser(clap::builder::PossibleValuesParser::new(
                            variants.iter().copied(),
                        ))
                }
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
                // `--links` alone, or `--links=urls`: the value must be glued
                // on, or it would swallow the URL that follows the flag.
                ParamKind::BoolOr(variants) => arg
                    .value_name("MODE")
                    .num_args(0..=1)
                    .require_equals(true)
                    .default_missing_value(variants[0])
                    .value_parser(clap::builder::PossibleValuesParser::new(
                        variants.iter().copied(),
                    )),
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
                ParamKind::Str | ParamKind::Enum(_) => {
                    if let Some(v) = m.get_one::<String>(p.name) {
                        map.insert(p.name.into(), json!(v));
                    }
                }
                ParamKind::StrOrListMax(_) => {
                    let items: Vec<&str> = m
                        .get_many::<String>(p.name)
                        .map(|v| v.map(String::as_str).collect())
                        .unwrap_or_default();
                    match items.as_slice() {
                        [] => {}
                        [one] => {
                            map.insert(p.name.into(), json!(one));
                        }
                        many => {
                            map.insert(p.name.into(), json!(many));
                        }
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
                ParamKind::BoolOr(variants) => {
                    if let Some(v) = m.get_one::<String>(p.name) {
                        let value = if v == variants[0] {
                            json!(true)
                        } else {
                            json!(v)
                        };
                        map.insert(p.name.into(), value);
                    }
                }
            },
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    #[test]
    fn removed_fetch_params_get_removal_notes_and_leave_the_cli() {
        let tool = super::by_cli_cmd("fetch").unwrap();
        let schema = mcp_schema(tool);
        let props = &schema["inputSchema"]["properties"];
        for (name, replacement) in [
            ("actions", "local engine"),
            ("shot", "web_screenshot"),
            ("budget_tokens", "max_chars"),
            ("mode", "max_chars"),
            ("stitch", "next_page"),
            ("selector", "section"),
            ("media", "image_text"),
        ] {
            assert!(props.get(name).is_none(), "{name} must leave the schema");
            let mut args = serde_json::json!({ "url": "https://example.com" });
            args[name] = serde_json::json!("x");
            let err = check_args(tool, &args).expect_err("removed param must be refused");
            assert!(
                err.starts_with(&format!("{name} was removed")),
                "{name}: {err}"
            );
            assert!(err.contains(replacement), "{name}: {err}");
            let flag = format!("--{}", name.replace('_', "-"));
            let cli = super::cli_command(tool)
                .try_get_matches_from(["fetch", "https://example.com", flag.as_str(), "x"])
                .expect_err("removed flag must leave the CLI");
            assert_eq!(cli.kind(), clap::error::ErrorKind::UnknownArgument);
        }
        let err = check_args(tool, &serde_json::json!({ "nope": 1 }))
            .expect_err("unknown param must be refused");
        assert!(err.contains("unknown parameter"), "{err}");
    }

    #[test]
    fn web_fetch_offers_exactly_its_option_set() {
        let schema = mcp_schema(fetch_tool());
        let mut names: Vec<&str> = schema["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "deadline_ms",
                "focus",
                "image_text",
                "links",
                "max_chars",
                "must_contain",
                "offset",
                "section",
                "since_last",
                "toc",
                "url",
            ]
        );
        assert_eq!(schema["inputSchema"]["required"], json!(["url"]));
        assert_eq!(schema["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn the_fetch_contract_names_every_option_and_stays_compact() {
        let tool = fetch_tool();
        for p in tool.params {
            assert!(
                tool.description.contains(p.name),
                "the full description must explain {}",
                p.name
            );
            assert!(p.mcp_help.is_some(), "{} needs a model-facing line", p.name);
        }
        // Every connected client pays for the schema on every turn.
        let bytes = mcp_schema(tool).to_string().len();
        assert!(bytes < 3_600, "web_fetch schema grew to {bytes} bytes");
    }

    #[test]
    fn must_contain_takes_one_term_or_a_bounded_list_of_strings() {
        let tool = fetch_tool();
        let prop = &mcp_schema(tool)["inputSchema"]["properties"]["must_contain"];
        assert_eq!(prop["anyOf"][0]["type"], "string");
        assert_eq!(prop["anyOf"][1]["maxItems"], 8);
        let url = |extra: Value| {
            let mut args = json!({ "url": "https://example.com" });
            args["must_contain"] = extra;
            check_args(tool, &args)
        };
        assert_eq!(url(json!("rate limit")), Ok(()));
        assert_eq!(url(json!(["a", "b", "/c+/"])), Ok(()));
        let eight: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
        assert_eq!(url(json!(eight)), Ok(()));
        let nine: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
        assert!(url(json!(nine)).is_err());
        assert!(url(json!(["a", 1])).is_err());
        assert!(url(json!(true)).is_err());
        // A url batch keeps its own bound.
        let thirteen: Vec<String> = (0..13).map(|i| format!("https://e.test/{i}")).collect();
        assert!(check_args(tool, &json!({ "url": thirteen })).is_err());
    }

    #[test]
    fn links_take_true_or_a_named_mode_on_the_wire_and_on_the_cli() {
        let tool = fetch_tool();
        for ok in [json!(true), json!(false), json!("urls"), json!("handles")] {
            check_args(tool, &json!({ "url": "https://e.test/", "links": ok }))
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [json!("raw"), json!(1), json!(["urls"])] {
            assert!(
                check_args(tool, &json!({ "url": "https://e.test/", "links": bad })).is_err(),
                "{bad}"
            );
        }
        let parse = |argv: &[&str]| {
            matches_to_json(tool, &cli_command(tool).try_get_matches_from(argv).unwrap())
        };
        // The bare flag keeps its old meaning; the mode is glued on, so the
        // URL after the flag is still the URL.
        assert_eq!(
            parse(&["fetch", "https://e.test/", "--links"])["links"],
            true
        );
        assert_eq!(
            parse(&["fetch", "--links", "https://e.test/"])["links"],
            true
        );
        assert_eq!(
            parse(&["fetch", "https://e.test/", "--links=urls"])["links"],
            true
        );
        assert_eq!(
            parse(&["fetch", "https://e.test/", "--links=handles"])["links"],
            "handles"
        );
        assert_eq!(
            parse(&["fetch", "--links", "https://e.test/"])["url"],
            "https://e.test/"
        );
        assert!(parse(&["fetch", "https://e.test/"]).get("links").is_none());
        assert!(
            cli_command(tool)
                .try_get_matches_from(["fetch", "https://e.test/", "--links=raw"])
                .is_err()
        );
    }

    #[test]
    fn the_cli_sends_one_term_as_a_string_and_repeats_as_a_list() {
        let tool = fetch_tool();
        let parse = |argv: &[&str]| {
            matches_to_json(tool, &cli_command(tool).try_get_matches_from(argv).unwrap())
        };
        let one = parse(&["fetch", "https://e.test/", "--must-contain", "rate limit"]);
        assert_eq!(one["must_contain"], "rate limit");
        let two = parse(&[
            "fetch",
            "https://e.test/",
            "--must-contain",
            "a",
            "--must-contain",
            "/b+/",
        ]);
        assert_eq!(two["must_contain"], json!(["a", "/b+/"]));
        let viewed = parse(&[
            "fetch",
            "https://e.test/",
            "--toc",
            "--section",
            "s2",
            "--max-chars",
            "900",
        ]);
        assert_eq!(
            viewed,
            json!({"url": "https://e.test/", "toc": true, "section": "s2", "max_chars": 900})
        );
        assert!(
            parse(&["fetch", "https://e.test/"])
                .get("must_contain")
                .is_none()
        );
    }

    #[test]
    fn report_audit_crawl_schema_permits_resume_without_a_seed() {
        let tool = TOOLS.iter().find(|t| t.name == "web_crawl").unwrap();
        let schema = mcp_schema(tool)["inputSchema"].clone();
        assert!(
            !schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|v| v == "url"))
        );
        assert_eq!(schema["properties"]["url"]["type"], "string");
        assert_eq!(schema["properties"]["resume"]["type"], "string");
    }

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
        for flag in ["--focus", "--must-contain", "--toc", "--section"] {
            assert!(long_help.contains(flag), "{flag}: {long_help}");
        }
        assert!(long_help.contains("handle"), "{long_help}");
        let long_search = cli_command(search_tool()).render_long_help().to_string();
        assert!(long_search.contains("--intent"), "{long_search}");
        assert!(long_search.contains("reusable URLs"), "{long_search}");
        let long_crawl = cli_command(crawl_tool()).render_long_help().to_string();
        assert!(long_crawl.contains("--resume"), "{long_crawl}");
    }

    #[test]
    fn enum_args_outside_the_spec_are_refused() {
        for bad in ["asdf", "images", "Code"] {
            let args = json!({ "intent": bad });
            assert!(check_args(search_tool(), &args).is_err(), "{bad}");
        }
        for bad in ["asdf", "deep", "MAP"] {
            let args = json!({ "mode": bad });
            assert!(check_args(crawl_tool(), &args).is_err(), "{bad}");
        }
    }

    // The archive fallback was removed (the audit's research foot-gun:
    // an "auto" archive served a 395-day-old snapshot). An agent that
    // learned the parameter from an older schema gets the removal
    // spelled out, not a generic unknown-key shrug.
    #[test]
    fn removed_archive_argument_gets_a_removal_note() {
        let err = check_args(
            fetch_tool(),
            &json!({ "url": "https://example.com", "archive": "auto" }),
        )
        .expect_err("archive must not be accepted");
        assert!(err.contains("archive was removed"), "{err}");
        assert!(err.contains("live"), "{err}");
        // The note is web_fetch-specific; other tools keep the generic
        // unknown-parameter answer.
        let err = check_args(search_tool(), &json!({ "query": "x", "archive": "auto" }))
            .expect_err("archive is not a search parameter");
        assert!(err.contains("unknown parameter"), "{err}");
    }

    // The tier argument was removed with the browser tier (v5): every
    // fetch runs the one local engine. A learned `tier` gets the
    // removal spelled out, like `archive`.
    #[test]
    fn removed_tier_argument_gets_a_removal_note() {
        let err = check_args(
            fetch_tool(),
            &json!({ "url": "https://example.com", "tier": "1" }),
        )
        .expect_err("tier must not be accepted");
        assert!(err.contains("tier was removed"), "{err}");
        assert!(err.contains("local engine"), "{err}");
    }

    #[test]
    fn listed_absent_and_null_enum_args_pass() {
        for tool in TOOLS {
            assert_eq!(check_args(tool, &json!({})), Ok(()), "{}", tool.name);
            for p in tool.params {
                let ParamKind::Enum(variants) = p.kind else {
                    continue;
                };
                for v in variants {
                    let mut args = json!({});
                    args[p.name] = json!(v);
                    assert_eq!(
                        check_args(tool, &args),
                        Ok(()),
                        "{}.{} = {v}",
                        tool.name,
                        p.name
                    );
                }
                let mut args = json!({});
                args[p.name] = Value::Null;
                assert_eq!(check_args(tool, &args), Ok(()), "{}.{}", tool.name, p.name);
            }
        }
    }

    #[test]
    fn ignore_rules_is_a_cli_flag_that_never_reaches_the_tool_arguments() {
        for (name, argv) in [
            (
                "web_fetch",
                ["fetch", "https://example.com", "--ignore-rules"],
            ),
            (
                "web_crawl",
                ["crawl", "https://example.com", "--ignore-rules"],
            ),
            (
                "web_screenshot",
                ["screenshot", "https://example.com", "--ignore-rules"],
            ),
        ] {
            let tool = TOOLS.iter().find(|t| t.name == name).unwrap();
            let matches = cli_command(tool)
                .try_get_matches_from(argv)
                .unwrap_or_else(|e| panic!("{name} must accept --ignore-rules: {e}"));
            assert!(matches.get_flag("ignore_rules"), "{name}");
            let args = matches_to_json(tool, &matches);
            assert!(args.get("ignore_rules").is_none(), "{name}: {args}");
            assert!(
                mcp_schema(tool)["inputSchema"]["properties"]
                    .get("ignore_rules")
                    .is_none(),
                "{name}: the escape hatch must stay out of the MCP schema"
            );
        }
    }

    #[test]
    fn ignore_rules_is_refused_where_no_rule_applies_and_over_mcp() {
        // search fetches no caller URL, so the flag is not declared there.
        let err = cli_command(search_tool())
            .try_get_matches_from(["search", "rust", "--ignore-rules"])
            .expect_err("search must not take --ignore-rules");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        // A model that sends the key over MCP gets an unknown-parameter
        // error, not a silent bypass.
        let err = check_args(
            fetch_tool(),
            &json!({ "url": "https://example.com", "ignore_rules": true }),
        )
        .expect_err("ignore_rules must not be an MCP argument");
        assert!(err.contains("unknown parameter"), "{err}");
    }
}
