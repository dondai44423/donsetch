//! Domain intelligence: keyless adapters for the sites agents
//! actually use (v3 Pillar E).
//!
//! Two hooks, one registry:
//! - [`rewrite`] : fetch-level: some pages have a *better* URL
//!   (the site's own public JSON endpoint). Rewriting gets
//!   structured truth in ONE cheap local request and often
//!   skips the wall entirely (registry CDNs don't challenge).
//! - [`extract_json`] / [`extract_html`] : extract-level: pages
//!   whose HTML the generic pipeline mangles (GitHub issues,
//!   Stack Exchange QA trees) restructured from the DOM.
//!
//! Discipline: every adapter is small, fixture-tested, and
//! returns `None` on anything it doesn't confidently recognize :
//! the generic DonSift path is always the fallback. A site
//! redesign degrades one adapter, never the core.
//! Kill switch: `DONSETCH_NO_ADAPTERS=1` disables the registry.

pub mod docs_outline;
pub mod github;
pub mod packages;
pub mod plugins;
pub mod reddit_html;
pub mod reddit_json;
pub mod stackexchange;
pub mod wiki_infobox;

/// Kill switch : checked once, then cached.
fn enabled() -> bool {
    crate::config::cfg().fetch.adapters
}

/// Debug capture: `DONSETCH_ADAPTER_DUMP=<dir>` writes every body
/// an adapter inspects : fixture capture for adapter development.
/// Best-effort, never a failure path.
fn debug_dump(html: &str, url: &str) {
    let dir = crate::config::cfg().fetch.adapter_dump_dir.clone();
    if dir.is_empty() {
        return;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    let p = std::path::Path::new(&dir).join(format!("{:016x}.html", h.finish()));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(p, format!("<!-- {url} -->\n{html}"));
}

/// One bundled rewrite adapter.
#[derive(Clone, Copy)]
pub struct BuiltinRewrite {
    /// The honest via label ("adapter:<suffix>").
    pub via: &'static str,
    /// One-line description for `donsetch adapters`.
    pub description: &'static str,
    /// Match + rewrite, returns the new URL string.
    pub apply: fn(&url::Url) -> Option<String>,
}

/// The bundled rewrite set, in dispatch order.
///
/// There is deliberately no old.reddit.com retarget (issue #283):
/// both reddit rewrites used to point there, the host serves a
/// login wall to anonymous clients on HTML and `.json` alike, and
/// the shape-mismatch guard silently repaired the result afterwards
/// : every reddit fetch paid a dead hop (and, on a host already
/// recorded as walled, an unlocker escalation) while a caller who
/// supplied the working `www.reddit.com/....json` URL was detoured
/// through the wall first. `www.reddit.com` serves the same JSON
/// the adapter parses, so the host stays the caller's.
const BUILTIN_REWRITES: [BuiltinRewrite; 7] = [
    BuiltinRewrite {
        via: "adapter:reddit-host",
        description: "old./np.reddit.com -> www.reddit.com, the host that answers anonymous clients",
        apply: reddit_host_rw,
    },
    BuiltinRewrite {
        via: "adapter:stackexchange-api",
        description: "Stack Overflow questions -> public API question, answers and comments",
        apply: stackexchange::api_url,
    },
    BuiltinRewrite {
        via: "adapter:npm-registry",
        description: "npmjs.com/package/<pkg> -> registry.npmjs.org packument or version",
        apply: npm_registry_rw,
    },
    BuiltinRewrite {
        via: "adapter:pypi-json",
        description: "pypi.org/project/<pkg> -> pypi.org/pypi/<pkg>/json",
        apply: pypi_json_rw,
    },
    BuiltinRewrite {
        via: "adapter:crates-api",
        description: "crates.io/crates/<crate> -> crates.io/api/v1/crates/<crate>",
        apply: crates_api_rw,
    },
    BuiltinRewrite {
        via: "adapter:go-proxy",
        description: "pkg.go.dev module pages -> proxy.golang.org @latest",
        apply: go_proxy_rw,
    },
    BuiltinRewrite {
        via: "adapter:rubygems-api",
        description: "rubygems.org/gems/<gem> -> rubygems.org/api/v1/gems/<gem>.json",
        apply: rubygems_api_rw,
    },
];

/// Bundled adapter list for `donsetch adapters` and the loader's
/// name-collision check (the via suffix after "adapter:").
pub fn builtins() -> &'static [BuiltinRewrite] {
    &BUILTIN_REWRITES
}

/// The bundled adapters' suffixes ("reddit-json", ...) for the
/// user-loader collision gate.
pub(crate) fn builtin_via_suffixes() -> Vec<&'static str> {
    BUILTIN_REWRITES
        .iter()
        .map(|b| b.via.strip_prefix("adapter:").unwrap_or(b.via))
        .collect()
}

/// Fetch-level URL rewrite: the registry.
///
/// Order: bundled entries first (the historical order), then the
/// user plugin catalog, first match wins, and every result carries
/// the honest via label.
///
/// `None` = no adapter (fetch the URL as given).
pub fn rewrite(u: &url::Url) -> Option<(String, &'static str)> {
    if !enabled() {
        return None;
    }
    for b in &BUILTIN_REWRITES {
        if let Some(new_url) = (b.apply)(u) {
            return Some((new_url, b.via));
        }
    }
    for row in plugins::catalog() {
        if let Some(new_url) = plugins::apply(&row.plugin, u) {
            return Some((new_url, row.via));
        }
    }
    None
}

// -- Bundled adapters: matchers + rewrites. -------------------------
// Each returns the rewritten URL name; `rewrite` attaches the via.

/// Legacy reddit hosts -> the content host, and nothing else. `old.` and
/// `np.` serve a login wall to anonymous clients, so a caller who names them is
/// sent to the host that answers. Every other reddit URL is fetched exactly as
/// written: the v5 core's own encounter answers the page, running the site's JS
/// gate in-process, and reddit's `.json` is refused to anonymous clients, so
/// rewriting a page onto it bought a guaranteed 403 and a second request.
fn reddit_host_rw(u: &url::Url) -> Option<String> {
    match u.host_str()? {
        "old.reddit.com" | "np.reddit.com" => rebuilt(u, "www.reddit.com", u.path()),
        _ => None,
    }
}

/// One rebuilt URL: the caller's URL, host swapped when needed,
/// path replaced, fragment dropped, query kept.
fn rebuilt(u: &url::Url, host: &str, path: &str) -> Option<String> {
    let mut u2 = u.clone();
    u2.set_host(Some(host)).ok()?;
    u2.set_path(path);
    u2.set_fragment(None);
    Some(u2.to_string())
}

/// The content-host URL for a caller URL on a legacy host
/// (`old.`/`np.`): those serve a login wall to anonymous clients,
/// so fallback retries head for www instead. `None` = the URL is
/// not on a legacy host (use it as-is).
pub fn reddit_content_url(u: &url::Url) -> Option<String> {
    match u.host_str()? {
        "old.reddit.com" | "np.reddit.com" => {
            let mut u2 = u.clone();
            u2.set_host(Some("www.reddit.com")).ok()?;
            Some(u2.to_string())
        }
        _ => None,
    }
}

/// Domain-label host check for reddit, shared by the JSON adapter
/// and the HTML extractor: `reddit.com` itself or any `*.reddit.com`
/// subdomain, nothing else. A bare `ends_with("reddit.com")` also
/// claimed look-alikes like `notreddit.com`.
pub(crate) fn is_reddit_host(host: &str) -> bool {
    host == "reddit.com"
        || host
            .strip_suffix("reddit.com")
            .is_some_and(|p| p.ends_with('.'))
}

/// 1_234_567 → "1.2M" : shared by the package cards and the
/// reddit cards.
pub(crate) fn human_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn npm_registry_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if host != "www.npmjs.com" && host != "npmjs.com" {
        return None;
    }
    // /package/<pkg> or /package/<pkg>/v/<ver>
    let rest = u.path().strip_prefix("/package/")?;
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        return None;
    }
    // /v/<ver> suffix = version manifest; else full packument.
    let (pkg, ver): (&str, Option<&str>) = match rest.split_once("/v/") {
        Some((p, v)) => (p, Some(v)),
        None => (rest, None),
    };
    let api_path = ver.map_or_else(|| pkg.to_string(), |v| format!("{pkg}/{v}"));
    let u2 = url::Url::parse(&format!("https://registry.npmjs.org/{api_path}")).ok()?;
    Some(u2.to_string())
}

fn pypi_json_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if host != "pypi.org" && host != "pypi.python.org" {
        return None;
    }
    // /project/<pkg>(/<ver>)
    let rest = u.path().strip_prefix("/project/")?;
    let rest = rest.trim_matches('/');
    let mut parts = rest.split('/');
    let pkg = parts.next()?;
    if pkg.is_empty() {
        return None;
    }
    let ver = parts.next().filter(|v| !v.is_empty());
    // PEP 503 name normalization: case + -_. runs = -
    // PEP 503: runs of `-_.` collapse to ONE dash ("zope..interface"
    // and "zope.interface" both normalize to "zope-interface"); the
    // old per-char replace kept the run and 404'd the API call.
    let norm: String = pkg
        .to_lowercase()
        .replace(['_', '.'], "-")
        .chars()
        .collect::<Vec<char>>()
        .into_iter()
        .fold(String::new(), |mut acc, c| {
            if !(c == '-' && acc.ends_with('-')) {
                acc.push(c);
            }
            acc
        });
    let api_path = ver.map_or_else(|| format!("{norm}/json"), |v| format!("{norm}/{v}/json"));
    let u2 = url::Url::parse(&format!("https://pypi.org/pypi/{api_path}")).ok()?;
    Some(u2.to_string())
}

fn crates_api_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if host != "crates.io" && host != "www.crates.io" {
        return None;
    }
    // /crates/<name>(/<ver>)
    let rest = u.path().strip_prefix("/crates/")?;
    let rest = rest.trim_matches('/');
    let mut parts = rest.split('/');
    let name = parts.next()?;
    if name.is_empty() {
        return None;
    }
    let ver = parts.next().filter(|v| !v.is_empty());
    // Version-specific: the version endpoint carries deps.
    let api_path = ver.map_or_else(|| name.to_string(), |v| format!("{name}/{v}"));
    let u2 = url::Url::parse(&format!("https://crates.io/api/v1/crates/{api_path}")).ok()?;
    Some(u2.to_string())
}

fn go_proxy_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if host != "pkg.go.dev" {
        return None;
    }
    // /<module path> -> Go module proxy. Uppercase paths need
    // !escaping on the proxy (rare) : skip those, generic
    // handles them. Stdlib paths (no dot in the first
    // element: /fmt, /net/http) have no proxy module : skip.
    let rest = u.path().strip_prefix('/')?;
    if rest.is_empty() || rest.starts_with("std") {
        return None;
    }
    let first = rest.split('/').next().unwrap_or("");
    if !first.contains('.') || rest.chars().any(|c| c.is_uppercase()) {
        return None;
    }
    // Version-pinned module (/module@v1.2.3): the module path is the
    // part before the '@', and the proxy serves the same payload
    // @latest does, pinned, under /@v/<version>.info. Sending the
    // '@' through to @latest made the proxy read "module@v1.2.3"
    // as the module name: 404, fallback, and the caller's page
    // fetched anyway, one wasted hop later.
    if let Some(at) = rest.find('@') {
        let module = &rest[..at];
        let ver = &rest[at + 1..];
        if module.is_empty() || ver.is_empty() || ver.contains('/') || !ver.starts_with('v') {
            return None;
        }
        let u2 =
            url::Url::parse(&format!("https://proxy.golang.org/{module}/@v/{ver}.info")).ok()?;
        return Some(u2.to_string());
    }
    let u2 = url::Url::parse(&format!("https://proxy.golang.org/{rest}/@latest")).ok()?;
    Some(u2.to_string())
}

fn rubygems_api_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if host != "rubygems.org" && host != "www.rubygems.org" {
        return None;
    }
    let rest = u.path().strip_prefix("/gems/")?;
    let gem = rest.trim_matches('/');
    if gem.is_empty() || gem.contains('/') {
        return None;
    }
    let u2 = url::Url::parse(&format!("https://rubygems.org/api/v1/gems/{gem}.json")).ok()?;
    Some(u2.to_string())
}

/// Extract-level dispatch for JSON bodies (post-rewrite).
/// Returns `None` → generic passthrough. Runs BEFORE the
/// non-HTML passthrough so adapter JSON never dumps raw.
pub fn extract_json(
    body: &[u8],
    ct: &str,
    url: &str,
    opts: &crate::extract::ExtractOptions,
) -> Option<crate::extract::Extracted> {
    if !enabled() {
        return None;
    }
    if let Some(ex) = stackexchange::extract_api(body, url, opts) {
        return Some(ex);
    }
    // Cut contract, JSON edition: `must_contain` bails to the
    // non-HTML probe in extract(), which serves it properly. focus /
    // toc / section do NOT bail: the generic machinery for them
    // runs on HTML blocks, a JSON payload has none, and bailing
    // fell through to the raw-JSON passthrough, which silently
    // dropped the cut and dumped the whole payload. The card IS
    // the compact truth of this endpoint: serve it.
    let looks_json = ct.contains("json") || matches!(body.first(), Some(b'{') | Some(b'['));
    if !looks_json {
        return None;
    }
    if let Ok(s) = std::str::from_utf8(body) {
        debug_dump(s, url);
    }
    reddit_json::extract(body, url, opts).or_else(|| packages::extract(body, url, opts))
}

/// Extract-level dispatch for HTML bodies. Returns `None` →
/// generic DonSift.
pub fn extract_html(
    html: &str,
    url: &str,
    opts: &crate::extract::ExtractOptions,
) -> Option<crate::extract::Extracted> {
    // Crawl's block focus is a pipeline feature the adapters don't
    // reproduce: the generic path (which implements it) wins. Fetch views
    // apply to adapter markdown afterwards, like to any other.
    if !enabled() || opts.focus.is_some() {
        return None;
    }
    debug_dump(html, url);
    reddit_html::extract(html, url, opts)
        .or_else(|| github::extract(html, url, opts))
        .or_else(|| stackexchange::extract(html, url, opts))
        .or_else(|| wiki_infobox::extract(html, url, opts))
        .or_else(|| docs_outline::extract(html, url, opts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(u: &str) -> Option<(String, &'static str)> {
        rewrite(&url::Url::parse(u).unwrap())
    }

    // A reddit page is fetched exactly as written: the v5 core's own encounter
    // answers it (the JS gate runs in-process), and its `.json` is refused to
    // anonymous clients, so rewriting onto it only bought a 403 and a second
    // request.
    #[test]
    fn reddit_pages_are_not_rewritten() {
        assert!(rw("https://www.reddit.com/r/rust/comments/abc123/title_here/").is_none());
        assert!(rw("https://reddit.com/r/programming/?t=top").is_none());
        assert!(rw("https://www.reddit.com/").is_none());
        assert!(rw("https://www.reddit.com/r/rust/wiki/index").is_none());
        assert!(rw("https://www.reddit.com/user/spez/").is_none());
    }

    // Legacy hosts serve nothing to anonymous clients: their URLs go to the
    // content host, path and query intact.
    #[test]
    fn reddit_legacy_hosts_retarget_to_www() {
        let (u, via) = rw("https://old.reddit.com/r/rust/comments/abc/x/").unwrap();
        assert_eq!(u, "https://www.reddit.com/r/rust/comments/abc/x/");
        assert_eq!(via, "adapter:reddit-host");
        let (u, _) = rw("https://np.reddit.com/r/rust/?t=top").unwrap();
        assert_eq!(u, "https://www.reddit.com/r/rust/?t=top");
    }

    #[test]
    fn reddit_content_url_swaps_legacy_hosts_only() {
        let c = |u: &str| reddit_content_url(&url::Url::parse(u).unwrap());
        assert_eq!(
            c("https://old.reddit.com/r/rust/").unwrap(),
            "https://www.reddit.com/r/rust/"
        );
        assert_eq!(
            c("https://np.reddit.com/r/rust/").unwrap(),
            "https://www.reddit.com/r/rust/"
        );
        assert!(c("https://www.reddit.com/r/rust/").is_none());
        assert!(c("https://example.com/").is_none());
    }

    #[test]
    fn a_json_url_is_never_rewritten() {
        // A caller who supplies the URL that actually works is not
        // detoured, and nothing is appended to it (issue #283).
        assert!(rw("https://www.reddit.com/r/rust/comments/abc/x.json").is_none());
        assert!(rw("https://www.reddit.com/r/rust.json?limit=50").is_none());
        // A legacy host goes to the content host with its own path, as written.
        let (u, _) = rw("https://old.reddit.com/r/rust.json").unwrap();
        assert_eq!(u, "https://www.reddit.com/r/rust.json");
    }

    #[test]
    fn npm_packument() {
        let (u, via) = rw("https://www.npmjs.com/package/react").unwrap();
        assert_eq!(u, "https://registry.npmjs.org/react");
        assert_eq!(via, "adapter:npm-registry");
    }

    #[test]
    fn npm_scoped_and_version() {
        let (u, _) = rw("https://www.npmjs.com/package/@babel/core").unwrap();
        assert_eq!(u, "https://registry.npmjs.org/@babel/core");
        let (u, _) = rw("https://www.npmjs.com/package/typescript/v/5.6.0").unwrap();
        assert_eq!(u, "https://registry.npmjs.org/typescript/5.6.0");
    }

    #[test]
    fn pypi_normalized() {
        let (u, via) = rw("https://pypi.org/project/Flask/").unwrap();
        assert_eq!(u, "https://pypi.org/pypi/flask/json");
        assert_eq!(via, "adapter:pypi-json");
        let (u, _) = rw("https://pypi.org/project/zope_interface/2.1.0/").unwrap();
        assert_eq!(u, "https://pypi.org/pypi/zope-interface/2.1.0/json");
    }

    #[test]
    fn crates_versions() {
        let (u, _) = rw("https://crates.io/crates/serde").unwrap();
        assert_eq!(u, "https://crates.io/api/v1/crates/serde");
        let (u, _) = rw("https://crates.io/crates/tokio/1.40.0").unwrap();
        assert_eq!(u, "https://crates.io/api/v1/crates/tokio/1.40.0");
    }

    #[test]
    fn go_proxy() {
        let (u, via) = rw("https://pkg.go.dev/github.com/gin-gonic/gin").unwrap();
        assert_eq!(
            u,
            "https://proxy.golang.org/github.com/gin-gonic/gin/@latest"
        );
        assert_eq!(via, "adapter:go-proxy");
        // stdlib + subpaths + uppercase: no adapter.
        assert!(rw("https://pkg.go.dev/fmt").is_none());
        assert!(rw("https://pkg.go.dev/").is_none());
    }

    #[test]
    fn rubygems() {
        let (u, _) = rw("https://rubygems.org/gems/rails").unwrap();
        assert_eq!(u, "https://rubygems.org/api/v1/gems/rails.json");
        assert!(rw("https://rubygems.org/gems/").is_none());
    }

    #[test]
    fn non_adapter_sites_pass_through() {
        assert!(rw("https://example.com/foo").is_none());
        assert!(rw("https://github.com/tokio-rs/tokio").is_none());
    }

    // pkg.go.dev version pins map to the proxy's pinned .info; the
    // '@' used to ride along into @latest, whose 404 cost a full
    // adapter detour.
    #[test]
    fn go_proxy_version_pinned() {
        let (u, via) = rw("https://pkg.go.dev/github.com/gin-gonic/gin@v1.10.0").unwrap();
        assert_eq!(
            u,
            "https://proxy.golang.org/github.com/gin-gonic/gin/@v/v1.10.0.info"
        );
        assert_eq!(via, "adapter:go-proxy");
        // A malformed pin is not a proxy path: generic handles it.
        assert!(rw("https://pkg.go.dev/github.com/gin-gonic/gin@nope").is_none());
        assert!(rw("https://pkg.go.dev/github.com/gin-gonic/gin@v1.0.0/sub").is_none());
    }

    #[test]
    fn reddit_host_check_is_label_aware() {
        assert!(is_reddit_host("reddit.com"));
        assert!(is_reddit_host("www.reddit.com"));
        assert!(is_reddit_host("old.reddit.com"));
        assert!(!is_reddit_host("notreddit.com"));
        assert!(!is_reddit_host("reddit.com.evil.io"));
    }

    // Crawl's block focus cannot apply to a JSON payload: the extractors
    // used to bail and the raw JSON got dumped, silently dropping the cut.
    // The card is the answer, with or without focus.
    #[test]
    fn json_cards_survive_focus() {
        let body = br#"{"kind":"Listing","data":{"children":[{"kind":"t3","data":{
          "title":"Post","subreddit":"rust","author":"a","score":1,
          "created_utc":1755800000.0,"num_comments":0,"selftext":"","over_18":false}}]}}"#;
        let url = "https://www.reddit.com/r/rust.json";
        for cut in [
            crate::extract::ExtractOptions {
                focus: Some("post".into()),
                ..Default::default()
            },
            crate::extract::ExtractOptions::default(),
        ] {
            let ex = extract_json(body, "application/json", url, &cut).expect("card");
            assert!(ex.markdown.contains("**Post**"), "{}", ex.markdown);
        }
    }
}
