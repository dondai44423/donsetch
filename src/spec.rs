//! Tool spec table — the single source of truth for the three
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
    /// JSON boolean; CLI flag whose presence sets `true`.
    SetTrue,
    /// JSON boolean; CLI flag whose presence sets `false`
    /// (negating flags like --any-host for same_host).
    SetFalse,
    /// JSON value passed through as-is (array of objects);
    /// CLI takes a JSON string and parses it.
    JsonStr,
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
    /// Description string — used verbatim as the MCP schema
    /// description AND the clap help text. One string, both
    /// interfaces.
    pub help: &'static str,
}

pub struct ToolSpec {
    /// MCP tool name (`web_fetch`).
    pub name: &'static str,
    /// CLI subcommand (`fetch`).
    pub cli_cmd: &'static str,
    /// One-liner for `donsetch --help` listing.
    pub summary: &'static str,
    /// Full description — MCP tool description AND CLI long help.
    pub description: &'static str,
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
        help: "URL to fetch: http(s) URL, a handle (L1/S2 from earlier results), or an array of up to 12 for one parallel batch call.",
    },
    ParamSpec {
        name: "budget_tokens",
        flag: "budget-tokens",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Batch mode (array url): total output token budget shared across all results (200-500k). DonSeTch allocates it across pages by size — small pages stay whole, big ones get sliced with a resume note. Without it each page uses max_chars independently.",
    },
    ParamSpec {
        name: "focus",
        flag: "focus",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Relevance query — returns ONLY blocks relevant to your question, cutting tokens 50-80% on long pages. Hybrid keyword + semantic matching catches blocks with different vocabulary than your query. If nothing matches, returns full page with a notice. ALWAYS set when you know what you're looking for — #1 token saver.",
    },
    ParamSpec {
        name: "max_chars",
        flag: "max-chars",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max markdown chars (default 16000). Truncated pages include next_offset for resumption.",
    },
    ParamSpec {
        name: "offset",
        flag: "offset",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Resume from a previous response's next_offset to continue a truncated page.",
    },
    ParamSpec {
        name: "section",
        flag: "section",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Heading name (substring, case-insensitive) — return only that section. Use after toc to target a specific part.",
    },
    ParamSpec {
        name: "toc",
        flag: "toc",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "true = heading outline only, no body text. Read structure first, then target with section or focus.",
    },
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time budget for this call in ms (500-600000). On expiry: honest deadline error + next_action — never a silent hang. Batch mode: per-URL budget.",
    },
    ParamSpec {
        name: "archive",
        flag: "archive",
        kind: ParamKind::Enum(&["auto", "only", "off"]),
        cli: CliKind::Flag,
        required: false,
        help: "Dead-page recovery via the keyless Wayback Machine. auto (default): on hard failure (404/paywall/unsolvable wall) serve the nearest archived snapshot, clearly labeled with its date. only: skip the live fetch, go straight to the archive. off: never.",
    },
    ParamSpec {
        name: "since_last",
        flag: "since-last",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Change check instead of full read: if the page is unchanged since your last fetch, output collapses to a one-line verdict; if changed, you get the section-level delta (added/removed/changed). Refetch without it for full content. Monitoring/re-verification at ~zero tokens.",
    },
    ParamSpec {
        name: "stitch",
        flag: "stitch",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Multi-page articles: follow rel=next pagination and return the WHOLE article in one call (up to 6 parts / 48k chars) with *(part N)* markers, instead of re-fetching page by page. Same-host only.",
    },
    ParamSpec {
        name: "image_text",
        flag: "image-text",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "OCR the page's content images (up to 4) and append an 'image text' section. For infographics/comics/screenshots whose meaning IS the image. Costs extra fetch+compute — only when image content matters.",
    },
    ParamSpec {
        name: "must_contain",
        flag: "must-contain",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Probe mode: verify the page mentions a string/pattern WITHOUT loading it into context. Output = MATCH/NO-MATCH verdict + up to 3 short context excerpts. Case-insensitive substring, or /regex/ (e.g. \"/CVE-2026-\\d+/\"). Full fetch still happens (tiers, walls, PDFs) — only the output collapses. For verification questions, not reading.",
    },
    ParamSpec {
        name: "selector",
        flag: "selector",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "CSS selector — extract only from matching elements. Narrows scope precisely.",
    },
    ParamSpec {
        name: "tier",
        flag: "tier",
        kind: ParamKind::Enum(&["auto", "1", "2"]),
        cli: CliKind::Flag,
        required: false,
        help: "auto (default, always use for real work): HTTP first, auto-escalates to headless browser on bot-walls/JS-shells, auto-detects and parses PDFs. \"1\" (testing): HTTP only, no browser — fails on JS sites. \"2\" (testing): browser directly — slower, skips HTTP entirely.",
    },
    ParamSpec {
        name: "links",
        flag: "links",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Include [text](url) link URLs. Default false — saves ~30% tokens. Enable only when you need the URLs.",
    },
    ParamSpec {
        name: "media",
        flag: "media",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Include image alt text and sources. Default false.",
    },
    ParamSpec {
        name: "actions",
        flag: "actions",
        kind: ParamKind::JsonStr,
        cli: CliKind::Flag,
        required: false,
        help: "Browser steps to run BEFORE extraction — page control inside fetch: [{\"do\":\"click\",\"selector\":\"#load-more\"},{\"do\":\"type\",\"selector\":\"input[q]\",\"text\":\"query\"},{\"do\":\"press\",\"key\":\"Enter\"},{\"do\":\"wait_text\",\"text\":\"results\"}]. Steps: wait {ms}, wait_selector {selector,timeout_ms}, wait_text {text,timeout_ms}, click {selector OR text}, hover, type {selector?,text}, press {key: Enter|Tab|Escape|Backspace|ArrowDown|...}, scroll {to: top|bottom|down | px}. Max 16 steps. Actions run in the headless browser (tier auto/2, never 1); after them the page is extracted normally — focus/section/toc still apply. First failing step aborts honestly with per-step results in structuredContent.actions; fix that step and re-run.",
    },
    ParamSpec {
        name: "shot",
        flag: "shot",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "File path — saves a PNG screenshot when blocked by interactive captcha. Only fires on captcha walls; not a general screenshot tool.",
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
    },
    ParamSpec {
        name: "max_results",
        flag: "max-results",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max results (default 7, max 12). The most relevant results almost always live in the top 7. Increase only when results are weak.",
    },
    ParamSpec {
        name: "deadline_ms",
        flag: "deadline-ms",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard time budget in ms (500-600000). Engines have their own timeouts; this caps the whole call. On expiry: honest deadline error.",
    },
    ParamSpec {
        name: "intent",
        flag: "intent",
        kind: ParamKind::Enum(&["auto", "web", "code", "paper", "news", "entity"]),
        cli: CliKind::Flag,
        required: false,
        help: "auto (default) detects from query. code: adds GitHub, HN, StackExchange, MDN verticals. paper: adds Scholar, arXiv. news: adds Google News, HN. entity: adds Wikipedia. web: general only.",
    },
];

// ── web_crawl ────────────────────────────────────────────────

const CRAWL_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "url",
        flag: "",
        kind: ParamKind::Str,
        cli: CliKind::PositionalSingle,
        required: true,
        help: "Seed http(s) URL to crawl from.",
    },
    ParamSpec {
        name: "mode",
        flag: "mode",
        kind: ParamKind::Enum(&["full", "map", "content"]),
        cli: CliKind::Flag,
        required: false,
        help: "full (default): sitemap map + content. map: URL inventory only (very cheap). content: skip sitemap, BFS from seed.",
    },
    ParamSpec {
        name: "focus",
        flag: "topic",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Relevance query — rank pages by relevance and crawl only matching ones. Uses hybrid keyword + semantic matching. Essential for large sites; without it the crawl wastes budget on noise. Set this whenever you have a specific topic in mind.",
    },
    ParamSpec {
        name: "max_pages",
        flag: "max-pages",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max pages to fetch+extract (default 10, cap 200).",
    },
    ParamSpec {
        name: "max_depth",
        flag: "max-depth",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max link depth from seed (default 2). 0 = seed only.",
    },
    ParamSpec {
        name: "max_total_chars",
        flag: "max-chars",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Total extracted-char budget across all pages (default 60000, range 4000-500000).",
    },
    ParamSpec {
        name: "per_page_max",
        flag: "per-page-max",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Max markdown chars per page (default 8000, range 400-40000).",
    },
    ParamSpec {
        name: "include_paths",
        flag: "include",
        kind: ParamKind::StrList,
        cli: CliKind::Flag,
        required: false,
        help: "Path globs to include (e.g. [\"/docs/*\"]). Empty = all.",
    },
    ParamSpec {
        name: "exclude_paths",
        flag: "exclude",
        kind: ParamKind::StrList,
        cli: CliKind::Flag,
        required: false,
        help: "Path globs to exclude (e.g. [\"*/tags/*\", \"*/archive/*\"]).",
    },
    ParamSpec {
        name: "same_host",
        flag: "any-host",
        kind: ParamKind::SetFalse,
        cli: CliKind::Flag,
        required: false,
        help: "Stay on seed's host (default true). false = follow cross-domain links.",
    },
    ParamSpec {
        name: "respect_robots",
        flag: "no-robots",
        kind: ParamKind::SetFalse,
        cli: CliKind::Flag,
        required: false,
        help: "Obey robots.txt Disallow + crawl-delay (default true).",
    },
    ParamSpec {
        name: "deadline_s",
        flag: "deadline",
        kind: ParamKind::Usize,
        cli: CliKind::Flag,
        required: false,
        help: "Hard crawl deadline in seconds (default 120, range 5-600). Partial results return after.",
    },
    ParamSpec {
        name: "since_last",
        flag: "since-last",
        kind: ParamKind::SetTrue,
        cli: CliKind::Flag,
        required: false,
        help: "Delta crawl: skip pages you already fetched in the last 24h (fingerprint on file) — only new/changed pages are fetched and counted. Monitoring and re-crawls at a fraction of the cost.",
    },
    ParamSpec {
        name: "resume",
        flag: "resume",
        kind: ParamKind::Str,
        cli: CliKind::Flag,
        required: false,
        help: "Resume token from a previous response to continue a stopped crawl. Valid for 30 min.",
    },
];

// ── The table ────────────────────────────────────────────────

pub static TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "web_fetch",
        cli_cmd: "fetch",
        summary: "Fetch a URL as clean markdown (auto bot-wall bypass, PDF, JS render)",
        description: "Fetch one URL (or a batch) as clean markdown — use when you have a specific URL to read. To find URLs use web_search; for whole sites use web_crawl.\n\nURL forms: a URL · an L-handle from earlier fetch output ([text](L12) → fetch L12) · an S-handle from search (fetch S3 = result 3) · an array of up to 12 for ONE parallel batch call (share a budget with budget_tokens).\n\nPick the CHEAPEST reading mode for the job:\n- Verification question (\"does it mention X?\") → must_contain=\"X\" (or /regex/) — returns MATCH/NO-MATCH + ≤3 excerpts, ~60 tokens.\n- Don't know where it is in a long page → toc=true (outline with section ids+sizes) → section=\"s3\" or section=\"heading text\" for just that part.\n- Know the topic → focus=\"query\" — only relevant blocks, 50-80% cheaper.\n- Just reading → default full page.\n\nRe-checking a page you fetched before: since_last=true → one-line unchanged verdict, or the section-level diff if it changed (~30 tokens). structuredContent.changed carries the verdict on every fetch.\n\nMulti-page articles (rel=next chains): stitch=true returns the whole article in one call (≤6 parts, *(part N)* markers).\n\nDead links: archive=auto (default) serves the nearest Wayback snapshot, honestly labeled with its age; archive=only skips the live web.\n\nReliability: PDFs (even scanned, ≤100MB) auto-parsed; bot walls auto-escalate to a headless browser, solve, and hand back to fast HTTP; known-walled sites that return decoy content to plain HTTP get an equivalence check (decoy_suspected flag). JS-only pages need actions=[{click|type|press|scroll|wait,...}] — deterministic wait_selector/wait_text beats blind sleeps. image_text=true OCRs content images (infographics/comics).\n\nTime control: deadline_ms caps any fetch (honest deadline error, never a hang). Send _meta.progressToken for per-URL progress on batches. Long output: structuredContent.next_offset → call again with offset.\n\nDomain intelligence: reddit threads/listings, npm/PyPI/crates.io/Go/RubyGems pages, GitHub issues/releases/commits, Stack Overflow, Wikipedia infoboxes and docs sites are auto-restructured from each site's best source (labeled via=adapter:... in structuredContent) — no special params, it just returns clean structure.\n\nResponse: content[0].text = markdown; structuredContent = {status, tier, verdict, content_ok, thin, content_kind (Article|Listing|Forum|Docs|Table|Page), quality 0-1, lang, title, changed, fingerprint, via, stitched, prewarmed_by_search, next_offset, tokens_est, ms, escalation (per-step ms trace), url}. content_ok=false or thin=true = possible JS shell. Errors: isError=true with structuredContent {code (stable: wall.challenge, wall.paywall, guard.ssrf, deadline.hit, content.binary, archive.stale, ...), next_action, escalation} — next_action says exactly what to do next",
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
        summary: "Web search — 5 keyless engines merged + reranked, or your API keys",
        description: "Web search — returns ranked URLs + titles + snippets. Use to decide WHAT to fetch (web_fetch reads content; this never does).\n\nResults list position handles: fetch S1 = result 1, S2 = result 2... (raw URLs in structuredContent for citation). Domains known to need the browser carry a ⚠ needs-browser hint — pick a faster source or budget time before fetching. A *degraded:* footer names any engines that failed — silent quality loss is visible.\n\nEngines: 10+ keyless backends fused by cross-engine consensus + local semantic reranking (automatic). Verticals via intent: GitHub, Wikipedia, HN, Scholar, news, StackExchange, MDN. BYOK: providers configured via `donsetch keys` (Tavily/Exa/Serper/TinyFish) take over; structuredContent.provider names what served (null = local keyless).\n\ndeadline_ms caps the fan-out (honest deadline error, never a hang).\n\nResponse: numbered markdown list (N. **Title** — domain / snippet / Sn / engines+score). structuredContent = {intent, weak, cached, elapsed_ms, provider, results:[{title,url,snippet,score,consensus,engines}], engines:[{engine,status,hits,ms}]}. Key signals: weak=true = low consensus, treat with care; consensus = independent engines agreeing (authority); engines[].status = per-engine health.\n\nAfter search: fetch the best result via its S-handle — enrichment pre-fetches top results, so the next fetch of S1…S5 is near-instant (prewarmed_by_search=true)",
        params: SEARCH_PARAMS,
        examples: &[
            "donsetch search rust async trait objects",
            "donsetch search \"exact phrase\" --intent code",
            "donsetch search site:github.com tokio --max-results 10",
        ],
    },
    ToolSpec {
        name: "web_crawl",
        cli_cmd: "crawl",
        summary: "Crawl a site into markdown (sitemap-aware, focus-ranked, resumable)",
        description: "Crawl a site from a seed — for multi-page extraction (docs, API refs, wikis). Single page → web_fetch; finding sites → web_search.\n\nTwo-phase: sitemap discovery (cheap URL inventory) first, then focus-ranked page fetching with adaptive per-host pacing. Docs sites (mkdocs/docusaurus/sphinx/antora) get their nav as the site map automatically.\n\nModes: full (default) = map + content · map = URL inventory only, very cheap — see what a site has before committing · content = BFS from seed, no sitemap (use when sitemap is missing). PDF pages auto-parsed, not skipped.\n\nBudgets: focus (topic) ranks pages by keyword+semantic relevance and crawls only matches — set it whenever you have a topic. max_pages / max_total_chars / deadline_s cap the run; resume tokens continue across calls. since_last=true skips pages unchanged since your last crawl of the site (fingerprint memory — returns only what moved). Send _meta.progressToken for live per-page progress (\"12 pages, 34 queued\"); cancellation stops gracefully and keeps the resume token.\n\nResponse: map (if any) + pages as markdown. structuredContent = {seed, pages:[{url,title,kind,chars,quality}], map, queued, filtered_out, skipped:[{url,reason}], stop, elapsed_s, resume}. stop = FrontierEmpty (done) | MaxPages|CharBudget|DepthLimit|Deadline (budget — resume to continue) | ThrottledOut (site pushed back — wait, then resume) | Cancelled (resume token kept)",
        params: CRAWL_PARAMS,
        examples: &[
            "donsetch crawl https://docs.site.com --topic \"authentication\"",
            "donsetch crawl https://docs.site.com --mode map",
            "donsetch crawl https://docs.site.com --max-pages 25 --deadline 300",
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
                schema.insert("type".into(), json!(["string", "array"]));
                schema.insert("items".into(), json!({ "type": "string" }));
                schema.insert("description".into(), json!(p.help));
                props.insert(p.name.into(), Value::Object(schema));
                if p.required {
                    required.push(json!(p.name));
                }
                continue;
            }
            ParamKind::Usize => "integer",
            ParamKind::StrList | ParamKind::JsonStr => "array",
            ParamKind::SetTrue | ParamKind::SetFalse => "boolean",
        };
        schema.insert("type".into(), json!(ty));
        if let ParamKind::Enum(variants) = p.kind {
            schema.insert("enum".into(), json!(variants));
        }
        if p.kind == ParamKind::StrList {
            schema.insert("items".into(), json!({ "type": "string" }));
        }
        if p.kind == ParamKind::JsonStr {
            schema.insert("items".into(), json!({ "type": "object" }));
        }
        schema.insert("description".into(), json!(p.help));
        props.insert(p.name.into(), Value::Object(schema));
        if p.required {
            required.push(json!(p.name));
        }
    }
    json!({
        "name": tool.name,
        "description": tool.description,
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
    let mut cmd = clap::Command::new(tool.cli_cmd)
        .about(tool.summary)
        .long_about(tool.description)
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
                ParamKind::JsonStr => arg.value_name("JSON"),
                ParamKind::StrList => arg
                    .value_name("GLOB")
                    .action(ArgAction::Append)
                    .value_delimiter(','),
                ParamKind::SetTrue => arg.action(ArgAction::SetTrue),
                ParamKind::SetFalse => arg.action(ArgAction::SetFalse),
            }
        }
    }
}

/// Convert parsed CLI matches into the exact JSON args Value
/// the MCP dispatcher receives. Unset flags are omitted — the
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
                ParamKind::JsonStr => {
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
                ParamKind::StrList => {
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
