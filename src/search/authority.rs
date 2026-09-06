//! authority.rs : the decisive top-placement layer.
//!
//! History: v1 ranking had excellent top-5 recall (23/25 in the
//! 50-case benchmark) but weak top-1/top-3 placement (6/25 top-1
//! vs hound's 13/25). The recall machinery : cross-engine
//! consensus, family-deduped RRF, BM25, coverage penalties :
//! reliably puts the RIGHT result IN the list, but nothing made
//! a decisive #1 choice: the domain prior was a flat +0.15 on
//! the wrong scale, and the cross-encoder blend (60/40, min-max
//! normalized) destroys absolute score information by design.
//!
//! This module is the layer that knows what a human would click
//! FIRST. It runs AFTER rerank and coverage (so it rides on the
//! final blended score) and multiplies in three signals:
//!
//! 1. **Query-aware official domains.** A "rust ownership" query
//!    maps to rust-lang.org/docs.rs : the OFFICIAL sources for
//!    the thing the query names : via a curated registry of
//!    tech-token → official-domain mappings. Generic
//!    (non-query-aware) authority tables from `intent::domain_prior`
//!    get a smaller lift.
//! 2. **Title decisiveness.** The fraction of query entity terms
//!    present in the title, plus an exact-phrase bonus. This is
//!    the classic SERP placement signal; it was diluted through
//!    the 60/40 blend and is restored here as a placement signal.
//! 3. **Freshness (News intent).** `published` timestamps (dead
//!    data in v1) now rank fresh wire results up and stale ones
//!    down.
//!
//! All boosts are MULTIPLICATIVE, which self-limits: a garbage
//! result scoring near zero stays near zero no matter how
//! official its domain. Only a result that consensus + semantics
//! already placed mid-pack can be lifted to the top.

use super::intent::{self, Intent};
use super::rank::{Merged, host_of};

/// Query-aware official sources: (query tokens → official
/// domains). ALL tokens in an entry must appear in the query.
/// Single common-English tokens that name a tech only when
/// combined (next + js, spring + boot, go + concurrency) are
/// encoded as multi-token entries so they can't fire alone.
const OFFICIAL: &[(&[&str], &[&str])] = &[
    // ── Languages & runtimes ──
    (
        &["rust"],
        &["rust-lang.org", "doc.rust-lang.org", "docs.rs", "crates.io"],
    ),
    (&["python"], &["docs.python.org", "pypi.org"]),
    (&["javascript"], &["developer.mozilla.org"]),
    (&["typescript"], &["typescriptlang.org"]),
    (&["nodejs"], &["nodejs.org"]),
    (&["node", "js"], &["nodejs.org"]),
    (&["node", "javascript"], &["nodejs.org"]),
    (&["golang"], &["go.dev"]),
    (&["go", "goroutine"], &["go.dev"]),
    (&["go", "concurrency"], &["go.dev"]),
    (&["go", "channel"], &["go.dev"]),
    (&["java"], &["docs.oracle.com", "dev.java"]),
    (&["kotlin"], &["kotlinlang.org"]),
    (&["swift"], &["swift.org", "developer.apple.com"]),
    (&["ruby"], &["ruby-lang.org", "ruby-doc.org"]),
    (&["php"], &["php.net"]),
    (&["cpp"], &["cppreference.com"]),
    (
        &["dotnet"],
        &["learn.microsoft.com", "dotnet.microsoft.com"],
    ),
    (&["csharp"], &["learn.microsoft.com"]),
    (&["dotnet", "c#"], &["learn.microsoft.com"]),
    (&["dart"], &["dart.dev"]),
    (&["zig"], &["ziglang.org"]),
    (&["elixir"], &["elixir-lang.org"]),
    (&["haskell"], &["haskell.org"]),
    (&["scala"], &["scala-lang.org"]),
    (&["julia"], &["docs.julialang.org"]),
    (&["lua"], &["lua.org"]),
    // ── Frameworks & libraries ──
    (&["react"], &["react.dev"]),
    (&["react", "hooks"], &["react.dev"]),
    (&["vue"], &["vuejs.org"]),
    (&["angular"], &["angular.dev"]),
    (&["svelte"], &["svelte.dev"]),
    (&["nextjs"], &["nextjs.org"]),
    (&["next", "js"], &["nextjs.org"]),
    (&["nuxt"], &["nuxt.com"]),
    (&["django"], &["docs.djangoproject.com"]),
    (&["flask"], &["flask.palletsprojects.com"]),
    (&["fastapi"], &["fastapi.tiangolo.com"]),
    (&["rails"], &["guides.rubyonrails.org"]),
    (&["ruby", "on", "rails"], &["guides.rubyonrails.org"]),
    (&["spring", "boot"], &["spring.io"]),
    (&["spring", "framework"], &["spring.io"]),
    (&["spring", "java"], &["spring.io"]),
    (&["laravel"], &["laravel.com"]),
    (&["flutter"], &["flutter.dev", "api.flutter.dev"]),
    (&["react", "native"], &["reactnative.dev"]),
    (&["tailwind"], &["tailwindcss.com"]),
    (&["bootstrap"], &["getbootstrap.com"]),
    (&["vite"], &["vitejs.dev"]),
    (&["webpack"], &["webpack.js.org"]),
    (&["expressjs"], &["expressjs.com"]),
    (&["express", "js"], &["expressjs.com"]),
    (&["express", "node"], &["expressjs.com"]),
    (&["tokio"], &["tokio.rs"]),
    (&["serde"], &["serde.rs"]),
    (&["jquery"], &["jquery.com"]),
    (&["htmx"], &["htmx.org"]),
    // ── Data stores & messaging ──
    (&["postgresql"], &["postgresql.org"]),
    (&["postgres"], &["postgresql.org"]),
    (&["mysql"], &["dev.mysql.com", "mysql.com"]),
    (&["sqlite"], &["sqlite.org"]),
    (&["mongodb"], &["mongodb.com"]),
    (&["redis"], &["redis.io"]),
    (&["kafka"], &["kafka.apache.org"]),
    (&["elasticsearch"], &["elastic.co"]),
    (&["rabbitmq"], &["rabbitmq.com"]),
    (&["clickhouse"], &["clickhouse.com"]),
    // ── Infra & DevOps ──
    (&["docker"], &["docs.docker.com"]),
    (&["kubernetes"], &["kubernetes.io"]),
    (&["k8s"], &["kubernetes.io"]),
    (&["terraform"], &["developer.hashicorp.com", "terraform.io"]),
    (&["ansible"], &["docs.ansible.com"]),
    (&["nginx"], &["nginx.org"]),
    (&["apache"], &["httpd.apache.org"]),
    (&["caddy"], &["caddyserver.com"]),
    (&["traefik"], &["doc.traefik.io"]),
    (&["haproxy"], &["haproxy.org"]),
    (&["prometheus"], &["prometheus.io"]),
    (&["grafana"], &["grafana.com"]),
    (&["jenkins"], &["jenkins.io"]),
    (&["gitlab"], &["docs.gitlab.com"]),
    (&["systemd"], &["systemd.io"]),
    (&["nginx", "ingress"], &["kubernetes.io", "docs.docker.com"]),
    // ── Cloud & platforms ──
    (&["aws"], &["aws.amazon.com", "docs.aws.amazon.com"]),
    (
        &["amazon", "web", "services"],
        &["aws.amazon.com", "docs.aws.amazon.com"],
    ),
    (&["azure"], &["learn.microsoft.com", "azure.microsoft.com"]),
    (&["gcp"], &["cloud.google.com"]),
    (&["google", "cloud"], &["cloud.google.com"]),
    (&["vercel"], &["vercel.com"]),
    (&["netlify"], &["docs.netlify.com"]),
    (
        &["cloudflare"],
        &["developers.cloudflare.com", "cloudflare.com"],
    ),
    (&["fastly"], &["developer.fastly.com"]),
    (&["heroku"], &["devcenter.heroku.com"]),
    (&["digitalocean"], &["docs.digitalocean.com"]),
    (&["supabase"], &["supabase.com"]),
    (&["firebase"], &["firebase.google.com"]),
    (&["stripe"], &["docs.stripe.com", "stripe.com"]),
    (&["twilio"], &["twilio.com"]),
    (&["fly", "io"], &["fly.io"]),
    // ── Dev tools ──
    (&["git"], &["git-scm.com"]),
    (&["github", "actions"], &["docs.github.com"]),
    (&["archlinux"], &["wiki.archlinux.org"]),
    (&["arch", "linux"], &["wiki.archlinux.org"]),
    (
        &["ubuntu"],
        &["help.ubuntu.com", "documentation.ubuntu.com"],
    ),
    (&["debian"], &["debian.org"]),
    (&["fedora"], &["docs.fedoraproject.org"]),
    (&["neovim"], &["neovim.io"]),
    (&["vim"], &["vimhelp.org"]),
    (&["emacs"], &["gnu.org"]),
    (&["bash"], &["gnu.org"]),
    (&["ffmpeg"], &["ffmpeg.org"]),
    (&["curl"], &["curl.se", "everything.curl.dev"]),
    (&["wget"], &["gnu.org"]),
    (&["openssl"], &["openssl.org", "docs.openssl.org"]),
    (&["openssh"], &["man.openbsd.org", "openssh.com"]),
    (&["ssh"], &["man.openbsd.org", "openssh.com"]),
    (&["pip"], &["pip.pypa.io"]),
    (&["npm"], &["docs.npmjs.com"]),
    (&["yarn"], &["yarnpkg.com"]),
    (&["homebrew"], &["docs.brew.sh"]),
    (&["cargo", "rust"], &["doc.rust-lang.org"]),
    // ── AI/ML ──
    (&["pytorch"], &["pytorch.org"]),
    (&["tensorflow"], &["tensorflow.org"]),
    (&["huggingface"], &["huggingface.co"]),
    (&["scikit"], &["scikit-learn.org"]),
    (&["sklearn"], &["scikit-learn.org"]),
    (&["numpy"], &["numpy.org"]),
    (&["pandas"], &["pandas.pydata.org"]),
    (&["opencv"], &["docs.opencv.org"]),
    (&["ollama"], &["ollama.com"]),
    (&["openai"], &["openai.com", "platform.openai.com"]),
    (&["anthropic"], &["anthropic.com", "docs.anthropic.com"]),
    (&["claude"], &["docs.anthropic.com", "anthropic.com"]),
    (&["langchain"], &["python.langchain.com"]),
    (&["mcp"], &["modelcontextprotocol.io"]),
    (
        &["model", "context", "protocol"],
        &["modelcontextprotocol.io"],
    ),
    (&["transformers", "huggingface"], &["huggingface.co"]),
    // ── Protocols & specs ──
    (&["oauth"], &["oauth.net"]),
    (&["jwt"], &["jwt.io"]),
    (&["json", "web", "token"], &["jwt.io"]),
    (&["grpc"], &["grpc.io"]),
    (&["graphql"], &["graphql.org"]),
    (&["websocket"], &["developer.mozilla.org"]),
    (&["websockets"], &["developer.mozilla.org"]),
    (&["jsonrpc"], &["jsonrpc.org"]),
    (&["json", "rpc"], &["jsonrpc.org"]),
    (&["rfc"], &["rfc-editor.org", "datatracker.ietf.org"]),
    (&["ietf"], &["datatracker.ietf.org", "ietf.org"]),
    // ── Research repositories ──
    (&["arxiv"], &["arxiv.org"]),
    (&["semanticscholar"], &["semanticscholar.org"]),
    (&["openreview"], &["openreview.net"]),
];

/// Meta-words about the SEARCH ITSELF, never about the target
/// entity : excluded from title-coverage requirements. "docs"
/// and "news" describe what the agent wants, not what the page
/// is about; requiring them in titles would punish the best
/// results (official doc pages often don't say "docs" in the
/// title).
const TITLE_STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "how",
    "what",
    "why",
    "who",
    "does",
    "with",
    "using",
    "from",
    "into",
    "about",
    "are",
    "was",
    "were",
    "has",
    "have",
    "you",
    "your",
    "can",
    "not",
    "but",
    "all",
    "any",
    "get",
    "set",
    "new",
    "latest",
    "news",
    "update",
    "updates",
    "official",
    "documentation",
    "docs",
    "doc",
    "guide",
    "tutorial",
    "explained",
    "example",
    "examples",
    "vs",
    "versus",
    "best",
    "good",
    "modern",
    "simple",
    "complete",
    "difference",
    "between",
    "when",
];

/// Docs-seeking queries amplify the official boost : when the
/// agent explicitly asks for documentation, official sources are
/// near-certain to be the intended #1.
const DOCS_WORDS: &[&str] = &[
    "docs",
    "documentation",
    "documented",
    "reference",
    "api",
    "specification",
    "spec",
    "official",
    "manual",
    "man page",
    "rfc",
    "changelog",
    "release notes",
];

/// Paper-seeking signal: the query wants primary research.
const PAPER_WORDS: &[&str] = &["paper", "papers", "study", "studies", "research", "arxiv"];

/// Canonical research repositories for paper-seeking queries.
const PAPER_AUTHORITY: &[&str] = &[
    "arxiv.org",
    "openreview.net",
    "semanticscholar.org",
    "scholar.google.com",
    "pubmed.ncbi.nlm.nih.gov",
    "aclanthology.org",
    "biorxiv.org",
    "papers.neurips.cc",
    "openaccess.thecvf.com",
];

/// Boost strengths. Calibrated against the 50-case benchmark:
/// the blended (60% RRF + 40% cross-encoder, min-max normalized)
/// top score is ~0.6-1.0; a mid-pack result sits at ~0.3-0.5.
/// OFFICIAL × title coverage can lift 0.35 → ~0.9, enough to
/// take #1 from a consensus-heavy aggregator without letting a
/// semantically-irrelevant official page (blended ~0.1) anywhere
/// near the top.
const OFFICIAL_MULT: f64 = 1.6;
const OFFICIAL_CODE_MULT: f64 = 1.8;
const DOCS_SEEK_MULT: f64 = 1.15;
const PRIOR_MULT: f64 = 1.2;
const PAPER_SEEK_MULT: f64 = 1.35;
const TITLE_W: f64 = 0.35;
const PHRASE_MULT: f64 = 1.15;

fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(String::from)
        .collect()
}

/// Official domains for THIS query: registry entries whose token
/// set ⊆ query tokens. Empty when nothing matches : the layer
/// then no-ops (non-tech queries are untouched).
pub fn official_domains(query: &str) -> Vec<&'static str> {
    let toks = query_tokens(query);
    let has = |t: &str| toks.iter().any(|q| q == t);
    let mut out = Vec::new();
    for (entry_tokens, domains) in OFFICIAL {
        if entry_tokens.iter().all(|t| has(t)) {
            out.extend_from_slice(domains);
        }
    }
    out
}

fn docs_seeking(query: &str) -> bool {
    let q = query.to_lowercase();
    DOCS_WORDS.iter().any(|w| q.contains(w))
}

fn paper_seeking(query: &str) -> bool {
    let q = query.to_lowercase();
    PAPER_WORDS.iter().any(|w| q.contains(w))
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Fraction of query ENTITY terms present in the title.
/// Meta-words (TITLE_STOPWORDS) are excluded from both sides of
/// the ratio : the signal is entity match, not filler match.
fn title_terms_ratio(query: &str, title: &str) -> f64 {
    let q = query.to_lowercase();
    let terms: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2 && !TITLE_STOPWORDS.contains(t))
        .collect();
    if terms.is_empty() {
        return 0.0;
    }
    let tl = title.to_lowercase();
    let hits = terms.iter().filter(|t| tl.contains(**t)).count();
    hits as f64 / terms.len() as f64
}

/// Exact query phrase (normalized) appears in the title.
fn phrase_in_title(query: &str, title: &str) -> bool {
    let norm = |s: &str| {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string()
    };
    let q = norm(query);
    if q.len() < 8 {
        return false; // short queries: term ratio already covers it
    }
    norm(title).contains(&q)
}

/// Freshness multiplier for News intent from an ISO
/// "YYYY-MM-DD…" published stamp. None/parse-fail = neutral.
fn freshness_mult(published: &Option<String>) -> f64 {
    let Some(p) = published else {
        return 1.0;
    };
    let Some(days_old) = iso_days_ago(p) else {
        return 1.0;
    };
    match days_old {
        // Negative = future-dated (a GMT pubDate ahead of a local
        // clock near midnight): fresh, not stale. The lower-open arm
        // is what makes iso_days_ago's "clamped to fresh" true; a
        // `0..=1` arm sent every negative value to the stale 0.85.
        ..=1 => 1.5,
        2..=3 => 1.3,
        4..=7 => 1.15,
        8..=30 => 1.0,
        _ => 0.85,
    }
}

/// Days between an ISO date prefix and today (negative = future,
/// clamped to fresh).
fn iso_days_ago(iso: &str) -> Option<i64> {
    let mut it = iso.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let then = days_from_civil(y, m, d);
    let now = epoch_days_now();
    Some(now - then)
}

/// Howard Hinnant's days_from_civil : proleptic Gregorian.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn epoch_days_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0)
}

/// Inverse of days_from_civil for tests: epoch days → ISO date.
#[cfg(test)]
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The decisive top-placement layer. Called from
/// `rank::merge` AFTER rerank + coverage, BEFORE the final
/// sort : the same slot the coverage penalty proved effective
/// in (the layer the cross-encoder blend cannot wash out).
pub fn apply(query: &str, intent: Intent, results: &mut [Merged]) {
    let official = official_domains(query);
    let docs_seek = docs_seeking(query);
    let paper_seek = paper_seeking(query);
    for r in results.iter_mut() {
        let host = host_of(&r.url);
        let mut m = 1.0;

        let official_hit = !official.is_empty() && official.iter().any(|d| host_matches(&host, d));
        if official_hit {
            m *= match intent {
                Intent::Code => OFFICIAL_CODE_MULT,
                _ => OFFICIAL_MULT,
            };
            if docs_seek {
                m *= DOCS_SEEK_MULT;
            }
        } else if intent::domain_prior(intent, &host, query) > 0.0 {
            m *= PRIOR_MULT;
        }

        // Paper-seeking queries prefer primary repositories over
        // blog summaries of papers.
        if paper_seek && !official_hit && PAPER_AUTHORITY.iter().any(|d| host_matches(&host, d)) {
            m *= PAPER_SEEK_MULT;
        }

        // Title decisiveness : the placement signal the 60/40
        // blend dilutes. Applied to every result: it reorders
        // results by entity-title match, independent of domain.
        let ratio = title_terms_ratio(query, &r.title);
        m *= 1.0 + TITLE_W * ratio;
        if phrase_in_title(query, &r.title) {
            m *= PHRASE_MULT;
        }

        // Freshness only ranks within News intent: old sources
        // for a "latest news" query are the classic metasearch
        // failure.
        if intent == Intent::News {
            m *= freshness_mult(&r.published);
        }

        r.score *= m;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_domains_simple_token() {
        let d = official_domains("rust ownership explained");
        assert!(d.contains(&"rust-lang.org"), "got {d:?}");
        assert!(d.contains(&"docs.rs"));
    }

    #[test]
    fn official_domains_multi_token_requires_all() {
        assert!(
            official_domains("next generation sequencing").is_empty(),
            "'next' alone must not fire nextjs.org"
        );
        assert!(official_domains("next js app router").contains(&"nextjs.org"));
    }

    #[test]
    fn official_domains_ambiguous_go_never_fires_alone() {
        assert!(
            official_domains("how do i go about testing").is_empty(),
            "plain 'go' must not fire go.dev"
        );
        assert!(official_domains("go concurrency patterns").contains(&"go.dev"));
    }

    #[test]
    fn official_domains_empty_for_non_tech() {
        assert!(official_domains("best budget mechanical keyboard 2026").is_empty());
        assert!(official_domains("nash equilibrium explained").is_empty());
    }

    #[test]
    fn official_domains_cpp_and_rfc() {
        assert!(official_domains("cpp std vector reserve").contains(&"cppreference.com"));
        assert!(official_domains("rfc 8949 cbor").contains(&"rfc-editor.org"));
    }

    fn merged(title: &str, url: &str, score: f64) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            sources: vec![("bing".into(), 0)],
            score,
            published: None,
        }
    }

    #[test]
    fn official_outranks_slightly_higher_aggregator() {
        // The benchmark failure shape: keyword-farm/aggregator
        // consensus edge over the official doc page. The
        // authority layer must flip the placement.
        let mut rs = vec![
            merged(
                "Understanding Ownership in Rust - SomeBlog",
                "https://someblog.dev/rust-ownership",
                0.80,
            ),
            merged(
                "Ownership - Rust By Example",
                "https://doc.rust-lang.org/rust-by-example/scope/ownership.html",
                0.55,
            ),
        ];
        apply("rust ownership explained", Intent::Web, &mut rs);
        rs.sort_by(|a, b| b.score.total_cmp(&a.score));
        assert!(
            rs[0].url.contains("doc.rust-lang.org"),
            "official doc must place first: {:?}",
            rs.iter()
                .map(|r| (r.url.clone(), r.score))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_boost_for_garbage_official_page() {
        // Self-limiting: an official-domain page the consensus +
        // semantics scored near zero stays near zero.
        let mut rs = vec![
            merged(
                "Kitchen Sinks 2026 Catalog",
                "https://someblog.dev/sinks",
                0.90,
            ),
            merged(
                "Unrelated rust-lang page",
                "https://rust-lang.org/unrelated",
                0.05,
            ),
        ];
        apply("rust ownership explained", Intent::Web, &mut rs);
        rs.sort_by(|a, b| b.score.total_cmp(&a.score));
        assert!(rs[0].url.contains("someblog.dev"));
    }

    #[test]
    fn title_phrase_beats_term_scatter() {
        let mut rs = vec![
            merged(
                "The ultimate guide to everything about transformers",
                "https://a.com/x",
                0.80,
            ),
            merged(
                "Attention Is All You Need",
                "https://arxiv.org/abs/1706.03762",
                0.62,
            ),
        ];
        apply("attention is all you need", Intent::Paper, &mut rs);
        rs.sort_by(|a, b| b.score.total_cmp(&a.score));
        assert!(rs[0].url.contains("arxiv.org"));
    }

    #[test]
    fn stopwords_do_not_gut_title_ratio() {
        // "latest news" excluded from the ratio: a wire story
        // title containing only the ENTITY terms still scores 1.0.
        let r = title_terms_ratio(
            "ukraine war latest news",
            "Ukraine war: Russia strikes Kyiv power grid",
        );
        assert!((r - 1.0).abs() < 0.01, "got {r}"); // ukraine+war present, latest+news stopworded
    }

    #[test]
    fn freshness_prefers_recent() {
        // Relative check : no pinned "today".
        let now = epoch_days_now();
        let iso_now = civil_from_days(now);
        let iso_old = civil_from_days(now - 400);
        let recent = freshness_mult(&Some(iso_now));
        let old = freshness_mult(&Some(iso_old));
        let none = freshness_mult(&None);
        assert!(recent > old, "recent ({recent}) must outrank old ({old})");
        assert_eq!(none, 1.0);
    }

    #[test]
    fn freshness_tiers_parse_iso_prefix() {
        // Relative to today: a literal date here silently walks out
        // of the 30-day neutral tier and starts failing the build
        // (the previous "2026-08-16" would have from 2026-09-16).
        let now = epoch_days_now();
        let tiers = [
            (0, 1.5),
            (1, 1.5),
            (2, 1.3),
            (3, 1.3),
            (4, 1.15),
            (7, 1.15),
            (8, 1.0),
            (30, 1.0),
            (31, 0.85),
            (400, 0.85),
        ];
        for (age, want) in tiers {
            let iso = civil_from_days(now - age);
            let got = freshness_mult(&Some(iso.clone()));
            assert_eq!(got, want, "{age} days old ({iso})");
        }
        // Unparseable → neutral.
        assert_eq!(freshness_mult(&Some("garbage".into())), 1.0);
    }

    #[test]
    fn freshness_treats_future_dates_as_fresh() {
        let now = epoch_days_now();
        for ahead in [1, 2, 30] {
            let iso = civil_from_days(now + ahead);
            assert_eq!(freshness_mult(&Some(iso.clone())), 1.5, "{iso}");
        }
    }

    #[test]
    fn paper_seeking_boosts_arxiv_over_blog() {
        let mut rs = vec![
            merged(
                "A blog summary of the transformer paper",
                "https://blog.dev/transformers",
                0.85,
            ),
            merged(
                "Attention Is All You Need",
                "https://arxiv.org/abs/1706.03762",
                0.50,
            ),
        ];
        apply("attention is all you need paper", Intent::Paper, &mut rs);
        rs.sort_by(|a, b| b.score.total_cmp(&a.score));
        assert!(rs[0].url.contains("arxiv.org"));
    }

    #[test]
    fn no_registry_match_leaves_scores_scaled_only_by_title() {
        let mut rs = vec![merged("Some Title", "https://a.com/x", 1.0)];
        apply("nash equilibrium explained", Intent::Web, &mut rs);
        // Title contains "nash"? No → ratio 0.5 (nash+equilibrium, title has neither → 0.0)
        assert!(
            rs[0].score <= 1.0 + 1e-9,
            "nothing to boost, score must not grow"
        );
    }
}
