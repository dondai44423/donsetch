//! Domain intelligence: keyless adapters for the sites agents
//! actually use (v3 Pillar E).
//!
//! Two hooks, one registry:
//! - [`rewrite`] : fetch-level: some pages have a *better* URL
//!   (the site's own public JSON endpoint). Rewriting gets
//!   structured truth in ONE cheap tier-1 request and often
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
pub mod reddit_json;
pub mod stackexchange;
pub mod wiki_infobox;

/// Kill switch : checked once, then cached.
fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("DONSETCH_NO_ADAPTERS").is_err())
}

/// Debug capture: `DONSETCH_ADAPTER_DUMP=<dir>` writes every body
/// an adapter inspects : fixture capture for adapter development.
/// Best-effort, never a failure path.
fn debug_dump(html: &str, url: &str) {
    let Ok(dir) = std::env::var("DONSETCH_ADAPTER_DUMP") else {
        return;
    };
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
const BUILTIN_REWRITES: [BuiltinRewrite; 7] = [
    BuiltinRewrite {
        via: "adapter:reddit-json",
        description: "reddit thread and listing pages -> old.reddit .json endpoints",
        apply: reddit_json_rw,
    },
    BuiltinRewrite {
        via: "adapter:reddit-old",
        description: "other reddit pages -> the legacy SSR host old.reddit.com",
        apply: reddit_old_rw,
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

fn reddit_json_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if !matches!(host, "www.reddit.com" | "reddit.com" | "old.reddit.com") {
        return None;
    }
    let path = u.path().to_string();
    let trimmed = path.trim_end_matches('/');
    let path_part = if trimmed.is_empty() { "/" } else { trimmed };
    let is_thread = path.contains("/comments/");
    let is_listing = path == "/" || path.starts_with("/r/") || path.starts_with("/comments");
    if !(is_thread || is_listing) || path_part.ends_with(".json") {
        return None;
    }
    // Keep query (?t=top sorts) : drop fragments only.
    let mut u2 = u.clone();
    let _ = u2.set_host(Some("old.reddit.com"));
    u2.set_path(&format!("{path_part}.json"));
    u2.set_fragment(None);
    Some(u2.to_string())
}

fn reddit_old_rw(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    if !matches!(host, "www.reddit.com" | "reddit.com") {
        return None;
    }
    let mut u2 = u.clone();
    let _ = u2.set_host(Some("old.reddit.com"));
    Some(u2.to_string())
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
    let norm = pkg.to_lowercase().replace(['_', '.'], "-");
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
    if !enabled() {
        return None;
    }
    // Focus/toc/section/probe are pipeline features the adapters don't
    // reproduce: when the agent asks for a specific cut, the generic
    // path (which implements them) wins.
    if opts.focus.is_some() || opts.toc || opts.must_contain.is_some() || opts.section.is_some() {
        return None;
    }
    debug_dump(html, url);
    github::extract(html, url, opts)
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

    #[test]
    fn reddit_thread_gets_json() {
        let (u, via) = rw("https://www.reddit.com/r/rust/comments/abc123/title_here/").unwrap();
        assert_eq!(
            u,
            "https://old.reddit.com/r/rust/comments/abc123/title_here.json"
        );
        assert_eq!(via, "adapter:reddit-json");
    }

    #[test]
    fn reddit_listing_gets_json() {
        let (u, via) = rw("https://reddit.com/r/programming/?t=top").unwrap();
        assert_eq!(u, "https://old.reddit.com/r/programming.json?t=top");
        assert_eq!(via, "adapter:reddit-json");
        let (u, _) = rw("https://www.reddit.com/").unwrap();
        assert_eq!(u, "https://old.reddit.com/.json");
    }

    #[test]
    fn reddit_user_page_gets_old_domain_only() {
        let (u, via) = rw("https://www.reddit.com/user/spez/").unwrap();
        assert_eq!(u, "https://old.reddit.com/user/spez/");
        assert_eq!(via, "adapter:reddit-old");
    }

    #[test]
    fn already_json_not_double_appended() {
        assert!(rw("https://old.reddit.com/r/rust.json").is_none());
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
}
