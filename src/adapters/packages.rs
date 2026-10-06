//! Package registry adapter: npm / PyPI / crates.io / Go proxy /
//! RubyGems page URLs are rewritten to their keyless JSON APIs
//! (see `super::rewrite`); this module renders those payloads
//! into one unified, agent-first package card.
//!
//! What an agent wants from a package lookup: what it is, the
//! current version, whether it's maintained (publish dates),
//! license, repo, dependencies, and deprecation/yank warnings.
//! One compact card, zero JS-shell noise.

use serde_json::Value;

use crate::extract::{ContentKind, ExtractOptions, Extracted};

const MAX_DEPS: usize = 16;
const MAX_VERSIONS: usize = 10;

/// Entry point. `url` is the final fetched (API) URL.
pub fn extract(body: &[u8], url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_string();
    let v: Value = serde_json::from_slice(body).ok()?;

    let (via, registry, card) = if host == "registry.npmjs.org" {
        ("adapter:npm-registry", "npm", npm_card(&v)?)
    } else if host == "pypi.org" {
        ("adapter:pypi-json", "PyPI", pypi_card(&v)?)
    } else if host == "crates.io" {
        ("adapter:crates-api", "crates.io", crates_card(&v)?)
    } else if host == "proxy.golang.org" {
        ("adapter:go-proxy", "Go modules", go_card(&v, url)?)
    } else if host == "rubygems.org" {
        ("adapter:rubygems-api", "RubyGems", rubygems_card(&v)?)
    } else {
        return None;
    };

    let md = render(registry, &card);
    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    let deps_n = card.deps.len();
    Some(Extracted {
        tokens_est: slice.len() / 4,
        markdown: slice,
        title: Some(format!("{} {}", card.name, card.version)),
        byline: None,
        published: card.published.clone(),
        site: Some(registry.to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: deps_n,
        blocks_shown: deps_n,
        thin: false,
        content_kind: ContentKind::Listing,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some(via),
        partial: None,
    })
}

// ── unified card ──────────────────────────────────────────────

struct PkgCard {
    name: String,
    version: String,
    description: String,
    published: Option<String>,
    modified: Option<String>,
    license: Option<String>,
    repo: Option<String>,
    homepage: Option<String>,
    downloads: Option<u64>,
    /// Registry-specific one-liners (deprecation, requires_python…).
    extra: Vec<String>,
    deps: Vec<String>,
    /// Where deps live when this endpoint doesn't carry them.
    deps_hint: Option<String>,
    /// (version, date, yanked) newest first.
    versions: Vec<(String, String, bool)>,
}

/// Stable release: digits and dots only (no -canary, rc1, .dev…).
fn is_stable_version(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Prefer stable releases in version lists: canaries/RCs flood
/// the recent-versions window on active packages (react: 2900+
/// versions, top-10-by-date all canaries). Falls back to
/// everything when a package has (almost) no stable releases.
fn prefer_stable(mut versions: Vec<(String, String, bool)>) -> Vec<(String, String, bool)> {
    let stable: Vec<_> = versions
        .iter()
        .filter(|(n, _, _)| is_stable_version(n))
        .cloned()
        .collect();
    if stable.len() >= 3 {
        versions = stable;
    }
    versions
}

fn render(registry: &str, c: &PkgCard) -> String {
    let mut md = format!("# {} {}\n", c.name, c.version);
    if !c.description.is_empty() {
        md.push_str(&format!("{}\n", c.description));
    }
    let mut facts: Vec<String> = vec![registry.to_string()];
    if let Some(p) = &c.published {
        facts.push(format!("published {p}"));
    }
    if let Some(m) = &c.modified {
        facts.push(format!("updated {m}"));
    }
    if let Some(l) = &c.license {
        facts.push(format!("license {l}"));
    }
    if let Some(d) = c.downloads {
        facts.push(format!("{} downloads", super::human_count(d)));
    }
    md.push_str(&format!("{}\n", facts.join(" · ")));
    for e in &c.extra {
        md.push_str(&format!("**{e}**\n"));
    }
    let links: Vec<String> = [c.repo.as_deref(), c.homepage.as_deref()]
        .into_iter()
        .flatten()
        .map(String::from)
        .collect();
    if !links.is_empty() {
        md.push_str(&format!("{}\n", links.join(" · ")));
    }
    md.push('\n');

    if !c.deps.is_empty() {
        md.push_str(&format!("## Dependencies ({})\n", c.deps.len()));
        let shown: Vec<&str> = c.deps.iter().take(MAX_DEPS).map(String::as_str).collect();
        md.push_str(&shown.join(" · "));
        md.push('\n');
        if c.deps.len() > MAX_DEPS {
            md.push_str(&format!("*(+{} more)*\n", c.deps.len() - MAX_DEPS));
        }
        md.push('\n');
    } else if let Some(hint) = &c.deps_hint {
        md.push_str(&format!("## Dependencies\n{hint}\n\n"));
    }

    if !c.versions.is_empty() {
        md.push_str("## Recent versions\n");
        for (num, date, yanked) in c.versions.iter().take(MAX_VERSIONS) {
            let flag = if *yanked { " *(yanked)*" } else { "" };
            md.push_str(&format!("- {num} : {date}{flag}\n"));
        }
        md.push('\n');
    }
    md.trim_end().to_string()
}

// ── npm ───────────────────────────────────────────────────────

fn npm_card(v: &Value) -> Option<PkgCard> {
    if v.get("versions").is_some() {
        // Full packument.
        let name = v.get("name").and_then(Value::as_str)?.to_string();
        let latest = v
            .pointer("/dist-tags/latest")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let manifest = v
            .pointer(&format!("/versions/{latest}"))
            .cloned()
            .unwrap_or_default();
        let times = v.get("time").cloned().unwrap_or_default();
        let mut versions: Vec<(String, String, bool)> = times
            .as_object()?
            .iter()
            .filter(|(k, _)| !k.starts_with("modified") && !k.starts_with("created"))
            .map(|(k, t)| {
                let yanked = manifest_yanked(v, k);
                (
                    k.clone(),
                    t.as_str().map(date_of).unwrap_or_default(),
                    yanked,
                )
            })
            .collect();
        versions.sort_by(|a, b| b.1.cmp(&a.1));
        let versions = prefer_stable(versions);
        Some(PkgCard {
            description: v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            published: times.get(&latest).and_then(Value::as_str).map(date_of),
            modified: times.get("modified").and_then(Value::as_str).map(date_of),
            license: manifest
                .get("license")
                .and_then(Value::as_str)
                .map(clean_license),
            repo: npm_repo(v, &manifest),
            homepage: manifest
                .get("homepage")
                .or_else(|| v.get("homepage"))
                .and_then(Value::as_str)
                .map(String::from),
            deps: deps_map(manifest.get("dependencies")),
            extra: deprecation(&manifest),
            versions,
            deps_hint: None,
            name,
            version: latest,
            downloads: None,
        })
    } else if v.get("version").is_some() {
        // Single-version manifest (/package/x/v/<ver> or /x/<ver>).
        let manifest = v.clone();
        Some(PkgCard {
            name: manifest.get("name").and_then(Value::as_str)?.to_string(),
            version: manifest.get("version").and_then(Value::as_str)?.to_string(),
            description: manifest
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            published: None,
            modified: None,
            license: manifest
                .get("license")
                .and_then(Value::as_str)
                .map(clean_license),
            repo: npm_repo(v, &manifest),
            homepage: manifest
                .get("homepage")
                .and_then(Value::as_str)
                .map(String::from),
            deps: deps_map(manifest.get("dependencies")),
            extra: deprecation(&manifest),
            versions: Vec::new(),
            deps_hint: None,
            downloads: None,
        })
    } else {
        None
    }
}

fn manifest_yanked(_v: &Value, _k: &str) -> bool {
    false // npm has no yank; deprecation is the equivalent
}

fn npm_repo(packument: &Value, manifest: &Value) -> Option<String> {
    // repository lives on the version manifest AND the top-level
    // packument; take whichever is present.
    let r = manifest
        .get("repository")
        .or_else(|| packument.get("repository"))?;
    let url = match r {
        Value::String(s) => s.clone(),
        Value::Object(_) => r.get("url").and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    Some(
        url.trim_start_matches("git+")
            .trim_start_matches("git://")
            .replace("git@", "github.com:")
            .trim_end_matches(".git")
            .to_string(),
    )
}

fn deps_map(deps: Option<&Value>) -> Vec<String> {
    deps.and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, req)| format!("{k} {}", req.as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

fn deprecation(manifest: &Value) -> Vec<String> {
    manifest
        .get("deprecated")
        .and_then(Value::as_str)
        .map(|d| vec![format!("DEPRECATED: {d}")])
        .unwrap_or_default()
}

// ── PyPI ──────────────────────────────────────────────────────

fn pypi_card(v: &Value) -> Option<PkgCard> {
    let info = v.get("info")?;
    let name = info.get("name").and_then(Value::as_str)?.to_string();
    let version = info.get("version").and_then(Value::as_str)?.to_string();

    // Version history from releases: newest upload_time first.
    let mut versions: Vec<(String, String, bool)> = v
        .get("releases")
        .and_then(Value::as_object)
        .map(|rels| {
            rels.iter()
                .map(|(num, files)| {
                    let date = files
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|f| f.get("upload_time"))
                        .and_then(Value::as_str)
                        .map(date_of)
                        .unwrap_or_default();
                    (num.clone(), date, false)
                })
                .collect()
        })
        .unwrap_or_default();
    versions.sort_by(|a, b| b.1.cmp(&a.1));
    let versions = prefer_stable(versions);

    let urls = info.get("project_urls").and_then(Value::as_object);
    let repo = urls
        .and_then(|u| {
            u.get("Repository")
                .or_else(|| u.get("Source"))
                .or_else(|| u.get("GitHub"))
        })
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            info.get("home_page")
                .and_then(Value::as_str)
                .map(String::from)
        });
    let homepage = urls
        .and_then(|u| u.get("Homepage").or_else(|| u.get("Docs")))
        .and_then(Value::as_str)
        .map(String::from);

    let mut extra = Vec::new();
    if let Some(py) = info.get("requires_python").and_then(Value::as_str)
        && !py.is_empty()
    {
        extra.push(format!("requires Python {py}"));
    }
    // yanked equivalents: nothing on PyPI; note dead projects by date only.

    let deps: Vec<String> = info
        .get("requires_dist")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    let published = versions
        .iter()
        .find(|(n, _, _)| n == &version)
        .map(|(_, d, _)| d.clone())
        .filter(|d| !d.is_empty());

    Some(PkgCard {
        description: info
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        license: info
            .get("license")
            .and_then(Value::as_str)
            .map(clean_license),
        version,
        published,
        modified: None,
        repo,
        homepage,
        downloads: None,
        deps,
        deps_hint: None,
        versions,
        extra,
        name,
    })
}

// ── crates.io ─────────────────────────────────────────────────

fn crates_card(v: &Value) -> Option<PkgCard> {
    if let Some(c) = v.get("crate") {
        // Crate summary endpoint.
        let versions: Vec<(String, String, bool)> = v
            .get("versions")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|ver| {
                        Some((
                            ver.get("num").and_then(Value::as_str)?.to_string(),
                            ver.get("created_at")
                                .and_then(Value::as_str)
                                .map(date_of)
                                .unwrap_or_default(),
                            ver.get("yanked").and_then(Value::as_bool).unwrap_or(false),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut sorted = versions;
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let sorted = prefer_stable(sorted);
        let version = c
            .get("newest_version")
            .or_else(|| c.get("max_version"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let deps_hint = format!(
            "deps live per version : fetch crates.io/crates/{}/{version} for the tree",
            c.get("name").and_then(Value::as_str).unwrap_or("?")
        );
        Some(PkgCard {
            name: c.get("name").and_then(Value::as_str)?.to_string(),
            version,
            description: c
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            published: sorted
                .first()
                .map(|(_, d, _)| d.clone())
                .filter(|d| !d.is_empty()),
            modified: c.get("updated_at").and_then(Value::as_str).map(date_of),
            license: c
                .get("license")
                .and_then(Value::as_str)
                .map(|l| l.replace('/', " OR ")),
            repo: c
                .get("repository")
                .and_then(Value::as_str)
                .map(String::from),
            homepage: c
                .get("documentation")
                .or_else(|| c.get("homepage"))
                .and_then(Value::as_str)
                .map(String::from),
            downloads: c.get("downloads").and_then(Value::as_u64),
            extra: Vec::new(),
            deps: Vec::new(),
            deps_hint: Some(deps_hint),
            versions: sorted,
        })
    } else if v.get("version").is_some() {
        // Version endpoint (/api/v1/crates/<n>/<v>) : carries deps.
        let ver = v.get("version")?;
        let deps: Vec<String> = ver
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|d| d.get("kind").and_then(Value::as_str) == Some("normal"))
                    .filter_map(|d| {
                        Some(format!(
                            "{} {}",
                            d.get("crate_id").and_then(Value::as_str)?,
                            d.get("req").and_then(Value::as_str).unwrap_or("")
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let n_dev = ver
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|d| d.get("kind").and_then(Value::as_str) == Some("dev"))
                    .count()
            })
            .unwrap_or(0);
        let mut extra = Vec::new();
        if ver.get("yanked").and_then(Value::as_bool).unwrap_or(false) {
            extra.push("YANKED".into());
        }
        if n_dev > 0 {
            extra.push(format!("+{n_dev} dev-dependencies"));
        }
        Some(PkgCard {
            name: ver
                .get("crate")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            version: ver.get("num").and_then(Value::as_str)?.to_string(),
            description: ver
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            published: ver.get("created_at").and_then(Value::as_str).map(date_of),
            modified: None,
            license: ver
                .get("license")
                .and_then(Value::as_str)
                .map(|l| l.replace('/', " OR ")),
            repo: None,
            homepage: None,
            downloads: ver.get("downloads").and_then(Value::as_u64),
            extra,
            deps,
            deps_hint: None,
            versions: Vec::new(),
        })
    } else if let Some(arr) = v.get("versions").and_then(Value::as_array) {
        // /crates/<name>/versions : {"versions":[...]} as served
        // (newest first), one object per release. This shape used
        // to fall through to the raw-JSON passthrough; render the
        // same card shape as the summary endpoint.
        let first = arr.first()?;
        let name = first.get("crate").and_then(Value::as_str)?.to_string();
        let mut all: Vec<(String, String, bool)> = arr
            .iter()
            .filter_map(|ver| {
                Some((
                    ver.get("num").and_then(Value::as_str)?.to_string(),
                    ver.get("created_at")
                        .and_then(Value::as_str)
                        .map(date_of)
                        .unwrap_or_default(),
                    ver.get("yanked").and_then(Value::as_bool).unwrap_or(false),
                ))
            })
            .collect();
        all.sort_by(|a, b| b.1.cmp(&a.1));
        let headline = all.first().map(|(n, _, _)| n.clone()).unwrap_or_default();
        let published = all
            .first()
            .map(|(_, d, _)| d.clone())
            .filter(|d| !d.is_empty());
        let versions = prefer_stable(all);
        let mut extra = Vec::new();
        if arr.len() > MAX_VERSIONS {
            extra.push(format!("{} versions total", arr.len()));
        }
        Some(PkgCard {
            description: String::new(),
            published,
            modified: None,
            license: first
                .get("license")
                .and_then(Value::as_str)
                .map(|l| l.replace('/', " OR ")),
            repo: None,
            homepage: None,
            downloads: first.get("downloads").and_then(Value::as_u64),
            extra,
            deps: Vec::new(),
            deps_hint: Some(format!(
                "deps live per version : fetch crates.io/crates/{name}/{headline} for the tree"
            )),
            versions,
            version: headline,
            name,
        })
    } else {
        None
    }
}

// ── Go proxy ──────────────────────────────────────────────────

fn go_card(v: &Value, url: &str) -> Option<PkgCard> {
    let version = v.get("Version").and_then(Value::as_str)?.to_string();
    // The proxy payload carries no name : the module path IS the
    // name; recover it from the URL (`/<module>/@latest` or the
    // version-pinned `/<module>/@v/<version>.info`).
    let name = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let p = u.path().to_string();
            if let Some(m) = p.strip_suffix("/@latest") {
                return Some(m.trim_start_matches('/').to_string());
            }
            p.split_once("/@v/")
                .map(|(m, _)| m.trim_start_matches('/').to_string())
        })
        .unwrap_or_default();
    let date = v.get("Time").and_then(Value::as_str).map(date_of);
    Some(PkgCard {
        name,
        description: String::new(),
        published: date.clone(),
        license: None,
        repo: None,
        homepage: None,
        downloads: None,
        extra: Vec::new(),
        deps: Vec::new(),
        deps_hint: Some("deps: the pkg.go.dev page".into()),
        versions: vec![(version.clone(), date.unwrap_or_default(), false)],
        version,
        modified: None,
    })
}

// ── RubyGems ──────────────────────────────────────────────────

fn rubygems_card(v: &Value) -> Option<PkgCard> {
    let name = v.get("name").and_then(Value::as_str)?.to_string();
    let version = v.get("version").and_then(Value::as_str)?.to_string();
    let deps: Vec<String> = v
        .pointer("/dependencies/runtime")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    Some(format!(
                        "{} {}",
                        d.get("name").and_then(Value::as_str)?,
                        d.get("requirements").and_then(Value::as_str).unwrap_or("")
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let n_dev = v
        .pointer("/dependencies/development")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut extra = Vec::new();
    if n_dev > 0 {
        extra.push(format!("+{n_dev} dev-dependencies"));
    }
    let license = v.get("licenses").and_then(Value::as_array).map(|l| {
        l.iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" OR ")
    });
    Some(PkgCard {
        description: v.get("info").and_then(Value::as_str).unwrap_or("").into(),
        published: v.get("created_at").and_then(Value::as_str).map(date_of),
        modified: v.get("updated_at").and_then(Value::as_str).map(date_of),
        repo: v
            .get("source_code_uri")
            .and_then(Value::as_str)
            .map(String::from),
        homepage: v
            .get("homepage_uri")
            .and_then(Value::as_str)
            .map(String::from),
        downloads: v.get("downloads").and_then(Value::as_u64),
        license,
        deps,
        deps_hint: None,
        versions: Vec::new(),
        extra,
        version,
        name,
    })
}

// ── shared helpers ────────────────────────────────────────────

/// ISO timestamp → date part (first 10 chars).
fn date_of(t: &str) -> String {
    t.chars().take(10).collect()
}

/// License strings sometimes carry full text; keep the label.
fn clean_license(l: &str) -> String {
    let l = l.trim();
    if l.chars().count() > 40 {
        l.chars().take(40).collect()
    } else {
        l.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const NPM: &str = r#"{
      "name":"react","description":"React is a JS library",
      "dist-tags":{"latest":"19.1.0"},
      "versions":{"19.1.0":{"license":"MIT","dependencies":{"loose-envify":"^1.1.0"}},
                  "19.0.0":{"license":"MIT"}},
      "time":{"modified":"2026-05-01T00:00:00.000Z","created":"2013-05-24T00:00:00.000Z",
              "19.1.0":"2026-04-20T00:00:00.000Z","19.0.0":"2025-12-10T00:00:00.000Z"},
      "homepage":"https://react.dev",
      "repository":{"type":"git","url":"git+https://github.com/facebook/react.git"}
    }"#;

    #[test]
    fn npm_packument_card() {
        let ex = extract(NPM.as_bytes(), "https://registry.npmjs.org/react", &opts()).unwrap();
        assert_eq!(ex.via, Some("adapter:npm-registry"));
        assert!(ex.markdown.contains("# react 19.1.0"));
        assert!(ex.markdown.contains("React is a JS library"));
        assert!(ex.markdown.contains("license MIT"));
        assert!(ex.markdown.contains("loose-envify ^1.1.0"));
        assert!(ex.markdown.contains("github.com/facebook/react"));
        assert!(ex.markdown.contains("published 2026-04-20"));
        assert!(ex.markdown.contains("## Recent versions"));
    }

    #[test]
    fn npm_deprecated_manifest() {
        let m = r#"{"name":"old-pkg","version":"1.0.0","deprecated":"This package is no longer supported.",
                    "license":"ISC","dependencies":{"a":"^1.0.0"},"description":"x"}"#;
        let ex = extract(
            m.as_bytes(),
            "https://registry.npmjs.org/old-pkg/1.0.0",
            &opts(),
        )
        .unwrap();
        assert!(
            ex.markdown
                .contains("DEPRECATED: This package is no longer supported.")
        );
    }

    #[test]
    fn pypi_card() {
        let p = r#"{"info":{"name":"Flask","version":"3.1.0","summary":"A simple framework",
          "requires_python":">=3.9","home_page":"https://flask.palletsprojects.com",
          "license":"BSD-3-Clause",
          "project_urls":{"Repository":"https://github.com/pallets/flask"},
          "requires_dist":["Werkzeug>=3.1","Jinja2>=3.0"]},
          "releases":{"3.1.0":[{"upload_time":"2026-01-30T00:00:00"}],
                      "3.0.0":[{"upload_time":"2025-01-30T00:00:00"}]}}"#;
        let ex = extract(p.as_bytes(), "https://pypi.org/pypi/flask/json", &opts()).unwrap();
        assert_eq!(ex.via, Some("adapter:pypi-json"));
        assert!(ex.markdown.contains("# Flask 3.1.0"));
        assert!(ex.markdown.contains("requires Python >=3.9"));
        assert!(ex.markdown.contains("Werkzeug>=3.1"));
        assert!(ex.markdown.contains("github.com/pallets/flask"));
    }

    #[test]
    fn crates_summary_card() {
        let c = r#"{"crate":{"name":"serde","max_version":"1.0.220","newest_version":"1.0.220",
          "description":"A generic serialization/deserialization framework",
          "downloads":100000000,"updated_at":"2026-06-01T00:00:00.000Z",
          "repository":"https://github.com/serde-rs/serde","documentation":"https://serde.rs",
          "license":"MIT OR Apache-2.0"},
          "versions":[{"num":"1.0.220","created_at":"2026-06-01T00:00:00.000Z","yanked":false},
                      {"num":"1.0.219","created_at":"2026-05-01T00:00:00.000Z","yanked":true}]}"#;
        let ex = extract(
            c.as_bytes(),
            "https://crates.io/api/v1/crates/serde",
            &opts(),
        )
        .unwrap();
        assert_eq!(ex.via, Some("adapter:crates-api"));
        assert!(ex.markdown.contains("# serde 1.0.220"));
        assert!(ex.markdown.contains("downloads"));
        assert!(ex.markdown.contains("*(yanked)*"));
        assert!(ex.markdown.contains("per version"));
    }

    #[test]
    fn go_latest_card() {
        let g = r#"{"Version":"v1.10.0","Time":"2025-03-01T00:00:00Z"}"#;
        let ex = extract(
            g.as_bytes(),
            "https://proxy.golang.org/github.com/gin-gonic/gin/@latest",
            &opts(),
        )
        .unwrap();
        assert_eq!(ex.via, Some("adapter:go-proxy"));
        assert!(ex.markdown.contains("v1.10.0"));
        assert!(ex.markdown.contains("2025-03-01"));
    }

    #[test]
    fn rubygems_card() {
        let r = r#"{"name":"rails","version":"8.0.1","info":"Full-stack web framework",
          "downloads":200000000,"homepage_uri":"https://rubyonrails.org",
          "source_code_uri":"https://github.com/rails/rails","licenses":["MIT"],
          "created_at":"2016-04-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z",
          "dependencies":{"runtime":[{"name":"rack","requirements":">= 2.2"},{"name":"sprockets","requirements":"~> 4.0"}],
                          "development":[{"name":"mocha","requirements":"~> 2.1"}]}}"#;
        let ex = extract(
            r.as_bytes(),
            "https://rubygems.org/api/v1/gems/rails.json",
            &opts(),
        )
        .unwrap();
        assert!(ex.markdown.contains("# rails 8.0.1"));
        assert!(ex.markdown.contains("rack >= 2.2"));
        assert!(ex.markdown.contains("+1 dev-dependencies"));
        assert!(ex.markdown.contains("license MIT"));
    }

    #[test]
    fn go_version_pinned_card_recovers_the_module_name() {
        let g = r#"{"Version":"v1.10.0","Time":"2024-05-07T03:23:42Z",
          "Origin":{"URL":"https://github.com/gin-gonic/gin"}}"#;
        let ex = extract(
            g.as_bytes(),
            "https://proxy.golang.org/github.com/gin-gonic/gin/@v/v1.10.0.info",
            &opts(),
        )
        .unwrap();
        assert!(ex.markdown.contains("# github.com/gin-gonic/gin v1.10.0"));
        assert!(ex.markdown.contains("2024-05-07"));
    }

    #[test]
    fn crates_versions_list_card() {
        let c = r#"{"versions":[
          {"crate":"serde","num":"1.0.229","created_at":"2026-07-18T23:05:13Z","yanked":false,"downloads":95220782,"license":"MIT OR Apache-2.0"},
          {"crate":"serde","num":"1.0.228","created_at":"2025-09-27T00:00:00Z","yanked":false,"downloads":1,"license":"MIT OR Apache-2.0"},
          {"crate":"serde","num":"1.0.95","created_at":"2019-07-16T00:00:00Z","yanked":true,"downloads":2,"license":"MIT OR Apache-2.0"}],
          "meta":{"total":316}}"#;
        let ex = extract(
            c.as_bytes(),
            "https://crates.io/api/v1/crates/serde/versions",
            &opts(),
        )
        .unwrap();
        assert_eq!(ex.via, Some("adapter:crates-api"));
        assert!(ex.markdown.contains("# serde 1.0.229"));
        assert!(ex.markdown.contains("95.2M downloads"));
        assert!(ex.markdown.contains("*(yanked)*"));
        assert!(ex.markdown.contains("fetch crates.io/crates/serde/1.0.229"));
    }

    #[test]
    fn wrong_host_rejected() {
        assert!(extract(NPM.as_bytes(), "https://example.com/react", &opts()).is_none());
    }
}
