//! Declarative catalog of every DonSeTch setting, preset, validation rule,
//! and config read/write adapter shared by the interactive `donsetch settings`
//! TUI and non-interactive `donsetch settings get|set|reset|list` commands.

use crate::config::{
    self, ColorMode, DonsetchConfig, Loaded, RouteMemory, TomlEditValue, TransportKind,
};
use crate::rules::RulesMode;

/// Top-level tabs in the `donsetch settings` Control Center.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabId {
    Search,
    Fetch,
    Providers,
    Proxies,
    Browser,
    Server,
    Rules,
    Presets,
}

impl TabId {
    pub const ALL: [TabId; 8] = [
        TabId::Search,
        TabId::Fetch,
        TabId::Providers,
        TabId::Proxies,
        TabId::Browser,
        TabId::Server,
        TabId::Rules,
        TabId::Presets,
    ];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Fetch => "Fetch",
            Self::Providers => "Providers",
            Self::Proxies => "Proxies",
            Self::Browser => "Browser",
            Self::Server => "Server",
            Self::Rules => "Rules",
            Self::Presets => "Presets",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Search => "Fan-out, slow-network mode, keyless engines & ranking",
            Self::Fetch => "Fetching, PDF limits, PP-OCR threads, crawl & DNS",
            Self::Providers => "BYOK search keys, default provider, SearXNG & plugins",
            Self::Proxies => "SOCKS5/HTTP proxy pool, routing switches & live probes",
            Self::Browser => "Screenshots, external CDP, Unlocker bypass, TLS & persona",
            Self::Server => "MCP transport, HTTP server auth, state & CLI colors",
            Self::Rules => "Per-host deny/allow rules and live URL tester",
            Self::Presets => "One-click network/daemon presets & debug diagnostics",
        }
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let len = Self::ALL.len();
        Self::ALL[(self.index() + len - 1) % len]
    }
}

/// Interactive control widget & validation shape for one config setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlKind {
    Toggle,
    SliderInt {
        min: i64,
        max: i64,
        step: i64,
        coarse: i64,
        unit: &'static str,
    },
    SliderFloat {
        min: f64,
        max: f64,
        step: f64,
        coarse: f64,
    },
    Enum(&'static [&'static str]),
    Text,
    Url,
    Secret,
    List,
}

/// One typed setting exposed in `donsetch settings`.
#[derive(Debug, Clone, Copy)]
pub struct SettingSpec {
    pub tab: TabId,
    pub group: &'static str,
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub summary: &'static str,
    pub help: &'static str,
    pub control: ControlKind,
    pub cli_hint: &'static str,
}

impl SettingSpec {
    pub fn full_key(&self) -> String {
        format!("{}.{}", self.section, self.key)
    }

    pub fn env_var(&self) -> String {
        format!(
            "DONSETCH_{}__{}",
            self.section.to_ascii_uppercase(),
            self.key.to_ascii_uppercase()
        )
    }

    pub fn is_secret(&self) -> bool {
        matches!(self.control, ControlKind::Secret)
            || matches!(
                (self.section, self.key),
                ("proxy", "http" | "https" | "all" | "pool")
                    | ("search", "searxng_url")
                    | ("browser", "cdp_url")
            )
    }
}

/// Curated one-click settings profile that stages multiple knobs together.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub tagline: &'static str,
    pub description: &'static str,
    pub changes: &'static [(&'static str, &'static str, &'static str)],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "slow-network",
        name: "Slow / High-RTT Network",
        tagline: "Sequential search · wave size 1 · 15s timeout",
        description: "Runs search engines one at a time with no early stop and a 15-second per-engine timeout so slow or congested links do not time out in parallel fan-outs (#470, #478).",
        changes: &[
            ("search", "sequential", "true"),
            ("search", "wave_size", "1"),
            ("search", "timeout_ms", "15000"),
        ],
    },
    Preset {
        id: "balanced",
        name: "Balanced Default",
        tagline: "Parallel 3-engine wave · 4s timeout · all keyless sources",
        description: "Restores the fast default search and fetch posture: 3-engine parallel wave with reserve failover, 4s fan-out timeout, and all keyless search endpoints enabled.",
        changes: &[
            ("search", "sequential", "false"),
            ("search", "wave_size", "3"),
            ("search", "timeout_ms", "4000"),
            ("search", "parallel_mcp", "true"),
            ("search", "keenable", "true"),
            ("search", "exa_mcp", "true"),
        ],
    },
    Preset {
        id: "low-memory",
        name: "Low-Memory MCP Daemon",
        tagline: "2 OCR threads · 60s idle unload · prewarm off",
        description: "Bounds ONNX OCR threads to 2, evicts idle OCR models after 60 seconds, and disables background search URL prewarming for NAS devices, containers, and small VPS hosts.",
        changes: &[
            ("fetch", "ocr_threads", "2"),
            ("fetch", "ocr_idle_secs", "60"),
            ("fetch", "prewarm", "false"),
        ],
    },
    Preset {
        id: "no-third-party-mcp",
        name: "Classic Web Engines Only",
        tagline: "Disable Parallel MCP, Keenable & Exa MCP keyless endpoints",
        description: "Turns off third-party keyless MCP/API search endpoints while keeping direct web engines and any configured self-hosted SearXNG / Degoog instance active.",
        changes: &[
            ("search", "parallel_mcp", "false"),
            ("search", "keenable", "false"),
            ("search", "exa_mcp", "false"),
        ],
    },
];

/// Retired or deprecated compatibility-only knobs kept in `config::fieldbook()`
/// solely so pre-v5 config files still parse without error. Hidden from the
/// human Settings UI so operators never waste time toggling no-ops.
pub const DEPRECATED_KNOBS: &[(&str, &str)] = &[
    ("fetch", "h3"),
    ("fetch", "alt_svc"),
    ("fetch", "shadow_fetch"),
    ("fetch", "shadow_deadline_ms"),
    ("search", "rerank_topup"),
    ("search", "rerank_threads"),
    ("search", "quality_prior"),
];

/// Complete registry of active DonSeTch configuration knobs.
/// A unit test enforces that every non-deprecated knob in `config::fieldbook()`
/// is listed here so new settings automatically land in `donsetch settings`.
pub const SETTINGS: &[SettingSpec] = &[
    // ── Search ───────────────────────────────────────────────────────────────
    SettingSpec {
        tab: TabId::Search,
        group: "Slow Network & Concurrency",
        section: "search",
        key: "sequential",
        label: "Sequential Search Mode",
        summary: "Run search lanes one at a time with no early stop",
        help: "Polls every planned search engine strictly one at a time instead of racing parallel waves. Ideal for slow, metered, or high-RTT connections where concurrent requests saturate the link and time out together.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.sequential true",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Slow Network & Concurrency",
        section: "search",
        key: "wave_size",
        label: "Initial Wave Size",
        summary: "Engines launched simultaneously in wave 1 (1..=8)",
        help: "Number of general search engines started at once when sequential mode is off. Lower to 1 or 2 on modest connections; default 3 balances latency against rate limits.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 8,
            step: 1,
            coarse: 2,
            unit: "lanes",
        },
        cli_hint: "donsetch settings set search.wave_size 1",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Slow Network & Concurrency",
        section: "search",
        key: "timeout_ms",
        label: "Fan-Out Timeout",
        summary: "Hard search timeout in ms (scales marks & connect timeouts)",
        help: "Hard timeout per search wave (or per engine in sequential mode). Soft hedge marks, late cutoffs, and TCP/TLS connect timeouts scale proportionally for slow or proxied links.",
        control: ControlKind::SliderInt {
            min: 1_000,
            max: 120_000,
            step: 1_000,
            coarse: 5_000,
            unit: "ms",
        },
        cli_hint: "donsetch settings set search.timeout_ms 15000",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Slow Network & Concurrency",
        section: "search",
        key: "request_budget",
        label: "Request Budget",
        summary: "Maximum lanes asked per search query (1..=20)",
        help: "Caps how many search lanes a single query may ask. Lanes beyond the budget are honestly reported as not-asked rather than silently dropped.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 20,
            step: 1,
            coarse: 2,
            unit: "lanes",
        },
        cli_hint: "donsetch settings set search.request_budget 6",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Keyless & Self-Hosted Sources",
        section: "search",
        key: "searxng_url",
        label: "SearXNG / Degoog URL",
        summary: "Self-hosted SearXNG or Degoog base URL (direct HTTP JSON)",
        help: "Point DonSeTch directly at a self-hosted SearXNG or Degoog instance (e.g. http://127.0.0.1:8080) with JSON format enabled. Supports explicit lang, news, and image search without spawning a plugin script.",
        control: ControlKind::Url,
        cli_hint: "donsetch settings set search.searxng_url http://127.0.0.1:8080",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Keyless & Self-Hosted Sources",
        section: "search",
        key: "parallel_mcp",
        label: "Parallel MCP Source",
        summary: "Include Parallel's anonymous keyless MCP search endpoint",
        help: "Uses Parallel's keyless public MCP endpoint on searches without an explicit language override. Disable if you want zero traffic to mcp.parallel.ai.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.parallel_mcp false",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Keyless & Self-Hosted Sources",
        section: "search",
        key: "keenable",
        label: "Keenable Public Source",
        summary: "Include Keenable's keyless public search endpoint",
        help: "Queries api.keenable.ai (stateless keyless tier) alongside other search engines when no explicit lang is set. Disable to keep searches off Keenable.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.keenable false",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Keyless & Self-Hosted Sources",
        section: "search",
        key: "exa_mcp",
        label: "Exa MCP Source",
        summary: "Include Exa's public keyless MCP search endpoint",
        help: "Queries mcp.exa.ai via stateless web_search_exa without keys or cookies when no explicit lang is set. Disable to exclude Exa from keyless fan-out.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.exa_mcp false",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "search_early",
        label: "Early Consensus Stop",
        summary: "Stop waiting once independent index families agree on top-3",
        help: "Cuts slow stragglers once independent search families agree on a top-3 URL. Ignored automatically when Sequential Search Mode is enabled.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.search_early false",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "learned_rank",
        label: "Compiled Linear Ranker",
        summary: "Use compiled linear weights on validated query shapes",
        help: "Ranks code, how-to, and shopping queries using compiled held-out weights (no LLM or runtime model). Turn off to rank every query strictly by family fusion vote.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.learned_rank true",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "query_compile",
        label: "Operator Compilation",
        summary: "Compile site:/filetype:/intitle: per engine",
        help: "Adapts search operators for each engine and strips syntax that an engine ignores so queries do not return empty SERPs.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.query_compile true",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "serp_instant",
        label: "SERP Instant Answers",
        summary: "Extract featured/instant/knowledge cards from SERP HTML",
        help: "Includes byte-derived featured snippets and knowledge boxes in search metadata. Off by default so the agent reads primary pages directly via web_fetch.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.serp_instant true",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "enrich",
        label: "Auto-Enrich Snippets",
        summary: "Fetch top result pages to replace SERP teasers (extra traffic)",
        help: "Fetches top search hits automatically to pull real titles and descriptions. Off by default; agents can ask per-call via `--read N` instead.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.enrich true",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "outcome_feedback",
        label: "Outcome Feedback Demotion",
        summary: "Soft-demote hosts that repeatedly fail must_contain/content_ok",
        help: "Experimental ranking adjustment that gently demotes hosts with repeated empty or failed reads in prior tool calls.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set search.outcome_feedback true",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Ranking & Behavior",
        section: "search",
        key: "tie_window",
        label: "Lexical Tie Window",
        summary: "Max lexical nudge share (0.0..=1.0) outside linear ranker",
        help: "Controls how much BM25+ lexical evidence may shift a tie where the compiled linear model does not apply. Default 0.0 relies on multi-family consensus.",
        control: ControlKind::SliderFloat {
            min: 0.0,
            max: 1.0,
            step: 0.05,
            coarse: 0.2,
        },
        cli_hint: "donsetch settings set search.tie_window 0.0",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Provider Extras",
        section: "search",
        key: "brightdata_zone",
        label: "Bright Data SERP Zone",
        summary: "Override Bright Data SERP zone name",
        help: "Optional zone override when using Bright Data SERP via BYOK (normally embedded in the key as TOKEN::ZONE).",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set search.brightdata_zone serp_api1",
    },
    SettingSpec {
        tab: TabId::Search,
        group: "Provider Extras",
        section: "search",
        key: "google_profile",
        label: "Google Legacy Profile",
        summary: "Optional legacy mobile-protocol probe profile ID",
        help: "Niche override for legacy Google web-light profile probing. Leave empty to use the default native CSE lane.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set search.google_profile \"\"",
    },
    // ── Fetch, PDF & OCR ─────────────────────────────────────────────────────
    SettingSpec {
        tab: TabId::Fetch,
        group: "Fetching & Security",
        section: "fetch",
        key: "allow_private_egress",
        label: "Allow Private / Loopback IPs",
        summary: "Permit fetching RFC1918, loopback & LAN addresses (disables SSRF block)",
        help: "Allows web_fetch and web_crawl to reach localhost, Docker networks, and LAN hosts. Keep off on untrusted multi-tenant agents.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set fetch.allow_private_egress true",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Fetching & Security",
        section: "fetch",
        key: "adapters",
        label: "Site Adapters",
        summary: "Use fast structured adapters (GitHub, Reddit, Wiki, npm, crates, YouTube)",
        help: "Fetches clean structured records from first-party public APIs before falling back to HTML parsing.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set fetch.adapters true",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Fetching & Security",
        section: "fetch",
        key: "prewarm",
        label: "Search Result Prewarming",
        summary: "Warm top search URLs in the background for faster follow-up fetches",
        help: "Pre-warms connections/cache for top search hits. Turn off on metered, slow, or low-RAM setups.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set fetch.prewarm false",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Fetching & Security",
        section: "fetch",
        key: "dns_cache_ttl_secs",
        label: "In-Process DNS Cache TTL",
        summary: "Seconds to reuse resolved IPs (0 disables DNS cache)",
        help: "Avoids paying two DNS round trips per fetch (SSRF guard + socket connect). Set to 0 to resolve fresh every request.",
        control: ControlKind::SliderInt {
            min: 0,
            max: 3600,
            step: 5,
            coarse: 60,
            unit: "s",
        },
        cli_hint: "donsetch settings set fetch.dns_cache_ttl_secs 30",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Fetching & Security",
        section: "fetch",
        key: "adapter_dump_dir",
        label: "Adapter Dump Directory",
        summary: "Write raw adapter payloads to this folder for debugging",
        help: "Developer hook: dumps raw JSON/HTML adapter payloads into the named directory. Leave blank for normal operation.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set fetch.adapter_dump_dir /tmp/adapters",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "PDF & PP-OCR",
        section: "fetch",
        key: "pdf_max_mb",
        label: "Max PDF Size",
        summary: "Maximum PDF download size in MB (1..=4096)",
        help: "Upper byte cap for PDF downloads fetched by web_fetch. Prevents runaway memory use on giant archive scans.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 4096,
            step: 10,
            coarse: 100,
            unit: "MB",
        },
        cli_hint: "donsetch settings set fetch.pdf_max_mb 100",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "PDF & PP-OCR",
        section: "fetch",
        key: "ocr",
        label: "PDF / Image OCR",
        summary: "Run PP-OCRv5 on scanned PDFs and `--image-text` reads",
        help: "Enables local ONNX PP-OCR arbitration for scanned or corrupted PDF pages (requires an `ocr`-enabled build). Turn off to never load ONNX Runtime.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set fetch.ocr true",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "PDF & PP-OCR",
        section: "fetch",
        key: "ocr_max_pages",
        label: "Max OCR Pages / PDF",
        summary: "Cap OCR pages processed per PDF (1..=500)",
        help: "Limits how many scanned pages in a single PDF run through OCR before stopping cleanly with a partial marker.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 500,
            step: 5,
            coarse: 25,
            unit: "pages",
        },
        cli_hint: "donsetch settings set fetch.ocr_max_pages 25",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "PDF & PP-OCR",
        section: "fetch",
        key: "ocr_threads",
        label: "OCR ONNX Threads",
        summary: "Intra-op threads per OCR session (0 = auto capped at 4)",
        help: "Caps CPU worker threads spawned by ONNX Runtime. 0 auto-picks min(cores, 4); set 1 or 2 on constrained containers.",
        control: ControlKind::SliderInt {
            min: 0,
            max: 64,
            step: 1,
            coarse: 4,
            unit: "thr",
        },
        cli_hint: "donsetch settings set fetch.ocr_threads 2",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "PDF & PP-OCR",
        section: "fetch",
        key: "ocr_idle_secs",
        label: "OCR Idle Unload",
        summary: "Seconds idle before unloading OCR models (0 = keep loaded)",
        help: "Reclaims ONNX memory arenas and worker threads in long-lived MCP daemons after OCR sits idle. Set 0 to keep models warm permanently.",
        control: ControlKind::SliderInt {
            min: 0,
            max: 86_400,
            step: 30,
            coarse: 300,
            unit: "s",
        },
        cli_hint: "donsetch settings set fetch.ocr_idle_secs 120",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Crawl Tuning",
        section: "fetch",
        key: "crawl_shape",
        label: "Crawl Digest Shaping",
        summary: "Shape crawled pages into concise focused digests",
        help: "Produces compact topic-ranked digests with `pN` handles instead of dumping raw boilerplate pages.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set fetch.crawl_shape true",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Crawl Tuning",
        section: "fetch",
        key: "resume_ttl_secs",
        label: "Crawl Resume TTL",
        summary: "Seconds a crawl resume token stays valid (300..=86400)",
        help: "How long persisted crawl job checkpoints remain resumable via `donsetch crawl resume <id>`.",
        control: ControlKind::SliderInt {
            min: 300,
            max: 86_400,
            step: 300,
            coarse: 3600,
            unit: "s",
        },
        cli_hint: "donsetch settings set fetch.resume_ttl_secs 7200",
    },
    SettingSpec {
        tab: TabId::Fetch,
        group: "Crawl Tuning",
        section: "fetch",
        key: "host_pace_file",
        label: "Shared Host Pacing File",
        summary: "Coordinate per-host crawl politeness across concurrent processes",
        help: "Shares per-host crawl pacing timestamps on disk so parallel agents do not hammer the same origin simultaneously.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set fetch.host_pace_file true",
    },
    // ── Proxies ──────────────────────────────────────────────────────────────
    SettingSpec {
        tab: TabId::Proxies,
        group: "Pool Routing",
        section: "proxy",
        key: "fetch_rotate",
        label: "Fetch Uses Proxy Pool",
        summary: "Route web_fetch through proxy-pool lanes (opt-in, default off)",
        help: "Same switch as `donsetch proxy fetch on|off`. Sticky per host; rotates on 429/407/dead/timeout. Off keeps web_fetch on your direct IP.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch proxy fetch on",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Pool Routing",
        section: "proxy",
        key: "crawl_rotate",
        label: "Crawl Uses Proxy Pool",
        summary: "Spread crawl reads across proxy-pool lanes (default on)",
        help: "Same switch as `donsetch proxy crawl on|off`. Crawl always contacts the seed directly first, then spreads across pool lanes when enabled.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch proxy crawl off",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Pool Routing",
        section: "proxy",
        key: "from_environment",
        label: "Honor Env Proxies",
        summary: "Use ambient HTTP_PROXY / HTTPS_PROXY / ALL_PROXY / NO_PROXY",
        help: "Reads standard OS/shell proxy environment variables when explicit config proxy URLs are unset.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set proxy.from_environment true",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Pool Routing",
        section: "proxy",
        key: "egress_persist",
        label: "Persist Proxy Health",
        summary: "Remember dead/throttled proxy lane health across restarts",
        help: "Saves per-lane health scores to cache so restarts do not re-hit dead proxies immediately.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set proxy.egress_persist true",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Explicit Forward Proxies",
        section: "proxy",
        key: "https",
        label: "HTTPS Proxy URL",
        summary: "Explicit HTTPS forward proxy (beats ambient HTTPS_PROXY)",
        help: "Forward proxy URL for HTTPS requests (credentials are masked in display and saved with 0600 permissions).",
        control: ControlKind::Url,
        cli_hint: "donsetch settings set proxy.https http://127.0.0.1:8080",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Explicit Forward Proxies",
        section: "proxy",
        key: "http",
        label: "HTTP Proxy URL",
        summary: "Explicit HTTP forward proxy (beats ambient HTTP_PROXY)",
        help: "Forward proxy URL for plain HTTP requests.",
        control: ControlKind::Url,
        cli_hint: "donsetch settings set proxy.http http://127.0.0.1:8080",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Explicit Forward Proxies",
        section: "proxy",
        key: "all",
        label: "All-Scheme Proxy URL",
        summary: "Fallback forward proxy for both HTTP and HTTPS",
        help: "Used for both HTTP and HTTPS traffic when scheme-specific proxy URLs are empty.",
        control: ControlKind::Url,
        cli_hint: "donsetch settings set proxy.all socks5://127.0.0.1:1080",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Explicit Forward Proxies",
        section: "proxy",
        key: "no_proxy",
        label: "No-Proxy Bypass List",
        summary: "Comma-separated hosts/CIDRs bypassing forward proxies",
        help: "Supports domain suffixes, literal IPs, CIDRs (10.0.0.0/8), host:port, and `*`. Overrides ambient NO_PROXY when non-empty.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set proxy.no_proxy localhost,127.0.0.1,.internal",
    },
    SettingSpec {
        tab: TabId::Proxies,
        group: "Explicit Forward Proxies",
        section: "proxy",
        key: "pool",
        label: "Config Pool List (TOML)",
        summary: "Comma-separated proxy URLs declared directly in donsetch.toml",
        help: "Extra proxy pool entries stored in donsetch.toml alongside proxies.txt. For day-to-day pool management, use the Interactive Proxy Pool section below.",
        control: ControlKind::List,
        cli_hint: "donsetch proxy add socks5://user:pass@host:1080",
    },
    // ── Browser, Bypass, TLS & Persona ───────────────────────────────────────
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "chromium_path",
        label: "Chromium Binary Path",
        summary: "Explicit path to Chrome / Chromium / Edge for web_screenshot",
        help: "Leave blank for automatic discovery across standard install paths, Playwright caches, and PATH. Only used for screenshots — never for fetch/search/crawl.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set browser.chromium_path /usr/bin/chromium",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "cdp_url",
        label: "External CDP Endpoint",
        summary: "Remote Chromium DevTools URL (http(s):// or ws(s)://) for NAS/Docker",
        help: "Connects web_screenshot to an external Chromium container using an isolated incognito context with full SSRF/rule enforcement.",
        control: ControlKind::Url,
        cli_hint: "donsetch settings set browser.cdp_url http://127.0.0.1:9222",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "playwright_path",
        label: "Playwright Browsers Root",
        summary: "Extra directory to search for Playwright-managed Chromium binaries",
        help: "Overrides ambient PLAYWRIGHT_BROWSERS_PATH when locating a Chromium binary for screenshots.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set browser.playwright_path /path/to/ms-playwright",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "route_probes",
        label: "Background Route Probes",
        summary: "Periodically re-verify learned site routes",
        help: "Controls background probing of stale site route records.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set browser.route_probes true",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "probe_scan_secs",
        label: "Probe Scan Interval",
        summary: "Seconds between background route probe scans (1..=3600)",
        help: "How frequently background route probe maintenance checks for stale entries.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 3600,
            step: 10,
            coarse: 60,
            unit: "s",
        },
        cli_hint: "donsetch settings set browser.probe_scan_secs 120",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "probe_stale_secs",
        label: "Probe Stale Window",
        summary: "Seconds before a route probe result is considered stale (60..=604800)",
        help: "Controls how long cached probe outcomes remain fresh before re-probing.",
        control: ControlKind::SliderInt {
            min: 60,
            max: 604_800,
            step: 600,
            coarse: 3600,
            unit: "s",
        },
        cli_hint: "donsetch settings set browser.probe_stale_secs 21600",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Screenshots & Chromium",
        section: "browser",
        key: "login_force",
        label: "Force Headless Mode",
        summary: "Run headless browser flows even without a display server",
        help: "Compatibility knob for headless environments without DISPLAY/WAYLAND_DISPLAY.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set browser.login_force false",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "enabled",
        label: "Unlocker Bypass Lane",
        summary: "Allow optional Bright Data Unlocker fallback when an unlocker key exists",
        help: "Only runs after a local bot wall AND only if you configured an `unlocker` key in Providers. Turn off to pause all paid bypass calls without deleting your key.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set bypass.enabled true",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "zone",
        label: "Unlocker Zone Override",
        summary: "Explicit Bright Data Web Unlocker zone name",
        help: "Optional zone override if not encoded in your `unlocker` key (`TOKEN::ZONE`).",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set bypass.zone web_unlocker1",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "max_daily",
        label: "Daily Bypass Cap",
        summary: "Maximum paid Unlocker solves per day (1..=10000)",
        help: "Hard daily spending guard: once reached, DonSeTch stops invoking Bright Data Web Unlocker until the next UTC day.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 10_000,
            step: 5,
            coarse: 50,
            unit: "/day",
        },
        cli_hint: "donsetch settings set bypass.max_daily 50",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "timeout_secs",
        label: "Bypass Solve Timeout",
        summary: "Seconds to wait for an Unlocker response (1..=3600)",
        help: "Upper timeout for a single Bright Data Web Unlocker request.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 3600,
            step: 10,
            coarse: 60,
            unit: "s",
        },
        cli_hint: "donsetch settings set bypass.timeout_secs 120",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "render",
        label: "Unlocker JS Rendering",
        summary: "Request remote browser rendering inside Unlocker solves",
        help: "Ask Bright Data Unlocker to render JS remotely on walled targets (may cost more or take longer).",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set bypass.render false",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "endpoint",
        label: "Unlocker Endpoint Override",
        summary: "Custom Unlocker API endpoint URL (leave blank for production)",
        help: "Overrides Bright Data's default API endpoint for testing or custom gateways.",
        control: ControlKind::Url,
        cli_hint: "donsetch settings set bypass.endpoint \"\"",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "cache",
        label: "Cache Bypass Solutions",
        summary: "Reuse recent Unlocker HTML replies locally to save credits",
        help: "Persists successful Unlocker results on disk so repeat fetches do not pay twice.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set bypass.cache true",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "cache_ttl_secs",
        label: "Bypass Cache TTL",
        summary: "Seconds to retain cached Unlocker solves (1..=86400)",
        help: "How long a cached Unlocker solve stays valid before expiring.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 86_400,
            step: 600,
            coarse: 3600,
            unit: "s",
        },
        cli_hint: "donsetch settings set bypass.cache_ttl_secs 21600",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "Bright Data Web Unlocker (BYOK)",
        section: "bypass",
        key: "cache_max_entries",
        label: "Bypass Cache Size",
        summary: "Maximum cached Unlocker entries on disk (1..=100000)",
        help: "Oldest entries are pruned automatically once the bypass cache exceeds this count.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 100_000,
            step: 25,
            coarse: 200,
            unit: "items",
        },
        cli_hint: "donsetch settings set bypass.cache_max_entries 200",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "TLS & Client Persona",
        section: "tls",
        key: "cert_file",
        label: "Extra CA Cert File",
        summary: "PEM bundle added on top of OS trust roots (corporate proxies)",
        help: "Additive CA certificate bundle file honored across fetch, search, BYOK, OCR downloads, and updates.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set tls.cert_file /path/to/corp-ca.pem",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "TLS & Client Persona",
        section: "tls",
        key: "cert_dir",
        label: "Extra CA Cert Directory",
        summary: "Directory of extra PEM CA certificates (additive)",
        help: "All parseable certificates inside this directory are added to outbound TLS trust stores.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set tls.cert_dir /etc/ssl/corp-certs",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "TLS & Client Persona",
        section: "tls",
        key: "mldsa_sigalgs",
        label: "ML-DSA TLS SigAlgs",
        summary: "Advertise ML-DSA 44/65/87 in ClientHello (Chrome parity)",
        help: "Matches modern Chrome post-quantum TLS signature algorithm lists. Disable only if a broken middlebox rejects ML-DSA handshakes.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set tls.mldsa_sigalgs true",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "TLS & Client Persona",
        section: "persona",
        key: "timezone",
        label: "Persona Timezone",
        summary: "IANA timezone override for client persona (blank = ambient TZ)",
        help: "Example: `America/New_York` or `UTC`. Leave blank to follow the host environment.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set persona.timezone UTC",
    },
    SettingSpec {
        tab: TabId::Browser,
        group: "TLS & Client Persona",
        section: "persona",
        key: "locale",
        label: "Persona Locale",
        summary: "Accept-Language / locale override (blank = ambient LANG)",
        help: "Example: `en-US`. Leave blank to match the host locale.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set persona.locale en-US",
    },
    // ── Server, MCP, State & CLI ─────────────────────────────────────────────
    SettingSpec {
        tab: TabId::Server,
        group: "MCP & HTTP Transport",
        section: "transport",
        key: "kind",
        label: "Daemon Transport",
        summary: "MCP server mode: stdio (default) or Streamable HTTP",
        help: "Select `stdio` for local MCP clients (Claude Code, OpenCode, Cursor, Hermes) or `http` to serve Streamable HTTP on `/mcp`.",
        control: ControlKind::Enum(&["stdio", "http"]),
        cli_hint: "donsetch settings set transport.kind http",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP & HTTP Transport",
        section: "transport",
        key: "host",
        label: "HTTP Bind Address",
        summary: "Interface for `donsetch mcp --http` (default 127.0.0.1)",
        help: "Keep `127.0.0.1` for local use. Always set a Bearer token below before binding `0.0.0.0`.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set transport.host 127.0.0.1",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP & HTTP Transport",
        section: "transport",
        key: "port",
        label: "HTTP Listen Port",
        summary: "TCP port for `donsetch mcp --http` (1..=65535, default 8765)",
        help: "Port served by Streamable HTTP MCP (`/mcp` and `/health`).",
        control: ControlKind::SliderInt {
            min: 1,
            max: 65_535,
            step: 1,
            coarse: 100,
            unit: "",
        },
        cli_hint: "donsetch settings set transport.port 8765",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP & HTTP Transport",
        section: "transport",
        key: "token",
        label: "HTTP Bearer Token",
        summary: "Bearer auth secret for `/mcp` (blank = auth off; visible ASCII only)",
        help: "When set, HTTP MCP clients must send `Authorization: Bearer <token>`. Required before enabling CORS or remote access.",
        control: ControlKind::Secret,
        cli_hint: "donsetch settings set transport.token <secret>",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP & HTTP Transport",
        section: "transport",
        key: "timeout_secs",
        label: "HTTP Idle Timeout",
        summary: "Per-request HTTP MCP timeout in seconds (1..=3600)",
        help: "Upper lifetime for an individual HTTP MCP call.",
        control: ControlKind::SliderInt {
            min: 1,
            max: 3600,
            step: 15,
            coarse: 60,
            unit: "s",
        },
        cli_hint: "donsetch settings set transport.timeout_secs 300",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP & HTTP Transport",
        section: "transport",
        key: "cors",
        label: "HTTP Permissive CORS",
        summary: "Send CORS headers for browser-based HTTP MCP clients (requires token)",
        help: "Enables cross-origin access on `/mcp` including `mcp-session-id`. Requires a non-empty HTTP Bearer token.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set transport.cors true",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP Output Format",
        section: "mcp",
        key: "text_only",
        label: "Fold [meta] Into Text",
        summary: "Emit leading `[meta] {JSON}` in text block for universal client support",
        help: "Default on so MCP clients that ignore structuredContent still see full receipts. Turn off for split structuredContent-only mode.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set mcp.text_only true",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "MCP Output Format",
        section: "mcp",
        key: "url_handles",
        label: "Compact URL Handles",
        summary: "Emit compact `[S1]`/`[L1]` handles in large tool outputs",
        help: "Cuts model token usage by replacing long repeated URLs with session-local short handles.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set mcp.url_handles true",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "State, Paths & CLI",
        section: "state",
        key: "route_memory",
        label: "Stealth Route Memory",
        summary: "Per-domain learned identity & replay store (read_write | read_only | off)",
        help: "`read_write` learns fast routes and replays; `read_only` uses existing state without updating disk; `off` starts every request cold.",
        control: ControlKind::Enum(&["read_write", "read_only", "off"]),
        cli_hint: "donsetch settings set state.route_memory read_write",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "State, Paths & CLI",
        section: "state",
        key: "no_disk_state",
        label: "Memory-Only State",
        summary: "Keep all learned state strictly in RAM (never write state files)",
        help: "Useful for ephemeral containers or tests where cache writes should never persist.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set state.no_disk_state false",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "State, Paths & CLI",
        section: "paths",
        key: "cache_dir",
        label: "Cache & State Directory",
        summary: "Override default cache root (DONSETCH_CACHE_DIR env still wins)",
        help: "Fallback cache/state directory when `DONSETCH_CACHE_DIR` is not exported in the environment.",
        control: ControlKind::Text,
        cli_hint: "donsetch settings set paths.cache_dir /path/to/cache",
    },
    SettingSpec {
        tab: TabId::Server,
        group: "State, Paths & CLI",
        section: "cli",
        key: "color",
        label: "CLI Color Mode",
        summary: "Terminal color output: auto | always | never",
        help: "`auto` detects TrueColor/256/16-color on TTYs and strips escapes on pipes or NO_COLOR.",
        control: ControlKind::Enum(&["auto", "always", "never"]),
        cli_hint: "donsetch settings set cli.color auto",
    },
    // ── Rules ────────────────────────────────────────────────────────────────
    SettingSpec {
        tab: TabId::Rules,
        group: "Policy Engine",
        section: "rules",
        key: "mode",
        label: "Local Rules Mode",
        summary: "Enforce per-host deny/allow rules (`enforce` | `off`)",
        help: "Master switch for `[rules.url]` policies across fetch, search, crawl, and screenshot. Switch to `off` to temporarily bypass all local host rules.",
        control: ControlKind::Enum(&["enforce", "off"]),
        cli_hint: "donsetch settings set rules.mode enforce",
    },
    SettingSpec {
        tab: TabId::Rules,
        group: "Policy Engine",
        section: "rules",
        key: "crawl_denied_urls_per_rule",
        label: "Crawl Denied URL Cap",
        summary: "Max denied URLs listed per rule in crawl output (0..=200)",
        help: "Limits how many excluded URLs each rule prints in a crawl's denied summary. 0 hides the URL list while keeping counts.",
        control: ControlKind::SliderInt {
            min: 0,
            max: 200,
            step: 5,
            coarse: 20,
            unit: "URLs",
        },
        cli_hint: "donsetch settings set rules.crawl_denied_urls_per_rule 20",
    },
    // ── Debug (shown in Presets & Diagnostics tab) ───────────────────────────
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "search",
        label: "Debug: Search Trace",
        summary: "Log per-engine search lanes, BYOK walk & ranking details to stderr",
        help: "Helpful when diagnosing slow links, engine timeouts, or BYOK fallback chains.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.search true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "ghost",
        label: "Debug: Fetch / Transport",
        summary: "Log fetch identity, route selection & transport steps to stderr",
        help: "Verbose trace of fetch identities, wire hops, and wall classification.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.ghost true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "extract",
        label: "Debug: Extraction Trace",
        summary: "Log HTML/content extraction choices to stderr",
        help: "Diagnostics for readability scoring and boilerplate stripping.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.extract true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "pdf",
        label: "Debug: PDF Pipeline",
        summary: "Log PDF parsing, layout & OCR arbitration steps to stderr",
        help: "Traces PDF page extraction, table recovery, and OCR triggers.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.pdf true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "pdf_matrix",
        label: "Debug: PDF Matrix",
        summary: "Dump PDF transform matrix diagnostics",
        help: "Deep PDF geometry trace for diagnosing broken glyph transforms.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.pdf_matrix true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "pdf_chars",
        label: "Debug: PDF Chars",
        summary: "Dump raw PDF character stream",
        help: "Verbose per-character PDF dump for encoding bug reports.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.pdf_chars true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "pdf_words",
        label: "Debug: PDF Words",
        summary: "Dump reconstructed PDF words and bounding boxes",
        help: "Verbose per-word PDF layout dump for column/table debugging.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.pdf_words true",
    },
    SettingSpec {
        tab: TabId::Presets,
        group: "Diagnostic Traces",
        section: "debug",
        key: "echo_scorecard",
        label: "Debug: Echo Scorecard",
        summary: "Emit wire-parity echo scorecard diagnostics",
        help: "Used during TLS/HTTP fingerprint parity verification.",
        control: ControlKind::Toggle,
        cli_hint: "donsetch settings set debug.echo_scorecard true",
    },
];

/// Look up a setting specification by `section.key` (case-insensitive).
pub fn find_setting(section_dot_key: &str) -> Option<&'static SettingSpec> {
    let (section, key) = section_dot_key.trim().split_once('.')?;
    SETTINGS.iter().find(|s| {
        s.section.eq_ignore_ascii_case(section.trim()) && s.key.eq_ignore_ascii_case(key.trim())
    })
}

/// Read the typed value of a setting from `DonsetchConfig`.
pub fn get_value(cfg: &DonsetchConfig, spec: &SettingSpec) -> TomlEditValue {
    match (spec.section, spec.key) {
        // transport
        ("transport", "kind") => TomlEditValue::Str(match cfg.transport.kind {
            TransportKind::Stdio => "stdio".into(),
            TransportKind::Http => "http".into(),
        }),
        ("transport", "host") => TomlEditValue::Str(cfg.transport.host.clone()),
        ("transport", "port") => TomlEditValue::Int(i64::from(cfg.transport.port)),
        ("transport", "token") => TomlEditValue::Str(cfg.transport.token.clone()),
        ("transport", "timeout_secs") => TomlEditValue::Int(cfg.transport.timeout_secs as i64),
        ("transport", "cors") => TomlEditValue::Bool(cfg.transport.cors),
        // mcp
        ("mcp", "text_only") => TomlEditValue::Bool(cfg.mcp.text_only),
        ("mcp", "url_handles") => TomlEditValue::Bool(cfg.mcp.url_handles),
        // paths
        ("paths", "cache_dir") => TomlEditValue::Str(cfg.paths.cache_dir.clone()),
        // state
        ("state", "no_disk_state") => TomlEditValue::Bool(cfg.state.no_disk_state),
        ("state", "route_memory") => TomlEditValue::Str(match cfg.state.route_memory {
            RouteMemory::Off => "off".into(),
            RouteMemory::ReadWrite => "read_write".into(),
            RouteMemory::ReadOnly => "read_only".into(),
        }),
        // proxy
        ("proxy", "from_environment") => TomlEditValue::Bool(cfg.proxy.from_environment),
        ("proxy", "http") => TomlEditValue::Str(cfg.proxy.http.clone()),
        ("proxy", "https") => TomlEditValue::Str(cfg.proxy.https.clone()),
        ("proxy", "all") => TomlEditValue::Str(cfg.proxy.all.clone()),
        ("proxy", "no_proxy") => TomlEditValue::Str(cfg.proxy.no_proxy.clone()),
        ("proxy", "pool") => TomlEditValue::List(cfg.proxy.pool.clone()),
        ("proxy", "egress_persist") => TomlEditValue::Bool(cfg.proxy.egress_persist),
        ("proxy", "fetch_rotate") => TomlEditValue::Bool(cfg.proxy.fetch_rotate),
        ("proxy", "crawl_rotate") => TomlEditValue::Bool(cfg.proxy.crawl_rotate),
        // tls
        ("tls", "cert_file") => TomlEditValue::Str(cfg.tls.cert_file.clone()),
        ("tls", "cert_dir") => TomlEditValue::Str(cfg.tls.cert_dir.clone()),
        ("tls", "mldsa_sigalgs") => TomlEditValue::Bool(cfg.tls.mldsa_sigalgs),
        // persona
        ("persona", "timezone") => TomlEditValue::Str(cfg.persona.timezone.clone()),
        ("persona", "locale") => TomlEditValue::Str(cfg.persona.locale.clone()),
        // cli
        ("cli", "color") => TomlEditValue::Str(match cfg.cli.color {
            ColorMode::Auto => "auto".into(),
            ColorMode::Never => "never".into(),
            ColorMode::Always => "always".into(),
        }),
        // fetch
        ("fetch", "allow_private_egress") => TomlEditValue::Bool(cfg.fetch.allow_private_egress),
        ("fetch", "adapters") => TomlEditValue::Bool(cfg.fetch.adapters),
        ("fetch", "adapter_dump_dir") => TomlEditValue::Str(cfg.fetch.adapter_dump_dir.clone()),
        ("fetch", "crawl_shape") => TomlEditValue::Bool(cfg.fetch.crawl_shape),
        ("fetch", "prewarm") => TomlEditValue::Bool(cfg.fetch.prewarm),
        ("fetch", "pdf_max_mb") => TomlEditValue::Int(cfg.fetch.pdf_max_mb as i64),
        ("fetch", "ocr") => TomlEditValue::Bool(cfg.fetch.ocr),
        ("fetch", "ocr_max_pages") => TomlEditValue::Int(i64::from(cfg.fetch.ocr_max_pages)),
        ("fetch", "ocr_threads") => TomlEditValue::Int(i64::from(cfg.fetch.ocr_threads)),
        ("fetch", "ocr_idle_secs") => TomlEditValue::Int(cfg.fetch.ocr_idle_secs as i64),
        ("fetch", "resume_ttl_secs") => TomlEditValue::Int(cfg.fetch.resume_ttl_secs as i64),
        ("fetch", "host_pace_file") => TomlEditValue::Bool(cfg.fetch.host_pace_file),
        ("fetch", "dns_cache_ttl_secs") => TomlEditValue::Int(cfg.fetch.dns_cache_ttl_secs as i64),
        // bypass
        ("bypass", "enabled") => TomlEditValue::Bool(cfg.bypass.enabled),
        ("bypass", "zone") => TomlEditValue::Str(cfg.bypass.zone.clone()),
        ("bypass", "max_daily") => TomlEditValue::Int(i64::from(cfg.bypass.max_daily)),
        ("bypass", "timeout_secs") => TomlEditValue::Int(cfg.bypass.timeout_secs as i64),
        ("bypass", "render") => TomlEditValue::Bool(cfg.bypass.render),
        ("bypass", "endpoint") => TomlEditValue::Str(cfg.bypass.endpoint.clone()),
        ("bypass", "cache") => TomlEditValue::Bool(cfg.bypass.cache),
        ("bypass", "cache_ttl_secs") => TomlEditValue::Int(cfg.bypass.cache_ttl_secs as i64),
        ("bypass", "cache_max_entries") => {
            TomlEditValue::Int(i64::from(cfg.bypass.cache_max_entries))
        }
        // search
        ("search", "parallel_mcp") => TomlEditValue::Bool(cfg.search.parallel_mcp),
        ("search", "keenable") => TomlEditValue::Bool(cfg.search.keenable),
        ("search", "exa_mcp") => TomlEditValue::Bool(cfg.search.exa_mcp),
        ("search", "searxng_url") => TomlEditValue::Str(cfg.search.searxng_url.clone()),
        ("search", "google_profile") => TomlEditValue::Str(cfg.search.google_profile.clone()),
        ("search", "brightdata_zone") => TomlEditValue::Str(cfg.search.brightdata_zone.clone()),
        ("search", "outcome_feedback") => TomlEditValue::Bool(cfg.search.outcome_feedback),
        ("search", "request_budget") => TomlEditValue::Int(cfg.search.request_budget as i64),
        ("search", "timeout_ms") => TomlEditValue::Int(cfg.search.timeout_ms as i64),
        ("search", "sequential") => TomlEditValue::Bool(cfg.search.sequential),
        ("search", "wave_size") => TomlEditValue::Int(cfg.search.wave_size as i64),
        ("search", "tie_window") => TomlEditValue::Float(cfg.search.tie_window),
        ("search", "learned_rank") => TomlEditValue::Bool(cfg.search.learned_rank),
        ("search", "search_early") => TomlEditValue::Bool(cfg.search.search_early),
        ("search", "query_compile") => TomlEditValue::Bool(cfg.search.query_compile),
        ("search", "serp_instant") => TomlEditValue::Bool(cfg.search.serp_instant),
        ("search", "enrich") => TomlEditValue::Bool(cfg.search.enrich),
        // browser
        ("browser", "chromium_path") => TomlEditValue::Str(cfg.browser.chromium_path.clone()),
        ("browser", "cdp_url") => TomlEditValue::Str(cfg.browser.cdp_url.clone()),
        ("browser", "playwright_path") => TomlEditValue::Str(cfg.browser.playwright_path.clone()),
        ("browser", "route_probes") => TomlEditValue::Bool(cfg.browser.route_probes),
        ("browser", "probe_scan_secs") => TomlEditValue::Int(cfg.browser.probe_scan_secs as i64),
        ("browser", "probe_stale_secs") => TomlEditValue::Int(cfg.browser.probe_stale_secs as i64),
        ("browser", "login_force") => TomlEditValue::Bool(cfg.browser.login_force),
        // debug
        ("debug", "ghost") => TomlEditValue::Bool(cfg.debug.ghost),
        ("debug", "search") => TomlEditValue::Bool(cfg.debug.search),
        ("debug", "pdf") => TomlEditValue::Bool(cfg.debug.pdf),
        ("debug", "pdf_matrix") => TomlEditValue::Bool(cfg.debug.pdf_matrix),
        ("debug", "pdf_chars") => TomlEditValue::Bool(cfg.debug.pdf_chars),
        ("debug", "pdf_words") => TomlEditValue::Bool(cfg.debug.pdf_words),
        ("debug", "extract") => TomlEditValue::Bool(cfg.debug.extract),
        ("debug", "echo_scorecard") => TomlEditValue::Bool(cfg.debug.echo_scorecard),
        // rules
        ("rules", "mode") => TomlEditValue::Str(cfg.rules.mode.as_str().into()),
        ("rules", "crawl_denied_urls_per_rule") => {
            TomlEditValue::Int(i64::from(cfg.rules.crawl_denied_urls_per_rule))
        }
        _ => TomlEditValue::Str(String::new()),
    }
}

/// Apply one typed value onto an in-memory `DonsetchConfig` (for live preview
/// inside the TUI before persisting to disk).
pub fn set_in_memory(
    cfg: &mut DonsetchConfig,
    spec: &SettingSpec,
    val: &TomlEditValue,
) -> Result<(), String> {
    let as_bool = || match val {
        TomlEditValue::Bool(b) => Ok(*b),
        _ => Err(format!("{} expects a boolean", spec.full_key())),
    };
    let as_int = || match val {
        TomlEditValue::Int(n) => Ok(*n),
        _ => Err(format!("{} expects an integer", spec.full_key())),
    };
    let as_float = || match val {
        TomlEditValue::Float(f) => Ok(*f),
        TomlEditValue::Int(n) => Ok(*n as f64),
        _ => Err(format!("{} expects a number", spec.full_key())),
    };
    let as_str = || match val {
        TomlEditValue::Str(s) => Ok(s.clone()),
        _ => Err(format!("{} expects a string", spec.full_key())),
    };
    let as_list = || match val {
        TomlEditValue::List(v) => Ok(v.clone()),
        TomlEditValue::Str(s) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()),
        _ => Err(format!("{} expects a list", spec.full_key())),
    };

    if let ControlKind::SliderInt { min, max, .. } = spec.control {
        let n = as_int()?;
        if !(min..=max).contains(&n) {
            return Err(format!("{} must be in {min}..={max}", spec.full_key()));
        }
    }
    if let ControlKind::SliderFloat { min, max, .. } = spec.control {
        let f = as_float()?;
        if !f.is_finite() || !(min..=max).contains(&f) {
            return Err(format!("{} must be in {min}..={max}", spec.full_key()));
        }
    }

    match (spec.section, spec.key) {
        ("transport", "kind") => {
            cfg.transport.kind = match as_str()?.as_str() {
                "stdio" => TransportKind::Stdio,
                "http" => TransportKind::Http,
                other => {
                    return Err(format!(
                        "transport.kind: expected stdio or http (got {other:?})"
                    ));
                }
            };
        }
        ("transport", "host") => cfg.transport.host = as_str()?,
        ("transport", "port") => cfg.transport.port = as_int()? as u16,
        ("transport", "token") => {
            let tok = as_str()?;
            if !tok.is_empty() && !tok.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(
                    "transport.token must be empty or visible ASCII without whitespace".into(),
                );
            }
            cfg.transport.token = tok;
        }
        ("transport", "timeout_secs") => cfg.transport.timeout_secs = as_int()? as u64,
        ("transport", "cors") => cfg.transport.cors = as_bool()?,
        ("mcp", "text_only") => cfg.mcp.text_only = as_bool()?,
        ("mcp", "url_handles") => cfg.mcp.url_handles = as_bool()?,
        ("paths", "cache_dir") => cfg.paths.cache_dir = as_str()?,
        ("state", "no_disk_state") => cfg.state.no_disk_state = as_bool()?,
        ("state", "route_memory") => {
            cfg.state.route_memory = match as_str()?.as_str() {
                "off" => RouteMemory::Off,
                "read_write" | "readwrite" => RouteMemory::ReadWrite,
                "read_only" | "readonly" => RouteMemory::ReadOnly,
                other => {
                    return Err(format!(
                        "state.route_memory: expected read_write, read_only, or off (got {other:?})"
                    ));
                }
            };
        }
        ("proxy", "from_environment") => cfg.proxy.from_environment = as_bool()?,
        ("proxy", "http") => cfg.proxy.http = as_str()?,
        ("proxy", "https") => cfg.proxy.https = as_str()?,
        ("proxy", "all") => cfg.proxy.all = as_str()?,
        ("proxy", "no_proxy") => cfg.proxy.no_proxy = as_str()?,
        ("proxy", "pool") => cfg.proxy.pool = as_list()?,
        ("proxy", "egress_persist") => cfg.proxy.egress_persist = as_bool()?,
        ("proxy", "fetch_rotate") => cfg.proxy.fetch_rotate = as_bool()?,
        ("proxy", "crawl_rotate") => cfg.proxy.crawl_rotate = as_bool()?,
        ("tls", "cert_file") => cfg.tls.cert_file = as_str()?,
        ("tls", "cert_dir") => cfg.tls.cert_dir = as_str()?,
        ("tls", "mldsa_sigalgs") => cfg.tls.mldsa_sigalgs = as_bool()?,
        ("persona", "timezone") => cfg.persona.timezone = as_str()?,
        ("persona", "locale") => cfg.persona.locale = as_str()?,
        ("cli", "color") => {
            cfg.cli.color = match as_str()?.as_str() {
                "auto" => ColorMode::Auto,
                "always" => ColorMode::Always,
                "never" => ColorMode::Never,
                other => {
                    return Err(format!(
                        "cli.color: expected auto, always, or never (got {other:?})"
                    ));
                }
            };
        }
        ("fetch", "allow_private_egress") => cfg.fetch.allow_private_egress = as_bool()?,
        ("fetch", "adapters") => cfg.fetch.adapters = as_bool()?,
        ("fetch", "adapter_dump_dir") => cfg.fetch.adapter_dump_dir = as_str()?,
        ("fetch", "crawl_shape") => cfg.fetch.crawl_shape = as_bool()?,
        ("fetch", "prewarm") => cfg.fetch.prewarm = as_bool()?,
        ("fetch", "pdf_max_mb") => cfg.fetch.pdf_max_mb = as_int()? as usize,
        ("fetch", "ocr") => cfg.fetch.ocr = as_bool()?,
        ("fetch", "ocr_max_pages") => cfg.fetch.ocr_max_pages = as_int()? as u32,
        ("fetch", "ocr_threads") => cfg.fetch.ocr_threads = as_int()? as u32,
        ("fetch", "ocr_idle_secs") => cfg.fetch.ocr_idle_secs = as_int()? as u64,
        ("fetch", "resume_ttl_secs") => cfg.fetch.resume_ttl_secs = as_int()? as u64,
        ("fetch", "host_pace_file") => cfg.fetch.host_pace_file = as_bool()?,
        ("fetch", "dns_cache_ttl_secs") => cfg.fetch.dns_cache_ttl_secs = as_int()? as u64,
        ("bypass", "enabled") => cfg.bypass.enabled = as_bool()?,
        ("bypass", "zone") => cfg.bypass.zone = as_str()?,
        ("bypass", "max_daily") => cfg.bypass.max_daily = as_int()? as u32,
        ("bypass", "timeout_secs") => cfg.bypass.timeout_secs = as_int()? as u64,
        ("bypass", "render") => cfg.bypass.render = as_bool()?,
        ("bypass", "endpoint") => cfg.bypass.endpoint = as_str()?,
        ("bypass", "cache") => cfg.bypass.cache = as_bool()?,
        ("bypass", "cache_ttl_secs") => cfg.bypass.cache_ttl_secs = as_int()? as u64,
        ("bypass", "cache_max_entries") => cfg.bypass.cache_max_entries = as_int()? as u32,
        ("search", "parallel_mcp") => cfg.search.parallel_mcp = as_bool()?,
        ("search", "keenable") => cfg.search.keenable = as_bool()?,
        ("search", "exa_mcp") => cfg.search.exa_mcp = as_bool()?,
        ("search", "searxng_url") => {
            let raw = as_str()?;
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let parsed = url::Url::parse(trimmed).map_err(|_| {
                    "search.searxng_url must be a valid http:// or https:// URL".to_string()
                })?;
                if !matches!(parsed.scheme(), "http" | "https") || !parsed.has_host() {
                    return Err(
                        "search.searxng_url must use http:// or https:// with a host".into(),
                    );
                }
            }
            cfg.search.searxng_url = trimmed.to_string();
        }
        ("search", "google_profile") => cfg.search.google_profile = as_str()?,
        ("search", "brightdata_zone") => cfg.search.brightdata_zone = as_str()?,
        ("search", "outcome_feedback") => cfg.search.outcome_feedback = as_bool()?,
        ("search", "request_budget") => cfg.search.request_budget = as_int()? as usize,
        ("search", "timeout_ms") => cfg.search.timeout_ms = as_int()? as u64,
        ("search", "sequential") => cfg.search.sequential = as_bool()?,
        ("search", "wave_size") => cfg.search.wave_size = as_int()? as usize,
        ("search", "tie_window") => cfg.search.tie_window = as_float()?,
        ("search", "learned_rank") => cfg.search.learned_rank = as_bool()?,
        ("search", "search_early") => cfg.search.search_early = as_bool()?,
        ("search", "query_compile") => cfg.search.query_compile = as_bool()?,
        ("search", "serp_instant") => cfg.search.serp_instant = as_bool()?,
        ("search", "enrich") => cfg.search.enrich = as_bool()?,
        ("browser", "chromium_path") => cfg.browser.chromium_path = as_str()?,
        ("browser", "cdp_url") => {
            let raw = as_str()?;
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let parsed = url::Url::parse(trimmed).map_err(|_| {
                    "browser.cdp_url must be a valid http(s):// or ws(s):// URL".to_string()
                })?;
                if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") || !parsed.has_host()
                {
                    return Err(
                        "browser.cdp_url must use http(s):// or ws(s):// with a host".into(),
                    );
                }
            }
            cfg.browser.cdp_url = trimmed.to_string();
        }
        ("browser", "playwright_path") => cfg.browser.playwright_path = as_str()?,
        ("browser", "route_probes") => cfg.browser.route_probes = as_bool()?,
        ("browser", "probe_scan_secs") => cfg.browser.probe_scan_secs = as_int()? as u64,
        ("browser", "probe_stale_secs") => cfg.browser.probe_stale_secs = as_int()? as u64,
        ("browser", "login_force") => cfg.browser.login_force = as_bool()?,
        ("debug", "ghost") => cfg.debug.ghost = as_bool()?,
        ("debug", "search") => cfg.debug.search = as_bool()?,
        ("debug", "pdf") => cfg.debug.pdf = as_bool()?,
        ("debug", "pdf_matrix") => cfg.debug.pdf_matrix = as_bool()?,
        ("debug", "pdf_chars") => cfg.debug.pdf_chars = as_bool()?,
        ("debug", "pdf_words") => cfg.debug.pdf_words = as_bool()?,
        ("debug", "extract") => cfg.debug.extract = as_bool()?,
        ("debug", "echo_scorecard") => cfg.debug.echo_scorecard = as_bool()?,
        ("rules", "mode") => {
            cfg.rules.mode = match as_str()?.as_str() {
                "enforce" => RulesMode::Enforce,
                "off" => RulesMode::Off,
                other => {
                    return Err(format!(
                        "rules.mode: expected enforce or off (got {other:?})"
                    ));
                }
            };
        }
        ("rules", "crawl_denied_urls_per_rule") => {
            cfg.rules.crawl_denied_urls_per_rule = as_int()? as u32;
        }
        _ => return Err(format!("unknown setting {}", spec.full_key())),
    }
    Ok(())
}

/// Parse a user/CLI string into the strongly-typed [`TomlEditValue`] expected
/// by `spec`.
pub fn parse_value(spec: &SettingSpec, raw: &str) -> Result<TomlEditValue, String> {
    let trimmed = raw.trim();
    match spec.control {
        ControlKind::Toggle => {
            for on in ["1", "true", "on", "yes", "y", "enable", "enabled"] {
                if trimmed.eq_ignore_ascii_case(on) {
                    return Ok(TomlEditValue::Bool(true));
                }
            }
            for off in ["0", "false", "off", "no", "n", "disable", "disabled"] {
                if trimmed.eq_ignore_ascii_case(off) {
                    return Ok(TomlEditValue::Bool(false));
                }
            }
            Err(format!(
                "{}: expected boolean (true/false/on/off/1/0), got {raw:?}",
                spec.full_key()
            ))
        }
        ControlKind::SliderInt { min, max, .. } => {
            let n = trimmed.parse::<i64>().map_err(|_| {
                format!(
                    "{}: expected integer in {min}..={max}, got {raw:?}",
                    spec.full_key()
                )
            })?;
            if !(min..=max).contains(&n) {
                return Err(format!(
                    "{}: value {n} out of range {min}..={max}",
                    spec.full_key()
                ));
            }
            Ok(TomlEditValue::Int(n))
        }
        ControlKind::SliderFloat { min, max, .. } => {
            let f = trimmed.parse::<f64>().map_err(|_| {
                format!(
                    "{}: expected number in {min}..={max}, got {raw:?}",
                    spec.full_key()
                )
            })?;
            if !f.is_finite() || !(min..=max).contains(&f) {
                return Err(format!(
                    "{}: value {f} out of range {min}..={max}",
                    spec.full_key()
                ));
            }
            Ok(TomlEditValue::Float((f * 1000.0).round() / 1000.0))
        }
        ControlKind::Enum(choices) => {
            let lower = trimmed.to_ascii_lowercase();
            // Accept legacy spelling aliases cleanly.
            let normalized = if spec.section == "state" && spec.key == "route_memory" {
                match lower.as_str() {
                    "readwrite" => "read_write",
                    "readonly" => "read_only",
                    other => other,
                }
            } else {
                lower.as_str()
            };
            if let Some(&matched) = choices.iter().find(|&&c| c == normalized) {
                Ok(TomlEditValue::Str(matched.to_string()))
            } else {
                Err(format!(
                    "{}: expected one of [{}], got {raw:?}",
                    spec.full_key(),
                    choices.join(", ")
                ))
            }
        }
        ControlKind::Text | ControlKind::Secret => Ok(TomlEditValue::Str(raw.to_string())),
        ControlKind::Url => Ok(TomlEditValue::Str(trimmed.to_string())),
        ControlKind::List => Ok(TomlEditValue::List(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )),
    }
}

/// Format a value for safe human display (masking tokens and URL credentials).
pub fn display_value(spec: &SettingSpec, val: &TomlEditValue) -> String {
    match val {
        TomlEditValue::Bool(b) => {
            if *b {
                "on".into()
            } else {
                "off".into()
            }
        }
        TomlEditValue::Int(n) => match spec.control {
            ControlKind::SliderInt { unit, .. } if !unit.is_empty() => {
                if spec.section == "fetch" && spec.key == "ocr_threads" && *n == 0 {
                    "0 (auto)".into()
                } else if spec.section == "fetch"
                    && matches!(spec.key, "ocr_idle_secs" | "dns_cache_ttl_secs")
                    && *n == 0
                {
                    "0 (off)".into()
                } else {
                    format!("{n} {unit}")
                }
            }
            _ => n.to_string(),
        },
        TomlEditValue::Float(f) => format!("{f:.2}"),
        TomlEditValue::Str(s) => {
            if s.is_empty() {
                "(unset)".into()
            } else if matches!(spec.control, ControlKind::Secret) {
                format!("*** ({} chars)", s.chars().count())
            } else if spec.is_secret() {
                config::mask_url_credentials(s)
            } else {
                s.clone()
            }
        }
        TomlEditValue::List(items) => {
            if items.is_empty() {
                "(empty)".into()
            } else {
                let masked: Vec<String> = items
                    .iter()
                    .map(|p| {
                        if spec.is_secret() {
                            config::mask_url_credentials(p)
                        } else {
                            p.clone()
                        }
                    })
                    .collect();
                masked.join(", ")
            }
        }
        TomlEditValue::Unset => "(default)".into(),
    }
}

/// Format a value for plain-text editing inside an input modal.
pub fn editable_text(val: &TomlEditValue) -> String {
    match val {
        TomlEditValue::Bool(b) => b.to_string(),
        TomlEditValue::Int(n) => n.to_string(),
        TomlEditValue::Float(f) => format!("{f}"),
        TomlEditValue::Str(s) => s.clone(),
        TomlEditValue::List(items) => items.join(", "),
        TomlEditValue::Unset => String::new(),
    }
}

/// Format a single setting's TOML snippet preview.
pub fn toml_preview(spec: &SettingSpec, val: &TomlEditValue) -> String {
    let shown = match val {
        TomlEditValue::Bool(b) => b.to_string(),
        TomlEditValue::Int(n) => n.to_string(),
        TomlEditValue::Float(f) => format!("{f:.2}"),
        TomlEditValue::Str(s) => {
            let safe = if matches!(spec.control, ControlKind::Secret) && !s.is_empty() {
                "***".to_string()
            } else if spec.is_secret() && !s.is_empty() {
                config::mask_url_credentials(s)
            } else {
                s.clone()
            };
            format!("\"{}\"", safe.replace('\\', "\\\\").replace('"', "\\\""))
        }
        TomlEditValue::List(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|p| {
                    let safe = if spec.is_secret() {
                        config::mask_url_credentials(p)
                    } else {
                        p.clone()
                    };
                    format!("\"{}\"", safe.replace('\\', "\\\\").replace('"', "\\\""))
                })
                .collect();
            format!("[{}]", parts.join(", "))
        }
        TomlEditValue::Unset => "# (uses default)".into(),
    };
    format!("[{}] {} = {shown}", spec.section, spec.key)
}

/// Origin label for one setting in a loaded config snapshot.
pub fn origin_label(loaded: &Loaded, spec: &SettingSpec) -> String {
    loaded
        .origin_of_path(&[spec.section, spec.key])
        .map(str::to_string)
        .unwrap_or_else(|| "default".to_string())
}

/// True when the active value comes from an environment variable rather than
/// defaults or `donsetch.toml`.
pub fn is_env_origin(origin: &str) -> bool {
    origin != "default" && !origin.starts_with("file:")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mechanical guardrail enforcing the AGENTS.md / project.md rule:
    /// every active (non-deprecated) knob in `config::fieldbook()` MUST be
    /// exposed in `donsetch settings` (`SETTINGS`).
    #[test]
    fn every_active_config_knob_is_exposed_in_settings() {
        for (section, key, _, _, _) in config::fieldbook() {
            if DEPRECATED_KNOBS.contains(&(*section, *key)) {
                continue;
            }
            assert!(
                SETTINGS
                    .iter()
                    .any(|s| s.section == *section && s.key == *key),
                "config knob `{section}.{key}` is missing from `src/cli/settings/schema.rs` (SETTINGS) — add it so `donsetch settings` stays complete"
            );
        }

        // No duplicate entries and every SETTINGS entry exists in fieldbook().
        let mut seen = std::collections::HashSet::new();
        for spec in SETTINGS {
            assert!(
                seen.insert((spec.section, spec.key)),
                "duplicate setting {}.{}",
                spec.section,
                spec.key
            );
            assert!(
                config::fieldbook()
                    .iter()
                    .any(|(s, k, _, _, _)| *s == spec.section && *k == spec.key),
                "SETTINGS entry `{}.{}` has no entry in config::fieldbook()",
                spec.section,
                spec.key
            );
        }
    }

    #[test]
    fn presets_parse_and_apply_cleanly_to_default_config() {
        for preset in PRESETS {
            let mut cfg = DonsetchConfig::default();
            for (section, key, raw) in preset.changes {
                let spec = find_setting(&format!("{section}.{key}")).unwrap_or_else(|| {
                    panic!("preset {} references unknown {section}.{key}", preset.id)
                });
                let val = parse_value(spec, raw)
                    .unwrap_or_else(|e| panic!("preset {} bad value {raw}: {e}", preset.id));
                set_in_memory(&mut cfg, spec, &val)
                    .unwrap_or_else(|e| panic!("preset {} failed apply: {e}", preset.id));
            }
        }
    }

    #[test]
    fn every_setting_roundtrips_get_and_set_on_default() {
        let def = DonsetchConfig::default();
        let mut copy = def.clone();
        for spec in SETTINGS {
            let val = get_value(&def, spec);
            set_in_memory(&mut copy, spec, &val)
                .unwrap_or_else(|e| panic!("roundtrip failed for {}: {e}", spec.full_key()));
            assert_eq!(get_value(&copy, spec), val, "{}", spec.full_key());
        }
        assert_eq!(copy, def);
    }

    #[test]
    fn secret_and_url_credentials_are_masked_in_display_and_preview() {
        let tok_spec = find_setting("transport.token").unwrap();
        let tok_val = TomlEditValue::Str("super-secret-bearer".into());
        assert!(!display_value(tok_spec, &tok_val).contains("super-secret"));
        assert!(!toml_preview(tok_spec, &tok_val).contains("super-secret"));

        let sx_spec = find_setting("search.searxng_url").unwrap();
        let sx_val = TomlEditValue::Str("https://alice:hunter2@search.example/s?token=abc".into());
        let shown = display_value(sx_spec, &sx_val);
        assert!(
            !shown.contains("hunter2") && !shown.contains("abc"),
            "{shown}"
        );
        let prev = toml_preview(sx_spec, &sx_val);
        assert!(!prev.contains("hunter2") && !prev.contains("abc"), "{prev}");
    }
}
