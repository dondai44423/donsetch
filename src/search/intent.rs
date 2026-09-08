//! Intent detection : routes the query to the right
//! engines + verticals, and feeds domain priors to rank.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Web,
    Code,
    Paper,
    News,
    Entity,
}

impl Intent {
    /// Stable u8 code for disk-backed keys (query cache). Debug strings
    /// coupled the disk format to variant names: renaming a variant
    /// silently remapped or orphaned old entries.
    pub fn code(self) -> u8 {
        match self {
            Intent::Web => 0,
            Intent::Code => 1,
            Intent::Paper => 2,
            Intent::News => 3,
            Intent::Entity => 4,
        }
    }

    /// Inverse of `code`; unknown values fall back to Web. Legacy
    /// Debug-string entries (one TTL generation) are remapped by
    /// `from_legacy_debug` at load time.
    pub fn from_code(c: u8) -> Self {
        match c {
            1 => Intent::Code,
            2 => Intent::Paper,
            3 => Intent::News,
            4 => Intent::Entity,
            _ => Intent::Web,
        }
    }
}

/// Intent is ADVISORY, never a gate: it selects bonus
/// priors and which verticals join the fan-out. Wrong
/// intent must never ruin a query : so signals split
/// into UNAMBIGUOUS (always count) and TECH-GATED (only
/// count with a tech token present). "How to fix a
/// leaking kitchen faucet" is plumbing, full stop.
const CODE_STRONG: &[&str] = &[
    "error",
    "exception",
    "traceback",
    "compile",
    "undefined",
    "null pointer",
    "segmentation fault",
    "syntax",
    "debug",
    "stack trace",
    "typeerror",
    "segfault",
    "stackoverflow",
];
const CODE_TECH_GATED: &[&str] = &[
    "how to", "install", "fix", "setup", "set up", "library", "function", "api", "crate",
    "package", "regex",
];
/// Word-token matched, never substring: "vec" must not
/// light up on "vector", "git" on "digit".
const TECH: &[&str] = &[
    "npm",
    "cargo",
    "pip",
    "github",
    "docker",
    "kubernetes",
    "rust",
    "python",
    "javascript",
    "typescript",
    "golang",
    "c++",
    "java",
    "ruby",
    "php",
    "swift",
    "kotlin",
    "linux",
    "git",
    "sql",
    "regex",
    "sdk",
    "cli",
    "json",
    "yaml",
    "api",
    "code",
    "script",
    "function",
    "library",
    "crate",
    "package",
    "compiler",
    "kernel",
    "async",
    "node",
    "react",
    "vue",
    "django",
    "flask",
    "postgres",
    "mysql",
    "redis",
    "nginx",
    "bash",
    "shell",
    "terminal",
    "vec",
];
const PAPER_SIGNALS: &[&str] = &[
    "paper", "arxiv", "doi", "journal", "citation", "preprint", "ablation",
];
const NEWS_SIGNALS: &[&str] = &[
    "news",
    "breaking",
    "latest",
    "announced",
    "dies",
    "election",
    "war",
    "stock",
];
const ENTITY_SIGNALS: &[&str] = &["what is", "who is", "who was", "define", "meaning of"];

/// A conceptual query asks for an explanation of a thing,
/// not a how-to or a product : Wikipedia is authoritative
/// here, so it joins the fan-out at full engine weight.
pub fn is_conceptual(query: &str) -> bool {
    let q = query.to_lowercase();
    const CONCEPT: &[&str] = &[
        "what is",
        "who is",
        "who was",
        "explained",
        "explain",
        "how does",
        "how do ",
        "meaning",
        "concept",
        "theory",
        "mechanism",
        "history of",
        "difference between",
        " vs ",
    ];
    CONCEPT.iter().any(|s| q.contains(s))
}

/// Query tokens: alphanumeric runs, keeping '+' so "c++" survives.
fn tokens(q: &str) -> Vec<&str> {
    q.split(|c: char| !c.is_alphanumeric() && c != '+')
        .filter(|w| !w.is_empty())
        .collect()
}

/// Does `signal` (one or more words) occur as a contiguous run of
/// whole tokens? Substring matching lit "war" up on "software",
/// "paper" on "wallpaper", "stock" on "stockholm", "dies" on
/// "diesel" -- and a false News/Paper label drops two of the five
/// engines and applies the stale-date penalty. The TECH list was
/// already token-matched for exactly this reason; the other signal
/// lists were not.
fn has_phrase(toks: &[&str], signal: &str) -> bool {
    let want = tokens(signal);
    !want.is_empty() && toks.windows(want.len()).any(|w| w == want.as_slice())
}

pub fn detect(query: &str) -> Intent {
    let q = query.to_lowercase();
    let toks = tokens(&q);
    let score = |signals: &[&str]| signals.iter().filter(|s| has_phrase(&toks, s)).count();
    let tech = toks.iter().any(|w| TECH.contains(w));
    // Ambiguous utility words need tech context; strong
    // signals never do.
    // Asymmetric by design: a false Code label on
    // "python habitat" costs a few wasted vertical lanes
    // (irrelevant hits BM25-sink out of the results),
    // while a false Web label on a real code query
    // LOSES the StackExchange/MDN/GitHub verticals : a
    // true quality loss. So a tech token alone leans Code.
    let code = score(CODE_STRONG) + if tech { score(CODE_TECH_GATED) + 1 } else { 0 };
    let paper = score(PAPER_SIGNALS);
    let news = score(NEWS_SIGNALS);
    let entity = score(ENTITY_SIGNALS);
    let max = code.max(paper).max(news).max(entity);
    if max == 0 {
        // Short proper-noun-ish query → probably an entity.
        let words = query.split_whitespace().count();
        if words <= 3
            && query
                .split_whitespace()
                .filter(|w| w.chars().next().is_some_and(char::is_uppercase))
                .count()
                >= 2
        {
            return Intent::Entity;
        }
        return Intent::Web;
    }
    if code == max {
        Intent::Code
    } else if news == max {
        Intent::News
    } else if paper == max {
        Intent::Paper
    } else {
        Intent::Entity
    }
}

/// Engines to fan out per intent. Order = trust prior.
/// Bing family (bing/ddg/yahoo) + independent indexes
/// (mojeek/brave/google) for consensus diversity.
/// DDG and Brave are PROXY_AVERSE : they prefer the direct
/// lane because proxy IPs get CAPTCHA'd/429'd.
pub fn engines_for(intent: Intent) -> &'static [&'static str] {
    match intent {
        // 6 engines, 4 index families (bing, mojeek, brave, google).
        Intent::Web | Intent::Code | Intent::News | Intent::Entity => {
            &["bing", "ddg", "mojeek", "yahoo", "brave", "google"]
        }
        Intent::Paper => &["bing", "ddg", "mojeek", "google"],
    }
}

/// Verticals to fan out per intent (keyless JSON APIs).
pub fn verticals_for(intent: Intent, query: &str) -> &'static [&'static str] {
    match intent {
        // Official keyless APIs first: near-100% reliable,
        // zero egress budget spent on engines for code.
        Intent::Code => &["stackexchange", "mdn", "github", "hn"],
        Intent::Paper => &["scholar", "arxiv"],
        Intent::News => &["news", "hn"],
        Intent::Entity => &["wikipedia"],
        // Wiki safety net only on CONCEPTUAL web queries :
        // firing it everywhere flooded merges and crowded
        // out real canonicals (bench round 2 regression).
        Intent::Web if is_conceptual(query) => &["wikipedia"],
        Intent::Web => &[],
    }
}

/// Domain quality prior per intent: 0.0..1.0 bonus mass.
/// Curated seed; the consensus signal usually dominates,
/// this just breaks ties toward known-good sources.
pub fn domain_prior(intent: Intent, host: &str, query: &str) -> f64 {
    let h = host.strip_prefix("www.").unwrap_or(host);
    let table: &[&str] = match intent {
        Intent::Code => &[
            "stackoverflow.com",
            "github.com",
            "docs.rs",
            "developer.mozilla.org",
            "learn.microsoft.com",
            "doc.rust-lang.org",
            "pkg.go.dev",
            "pypi.org",
            "crates.io",
            "npmjs.com",
            "readthedocs.io",
            "superuser.com",
            "serverfault.com",
            "news.ycombinator.com",
            "git-scm.com",
        ],
        Intent::Paper => &[
            "arxiv.org",
            "semanticscholar.org",
            "scholar.google.com",
            "nature.com",
            "science.org",
            "acm.org",
            "ieee.org",
            "openreview.net",
            "pubmed.ncbi.nlm.nih.gov",
            "doi.org",
        ],
        Intent::News => &[
            "reuters.com",
            "apnews.com",
            "bbc.com",
            "bbc.co.uk",
            "nytimes.com",
            "theguardian.com",
            "arstechnica.com",
            "techcrunch.com",
            "news.ycombinator.com",
            "bloomberg.com",
            "wsj.com",
        ],
        Intent::Entity => &[
            "wikipedia.org",
            "britannica.com",
            "wikidata.org",
            "imdb.com",
        ],
        Intent::Web => &[
            // Authoritative explainers : SEO-gamed titles
            // win BM25 otherwise.
            "cloudflare.com",
            "developer.mozilla.org",
            "wikipedia.org",
            "learn.microsoft.com",
            "aws.amazon.com",
            "kubernetes.io",
            "ietf.org",
            "rfc-editor.org",
        ],
    };
    let intent_prior: f64 = if table
        .iter()
        .any(|d| h == *d || h.ends_with(&format!(".{d}")))
    {
        1.0
    } else {
        0.0
    };
    intent_prior.max(utility_prior(query, h))
}

/// Cross-intent utility prior: DIY/how-to queries get
/// their canonical cluster regardless of Web/Code label.
/// Farms keyword-stuff titles (exact-match "How to Fix a
/// Leaking Kitchen Faucet") to win BM25 + consensus :
/// measured in bench/headtohead.py: engines returned
/// thisoldhouse/wikihow but exact-match farms outranked
/// them. This bonus is the only layer that knows humans
/// trust these domains. Advisory: it can only LIFT a
/// canonical result, never hide anything.
const UTILITY_WORDS: &[&str] = &[
    "how to", "fix", "repair", "install", "replace", "clean", "build", "make", "remove",
];
const DIY: &[&str] = &[
    "ifixit.com",
    "wikihow.com",
    "thisoldhouse.com",
    "familyhandyman.com",
    "homedepot.com",
    "lowes.com",
    "thespruce.com",
    "hgtv.com",
    "instructables.com",
];

pub fn utility_prior(query: &str, host: &str) -> f64 {
    let q = query.to_lowercase();
    if !UTILITY_WORDS.iter().any(|w| q.contains(w)) {
        return 0.0;
    }
    if DIY
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    {
        1.0
    } else {
        0.0
    }
}

/// A normalized recall variant of the query (strip
/// question scaffolding). Goes only to top-trust engines.
pub fn variant(query: &str) -> Option<String> {
    let mut q = query.to_string();
    for pre in [
        "how to ",
        "how do i ",
        "how do you ",
        "what is ",
        "who is ",
        "why does ",
        "why is ",
    ] {
        if q.to_lowercase().starts_with(pre) {
            q = q[pre.len()..].to_string();
            break;
        }
    }
    let q = q.trim_end_matches(['?', '.']).trim().to_string();
    if q.len() >= 4 && q != query {
        Some(q)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Intent is advisory-only, but classification must
    /// still be right on the realistic spread : a wrong
    /// label can never be allowed to steer a query.
    #[test]
    fn intent_never_steals_real_world_queries() {
        // Non-code how-tos MUST stay Web (the faucet bug).
        assert_eq!(detect("how to fix a leaking kitchen faucet"), Intent::Web);
        assert_eq!(detect("how to install a ceiling fan"), Intent::Web);
        assert_eq!(
            detect("how to fix wifi router keeps disconnecting"),
            Intent::Web
        );
        assert_eq!(detect("how to repair drywall hole"), Intent::Web);
        assert_eq!(detect("best budget mechanical keyboard 2026"), Intent::Web);
        // Real code queries MUST stay Code.
        assert_eq!(detect("rust vec vs hashmap performance"), Intent::Code);
        assert_eq!(detect("python asyncio gather vs wait"), Intent::Code);
        assert_eq!(detect("how to parse json in javascript"), Intent::Code);
        assert_eq!(detect("git rebase onto explained"), Intent::Code);
        assert_eq!(detect("cannot find module error node"), Intent::Code);
        // Concepts.
        assert_eq!(detect("nash equilibrium explained"), Intent::Web);
        assert_eq!(detect("how does japanese pitch accent work"), Intent::Web);
        // News / paper stay theirs.
        assert_eq!(detect("ukraine war latest news"), Intent::News);
        assert_eq!(
            detect("attention is all you need transformer paper"),
            Intent::Paper
        );
        assert_eq!(
            detect("retrieval augmented generation paper"),
            Intent::Paper
        );
    }

    // Signals used to be matched as raw substrings, so "software"
    // lit up on "war", "wallpaper" on "paper", "stockholm" on
    // "stock", "diesel" on "dies" -- and the News/Paper label
    // dropped two of the five engines and applied the stale-date
    // penalty to a query about socks. Only whole tokens count.
    #[test]
    fn signals_match_whole_words_not_substrings() {
        for q in [
            "software architecture patterns",
            "hardware wallet comparison",
            "warm socks for winter",
            "stockholm travel guide",
            "diesel engine maintenance",
            "feature selection algorithm",
            "how to change wallpaper on android",
            "wallpaper ideas living room",
            "case studies in urban planning",
        ] {
            assert_eq!(detect(q), Intent::Web, "{q}");
        }
        // The real signals still fire as whole words / phrases.
        assert_eq!(detect("breaking news ukraine war"), Intent::News);
        assert_eq!(detect("stock market crash today"), Intent::News);
        assert_eq!(detect("arxiv paper on attention"), Intent::Paper);
        assert_eq!(detect("null pointer in c"), Intent::Code);
        assert_eq!(detect("what is a monad"), Intent::Entity);
        // A multi-word phrase must be contiguous tokens, not a
        // substring across a word boundary ("show tools" is not
        // "how to").
        assert_ne!(detect("show tools for woodworking"), Intent::Code);
    }
}
