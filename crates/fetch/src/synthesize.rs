//! Dual-Plane Fusion: combine the materialized DOM with causal wire-transcript
//! payloads when client rendering is graphical (Canvas/SVG/WebGL), collapsed,
//! or incomplete despite 2xx API responses already arriving on the wire.

use crate::engine::TranscriptEntry;
use crate::hydrate::{assess_html_shell, escape_attr, escape_html, is_telemetry_url};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
pub struct SynthesizedDocument {
    pub html: String,
    #[allow(dead_code)]
    pub text_chars: usize,
    pub note: String,
}

/// Fuse the materialized DOM with verified data responses from `transcript`
/// when the DOM alone lacks full main-body substance.
pub fn fuse_dom_and_transcript(
    dom_html: &str,
    page_url: &str,
    transcript: &[TranscriptEntry],
) -> Option<SynthesizedDocument> {
    if transcript.is_empty() {
        return None;
    }
    let assess = assess_html_shell(dom_html, page_url);
    // When the live DOM already rendered substantial prose or structured items,
    // leave the DOM untouched as the primary truth.
    if assess.main_text_chars >= 500 && (assess.prose_blocks >= 2 || assess.data_items >= 5) {
        return None;
    }

    let data_entries: Vec<&TranscriptEntry> = transcript
        .iter()
        .filter(|e| (200..300).contains(&e.status))
        .filter(|e| !matches!(e.kind.as_str(), "script" | "style"))
        .filter(|e| !is_telemetry_url(&e.url))
        .filter(|e| e.body.as_deref().is_some_and(|b| !b.trim().is_empty()))
        .collect();

    if data_entries.is_empty() {
        return None;
    }

    // 1. Multi-package npm registry graph (e.g. npmgraph.js.org fetching registry.npmjs.org/*).
    if let Some((title, section_html, note)) = synthesize_npm_graph(&data_entries, page_url) {
        return build_fused(dom_html, page_url, &title, &section_html, note);
    }

    // 2. Score individual transcript responses (OpenAPI, HTML fragment, Entity JSON, Collection JSON).
    let mut best: Option<(usize, String, String, String)> = None;
    // Every usable candidate, so a payload that does not answer the URL's own
    // question can be passed over for one that does.
    let mut candidates: Vec<(usize, String, String, String)> = Vec::new();
    // The request each candidate came from: its own URL says which question it
    // answered.
    let mut request_urls: Vec<String> = Vec::new();
    let mut combined_sections: Vec<String> = Vec::new();
    let mut combined_title = String::new();

    for entry in &data_entries {
        let body = entry.body.as_deref().unwrap_or_default().trim();
        if let Ok(json) = serde_json::from_str::<Value>(body) {
            if let Some(scored) = synthesize_openapi(&json, &entry.url)
                .or_else(|| synthesize_json_payload(&json, &entry.url, page_url))
            {
                let (score, title, html, note) = scored;
                if score >= 250 {
                    if combined_title.is_empty() && !title.is_empty() {
                        combined_title = title.clone();
                    }
                    if combined_sections.len() < 3 {
                        combined_sections.push(html.clone());
                    }
                }
                if best.as_ref().is_none_or(|(b, _, _, _)| score > *b) {
                    best = Some((score, title.clone(), html.clone(), note.clone()));
                }
                candidates.push((score, title, html, note));
                request_urls.push(entry.url.clone());
            }
        } else if looks_like_html_fragment(body) {
            let text_len = visible_fragment_chars(body);
            if text_len >= 200 {
                let score = text_len.min(15_000);
                let title = extract_first_heading(body).unwrap_or_default();
                let note = "synthesized html fragment from wire transcript".to_string();
                if best.as_ref().is_none_or(|(b, _, _, _)| score > *b) {
                    best = Some((score, title.clone(), body.to_string(), note.clone()));
                }
                candidates.push((score, title, body.to_string(), note));
                request_urls.push(entry.url.clone());
            }
        } else if body.len() >= 250 && (body.starts_with("# ") || body.contains("\n## ")) {
            let rendered = render_markdown_to_html(body, page_url);
            let score = body.len().min(15_000);
            let title = body
                .lines()
                .find_map(|l| l.strip_prefix("# "))
                .unwrap_or_default()
                .trim()
                .to_string();
            let note = "synthesized markdown document from wire transcript".to_string();
            if best.as_ref().is_none_or(|(b, _, _, _)| score > *b) {
                best = Some((score, title.clone(), rendered.clone(), note.clone()));
            }
            candidates.push((score, title, rendered, note));
            request_urls.push(entry.url.clone());
        }
    }

    let (score, title, primary_html, note) = best?;
    if score < 180 {
        return None;
    }
    // The answer must answer the question. When the caller's URL carries a
    // search query, a fused payload that never mentions it is a different
    // question answered well — Target's search page fetched its store list and
    // delivery options alongside the products, and the store list scored higher.
    // Non-search query parameters (utm_*, intl=nosplash, tab, ref) never trigger
    // this filter, so entity/OpenAPI/fragment pages with tracking params stay intact.
    let terms = url_terms(page_url);
    let primary_html = if terms.is_empty() {
        primary_html
    } else {
        let mentions = |html: &str| {
            let lower = html.to_ascii_lowercase();
            terms.iter().any(|term| lower.contains(term.as_str()))
        };
        let collections: Vec<_> = candidates
            .iter()
            .enumerate()
            .filter(|(_, (_, _, _, note))| note.contains(" items from wire transcript"))
            .collect();
        if collections.is_empty() {
            // No JSON collection answered; keep a non-collection candidate only
            // if its request URL or rendered body actually answers the query.
            let matched = candidates
                .iter()
                .enumerate()
                .filter(|(index, (score, _, html, _))| {
                    *score >= 180
                        && (request_urls.get(*index).is_some_and(|url| {
                            let low = url.to_ascii_lowercase();
                            terms.iter().any(|term| low.contains(term.as_str()))
                        }) || mentions(html))
                })
                .max_by_key(|(_, (score, _, _, _))| *score);
            match matched {
                Some((_, (_, _, html, _))) => html.clone(),
                None => return None,
            }
        } else {
            // The request that asked the question names it too: Target's product
            // list comes from a request whose own URL carries `keyword=headphones`,
            // while the side panel's survey does not. Prefer that, then the body.
            let asked_in_url = collections
                .iter()
                .filter_map(|(index, (score, _, html, _))| {
                    let url = request_urls.get(*index)?;
                    let low = url.to_ascii_lowercase();
                    (*score >= 180 && terms.iter().any(|term| low.contains(term.as_str())))
                        .then(|| (*score, html.clone()))
                })
                .max_by_key(|(score, _)| *score)
                .map(|(_, html)| html);
            match asked_in_url {
                Some(html) => html,
                None => {
                    let asked = collections
                        .iter()
                        .filter(|(_, (score, _, html, _))| *score >= 180 && mentions(html))
                        .max_by_key(|(_, (score, _, _, _))| *score);
                    match asked {
                        Some((_, (_, _, html, _))) => html.clone(),
                        None if mentions(&primary_html) => primary_html,
                        None => return None,
                    }
                }
            }
        }
    };
    let section_html = if terms.is_empty() && combined_sections.len() > 1 {
        combined_sections.join("\n")
    } else {
        primary_html
    };
    let final_title = if !title.is_empty() {
        title
    } else {
        combined_title
    };
    build_fused(dom_html, page_url, &final_title, &section_html, note)
}

/// The words a search URL asks about: values of search-query parameters only,
/// split into words of three characters or more, lowercased. Tracking, paging,
/// and market parameters (`utm_*`, `intl`, `ref`, `page`, `sort`) are ignored
/// so non-search SPAs keep entity/OpenAPI/fragment synthesis.
fn url_terms(page_url: &str) -> Vec<String> {
    const QUERY_KEYS: &[&str] = &[
        "q",
        "query",
        "keyword",
        "keywords",
        "search",
        "searchterm",
        "search_query",
        "term",
        "terms",
        "st",
        "k",
        "s",
    ];
    let Ok(parsed) = url::Url::parse(page_url) else {
        return Vec::new();
    };
    let mut terms: Vec<String> = Vec::new();
    for (name, value) in parsed.query_pairs() {
        if !QUERY_KEYS.iter().any(|key| name.eq_ignore_ascii_case(key)) {
            continue;
        }
        for word in value.split(|c: char| !c.is_alphanumeric()) {
            let word = word.trim().to_ascii_lowercase();
            if word.chars().count() >= 3 {
                terms.push(word);
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
}

fn build_fused(
    dom_html: &str,
    page_url: &str,
    title: &str,
    section_html: &str,
    note: String,
) -> Option<SynthesizedDocument> {
    let mut html = if let Some(idx) = dom_html.rfind("</body>") {
        let mut merged = String::with_capacity(dom_html.len() + section_html.len() + 128);
        merged.push_str(&dom_html[..idx]);
        merged.push_str("<section data-synthesized=\"transcript\">");
        merged.push_str(section_html);
        merged.push_str("</section>");
        merged.push_str(&dom_html[idx..]);
        merged
    } else {
        format!(
            "<!DOCTYPE html><html><head><title>{}</title></head><body><main>{}</main></body></html>",
            escape_html(title),
            section_html
        )
    };

    if !title.is_empty() && !html.to_ascii_lowercase().contains("<title>") {
        if let Some(idx) = html.find("<head>") {
            let insert_at = idx + "<head>".len();
            html.insert_str(insert_at, &format!("<title>{}</title>", escape_html(title)));
        }
    }

    let verdict = crate::contract::verify(&html, page_url, &crate::Contract::generic());
    if verdict.text_chars < 200 {
        return None;
    }
    Some(SynthesizedDocument {
        html,
        text_chars: verdict.text_chars,
        note,
    })
}

type NpmGraphPkg = (String, String, String, String, Vec<(String, String)>);

fn synthesize_npm_graph(
    entries: &[&TranscriptEntry],
    page_url: &str,
) -> Option<(String, String, String)> {
    let mut packages: Vec<NpmGraphPkg> = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        if !entry.url.contains("registry.npmjs.org/") {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(entry.body.as_deref()?) else {
            continue;
        };
        let Some(name) = json.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(name.to_string()) {
            continue;
        }
        let latest = json
            .pointer("/dist-tags/latest")
            .and_then(Value::as_str)
            .or_else(|| json.get("version").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        let version_obj = json
            .get("versions")
            .and_then(|v| v.get(&latest))
            .unwrap_or(&json);
        let desc = version_obj
            .get("description")
            .or_else(|| json.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let license = version_obj
            .get("license")
            .or_else(|| json.get("license"))
            .and_then(|l| {
                l.as_str()
                    .map(str::to_string)
                    .or_else(|| l.get("type").and_then(Value::as_str).map(str::to_string))
            })
            .unwrap_or_default();
        let deps: Vec<(String, String)> = version_obj
            .get("dependencies")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        packages.push((name.to_string(), latest, desc, license, deps));
    }
    if packages.is_empty() {
        return None;
    }
    let (root_name, root_ver, root_desc, root_lic, root_deps) = &packages[0];
    let title = format!("{root_name}@{root_ver} dependency graph");
    let mut out = String::new();
    out.push_str(&format!(
        "<article><h1>{} ({})</h1>",
        escape_html(root_name),
        escape_html(root_ver)
    ));
    if !root_desc.is_empty() {
        out.push_str(&format!("<p>{}</p>", escape_html(root_desc)));
    }
    out.push_str(&format!(
        "<p>Total resolved packages in graph: {} · Direct dependencies: {}{}</p>",
        packages.len(),
        root_deps.len(),
        if root_lic.is_empty() {
            String::new()
        } else {
            format!(" · License: {}", escape_html(root_lic))
        }
    ));
    if !root_deps.is_empty() {
        out.push_str("<h2>Direct Dependencies</h2><ul>");
        let base = url::Url::parse(page_url).ok();
        for (dep, range) in root_deps {
            let link = base
                .as_ref()
                .map(|b| {
                    format!(
                        "{}://{}{}?q={}",
                        b.scheme(),
                        b.host_str().unwrap_or_default(),
                        b.path(),
                        dep
                    )
                })
                .unwrap_or_default();
            if link.is_empty() {
                out.push_str(&format!(
                    "<li><strong>{}</strong>: {}</li>",
                    escape_html(dep),
                    escape_html(range)
                ));
            } else {
                out.push_str(&format!(
                    "<li><a href=\"{}\">{}</a> ({})</li>",
                    escape_attr(&link),
                    escape_html(dep),
                    escape_html(range)
                ));
            }
        }
        out.push_str("</ul>");
    }
    if packages.len() > 1 {
        out.push_str("<h2>Transitive Dependency Modules</h2><ul>");
        for (name, ver, desc, lic, _) in packages.iter().skip(1).take(80) {
            out.push_str(&format!(
                "<li><strong>{}@{}</strong>{}{}{}</li>",
                escape_html(name),
                escape_html(ver),
                if lic.is_empty() { "" } else { " (" },
                escape_html(lic),
                if lic.is_empty() {
                    if desc.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", escape_html(desc))
                    }
                } else if desc.is_empty() {
                    ")".to_string()
                } else {
                    format!(") — {}", escape_html(desc))
                }
            ));
        }
        out.push_str("</ul>");
    }
    out.push_str("</article>");
    Some((
        title,
        out,
        format!(
            "synthesized dependency graph from {} registry manifests",
            packages.len()
        ),
    ))
}

fn synthesize_openapi(json: &Value, source_url: &str) -> Option<(usize, String, String, String)> {
    let is_openapi = json.get("swagger").is_some() || json.get("openapi").is_some();
    let paths = json.get("paths").and_then(Value::as_object)?;
    if !is_openapi || paths.is_empty() {
        return None;
    }
    let title = json
        .pointer("/info/title")
        .and_then(Value::as_str)
        .unwrap_or("API Specification")
        .trim()
        .to_string();
    let version = json
        .pointer("/info/version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let desc = json
        .pointer("/info/description")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str(&format!(
        "<article><h1>{}{}</h1>",
        escape_html(&title),
        if version.is_empty() {
            String::new()
        } else {
            format!(" <span>v{}</span>", escape_html(version))
        }
    ));
    if !desc.is_empty() {
        out.push_str(&render_markdown_to_html(desc, source_url));
    }
    out.push_str("<h2>Endpoints</h2><ul>");
    let mut op_count = 0usize;
    for (path, methods_val) in paths.iter().take(150) {
        let Some(methods) = methods_val.as_object() else {
            continue;
        };
        for (method, op_val) in methods {
            let m_upper = method.to_ascii_uppercase();
            if !matches!(
                m_upper.as_str(),
                "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
            ) {
                continue;
            }
            op_count += 1;
            let summary = op_val
                .get("summary")
                .or_else(|| op_val.get("description"))
                .or_else(|| op_val.get("operationId"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.push_str(&format!(
                "<li><code>{} {}</code>{}</li>",
                escape_html(&m_upper),
                escape_html(path),
                if summary.trim().is_empty() {
                    String::new()
                } else {
                    format!(" — {}", escape_html(summary.trim()))
                }
            ));
        }
    }
    out.push_str("</ul></article>");
    if op_count == 0 {
        return None;
    }
    Some((
        1000 + op_count * 80,
        title,
        out,
        format!("synthesized OpenAPI specification ({op_count} operations)"),
    ))
}

/// Do these records carry something a reader reads? A row is information when it
/// names something *and* says something about it: a sentence, a description, a
/// link. Form schemas, field enumerations and status lists name their fields and
/// nothing else — fusing one wraps a page in its own scaffolding, which is how
/// Target's search page became a list of input types.
fn records_carry_content(items: &[Value]) -> bool {
    let mut named = 0usize;
    let mut substantive = 0usize;
    for item in items.iter().filter_map(Value::as_object) {
        // The row inside a one-key wrapper (`{"node": {...}}`).
        let row = ["node", "item", "attributes", "fields", "entity", "record"]
            .iter()
            .find_map(|key| item.get(*key).and_then(Value::as_object))
            .filter(|inner| {
                inner.get("title").is_some()
                    || inner.get("name").is_some()
                    || inner.get("content").is_some()
            })
            .unwrap_or(item);
        let mut has_name = false;
        let mut has_substance = false;
        for value in row.values() {
            match value {
                Value::String(text) => {
                    let text = text.trim();
                    if text.is_empty() || text.len() > 4_000 {
                        continue;
                    }
                    has_name = true;
                    // A sentence, a description or a link is content; a bare
                    // identifier, a colour or a field type is not.
                    if text.chars().count() >= 24 || text.starts_with("http") {
                        has_substance = true;
                    }
                }
                Value::Array(inner) => {
                    let substantial = inner.iter().filter_map(Value::as_str).any(|text| {
                        text.trim().chars().count() >= 24 || text.trim().starts_with("http")
                    });
                    if substantial {
                        has_substance = true;
                        has_name = true;
                    }
                }
                _ => {}
            }
        }
        if has_name {
            named += 1;
            if has_substance {
                substantive += 1;
            }
        }
    }
    named >= 3 && substantive * 2 >= named
}

/// Whether a JSON object carries a page: a named field, a nested structure, or
/// enough prose. A flat map of metrics (a WAF quota reply) does not.
fn page_like_object(map: &serde_json::Map<String, Value>) -> bool {
    const PAGE_LIKE_KEYS: &[&str] = &[
        "display_name",
        "displayName",
        "name",
        "title",
        "headline",
        "description",
        "summary",
        "readme",
        "content",
        "body",
        "markdown",
        "text",
        "author",
        "byline",
        "slug",
    ];
    let named = PAGE_LIKE_KEYS.iter().any(|key| {
        map.get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let structured = map
        .values()
        .any(|value| value.is_array() || value.is_object());
    // Ids and counters inflate a byte count without being a page; only a named
    // field or a nested structure makes an object page-like.
    named || structured
}

pub fn synthesize_json_payload(
    json: &Value,
    request_url: &str,
    page_url: &str,
) -> Option<(usize, String, String, String)> {
    // Unwrap common envelope wrappers (`data`, `result`, `payload`, `data.attributes` JSON:API).
    let target = unwrap_json_envelope(json);

    // Case A: Array of domain items (e.g. Mastodon statuses, search hits, trends).
    if let Value::Array(items) = target {
        // A short array of flat telemetry rows (WAF quota/config pairs) is not
        // a document. A collection earns a page through its size or through a
        // page-like field; a list of ids alone does not.
        let named = items
            .iter()
            .filter_map(Value::as_object)
            .any(page_like_object);
        // A collection is a page through its fields (title/name/description/
        // content), never through its size: five WAF quota rows are still five
        // rows of ids and counters. It is also not a page through its field
        // *names*: a form schema names its inputs (`textArea`, `select`) and
        // fuses as scaffolding around a page that is already there.
        if !named || !records_carry_content(items) {
            return None;
        }
        return render_json_array(items, request_url, page_url);
    }

    let Value::Object(map) = target else {
        return None;
    };

    // Check if the object wraps a primary collection (e.g. `hits`, `items`, `packages`, `statuses`).
    for key in [
        "items",
        "hits",
        "results",
        "packages",
        "statuses",
        "posts",
        "entries",
        "records",
        "nodes",
        "jobPostings",
    ] {
        if let Some(Value::Array(arr)) = map.get(key) {
            if let Some(built) = render_json_array(arr, request_url, page_url) {
                return Some(built);
            }
        }
    }

    // A collection under a name this module does not know is still a page: a
    // wrapper object with an array of records (Bsky's labeler services) renders
    // like the named wrappers above. Only scalar-only configuration objects are
    // refused below.
    for (_key, value) in map {
        if let Value::Array(items) = value {
            if items.len() >= 3
                && items.iter().all(Value::is_object)
                && items
                    .iter()
                    .filter_map(Value::as_object)
                    .any(page_like_object)
                && records_carry_content(items)
            {
                if let Some(built) = render_json_array(items, request_url, page_url) {
                    return Some(built);
                }
            }
        }
    }

    if !page_like_object(map) {
        return None;
    }

    // Case B: Single rich entity object (Bundlephobia package stats, ArtifactHub package,
    // Terraform Registry doc item, etc.).
    let title = [
        "display_name",
        "displayName",
        "title",
        "name",
        "full_name",
        "package",
        "slug",
    ]
    .iter()
    .find_map(|k| map.get(*k).and_then(Value::as_str))
    .map(str::trim)
    .unwrap_or_default()
    .to_string();

    let mut scalars: Vec<(String, String)> = Vec::new();
    for (k, v) in map {
        if matches!(
            k.as_str(),
            "readme"
                | "content"
                | "body"
                | "markdown"
                | "descriptionHtml"
                | "values_schema"
                | " security_report"
        ) {
            continue;
        }
        match v {
            Value::Number(n) => {
                let formatted = if k == "size" || k == "gzip" {
                    if let Some(bytes) = n.as_f64() {
                        format!("{bytes:.0} B ({:.1} kB)", bytes / 1024.0)
                    } else {
                        n.to_string()
                    }
                } else {
                    n.to_string()
                };
                scalars.push((k.clone(), formatted));
            }
            Value::Bool(b) => scalars.push((k.clone(), b.to_string())),
            Value::String(s) => {
                let t = s.trim();
                if !t.is_empty() && t.len() <= 240 && t != title {
                    scalars.push((k.clone(), t.to_string()));
                }
            }
            _ => {}
        }
    }

    let prose = [
        "readme",
        "content",
        "markdown",
        "descriptionHtml",
        "body",
        "overview",
        "description",
    ]
    .iter()
    .find_map(|k| {
        map.get(*k)
            .and_then(Value::as_str)
            .filter(|s| s.trim().len() >= 80)
            .map(|s| (*k, s.trim()))
    });

    if title.is_empty() && prose.is_none() && scalars.len() < 4 {
        return None;
    }

    let mut html = String::new();
    html.push_str("<article>");
    if !title.is_empty() {
        html.push_str(&format!("<h1>{}</h1>\n", escape_html(&title)));
    }
    if !scalars.is_empty() {
        html.push_str("<table><tbody>");
        for (k, v) in scalars.iter().take(24) {
            if v.starts_with("https://") || v.starts_with("http://") {
                html.push_str(&format!(
                    "<tr><th>{}</th><td><a href=\"{}\">{}</a></td></tr>",
                    escape_html(k),
                    escape_attr(v),
                    escape_html(v)
                ));
            } else {
                html.push_str(&format!(
                    "<tr><th>{}</th><td>{}</td></tr>",
                    escape_html(k),
                    escape_html(v)
                ));
            }
        }
        html.push_str("</tbody></table>\n");
    }

    // Also include dependency size breakdowns if present (e.g. Bundlephobia `dependencySizes`).
    if let Some(Value::Array(dep_sizes)) = map.get("dependencySizes") {
        if !dep_sizes.is_empty() {
            html.push_str("<h2>Dependency Sizes</h2><ul>");
            for dep in dep_sizes.iter().take(40) {
                if let (Some(name), Some(approx)) = (
                    dep.get("name").and_then(Value::as_str),
                    dep.get("approximateSize").and_then(Value::as_f64),
                ) {
                    html.push_str(&format!(
                        "<li><strong>{}</strong>: {:.1} kB ({:.0} B)</li>",
                        escape_html(name),
                        approx / 1024.0,
                        approx
                    ));
                }
            }
            html.push_str("</ul>");
        }
    }

    let mut prose_len = 0usize;
    if let Some((field, text)) = prose {
        prose_len = text.chars().count();
        if field.to_ascii_lowercase().contains("html")
            || text.contains("<p>")
            || text.contains("<div")
            || text.contains("<h2")
        {
            html.push_str(text);
        } else {
            html.push_str(&render_markdown_to_html(text, page_url));
        }
    }
    html.push_str("</article>");

    let score = prose_len.min(12_000) + scalars.len() * 65 + usize::from(!title.is_empty()) * 120;
    if score < 220 {
        return None;
    }
    Some((
        score,
        title,
        html,
        "synthesized structured entity from wire transcript".to_string(),
    ))
}

pub(crate) fn render_entity_replay_html(
    value: &Value,
    page_url: &str,
) -> Option<(String, String, usize)> {
    let (_score, title, mut body_html, _note) = synthesize_json_payload(value, page_url, page_url)?;
    // If the response is a JSON:API document with an `included` array (e.g.
    // Terraform Registry `/v2/providers/...?include=...`), append included
    // documentation/overview or sub-resource summaries.
    if let Some(included) = value.get("included").and_then(Value::as_array) {
        let mut inc_items = String::new();
        for item in included.iter().take(40) {
            let Some(attrs) = item.get("attributes").and_then(Value::as_object) else {
                continue;
            };
            if let Some(md) = attrs
                .get("content")
                .or_else(|| attrs.get("overview"))
                .and_then(Value::as_str)
                .filter(|s| s.trim().len() >= 80)
            {
                body_html.push_str("<section>");
                body_html.push_str(&render_markdown_to_html(md.trim(), page_url));
                body_html.push_str("</section>");
            } else if let Some(item_title) = attrs
                .get("title")
                .or_else(|| attrs.get("name"))
                .or_else(|| attrs.get("version"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                let desc = attrs
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if desc.is_empty() {
                    inc_items.push_str(&format!("<li>{}</li>", escape_html(item_title.trim())));
                } else {
                    inc_items.push_str(&format!(
                        "<li><strong>{}</strong> — {}</li>",
                        escape_html(item_title.trim()),
                        escape_html(desc)
                    ));
                }
            }
        }
        if !inc_items.is_empty() {
            body_html.push_str("<section><ul>");
            body_html.push_str(&inc_items);
            body_html.push_str("</ul></section>");
        }
    }
    let full_title = if title.is_empty() {
        page_url.to_string()
    } else {
        title
    };
    let doc = format!(
        "<!DOCTYPE html><html><head><title>{}</title></head><body><main>{}</main></body></html>",
        escape_html(&full_title),
        body_html
    );
    let chars = visible_fragment_chars(&body_html);
    (chars >= 80).then_some((doc, full_title, chars))
}

pub(crate) fn unwrap_entity_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    unwrap_json_envelope(value).as_object()
}

fn unwrap_json_envelope(mut value: &Value) -> &Value {
    for _ in 0..4 {
        let Value::Object(map) = value else {
            break;
        };
        let is_entity_root = map.contains_key("name")
            || map.contains_key("title")
            || map.contains_key("readme")
            || map.len() > 6;
        if !is_entity_root {
            if let Some(data) = map.get("data") {
                if let Some(attrs) = data.get("attributes").filter(|a| a.is_object()) {
                    return attrs;
                }
                if data.is_object() || data.is_array() {
                    value = data;
                    continue;
                }
            }
        }
        if map.len() == 1 {
            if let Some((_k, inner)) = map
                .iter()
                .next()
                .filter(|(k, v)| *k != "attributes" && (v.is_object() || v.is_array()))
            {
                value = inner;
                continue;
            }
        }
        if map.len() <= 3 {
            if let Some(inner) = map
                .get("result")
                .or_else(|| map.get("payload"))
                .or_else(|| map.get("edges"))
                .or_else(|| map.get("nodes"))
                .filter(|v| v.is_object() || v.is_array())
            {
                value = inner;
                continue;
            }
        }
        break;
    }
    value
}

fn render_json_array(
    items: &[Value],
    request_url: &str,
    page_url: &str,
) -> Option<(usize, String, String, String)> {
    if items.len() < 2 {
        return None;
    }
    let mut rows_html = String::new();
    let mut count = 0usize;
    let mut total_chars = 0usize;

    for item in items.iter().take(80) {
        let Some(raw_obj) = item.as_object() else {
            continue;
        };
        let obj = if raw_obj.len() <= 3 {
            raw_obj
                .get("node")
                .and_then(Value::as_object)
                .unwrap_or(raw_obj)
        } else {
            raw_obj
        };
        let title = [
            "title",
            "name",
            "display_name",
            "displayName",
            "headline",
            "subject",
            "tag",
        ]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .or_else(|| {
            obj.get("account")
                .and_then(|a| a.get("display_name").or_else(|| a.get("acct")))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .unwrap_or_default();

        let body_html_or_text = [
            "content",
            "description",
            "summary",
            "body",
            "text",
            "excerpt",
        ]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or_default();

        if title.is_empty() && body_html_or_text.is_empty() {
            continue;
        }
        let url = [
            "url",
            "uri",
            "html_url",
            "web_url",
            "permalink",
            "href",
            "link",
        ]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .and_then(|u| {
            url::Url::parse(page_url)
                .or_else(|_| url::Url::parse(request_url))
                .ok()
                .and_then(|b| b.join(u).ok())
                .map(|u| u.to_string())
        });

        count += 1;
        total_chars += title.chars().count() + body_html_or_text.chars().count();
        rows_html.push_str("<article>");
        if !title.is_empty() {
            if let Some(u) = &url {
                rows_html.push_str(&format!(
                    "<h3><a href=\"{}\">{}</a></h3>",
                    escape_attr(u),
                    escape_html(title)
                ));
            } else {
                rows_html.push_str(&format!("<h3>{}</h3>", escape_html(title)));
            }
        } else if let Some(u) = &url {
            rows_html.push_str(&format!(
                "<p><a href=\"{}\">{}</a></p>",
                escape_attr(u),
                escape_html(u)
            ));
        }
        if !body_html_or_text.is_empty() {
            if body_html_or_text.contains("<p")
                || body_html_or_text.contains("<a ")
                || body_html_or_text.contains("<br")
            {
                rows_html.push_str(body_html_or_text);
            } else {
                rows_html.push_str(&format!("<p>{}</p>", escape_html(body_html_or_text)));
            }
        }
        rows_html.push_str("</article>\n");
    }

    if count < 2 || total_chars < 150 {
        return None;
    }
    Some((
        count * 100 + total_chars.min(10_000),
        String::new(),
        format!("<section>{rows_html}</section>"),
        format!("synthesized {count} items from wire transcript"),
    ))
}

fn looks_like_html_fragment(body: &str) -> bool {
    let head = body.trim_start();
    if !head.starts_with('<') || head.starts_with("<?xml") {
        return false;
    }
    let low = head.get(..512).unwrap_or(head).to_ascii_lowercase();
    low.contains("<div")
        || low.contains("<section")
        || low.contains("<article")
        || low.contains("<h1")
        || low.contains("<h2")
        || low.contains("<p>")
        || low.contains("<table")
}

fn visible_fragment_chars(html: &str) -> usize {
    let cleaned = crate::contract::strip_noise(html);
    let doc = scraper::Html::parse_fragment(&cleaned);
    crate::contract::normalize(&doc.root_element().text().collect::<Vec<_>>().join(" "))
        .chars()
        .count()
}

fn extract_first_heading(html: &str) -> Option<String> {
    let doc = scraper::Html::parse_fragment(html);
    for sel in ["h1", "h2", "h3"] {
        if let Ok(s) = scraper::Selector::parse(sel) {
            if let Some(el) = doc.select(&s).next() {
                let text = el.text().collect::<String>().trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Convert Markdown prose (headings, code blocks, lists, links, tables, paragraphs)
/// into clean semantic HTML with resolved `<a href>` links.
pub fn render_markdown_to_html(md: &str, base_url: &str) -> String {
    let base = url::Url::parse(base_url).ok();
    let mut out = String::with_capacity(md.len() + md.len() / 4);
    let mut in_code = false;
    let mut in_list = false;
    let mut para = Vec::<String>::new();

    let flush_para = |para: &mut Vec<String>, out: &mut String| {
        if !para.is_empty() {
            let joined = para.join(" ");
            out.push_str("<p>");
            out.push_str(&render_inline_markdown(&joined, base.as_ref()));
            out.push_str("</p>\n");
            para.clear();
        }
    };

    for raw_line in md.lines() {
        let trimmed_end = raw_line.trim_end();
        let trimmed = trimmed_end.trim_start();

        if trimmed.starts_with("```") {
            flush_para(&mut para, &mut out);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            if in_code {
                out.push_str("</code></pre>\n");
                in_code = false;
            } else {
                out.push_str("<pre><code>");
                in_code = true;
            }
            continue;
        }

        if in_code {
            out.push_str(&escape_html(trimmed_end));
            out.push('\n');
            continue;
        }

        if trimmed.is_empty() {
            flush_para(&mut para, &mut out);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            continue;
        }

        // Headings (# .. ######)
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed.get(hashes..hashes + 1) == Some(" ") {
            flush_para(&mut para, &mut out);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            let heading = trimmed[hashes + 1..].trim();
            out.push_str(&format!(
                "<h{hashes}>{}</h{hashes}>\n",
                render_inline_markdown(heading, base.as_ref())
            ));
            continue;
        }

        // Unordered list items
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush_para(&mut para, &mut out);
            if !in_list {
                out.push_str("<ul>\n");
                in_list = true;
            }
            out.push_str("<li>");
            out.push_str(&render_inline_markdown(item.trim(), base.as_ref()));
            out.push_str("</li>\n");
            continue;
        }

        para.push(trimmed.to_string());
    }

    if in_code {
        out.push_str("</code></pre>\n");
    }
    flush_para(&mut para, &mut out);
    if in_list {
        out.push_str("</ul>\n");
    }
    out
}

fn render_inline_markdown(text: &str, base: Option<&url::Url>) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if let Some(code_rest) = rest.strip_prefix('`') {
            if let Some(end) = code_rest.find('`') {
                out.push_str("<code>");
                out.push_str(&escape_html(&code_rest[..end]));
                out.push_str("</code>");
                cursor += 1 + end + 1;
                continue;
            }
        }
        if rest.starts_with('[') {
            if let Some(close_bracket) = rest.find("](") {
                let label = &rest[1..close_bracket];
                let after = &rest[close_bracket + 2..];
                if let Some(close_paren) = after.find(')') {
                    let href_raw = after[..close_paren].split_whitespace().next().unwrap_or("");
                    if !href_raw.is_empty() && !label.contains('[') {
                        let resolved = base
                            .and_then(|b| b.join(href_raw).ok())
                            .filter(|u| matches!(u.scheme(), "http" | "https"))
                            .map(|u| u.to_string());
                        if let Some(href) = resolved {
                            out.push_str(&format!(
                                "<a href=\"{}\">{}</a>",
                                escape_attr(&href),
                                escape_html(label)
                            ));
                            cursor += close_bracket + 2 + close_paren + 1;
                            continue;
                        }
                    }
                }
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
        cursor += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_telemetry_object_is_not_fused_as_the_page() {
        let quota = serde_json::json!({
            "user_id": "85bc5744-77f3-d04f-2ed3-54edabbc644f",
            "quota_rate": 1,
            "timestamp": 1_795_009_798_808u64,
            "refresh_after": 86_400,
            "updated_at": 1_793_675_972_000u64,
        });
        assert!(
            synthesize_json_payload(&quota, "https://mcs.test/v1/list", "https://a.test/")
                .is_none(),
            "a WAF quota reply is not a document"
        );
        let package = serde_json::json!({
            "name": "react",
            "version": "19.0.0",
            "description": "A JavaScript library for building user interfaces"
        });
        assert!(
            synthesize_json_payload(
                &package,
                "https://registry.test/react",
                "https://bundlephobia.com/package/react"
            )
            .is_some(),
            "a named package entity is a document"
        );
    }

    fn transcript_entry(url: &str, body: &str) -> TranscriptEntry {
        TranscriptEntry {
            method: "GET".into(),
            url: url.into(),
            status: 200,
            bytes: body.len(),
            kind: "fetch".into(),
            body: Some(body.into()),
            request_headers: Vec::new(),
            request_body: None,
        }
    }

    #[test]
    fn already_hydrated_dom_is_never_overwritten_by_transcript_fusion() {
        let rich_dom = format!(
            "<html><head><title>Docs</title></head><body><main><article><p>{}</p><p>{}</p></article></main></body></html>",
            "Substantive server or client rendered documentation paragraph text. ".repeat(6),
            "Second paragraph confirming complete DOM materialization in main body. ".repeat(6),
        );
        let entries = vec![transcript_entry(
            "https://api.example.com/items",
            r#"[{"title":"Other","description":"Should not fuse into rich DOM"}]"#,
        )];
        assert!(fuse_dom_and_transcript(&rich_dom, "https://example.com/docs", &entries).is_none());
    }

    #[test]
    fn fuses_graphql_relay_edges_and_entity_json_with_root_data_field() {
        let empty_dom = "<html><head><title>Graph</title></head><body><svg></svg></body></html>";
        let gql_body = r#"{"data":{"tools":{"edges":[
            {"node":{"name":"Kube Prometheus Stack","description":"End-to-end Kubernetes cluster monitoring with Prometheus and Grafana dashboards","url":"/packages/prom"}},
            {"node":{"name":"Bitnami PostgreSQL","description":"Production-ready relational database chart with replication and metrics","url":"/packages/pg"}},
            {"node":{"name":"Cert Manager","description":"Automated TLS certificate issuance and renewal for Kubernetes ingress","url":"/packages/cert"}}
        ]}}}"#;
        let entries = vec![transcript_entry("https://example.com/graphql", gql_body)];
        let fused = fuse_dom_and_transcript(empty_dom, "https://example.com/packages", &entries)
            .expect("fused");
        assert!(fused.html.contains("Kube Prometheus Stack"));
        assert!(fused.html.contains("Bitnami PostgreSQL"));

        // Entity JSON that also happens to carry a chart-specific `data` sub-object
        // must keep its root entity fields (`name`, `version`, `readme`).
        let pkg = serde_json::json!({
            "name": "kube-prometheus",
            "version": "93.3.1",
            "description": "Cluster monitoring",
            "readme": "# Overview\n\nComplete monitoring stack for Kubernetes clusters with alerting rules and dashboards. ".repeat(3),
            "data": {"dependencies": []}
        });
        let (html, title, chars) = render_entity_replay_html(
            &pkg,
            "https://artifacthub.io/packages/helm/prom/kube-prometheus",
        )
        .expect("entity html");
        assert_eq!(title, "kube-prometheus");
        assert!(chars >= 200);
        assert!(html.contains("93.3.1"));
    }

    #[test]
    fn non_search_query_params_preserve_entity_fusion_and_search_urls_drop_unrelated_widgets() {
        let empty_dom =
            "<html><head><title>App</title></head><body><div id=\"root\"></div></body></html>";
        let entity = serde_json::json!({
            "name": "tokio",
            "version": "1.50.0",
            "description": "An event-driven, non-blocking I/O platform for writing asynchronous applications with Rust.",
            "readme": "Tokio is an asynchronous runtime for the Rust programming language providing building blocks for reliable network apps. ".repeat(3)
        })
        .to_string();
        let entries = vec![transcript_entry(
            "https://api.example.com/pkg/tokio",
            &entity,
        )];

        // Non-search tracking/market query params must NOT break Entity JSON fusion.
        let fused = fuse_dom_and_transcript(
            empty_dom,
            "https://example.com/pkg/tokio?utm_source=hn&intl=nosplash",
            &entries,
        )
        .expect("entity fusion survives tracking params");
        assert!(fused.html.contains("asynchronous runtime"));

        // On an actual search URL (?q=headphones), unrelated high-scoring side-panel
        // collections must not be mixed back in via combined_sections.
        let stores = r#"{"items":[
            {"title":"Downtown Store Location","description":"Open daily 8am to 10pm with curbside pickup and pharmacy services","url":"/store/1"},
            {"title":"Northside Store Location","description":"Open daily 8am to 10pm with optical center and bakery department","url":"/store/2"},
            {"title":"Westside Store Location","description":"Open daily 8am to 10pm with electronics counter and photo center","url":"/store/3"}
        ]}"#;
        let products = r#"{"items":[
            {"title":"Sony Wireless Headphones","description":"Noise cancelling over-ear Bluetooth headphones with 30-hour battery life","url":"/p/1"},
            {"title":"Bose QuietComfort Headphones","description":"Lightweight wireless headphones with custom sound modes and clear calls","url":"/p/2"},
            {"title":"Sennheiser Studio Headphones","description":"Open-back audiophile headphones engineered for reference studio monitoring","url":"/p/3"}
        ]}"#;
        let search_entries = vec![
            transcript_entry("https://api.example.com/stores/nearby", stores),
            transcript_entry("https://api.example.com/search?q=headphones", products),
        ];
        let fused_search = fuse_dom_and_transcript(
            empty_dom,
            "https://example.com/search?q=headphones&intl=nosplash",
            &search_entries,
        )
        .expect("search fusion picks product list");
        assert!(fused_search.html.contains("Sony Wireless Headphones"));
        assert!(!fused_search.html.contains("Downtown Store Location"));
    }
}
