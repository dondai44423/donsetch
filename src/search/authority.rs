//! authority.rs: which hosts own the thing a query names, and how fresh a dated
//! item is.
//!
//! The registry answers one question: for a query that names a technology, which
//! official sites would carry its documentation? That is used for *retrieval*:
//! a bounded `site:` recall puts those pages in the candidate pool, where the
//! engines' own ordering then ranks them. It is deliberately not used for
//! *ranking*. This module used to multiply the score of any result on an
//! official host, plus a title-match and a paper-host boost; measured with the
//! sources' own results held fixed, those multipliers took hit@1 on a 40-query
//! gold set from 0.62 (no boost) to 0.42, at every strength from 0.2 up, and
//! never improved it. A host list cannot tell the right page of an official site
//! from the wrong one; the engine that indexed it can.
//!
//! The one ranking signal kept is freshness for dated news items, because a
//! publication date is data the source provided, not a guess about the page.

use super::intent::Intent;
use super::rank::Merged;

/// Query-aware official sources: (query tokens → official
/// domains). ALL tokens in an entry must appear in the query.
/// Single common-English tokens that name a tech only when
/// combined (next + js, spring + boot, go + concurrency) are
/// encoded as multi-token entries so they can't fire alone.
const OFFICIAL: &[(&[&str], &[&str])] = &[
    // Product names require context: "uplift" alone is ordinary prose.
    (&["uplift", "desk"], &["upliftdesk.com"]),
    (&["vari", "desk"], &["vari.com"]),
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

/// Bounded operator-only recall: preserve the caller's words and retrieve
/// actual engine hits from matching primary sources. Explicit site wins.
pub(super) fn official_queries(compiled: &super::query::CompiledQuery) -> Vec<String> {
    if compiled.has_operators() {
        return Vec::new();
    }
    let mut domains: Vec<&str> = Vec::new();
    for domain in official_domains(&compiled.text) {
        if domains
            .iter()
            .any(|parent| domain == *parent || domain.ends_with(&format!(".{parent}")))
        {
            continue;
        }
        domains.push(domain);
        if domains.len() == 2 {
            break;
        }
    }
    domains
        .into_iter()
        .map(|domain| format!("{} site:{domain}", compiled.text))
        .collect()
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
    // The year is provider text (an RSS pubDate token, parsed as
    // i64): days_from_civil does era * 146_097 on it, which
    // overflows past ~6e13 and wraps into a garbage age in release.
    // A date outside the Gregorian range is not a date.
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
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

/// Lift fresh dated items and sink stale ones, for News intent only. An item with
/// no date, or one that does not parse, is neutral.
pub fn freshen(intent: Intent, results: &mut [Merged]) {
    if intent != Intent::News {
        return;
    }
    for r in results.iter_mut() {
        r.score *= freshness_mult(&r.published);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Google News pubDate is provider text; rss_date_to_iso copies
    // its year token verbatim. i64::MAX as a year overflowed
    // days_from_civil's era * 146_097 (panic under overflow checks,
    // a wrapped garbage age in release).
    #[test]
    fn wave450_official_recall_is_bounded_and_respects_explicit_operators() {
        let queries = official_queries(&super::super::query::compile(
            "best standing desk 2026 Uplift vs Vari",
        ));
        assert_eq!(queries.len(), 2);
        assert!(queries.iter().any(|q| q.ends_with("site:upliftdesk.com")));
        assert!(queries.iter().any(|q| q.ends_with("site:vari.com")));
        assert!(
            queries
                .iter()
                .all(|q| q.starts_with("best standing desk 2026 Uplift vs Vari "))
        );
        assert!(
            official_queries(&super::super::query::compile(
                "uplift in economic productivity"
            ))
            .is_empty()
        );
        assert!(
            official_queries(&super::super::query::compile(
                "rust ownership site:example.com"
            ))
            .is_empty()
        );
        let queries = official_queries(&super::super::query::compile("rust tokio documentation"));
        assert_eq!(queries.len(), 2);
        assert!(
            !queries.iter().any(|q| q.contains("site:doc.rust-lang.org")),
            "a parent-domain query already covers the subdomain"
        );
    }

    #[test]
    fn an_absurd_year_is_not_a_date_and_does_not_overflow() {
        assert_eq!(iso_days_ago("9223372036854775807-01-01"), None);
        assert_eq!(iso_days_ago("-9223372036854775808-01-01"), None);
        assert_eq!(iso_days_ago("99999999999-06-15"), None);
        assert_eq!(iso_days_ago("0-01-01"), None);
        assert!(iso_days_ago("2026-01-01").is_some());
        assert!(iso_days_ago("9999-12-31").is_some());
        let iso = crate::search::verticals::rss_date_to_iso(
            "Thu, 01 Jan 9223372036854775807 00:00:00 GMT",
        )
        .unwrap();
        assert_eq!(iso_days_ago(&iso), None);
    }

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

    fn merged(title: &str, url: &str, score: f64, published: Option<String>) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            sources: vec![("bing".into(), 0)],
            score,
            published,
            families: 1,
            from_page: false,
        }
    }

    #[test]
    fn wave450_desk_vendor_sources_require_product_context() {
        assert!(official_domains("Uplift vs Vari standing desk").contains(&"upliftdesk.com"));
        assert!(official_domains("Uplift vs Vari standing desk").contains(&"vari.com"));
        assert!(official_domains("uplift in economic productivity").is_empty());
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
    fn freshen_moves_only_dated_news_items() {
        let now = epoch_days_now();
        let mut rs = vec![
            merged(
                "old",
                "https://a.com/old",
                1.0,
                Some(civil_from_days(now - 400)),
            ),
            merged(
                "fresh",
                "https://a.com/new",
                1.0,
                Some(civil_from_days(now)),
            ),
            merged("undated", "https://a.com/none", 1.0, None),
        ];
        freshen(Intent::News, &mut rs);
        assert!(rs[1].score > rs[2].score && rs[2].score > rs[0].score);
        assert_eq!(rs[2].score, 1.0, "no date is neutral");
        // Any other intent is untouched: a date is a news signal, not a quality one.
        let mut web = vec![merged(
            "fresh",
            "https://a.com/new",
            1.0,
            Some(civil_from_days(now)),
        )];
        freshen(Intent::Web, &mut web);
        assert_eq!(web[0].score, 1.0);
    }
}
