//! User-side rewrite adapters (phase-5.3 registry v1).
//!
//! Pure-data plugins: one JSON file per rewrite rule, loaded from
//! `cache_dir()/adapters/` on first use, sorted by filename, capped.
//! No executables, no scripts, no regex: a plugin can only rewrite a
//! matched URL to another https URL and narrow which hosts fire, so
//! unsigned local trust here is honest. The fetcher's own egress
//! guards still apply to whatever a plugin rewrites to.
//!
//! File contract (strict, violations = skip + one stderr receipt,
//! never a failure path, law 5 shape):
//! - "name": `[a-z0-9][a-z0-9-]{0,39}`, unique among user files; a
//!   name that would collide with a bundled adapter's via suffix is
//!   skipped (builtins are untouchable).
//! - "hosts": array of 1..=8 DNS hostnames, exact match only. Ports
//!   and IP literals are rejected.
//! - "path_prefix": optional, must start with `/`.
//! - "target": an https:// URL, no userinfo, no port, no IP-literal
//!   host, no `?`, no fragment, containing `{path}` exactly once
//!   (in the target's path).
//!
//! Rewrite semantics: `target` with `{path}` replaced by the input
//! path, then the input's query string is carried over. The target
//! scheme must be https (no downgrades); the input scheme does not
//! matter, upgrades win.

use url::Url;

/// Hard caps so a runaway adapters dir cannot stall a fetch: 64
/// files, 64 KiB each.
const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// One user rewrite adapter as parsed from a plugin file.
#[derive(Debug, Clone, PartialEq)]
pub struct UserRewrite {
    pub name: String,
    pub hosts: Vec<String>,
    pub path_prefix: Option<String>,
    /// https URL template with exactly one `{path}` placeholder.
    pub target: String,
}

/// Catalog entry: the parsed plugin plus its via label
/// (`adapter:u:<name>`). The label is leaked once per loaded plugin
/// (max 64 rows total) so the dispatch needs no per-entry work.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRow {
    pub plugin: UserRewrite,
    pub via: &'static str,
}

pub const VIA_PREFIX: &str = "adapter:u:";

/// Parse one plugin file's text. `what` names the file in the
/// receipt ("rules.json"). Errors are precise: the first rule broken
/// wins, quoted in the receipt.
pub fn parse(text: &str, what: &str) -> Result<UserRewrite, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = v.as_object().ok_or("top level must be a JSON object")?;

    let name = obj_str(obj, "name")?;
    is_name_ok(name, what)?;

    let hosts_json = obj
        .get("hosts")
        .and_then(|h| h.as_array())
        .filter(|a| !a.is_empty())
        .ok_or("hosts must be a non-empty array")?;
    if hosts_json.len() > 8 {
        return Err(format!("hosts has {} entries (max 8)", hosts_json.len()));
    }
    let mut hosts = Vec::with_capacity(hosts_json.len());
    for h in hosts_json {
        let h = h
            .as_str()
            .filter(|s| is_dns_host(s))
            .ok_or("every host must be a DNS hostname (no ports, no IP literals)")?;
        hosts.push(h.to_ascii_lowercase());
    }

    let path_prefix = match obj.get("path_prefix") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => {
            if s.is_empty() || !s.starts_with('/') {
                return Err("path_prefix must start with '/'".to_string());
            }
            Some(s.clone())
        }
        Some(_) => return Err("path_prefix must be a string".to_string()),
    };

    let target = obj_str(obj, "target")?;
    is_target_ok(target)?;

    Ok(UserRewrite {
        name: name.to_string(),
        hosts,
        path_prefix,
        target: target.to_string(),
    })
}

/// Apply one user plugin to an input URL. Returns the rewritten URL
/// string, or None when the plugin does not match.
pub fn apply(spec: &UserRewrite, u: &Url) -> Option<String> {
    let host = u.host_str()?;
    if !spec.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return None;
    }
    let path = u.path();
    if !path.starts_with('/') {
        return None;
    }
    if let Some(pfx) = &spec.path_prefix
        && !path.starts_with(pfx.as_str())
    {
        return None;
    }
    let new_path = spec.target.replacen("{path}", path, 1);
    let mut out = Url::parse(&new_path).ok()?;
    out.set_fragment(None);
    match u.query() {
        Some(q) => out.set_query(Some(q)),
        None => out.set_query(None),
    }
    // Guard against a template that parses weirdly after substitution
    // (e.g. a stray scheme-looking token): the result must still be a
    // clean https URL.
    if out.scheme() != "https" || out.port().is_some() || out.host_str().is_none() {
        return None;
    }
    Some(out.to_string())
}

/// Load the user catalog once per process. Sorted by file name for
/// determinism; first name wins on duplicates; receipts only, never
/// a hard failure.
pub fn catalog() -> &'static [CatalogRow] {
    catalog_state().rows.as_slice()
}

/// `(rows, skipped_files)` for `donsetch adapters`: rows = the
/// loaded user plugins, skipped = the files that failed their gate.
pub fn catalog_stats() -> (usize, usize) {
    let c = catalog_state();
    (c.rows.len(), c.skips)
}

struct LoadedCatalog {
    rows: Vec<CatalogRow>,
    skips: usize,
}

fn catalog_state() -> &'static LoadedCatalog {
    static CATALOG: std::sync::OnceLock<LoadedCatalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(load)
}

fn load() -> LoadedCatalog {
    let dir = crate::paths::cache_dir().join("adapters");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return LoadedCatalog {
            rows: Vec::new(),
            skips: 0,
        };
    };
    let mut files: Vec<std::path::PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            if !p.is_file() {
                return false;
            }
            let ext = p.extension().is_some_and(|e| e == "json");
            let visible = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'));
            ext && visible
        })
        .collect();
    files.sort();
    files.truncate(MAX_FILES);

    let mut skips = 0usize;
    let mut rows: Vec<CatalogRow> = Vec::new();
    for f in files {
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<non-utf8>")
            .to_string();
        let Ok(meta) = std::fs::metadata(&f) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            eprintln!(
                "[adapters] skip plugins/{name}: file is {} bytes (max {MAX_FILE_BYTES})",
                meta.len()
            );
            skips += 1;
            continue;
        }
        let text = match std::fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[adapters] skip plugins/{name}: unreadable: {e}");
                skips += 1;
                continue;
            }
        };
        match parse(&text, &name) {
            Ok(plugin) => {
                if rows.iter().any(|r| r.plugin.name == plugin.name) {
                    eprintln!(
                        "[adapters] skip plugins/{name}: adapter '{}' already loaded from an earlier file",
                        plugin.name
                    );
                    skips += 1;
                    continue;
                }
                if crate::adapters::builtin_via_suffixes().contains(&plugin.name.as_str()) {
                    eprintln!(
                        "[adapters] skip plugins/{name}: name '{}' is a bundled adapter name",
                        plugin.name
                    );
                    skips += 1;
                    continue;
                }
                let via: &'static str =
                    Box::leak(format!("{VIA_PREFIX}{}", plugin.name).into_boxed_str());
                rows.push(CatalogRow { plugin, via });
            }
            Err(e) => {
                eprintln!("[adapters] skip plugins/{name}: {e}");
                skips += 1;
            }
        }
    }
    LoadedCatalog { rows, skips }
}

fn obj_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<&'a str, String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or(format!("{key} must be a non-empty string"))
}

/// `[a-z0-9][a-z0-9-]{0,39}`
fn is_name_ok(name: &str, what: &str) -> Result<(), String> {
    let chars: Vec<char> = name.chars().collect();
    if chars.is_empty() || chars.len() > 40 {
        return Err(format!("{what}: name must be 1..40 chars"));
    }
    let ok = chars[0].is_ascii_lowercase() || chars[0].is_ascii_digit();
    let rest = chars[1..]
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-');
    if ok && rest {
        Ok(())
    } else {
        Err(format!("{what}: name must match [a-z0-9][a-z0-9-]*"))
    }
}

/// DNS hostname: no ports, no IP literals, no dangerous chars. A
/// single label (like `localhost`) is fine: the egress guard still
/// applies at fetch time.
fn is_dns_host(h: &str) -> bool {
    if h.is_empty() || h.contains(':') || h.len() > 253 {
        return false;
    }
    // IP literal (v4: digits+dots; v6: contains ':', already rejected)
    if !h.is_empty() && h.split('.').all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return false;
    }
    h.split('.').all(|label| {
        !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// The target gate: https, no userinfo, no port, no IP-literal
/// host, no query/fragment, and `{path}` in the target's path only.
fn is_target_ok(target: &str) -> Result<(), String> {
    let u = Url::parse(target).map_err(|e| format!("target is not a URL: {e}"))?;
    if u.scheme() != "https" {
        return Err("target must be https (no downgrades)".to_string());
    }
    if u.username() != "" || u.password().is_some() {
        return Err("target must not carry userinfo".to_string());
    }
    if u.port().is_some() {
        return Err("target must not carry a port".to_string());
    }
    let Some(host) = u.host_str() else {
        return Err("target must carry a host".to_string());
    };
    if !is_dns_host(host) {
        return Err("target host must be a DNS hostname".to_string());
    }
    if u.query().is_some() {
        return Err("target must not contain a query string".to_string());
    }
    if u.fragment().is_some() {
        return Err("target must not contain a fragment".to_string());
    }
    if u.path().contains("{path}") || target.matches("{path}").count() != 1 {
        return Err("target must contain exactly one {{path}} in its path".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(v: serde_json::Value) -> Result<UserRewrite, String> {
        parse(&v.to_string(), "rules.json")
    }

    fn good(mut extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({
            "name": "docs-md",
            "hosts": ["docs.example.io"],
            "target": "https://docs.example.io/api/v1{path}.json"
        });
        if let (serde_json::Value::Object(base), serde_json::Value::Object(e)) =
            (&mut v, &mut extra)
        {
            for (k, val) in e.iter() {
                base.insert(k.clone(), val.clone());
            }
        }
        v
    }

    #[test]
    fn happy_minimal_and_with_prefix() {
        let r = p(good(json!({}))).unwrap();
        assert_eq!(r.name, "docs-md");
        assert_eq!(r.hosts, vec!["docs.example.io".to_string()]);
        assert!(r.path_prefix.is_none());
        let r = p(good(json!({"path_prefix": "/projects/"}))).unwrap();
        assert_eq!(r.path_prefix.as_deref(), Some("/projects/"));
    }

    #[test]
    fn missing_required_keys_are_clean_skips() {
        // No Index panics: a missing key = one honest receipt text,
        // the same shape for every required field.
        assert_eq!(
            parse(r#"{"name": "gh-api"}"#, "a.json").unwrap_err(),
            "hosts must be a non-empty array"
        );
        let e = parse(
            r#"{"hosts": ["b.io"], "target": "https://b.io/{path}"}"#,
            "a.json",
        )
        .unwrap_err();
        assert!(e.contains("name"), "got: {e}");
        let e = parse(r#"{"name": "a", "hosts": ["b.io"]}"#, "a.json").unwrap_err();
        assert!(e.contains("target"), "got: {e}");
        // Non-string field values also land in the receipt.
        assert!(
            parse(
                r#"{"name": 5, "hosts": ["b.io"], "target": "https://b.io/{path}"}"#,
                "x"
            )
            .is_err()
        );
        assert!(parse("{}", "a.json").is_err());
        assert!(parse("[]", "a.json").is_err());
    }

    #[test]
    fn name_rules() {
        assert!(parse(&good(json!({"name": "Bad_Name"})).to_string(), "x").is_err());
        assert!(parse(&good(json!({"name": ""})).to_string(), "x").is_err());
        assert!(parse(&good(json!({"name": "a"})).to_string(), "x").is_ok());
        assert!(parse(&good(json!({"name": "-bad"})).to_string(), "x").is_err());
        let long = "a".repeat(41);
        assert!(parse(&good(json!({"name": long})).to_string(), "x").is_err());
    }

    #[test]
    fn host_rules() {
        assert!(p(json!({"name":"a","hosts":[] ,"target":"https://b.io/{path}"})).is_err());
        assert!(p(json!({"name":"a","hosts":"b.io","target":"https://b.io/{path}"})).is_err());
        assert!(
            p(json!({"name":"a","hosts":["b.io:443"],"target":"https://b.io/{path}"})).is_err()
        );
        assert!(
            p(json!({"name":"a","hosts":["127.0.0.1"],"target":"https://b.io/{path}"})).is_err()
        );
        // 9 hosts = too many
        let nine: Vec<String> = (0..9).map(|i| format!("h{i}.io")).collect();
        assert!(
            p(serde_json::json!({"name":"a","hosts":nine,"target":"https://b.io/{path}"})).is_err()
        );
    }

    #[test]
    fn target_rules() {
        // http downgrade
        assert!(p(json!({"name":"a","hosts":["b.io"],"target":"http://b.io/{path}"})).is_err());
        // port
        assert!(
            p(json!({"name":"a","hosts":["b.io"],"target":"https://b.io:8443/{path}"})).is_err()
        );
        // query
        assert!(
            p(json!({"name":"a","hosts":["b.io"],"target":"https://b.io/p{path}?x=1"})).is_err()
        );
        // fragment
        assert!(p(json!({"name":"a","hosts":["b.io"],"target":"https://b.io/{path}#f"})).is_err());
        // userinfo
        assert!(p(json!({"name":"a","hosts":["b.io"],"target":"https://op@b.io/{path}"})).is_err());
        // missing placeholder
        assert!(p(json!({"name":"a","hosts":["b.io"],"target":"https://b.io/api"})).is_err());
        // placeholder in host is nonsense: the URL parser sees it
        assert!(p(json!({"name":"a","hosts":["b.io"],"target":"https://{path}.b.io/x"})).is_err());
        // two placeholders
        assert!(
            p(json!({"name":"a","hosts":["b.io"],"target":"https://b.io/{path}{path}"})).is_err()
        );
        // not a URL
        assert!(p(json!({"name":"a","hosts":["b.io"],"target":"please"})).is_err());
    }

    #[test]
    fn apply_matches_and_rewrites() {
        let r = p(good(json!({"path_prefix": "/projects/"}))).unwrap();
        let u = Url::parse("https://docs.example.io/projects/foo/bar?x=1#frag").unwrap();
        let got = apply(&r, &u).unwrap();
        assert_eq!(
            got,
            "https://docs.example.io/api/v1/projects/foo/bar.json?x=1"
        );
        // wrong host
        let u2 = Url::parse("https://other.example.io/projects/foo").unwrap();
        assert!(apply(&r, &u2).is_none());
        // prefix miss
        let u3 = Url::parse("https://docs.example.io/other/foo").unwrap();
        assert!(apply(&r, &u3).is_none());
        // no query, no fragment leakage
        let u4 = Url::parse("https://docs.example.io/projects/ok").unwrap();
        assert_eq!(
            apply(&r, &u4).unwrap(),
            "https://docs.example.io/api/v1/projects/ok.json"
        );
    }

    #[test]
    fn apply_is_host_case_insensitive() {
        let r = p(good(json!({}))).unwrap();
        let u = Url::parse("https://DOCS.EXAMPLE.IO/api/x").unwrap();
        assert!(apply(&r, &u).is_some());
    }
}
