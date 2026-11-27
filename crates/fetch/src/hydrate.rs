//! Structural Hydration Deficit (SHD) gate and Stage 0 inline state materializer.
//!
//! Two jobs run before any V8 isolate is created:
//! 1. `assess_html_shell` decides in sub-millisecond time whether an HTML
//!    response is an unhydrated client-rendered shell (including partial-HTML
//!    app shells whose navigation/footer chrome exceeds 200 characters), with
//!    a hard zero-false-positive invariant: a document without an executable
//!    application script is NEVER classified as a shell.
//! 2. `materialize_inline_state` inspects inline JSON state (`window.__appData`,
//!    `__NEXT_DATA__`, `__NUXT__`, `<script type="application/json">`,
//!    `data-page`, `data-props`) and, when the page already carries its own
//!    structured entities or collections in HTML, materializes semantic HTML
//!    with links in under a millisecond without booting V8.

use scraper::{Html, Node, Selector};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

/// Third-party telemetry, analytics, ads, consent, and feature-flag hosts.
/// Scripts from these hosts do not make a static document an application shell,
/// and in content mode they are short-circuited so they never stall rendering.
const TELEMETRY_HOST_SUFFIXES: &[&str] = &[
    "googletagmanager.com",
    "google-analytics.com",
    "analytics.google.com",
    "googlesyndication.com",
    "doubleclick.net",
    "googleadservices.com",
    "cloudflareinsights.com",
    "launchdarkly.com",
    "sentry.io",
    "browser-intake-datadoghq.com",
    "datadoghq.com",
    "segment.io",
    "segment.com",
    "cdn.segment.com",
    "hotjar.com",
    "intercom.io",
    "intercomcdn.com",
    "amplitude.com",
    "mixpanel.com",
    "fullstory.com",
    "clarity.ms",
    "plausible.io",
    "posthog.com",
    "heapanalytics.com",
    "bugsnag.com",
    "newrelic.com",
    "nr-data.net",
    "onetrust.com",
    "cookielaw.org",
    "cookiebot.com",
    "hs-scripts.com",
    "hs-analytics.net",
    "hubspot.com",
    "facebook.net",
    "connect.facebook.net",
    "ads-twitter.com",
    "licdn.com",
    "tiktok.com",
    "optimizely.com",
    "statsigapi.net",
    "rudderlabs.com",
];

/// True when `url` points to a known third-party telemetry/analytics/consent endpoint.
pub fn is_telemetry_url(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let lower = host.to_ascii_lowercase();
    TELEMETRY_HOST_SUFFIXES
        .iter()
        .any(|suffix| lower == *suffix || lower.ends_with(&format!(".{suffix}")))
}

#[derive(Clone, Debug, Default)]
pub struct ShellAssessment {
    pub has_app_program: bool,
    pub app_scripts: usize,
    pub main_text_chars: usize,
    pub chrome_text_chars: usize,
    pub prose_blocks: usize,
    pub data_items: usize,
    pub empty_mount_root: bool,
    pub js_required_noscript: bool,
    pub explicit_unhydrated_payload: bool,
    pub is_unhydrated_shell: bool,
    pub reason: Option<&'static str>,
}

static SEL_BASE_HREF: std::sync::LazyLock<Selector> =
    std::sync::LazyLock::new(|| Selector::parse("base[href]").expect("base[href] selector"));
static SEL_SCRIPT: std::sync::LazyLock<Selector> =
    std::sync::LazyLock::new(|| Selector::parse("script").expect("script selector"));
static SEL_LINK_REL: std::sync::LazyLock<Selector> =
    std::sync::LazyLock::new(|| Selector::parse("link[rel]").expect("link[rel] selector"));
static SEL_NOSCRIPT: std::sync::LazyLock<Selector> =
    std::sync::LazyLock::new(|| Selector::parse("noscript").expect("noscript selector"));
static SEL_BODY: std::sync::LazyLock<Selector> =
    std::sync::LazyLock::new(|| Selector::parse("body").expect("body selector"));

/// Resolve the document's effective base URL (`<base href="...">` against `page_url`,
/// or `page_url` itself when no `<base href>` exists).
pub fn extract_base_href(html: &str, page_url: &str) -> String {
    let doc = Html::parse_document(html);
    extract_base_href_doc(&doc, page_url)
}

/// Resolve the document's effective base URL from an already-parsed document.
pub fn extract_base_href_doc(doc: &Html, page_url: &str) -> String {
    let Ok(base) = url::Url::parse(page_url) else {
        return page_url.to_string();
    };
    if let Some(el) = doc.select(&SEL_BASE_HREF).next() {
        if let Some(href) = el.value().attr("href") {
            let trimmed = href.trim();
            if !trimmed.is_empty() {
                if let Ok(resolved) = base.join(trimmed) {
                    return resolved.to_string();
                }
            }
        }
    }
    page_url.to_string()
}

/// Parse `<script type="importmap">` declarations into resolved URL mappings.
pub fn extract_import_map(html: &str, base_url: &str) -> HashMap<String, String> {
    let doc = Html::parse_document(html);
    extract_import_map_doc(&doc, base_url)
}

/// Parse `<script type="importmap">` declarations from an already-parsed document.
pub fn extract_import_map_doc(doc: &Html, base_url: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let base = url::Url::parse(base_url).ok();
    for script in doc.select(&SEL_SCRIPT) {
        let ty = script
            .value()
            .attr("type")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if ty != "importmap" {
            continue;
        }
        let text: String = script.text().collect();
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(imports) = json.get("imports").and_then(Value::as_object) {
            for (spec, target) in imports {
                let Some(target_str) = target.as_str() else {
                    continue;
                };
                let resolved = base
                    .as_ref()
                    .and_then(|b| b.join(target_str).ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| target_str.to_string());
                map.insert(spec.clone(), resolved);
            }
        }
    }
    map
}

const JS_REQUIRED_PHRASES: &[&str] = &[
    "enable javascript",
    "javascript is required",
    "javascript must be enabled",
    "need to enable javascript",
    "without javascript enabled",
    "requires javascript",
    "turn on javascript",
    "javascript is disabled",
    "doesn't work properly without javascript",
    "does not work properly without javascript",
];

/// Structural Hydration Deficit assessment.
///
/// Hard zero-false-positive guarantee:
/// - If `!has_app_program`, `is_unhydrated_shell` is ALWAYS `false`.
/// - If the main body outside navigation/header/footer chrome already holds
///   substantive prose or data rows, `is_unhydrated_shell` is `false`.
pub fn assess_html_shell(body: &str, page_url: &str) -> ShellAssessment {
    // Pages over 2 MB of HTML already carry massive server-rendered markup.
    if body.len() > 2 * 1024 * 1024 {
        return ShellAssessment::default();
    }
    let doc = Html::parse_document(body);
    assess_html_shell_doc(&doc, page_url)
}

/// Structural Hydration Deficit assessment over an already-parsed document.
pub fn assess_html_shell_doc(doc: &Html, page_url: &str) -> ShellAssessment {
    let base = url::Url::parse(&extract_base_href_doc(doc, page_url)).ok();
    let mut out = ShellAssessment::default();

    // 1. Detect executable first-party / app scripts and modulepreloads.
    for link in doc.select(&SEL_LINK_REL) {
        let rel = link
            .value()
            .attr("rel")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if rel.split_whitespace().any(|r| r == "modulepreload")
            && link.value().attr("href").is_some()
        {
            out.has_app_program = true;
            out.app_scripts += 1;
        }
    }

    let mut next_data_unhydrated = false;
    for script in doc.select(&SEL_SCRIPT) {
        let id = script.value().attr("id").unwrap_or_default();
        let ty = script
            .value()
            .attr("type")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        if id == "__NEXT_DATA__" {
            let raw: String = script.text().collect();
            if is_unhydrated_next_data(&raw) {
                next_data_unhydrated = true;
            }
            continue;
        }

        let is_module = ty == "module";
        let is_classic_js = ty.is_empty()
            || matches!(
                ty.as_str(),
                "text/javascript"
                    | "application/javascript"
                    | "text/ecmascript"
                    | "application/ecmascript"
            );
        if !is_module && !is_classic_js {
            continue;
        }

        if let Some(src) = script.value().attr("src") {
            let resolved = base
                .as_ref()
                .and_then(|b| b.join(src.trim()).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| src.trim().to_string());
            if !is_telemetry_url(&resolved)
                && !resolved.contains("recaptcha")
                && !resolved.contains("hcaptcha")
                && !resolved.contains("challenges.cloudflare.com/turnstile")
            {
                out.has_app_program = true;
                out.app_scripts += 1;
            }
        } else {
            let code: String = script.text().collect();
            let trimmed = code.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_module || looks_like_inline_app_bootstrap(trimmed) {
                out.has_app_program = true;
                out.app_scripts += 1;
            }
        }
    }

    // 2. Inspect <noscript> for explicit JS-required notices.
    for ns in doc.select(&SEL_NOSCRIPT) {
        let raw: String = ns.text().collect::<Vec<_>>().join(" ");
        let lower = crate::contract::fold_quotes(&raw.to_ascii_lowercase());
        if JS_REQUIRED_PHRASES.iter().any(|p| lower.contains(p)) {
            out.js_required_noscript = true;
        }
    }

    // 3. Separate Frame Chrome from Main Body substance.
    let body_root = doc.select(&SEL_BODY).next().map(|el| el.id());

    if let Some(body_id) = body_root {
        if let Some(body_node) = doc.tree.get(body_id) {
            walk_body(body_node, false, &mut out);
        }
    } else {
        walk_body(doc.tree.root(), false, &mut out);
    }

    out.explicit_unhydrated_payload = next_data_unhydrated;

    if !out.has_app_program {
        out.is_unhydrated_shell = false;
        return out;
    }

    // Decide whether the main body has a structural hydration deficit:
    // - Case 1: Practically empty main body (< 140 non-whitespace non-chrome chars)
    //   and fewer than 3 structured data items.
    if out.main_text_chars < 140 && out.data_items < 3 && out.prose_blocks == 0 {
        out.is_unhydrated_shell = true;
        out.reason = Some(if out.empty_mount_root {
            "empty spa mount root"
        } else if out.js_required_noscript {
            "js-required shell"
        } else {
            "chrome-only shell"
        });
        return out;
    }

    // - Case 2: Explicit unhydrated SSR payload (e.g., Next.js `__NEXT_DATA__`
    //   where `pageProps` keys are all null/empty, like Bundlephobia's
    //   `{"initialResult": null}`) and the page lacks article/table substance.
    if out.explicit_unhydrated_payload
        && out.main_text_chars < 900
        && out.prose_blocks < 3
        && out.data_items < 4
    {
        out.is_unhydrated_shell = true;
        out.reason = Some("unhydrated __NEXT_DATA__ app shell");
        return out;
    }

    // - Case 3: Empty SPA mount root + JS-required noscript notice alongside
    //   a modest promotional/header wrapper (< 320 main chars, <= 1 prose block).
    if out.empty_mount_root
        && out.js_required_noscript
        && out.main_text_chars < 320
        && out.prose_blocks <= 1
        && out.data_items < 3
    {
        out.is_unhydrated_shell = true;
        out.reason = Some("mount root with js-required fallback");
        return out;
    }

    out
}

fn looks_like_inline_app_bootstrap(code: &str) -> bool {
    const MARKERS: &[&str] = &[
        "createRoot",
        "hydrateRoot",
        "ReactDOM",
        "createApp(",
        "webpackChunk",
        "__webpack_require__",
        "System.import",
        "import(",
        "customElements.define",
        "SwaggerUIBundle",
        "Redoc.init",
        "document.getElementById(",
        "document.querySelector(",
        "fetch(",
        "XMLHttpRequest",
        "$RC(",
        "__next_f.push",
        "__APOLLO_STATE__",
        "__REMIX_CONTEXT__",
        "__NUXT__",
    ];
    MARKERS.iter().any(|m| code.contains(m))
}

fn is_unhydrated_next_data(raw: &str) -> bool {
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let Some(page_props) = val
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    if page_props.is_empty() {
        return false;
    }
    let mut saw_null = false;
    for (_k, v) in page_props {
        match v {
            Value::Null => saw_null = true,
            Value::Bool(_) => {}
            Value::Array(arr) if arr.is_empty() => saw_null = true,
            Value::Object(map) if map.is_empty() => {}
            _ => return false,
        }
    }
    saw_null
}

fn is_chrome_element(el: &scraper::node::Element) -> bool {
    let tag = el.name();
    if matches!(tag, "main" | "article") || el.attr("role") == Some("main") {
        return false;
    }
    if matches!(tag, "nav" | "footer" | "aside" | "dialog" | "header") {
        return true;
    }
    if let Some(role) = el.attr("role") {
        let r = role.trim().to_ascii_lowercase();
        if matches!(
            r.as_str(),
            "navigation"
                | "banner"
                | "contentinfo"
                | "complementary"
                | "menubar"
                | "menu"
                | "search"
                | "toolbar"
                | "alert"
        ) {
            return true;
        }
    }
    let id = el.attr("id").unwrap_or_default().to_ascii_lowercase();
    let class = el.attr("class").unwrap_or_default().to_ascii_lowercase();
    for token in std::iter::once(id.as_str()).chain(class.split_whitespace()) {
        if matches!(
            token,
            "navbar"
                | "nav"
                | "topbar"
                | "masthead"
                | "sidebar"
                | "sidenav"
                | "site-header"
                | "site-footer"
                | "footer"
                | "cookie-banner"
                | "cookie-consent"
                | "gdpr"
                | "breadcrumb"
                | "breadcrumbs"
        ) {
            return true;
        }
    }
    false
}

fn is_mount_candidate(el: &scraper::node::Element) -> bool {
    let tag = el.name();
    if tag.contains('-')
        && !matches!(
            tag,
            "font-face" | "missing-glyph" | "color-profile" | "fe-distant-light"
        )
    {
        return true;
    }
    if el.attr("data-reactroot").is_some() || el.attr("ng-version").is_some() {
        return true;
    }
    if let Some(id) = el.attr("id") {
        let lower = id.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "root"
                | "app"
                | "__next"
                | "__nuxt"
                | "svelte"
                | "app-root"
                | "swagger-ui"
                | "redoc"
                | "dish"
                | "ember-bootstrap"
                | "___gatsby"
        ) {
            return true;
        }
    }
    false
}

fn subtree_visible_non_ws(node: ego_tree::NodeRef<'_, Node>) -> usize {
    let mut count = 0usize;
    for child in node.children() {
        match child.value() {
            Node::Text(t) => {
                count += t.text.chars().filter(|c| !c.is_whitespace()).count();
            }
            Node::Element(el) => {
                if matches!(el.name(), "script" | "style" | "noscript" | "template") {
                    continue;
                }
                if el.attr("hidden").is_some() || el.attr("aria-hidden") == Some("true") {
                    continue;
                }
                count += subtree_visible_non_ws(child);
            }
            _ => {}
        }
    }
    count
}

/// Visible, non-whitespace characters of a document's *content*: its body text
/// outside frame chrome (nav/header/footer/aside and their roles), with
/// `<main>`/`<article>`/`role=main` always content and script/style/hidden
/// subtrees skipped. The measure that separates a page from a navigation frame.
///
/// A page whose text is only its chrome (a header, a footer, legal lines) is
/// what a client-rendered shell looks like before its program runs, and what a
/// false "content" verdict looks like after it: both are `main_text_chars` here.
pub fn content_text_chars(doc: &Html) -> usize {
    let mut out = ShellAssessment::default();
    match doc.select(&SEL_BODY).next().map(|el| el.id()) {
        Some(body_id) => {
            if let Some(body_node) = doc.tree.get(body_id) {
                walk_body(body_node, false, &mut out);
            }
        }
        None => walk_body(doc.tree.root(), false, &mut out),
    }
    out.main_text_chars
}

fn walk_body(node: ego_tree::NodeRef<'_, Node>, in_chrome: bool, out: &mut ShellAssessment) {
    for child in node.children() {
        match child.value() {
            Node::Text(t) => {
                let n = t.text.chars().filter(|c| !c.is_whitespace()).count();
                if in_chrome {
                    out.chrome_text_chars += n;
                } else {
                    out.main_text_chars += n;
                }
            }
            Node::Element(el) => {
                let tag = el.name();
                if matches!(tag, "script" | "style" | "noscript" | "template" | "svg") {
                    continue;
                }
                if el.attr("hidden").is_some() || el.attr("aria-hidden") == Some("true") {
                    continue;
                }
                if !in_chrome && is_mount_candidate(el) {
                    let mount_chars = subtree_visible_non_ws(child);
                    if mount_chars < 40 {
                        out.empty_mount_root = true;
                    }
                }
                let is_main_landmark =
                    matches!(tag, "main" | "article") || el.attr("role") == Some("main");
                let child_chrome = if is_main_landmark {
                    false
                } else {
                    in_chrome || is_chrome_element(el)
                };
                if !child_chrome {
                    // An image grid's content is its labels: a launch calendar
                    // or a gallery is pictures with `alt`/`aria-label` text and
                    // almost no text nodes. Those attributes are what a reader
                    // (and a screen reader) gets from the page.
                    for attr in ["alt", "aria-label"] {
                        if let Some(text) = el.attr(attr) {
                            let chars = text.trim().chars().filter(|c| !c.is_whitespace()).count();
                            if chars >= 6 {
                                out.main_text_chars += chars.min(200);
                            }
                        }
                    }
                    if matches!(tag, "p" | "pre" | "blockquote" | "dd") {
                        let chars = subtree_visible_non_ws(child);
                        if chars >= 70 {
                            out.prose_blocks += 1;
                        }
                    } else if matches!(tag, "tr" | "li" | "dt") {
                        let chars = subtree_visible_non_ws(child);
                        if chars >= 25 {
                            out.data_items += 1;
                        }
                    }
                }
                walk_body(child, child_chrome, out);
            }
            _ => {}
        }
    }
}

// ----- Stage 0: Universal Inline State Materializer -------------------------

#[derive(Clone, Debug)]
pub struct InlineMaterialized {
    pub source_name: String,
    pub title: String,
    pub html: String,
    pub items: usize,
    pub sample: Vec<String>,
    pub sample_urls: Vec<String>,
}

/// Extract structured inline JSON from ANY `<script>` assignment or JSON data block
/// when an unhydrated shell already carries its full payload in HTML (e.g., Ashby's
/// `window.__appData`, Inertia `data-page`, custom `window.__INITIAL_DATA__`).
pub fn materialize_inline_state(html: &str, page_url: &str) -> Option<InlineMaterialized> {
    let doc = Html::parse_document(html);
    let page_title = Selector::parse("title")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .map(|n| n.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let mut candidates: Vec<(String, Value)> = Vec::new();

    // 1. Check `data-page` (Inertia.js) and `data-props` attributes on mount elements.
    for attr_name in ["data-page", "data-props"] {
        if let Ok(sel) = Selector::parse(&format!("[{attr_name}]")) {
            for el in doc.select(&sel) {
                if let Some(raw) = el.value().attr(attr_name) {
                    if raw.len() >= 120 && raw.len() <= 8 * 1024 * 1024 {
                        if let Ok(val) = serde_json::from_str::<Value>(raw) {
                            candidates.push((attr_name.to_string(), val));
                        }
                    }
                }
            }
        }
    }

    // 2. Scan `<script>` elements for JSON blocks and `window.<ident> = {...}` assignments.
    if let Ok(script_sel) = Selector::parse("script") {
        for script in doc.select(&script_sel) {
            if script.value().attr("src").is_some() {
                continue;
            }
            let ty = script
                .value()
                .attr("type")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if ty == "application/ld+json" || ty == "importmap" {
                continue;
            }
            let raw: String = script.text().collect();
            let trimmed = raw.trim();
            if trimmed.len() < 120 || trimmed.len() > 8 * 1024 * 1024 {
                continue;
            }
            if ty == "application/json" {
                let label = script
                    .value()
                    .attr("id")
                    .unwrap_or("script[application/json]")
                    .to_string();
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    candidates.push((label, val));
                }
                continue;
            }
            scan_global_json_assignments(trimmed, &mut candidates);
        }
    }

    let mut best: Option<(usize, InlineMaterialized)> = None;
    for (source_name, value) in candidates {
        if let Some((score, built)) =
            render_inline_candidate(&source_name, &value, &page_title, page_url)
        {
            if best.as_ref().is_none_or(|(b_score, _)| score > *b_score) {
                best = Some((score, built));
            }
        }
    }
    // A materialized state is only a page when it reads like one: a metrics or
    // config blob (a WAF quota reply in TikTok's universal data) is not the
    // document, and materializing it hides the real page behind a table.
    best.filter(|(_, materialized)| inline_document_has_substance(&materialized.html))
        .map(|(_, materialized)| materialized)
}

/// Whether a materialized document carries prose a reader came for. A sentence
/// (a period followed by a space) or quoted text is prose; a key/value table of
/// ids and counters is not.
fn inline_document_has_substance(html: &str) -> bool {
    let document = Html::parse_document(html);
    let count = |selector: &str| {
        Selector::parse(selector)
            .ok()
            .map(|selector| document.select(&selector).count())
            .unwrap_or(0)
    };
    let text = document
        .root_element()
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // A list or a paragraph is a page even when it is short (a few job rows).
    // Without either, a metrics blob materializes as tables of ids and
    // counters: two or more tables and no prose is not the document.
    if count("li") + count("p") > 0 {
        return text.chars().count() >= 60;
    }
    text.chars().count() >= 200 && count("table") < 2
}

/// Scan inline JS for `window.X = {...}`, `globalThis.X = {...}`, `self.X = {...}`,
/// or `JSON.parse("...")` assignments, balancing JSON braces/brackets accurately.
fn scan_global_json_assignments(script: &str, out: &mut Vec<(String, Value)>) {
    let base_ptr = script.as_ptr() as usize;
    let offset_of = |sub: &str| -> usize { sub.as_ptr() as usize - base_ptr };
    for prefix in ["window.", "globalThis.", "self."] {
        let mut cursor = 0usize;
        while cursor < script.len() {
            let Some(rel) = script[cursor..].find(prefix) else {
                break;
            };
            let start = cursor + rel + prefix.len();
            let rest = &script[start..];
            let ident_len = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
                .map(char::len_utf8)
                .sum::<usize>();
            if ident_len == 0 {
                let Some(ch) = rest.chars().next() else {
                    break;
                };
                cursor = start + ch.len_utf8();
                continue;
            }
            let ident = &rest[..ident_len];
            let after_ident = rest[ident_len..].trim_start();
            let Some(after_eq) = after_ident.strip_prefix('=') else {
                cursor = start + ident_len;
                continue;
            };
            // Reject `==` / `===`.
            if let Some(after_second_eq) = after_eq.strip_prefix('=') {
                cursor = offset_of(after_second_eq);
                continue;
            }
            let rhs = after_eq.trim_start();
            if let Some(json_parse) = rhs.strip_prefix("JSON.parse(") {
                let arg = json_parse.trim_start();
                if let Some((decoded, consumed)) = extract_js_string_literal(arg) {
                    if decoded.len() >= 120 {
                        if let Ok(val) = serde_json::from_str::<Value>(&decoded) {
                            out.push((format!("{prefix}{ident}"), val));
                        }
                    }
                    cursor = (offset_of(arg) + consumed).min(script.len());
                    continue;
                }
            } else if rhs.starts_with('{') || rhs.starts_with('[') {
                if let Some(slice) = balanced_json_slice(rhs) {
                    if slice.len() >= 120 {
                        if let Ok(val) = serde_json::from_str::<Value>(slice) {
                            out.push((format!("{prefix}{ident}"), val));
                        }
                    }
                    cursor = (offset_of(rhs) + slice.len()).min(script.len());
                    continue;
                }
            }
            cursor = start + ident_len;
        }
    }
}

fn extract_js_string_literal(input: &str) -> Option<(String, usize)> {
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    if quote == '"' {
        let mut idx = 1usize;
        let bytes = input.as_bytes();
        while idx < bytes.len() {
            if bytes[idx] == b'\\' {
                idx += 2;
                continue;
            }
            if bytes[idx] == b'"' {
                let literal = &input[..=idx];
                let decoded: String = serde_json::from_str(literal).ok()?;
                return Some((decoded, idx + 1));
            }
            idx += 1;
        }
        None
    } else {
        let mut out = String::new();
        let mut chars = input[1..].char_indices();
        while let Some((idx, ch)) = chars.next() {
            if ch == '\\' {
                let (_, esc) = chars.next()?;
                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    'u' => {
                        let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                        if let Ok(code) = u32::from_str_radix(&hex, 16) {
                            if let Some(u) = char::from_u32(code) {
                                out.push(u);
                            }
                        }
                    }
                    'x' => {
                        let hex: String = chars.by_ref().take(2).map(|(_, c)| c).collect();
                        if let Ok(code) = u8::from_str_radix(&hex, 16) {
                            out.push(code as char);
                        }
                    }
                    other => out.push(other),
                }
                continue;
            }
            if ch == '\'' {
                return Some((out, idx + 2));
            }
            out.push(ch);
        }
        None
    }
}

pub(crate) fn balanced_json_slice(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    let open = *bytes.first()?;
    if open != b'{' && open != b'[' {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if in_str {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            idx += 1;
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[..=idx]);
                }
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

const NOISE_COLLECTION_KEYS: &[&str] = &[
    "locales",
    "languages",
    "translations",
    "i18n",
    "messages",
    "routes",
    "chunks",
    "flags",
    "featureflags",
    "experiments",
    "permissions",
    "countries",
    "currencies",
    "timezones",
    "icons",
    "themes",
    "breadcrumbs",
    "breadcrumb",
    "breadcrumblist",
    "nav",
    "navbar",
    "navigation",
    "menu",
    "megamenu",
    "sitemap",
    "sitelinks",
];

fn is_noise_path(path: &[String]) -> bool {
    path.iter().any(|seg| {
        let low = seg.to_ascii_lowercase();
        NOISE_COLLECTION_KEYS.contains(&low.as_str())
    })
}

type CollectionRow = (String, String, Option<String>);

fn render_inline_candidate(
    source_name: &str,
    value: &Value,
    page_title: &str,
    page_url: &str,
) -> Option<(usize, InlineMaterialized)> {
    // Reject explicitly unhydrated Next.js shells so V8 runs their client fetch.
    if source_name == "__NEXT_DATA__" {
        let props = value
            .get("props")
            .and_then(|p| p.get("pageProps"))
            .and_then(Value::as_object)?;
        if props.is_empty() || props.values().all(Value::is_null) {
            return None;
        }
    }

    // Search for the richest domain collection (>= 3 objects with title/name),
    // including normalized GraphQL/Apollo/Relay entity maps (`{"Post:1": {...}}`),
    // OR a single domain entity with substantive prose (>= 280 chars).
    let mut collections: Vec<(Vec<String>, Vec<Value>)> = Vec::new();
    collect_object_arrays(value, &mut Vec::new(), &mut collections);
    collect_normalized_entity_maps(value, &mut Vec::new(), &mut collections);

    let mut best_collection: Option<(usize, Vec<CollectionRow>)> = None;
    for (path, elements) in &collections {
        if elements.len() < 3 || is_noise_path(path) {
            continue;
        }
        let rows = extract_collection_rows(elements, page_url);
        if rows.len() < 3 {
            continue;
        }
        // A menu is a list of labels; a page's rows point somewhere or say
        // something. A collection whose rows are bare names is the site's
        // navigation shipped as JSON, and materializing it hides the page.
        let informative = rows
            .iter()
            .filter(|(_, detail, url)| url.is_some() || !detail.is_empty())
            .count();
        if informative * 2 < rows.len() {
            continue;
        }
        let total_chars: usize = rows
            .iter()
            .map(|(t, d, _)| t.chars().count() + d.chars().count())
            .sum();
        if total_chars < 90 {
            continue;
        }
        let score = rows.len() * 120 + total_chars.min(8000);
        if best_collection.as_ref().is_none_or(|(b, _)| score > *b) {
            best_collection = Some((score, rows));
        }
    }

    // Also inspect for a primary single entity (e.g. a single job posting, article, or package).
    let single_entity = extract_primary_entity(value, page_url);

    match (best_collection, single_entity) {
        (Some((c_score, _rows)), Some((e_score, entity_title, entity_html)))
            if e_score > c_score =>
        {
            let title = if !entity_title.is_empty() {
                entity_title
            } else if !page_title.is_empty() {
                page_title.to_string()
            } else {
                source_name.to_string()
            };
            let html = format!(
                "<!DOCTYPE html><html><head><title>{}</title></head><body><main>{}</main></body></html>",
                escape_html(&title),
                entity_html
            );
            Some((
                e_score,
                InlineMaterialized {
                    source_name: source_name.to_string(),
                    title: title.clone(),
                    html,
                    items: 1,
                    sample: vec![title],
                    sample_urls: Vec::new(),
                },
            ))
        }
        (Some((c_score, rows)), _) => {
            let heading = infer_board_title(value)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if !page_title.is_empty() {
                        page_title.to_string()
                    } else {
                        source_name.to_string()
                    }
                });
            let mut body_html = String::with_capacity(rows.len() * 180 + 256);
            body_html.push_str(&format!("<h1>{}</h1>\n<ul>\n", escape_html(&heading)));
            let mut sample = Vec::new();
            let mut sample_urls = Vec::new();
            for (title, detail, url) in &rows {
                if sample.len() < 5 {
                    sample.push(title.clone());
                }
                if let Some(u) = url {
                    if sample_urls.len() < 5 {
                        sample_urls.push(u.clone());
                    }
                    body_html.push_str(&format!(
                        "<li><a href=\"{}\">{}</a>",
                        escape_attr(u),
                        escape_html(title)
                    ));
                } else {
                    body_html.push_str(&format!("<li><strong>{}</strong>", escape_html(title)));
                }
                if !detail.is_empty() {
                    body_html.push_str(&format!(" — <span>{}</span>", escape_html(detail)));
                }
                body_html.push_str("</li>\n");
            }
            body_html.push_str("</ul>");
            let html = format!(
                "<!DOCTYPE html><html><head><title>{}</title></head><body><main>{}</main></body></html>",
                escape_html(&heading),
                body_html
            );
            Some((
                c_score,
                InlineMaterialized {
                    source_name: source_name.to_string(),
                    title: heading,
                    html,
                    items: rows.len(),
                    sample,
                    sample_urls,
                },
            ))
        }
        (None, Some((e_score, entity_title, entity_html))) => {
            let title = if !entity_title.is_empty() {
                entity_title
            } else if !page_title.is_empty() {
                page_title.to_string()
            } else {
                source_name.to_string()
            };
            let html = format!(
                "<!DOCTYPE html><html><head><title>{}</title></head><body><main>{}</main></body></html>",
                escape_html(&title),
                entity_html
            );
            Some((
                e_score,
                InlineMaterialized {
                    source_name: source_name.to_string(),
                    title: title.clone(),
                    html,
                    items: 1,
                    sample: vec![title],
                    sample_urls: Vec::new(),
                },
            ))
        }
        (None, None) => None,
    }
}

fn infer_board_title(value: &Value) -> Option<String> {
    for ptr in [
        "/organization/name",
        "/company/name",
        "/board/title",
        "/title",
        "/name",
    ] {
        if let Some(s) = value.pointer(ptr).and_then(Value::as_str) {
            let t = s.trim();
            if t.len() >= 2 && t.len() <= 120 {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn collect_object_arrays(
    value: &Value,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Vec<Value>)>,
) {
    // Relay and Apollo deliver a page's rows behind a wrapper chain
    // (`require…__bbox.result.data.<entity>.<connection>.edges`), which runs
    // deeper than a plain application's payload. The cap is a guard against
    // pathological nesting, not a statement about how sites shape their JSON.
    if path.len() > 24 {
        return;
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                path.push(k.clone());
                collect_object_arrays(v, path, out);
                path.pop();
            }
        }
        Value::Array(arr) => {
            if arr.len() >= 3 && arr.iter().take(5).all(Value::is_object) {
                let unwrapped: Vec<Value> = arr
                    .iter()
                    .map(|v| {
                        if let Some(obj) = v.as_object() {
                            if obj.len() <= 3 {
                                if let Some(node) = obj.get("node").filter(|n| n.is_object()) {
                                    return node.clone();
                                }
                            }
                        }
                        v.clone()
                    })
                    .collect();
                out.push((path.clone(), unwrapped));
            }
            for v in arr.iter().take(20) {
                collect_object_arrays(v, path, out);
            }
        }
        _ => {}
    }
}

/// Extract normalized entity caches such as Apollo (`{"Post:1": {"__typename": "Post", ...}}`),
/// Relay stores, or Redux/Pinia entity dictionaries keyed by ID.
fn collect_normalized_entity_maps(
    value: &Value,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Vec<Value>)>,
) {
    if path.len() > 5 {
        return;
    }
    let Value::Object(map) = value else {
        return;
    };
    if map.len() >= 3 {
        let mut by_type: HashMap<String, Vec<Value>> = HashMap::new();
        for (k, v) in map {
            if matches!(
                k.as_str(),
                "ROOT_QUERY" | "ROOT_MUTATION" | "ROOT_SUBSCRIPTION"
            ) {
                continue;
            }
            let Some(obj) = v.as_object() else {
                continue;
            };
            let group = obj
                .get("__typename")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| k.split_once(':').map(|(prefix, _)| prefix.to_string()));
            if let Some(ty) = group {
                by_type.entry(ty).or_default().push(v.clone());
            }
        }
        for (ty, items) in by_type {
            if items.len() >= 3 {
                let mut sub = path.clone();
                sub.push(ty);
                out.push((sub, items));
            }
        }
    }
    for (k, v) in map {
        if v.is_object() {
            path.push(k.clone());
            collect_normalized_entity_maps(v, path, out);
            path.pop();
        }
    }
}

/// A row's text field: a string, or the `text` of an object (GraphQL shipping a
/// caption or a description as `{text: "…"}` is the same fact in another shape).
fn row_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Object(map) => ["text", "title", "name", "content"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(|text| text.trim().to_string()),
        _ => None,
    }
    .filter(|text: &String| !text.is_empty())
}

/// The object a collection row really is. A row is often wrapped in exactly one
/// object-valued key (`node`, `data`, `attributes`, `fields`); the row is inside
/// it. Only a wrapper with no title/detail key of its own is unwrapped, so a
/// real row that happens to carry a `data` field keeps its own fields.
fn unwrap_row<'a>(obj: &'a serde_json::Map<String, Value>) -> &'a serde_json::Map<String, Value> {
    const WRAPPERS: &[&str] = &["node", "item", "attributes", "fields", "entity", "record"];
    for key in WRAPPERS {
        let Some(inner) = obj.get(*key).and_then(Value::as_object) else {
            continue;
        };
        if inner.get("title").is_some() || inner.get("name").is_some() {
            return inner;
        }
    }
    obj
}

fn extract_collection_rows(elements: &[Value], page_url: &str) -> Vec<CollectionRow> {
    const TITLE_KEYS: &[&str] = &[
        "title",
        "name",
        "headline",
        "label",
        "displayName",
        "display_name",
        "subject",
        "question",
        // A post's own words are its title when it has none: Instagram and
        // Mastodon ship `caption`/`content` and nothing else.
        "caption",
        "content",
    ];
    const DETAIL_KEYS: &[&str] = &[
        "departmentName",
        "teamName",
        "locationName",
        "employmentType",
        "workplaceType",
        "compensationTierSummary",
        "summary",
        "description",
        "subtitle",
        "excerpt",
        "category",
        "location",
        "department",
        "author",
        "version",
        "accessibility_caption",
    ];
    const URL_KEYS: &[&str] = &[
        "url",
        "jobUrl",
        "applyUrl",
        "externalLink",
        "permalink",
        "href",
        "html_url",
        "web_url",
        "link",
        "path",
        "slug",
        "seo_canonical_url",
        "canonical_url",
        "permalink_url",
    ];

    let mut seen_urls = BTreeSet::new();
    let mut out = Vec::new();
    for el in elements.iter().take(250) {
        let Some(obj) = el.as_object() else {
            continue;
        };
        // GraphQL, Relay and JSON:API wrap each row in one object-valued key
        // (`{"edges":[{"node":{…}}]}`, `{"data":[{…}]}`, `{"attributes":{…}}`).
        // The row is what is inside the wrapper; unwrapping it is what lets a
        // profile's posts be read from the app's own inline state.
        let obj = unwrap_row(obj);
        let title = TITLE_KEYS
            .iter()
            .find_map(|k| obj.get(*k).and_then(row_text))
            .map(|s| s.chars().take(240).collect::<String>())
            .filter(|s: &String| s.len() >= 2 && !s.starts_with("http"));
        let Some(title) = title else {
            continue;
        };

        let mut details = Vec::new();
        for key in DETAIL_KEYS {
            if let Some(val) = obj.get(*key).and_then(scalar_str) {
                let clean = strip_tags(&val);
                let clean = clean.trim();
                if !clean.is_empty() && clean != title && details.len() < 4 {
                    details.push(clean.chars().take(140).collect::<String>());
                }
            }
        }

        let item_url = URL_KEYS
            .iter()
            .find_map(|k| obj.get(*k).and_then(Value::as_str))
            .and_then(|raw| resolve_item_url(raw, page_url))
            .or_else(|| {
                // Common SPA pattern (e.g., Ashby job postings): each item has UUID `id`
                // and mounts directly under `<page_url>/<id>`.
                obj.get("id")
                    .and_then(Value::as_str)
                    .filter(|id| looks_like_route_id(id))
                    .and_then(|id| join_child_segment(page_url, id))
            })
            .filter(|u| seen_urls.insert(u.clone()));

        out.push((title.to_string(), details.join(" · "), item_url));
    }
    out
}

fn looks_like_route_id(id: &str) -> bool {
    let len = id.len();
    (8..=80).contains(&len)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && id.chars().any(|c| c.is_ascii_digit() || c == '-')
}

fn join_child_segment(page_url: &str, segment: &str) -> Option<String> {
    let mut url = url::Url::parse(page_url).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let base_path = url.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}/{segment}"));
    Some(url.to_string())
}

fn resolve_item_url(raw: &str, page_url: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(' ') {
        return None;
    }
    if let Ok(u) = url::Url::parse(trimmed) {
        if matches!(u.scheme(), "http" | "https") {
            return Some(u.to_string());
        }
        return None;
    }
    if trimmed.starts_with('/') {
        let base = url::Url::parse(page_url).ok()?;
        return base.join(trimmed).ok().map(|u| u.to_string());
    }
    join_child_segment(page_url, trimmed)
}

fn extract_primary_entity(value: &Value, page_url: &str) -> Option<(usize, String, String)> {
    let mut best: Option<(usize, String, String)> = None;
    walk_entities(value, 0, page_url, &mut best);
    best
}

fn walk_entities(
    value: &Value,
    depth: usize,
    page_url: &str,
    best: &mut Option<(usize, String, String)>,
) {
    if depth > 6 {
        return;
    }
    let Value::Object(map) = value else {
        return;
    };
    let title = ["title", "name", "displayName", "full_name", "headline"]
        .iter()
        .find_map(|k| map.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or_default();

    let prose = [
        "descriptionHtml",
        "descriptionPlain",
        "readme",
        "content",
        "body",
        "description",
        "markdown",
        "overview",
    ]
    .iter()
    .find_map(|k| {
        map.get(*k)
            .and_then(Value::as_str)
            .filter(|s| s.trim().len() >= 280)
            .map(|s| (*k, s.trim()))
    });

    if !title.is_empty() {
        if let Some((field_name, body_str)) = prose {
            let rendered_body = if field_name.to_ascii_lowercase().contains("html")
                || (body_str.contains("<p") || body_str.contains("<div") || body_str.contains("<h"))
            {
                body_str.to_string()
            } else {
                crate::synthesize::render_markdown_to_html(body_str, page_url)
            };
            let score = body_str.chars().count().min(12_000);
            if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
                let html = format!(
                    "<article><h1>{}</h1>\n{}</article>",
                    escape_html(title),
                    rendered_body
                );
                *best = Some((score, title.to_string(), html));
            }
        }
    }

    for child in map.values() {
        walk_entities(child, depth + 1, page_url, best);
    }
}

fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(crate) fn escape_attr(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_html_without_app_scripts_is_never_a_shell() {
        let short_static = "<html><head><title>Example Domain</title></head><body><h1>Example Domain</h1><p>This domain is for use in illustrative examples.</p></body></html>";
        let verdict = assess_html_shell(short_static, "https://example.com/");
        assert!(!verdict.has_app_program);
        assert!(!verdict.is_unhydrated_shell);

        let with_ld_and_analytics = r#"<html><head>
            <script type="application/ld+json">{"@type":"WebPage"}</script>
            <script async src="https://www.googletagmanager.com/gtag/js?id=G-123"></script>
        </head><body><h1>Tiny Note</h1><p>Hello world.</p></body></html>"#;
        let verdict = assess_html_shell(with_ld_and_analytics, "https://example.com/");
        assert!(!verdict.has_app_program);
        assert!(!verdict.is_unhydrated_shell);
    }

    #[test]
    fn partial_html_shell_with_heavy_chrome_and_null_next_data_is_detected() {
        let bundlephobia_like = r#"<html><head><title>BundlePhobia</title></head><body>
            <header><nav>Bundlephobia Scan package.json GitHub Sponsors Twitter Quick stats about npm packages and bundle size analysis</nav></header>
            <div id="__next"><div class="page-container"></div></div>
            <footer>Supporters DigitalOcean Algolia Moonrank Hosted with love for open source developers everywhere</footer>
            <script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"initialResult":null}},"page":"/package/[...package]"}</script>
            <script defer src="/_next/static/chunks/main.js"></script>
        </body></html>"#;
        let verdict =
            assess_html_shell(bundlephobia_like, "https://bundlephobia.com/package/react");
        assert!(verdict.has_app_program);
        assert!(verdict.explicit_unhydrated_payload);
        assert!(verdict.is_unhydrated_shell, "{verdict:?}");
    }

    #[test]
    fn ssr_page_with_real_prose_and_app_scripts_is_not_a_shell() {
        let ssr = format!(
            r#"<html><head><script type="module" src="/_app/entry.js"></script></head>
            <body><nav>Home Docs Blog</nav><main><article><h1>useState</h1><p>{}</p><p>{}</p></article></main></body></html>"#,
            "useState is a React Hook that lets you add a state variable to your component. "
                .repeat(4),
            "Call useState at the top level of your component to declare a state variable. "
                .repeat(4),
        );
        let verdict = assess_html_shell(&ssr, "https://react.dev/reference/react/useState");
        assert!(verdict.has_app_program);
        assert!(!verdict.is_unhydrated_shell, "{verdict:?}");
    }

    #[test]
    fn inline_window_appdata_materializes_collection_with_links() {
        let html = r#"<!doctype html><html><head><title>Jobs</title></head><body>
            <div id="root"></div>
            <noscript>You need to enable JavaScript to run this app.</noscript>
            <script>window.__appData = {"organization":{"name":"Linear"},"jobBoard":{"jobPostings":[
                {"id":"11111111-2222-3333-4444-555555555551","title":"Staff Systems Engineer","departmentName":"Engineering","locationName":"Remote"},
                {"id":"11111111-2222-3333-4444-555555555552","title":"Senior Product Designer","departmentName":"Design","locationName":"San Francisco"},
                {"id":"11111111-2222-3333-4444-555555555553","title":"Security Engineer","departmentName":"Security","locationName":"Europe"}
            ]}};</script>
            <script src="/assets/index.js"></script>
        </body></html>"#;
        let m = materialize_inline_state(html, "https://jobs.ashbyhq.com/linear")
            .expect("inline state");
        assert_eq!(m.items, 3);
        assert!(m.html.contains("Staff Systems Engineer"));
        assert!(m
            .html
            .contains("https://jobs.ashbyhq.com/linear/11111111-2222-3333-4444-555555555551"));
    }

    #[test]
    fn tailwind_sidebar_grid_wrapper_around_main_does_not_false_trigger_shell() {
        let html = format!(
            r#"<html><head><script type="module" src="/app.js"></script></head><body>
            <div class="lg:grid-cols-sidebar-content grid">
                <nav>Sidebar links and navigation items</nav>
                <main><article><h1>Hook Reference</h1><p>{}</p><p>{}</p></article></main>
            </div></body></html>"#,
            "Call useState at the top level of your component to declare state. ".repeat(4),
            "React preserves this state value across re-renders of the component. ".repeat(4),
        );
        let verdict = assess_html_shell(&html, "https://react.dev/reference/react/useState");
        assert!(!verdict.is_unhydrated_shell, "{verdict:?}");
    }

    #[test]
    fn normalized_apollo_state_entity_map_materializes_without_js() {
        let html = r#"<!doctype html><html><head><title>Directory</title></head><body>
            <div id="app"></div>
            <script>window.__APOLLO_STATE__ = {
                "ROOT_QUERY": {"__typename":"Query"},
                "Tool:1": {"__typename":"Tool","name":"Ripgrep","description":"Line-oriented recursive search tool","url":"/tools/ripgrep"},
                "Tool:2": {"__typename":"Tool","name":"Fd","description":"Fast and user-friendly alternative to find","url":"/tools/fd"},
                "Tool:3": {"__typename":"Tool","name":"Bat","description":"Cat clone with syntax highlighting and Git integration","url":"/tools/bat"}
            };</script>
        </body></html>"#;
        let m = materialize_inline_state(html, "https://tools.example.com/").expect("apollo map");
        assert_eq!(m.items, 3);
        assert!(m.html.contains("Ripgrep"));
        assert!(m.html.contains("https://tools.example.com/tools/ripgrep"));
    }

    #[test]
    fn scan_global_json_assignments_handles_utf8_and_eof_without_panic() {
        let mut out = Vec::new();
        for script in [
            "window.",
            "window.😀",
            "window.x\u{2003}=== 1; globalThis.y == 2;",
            "self.data =   JSON.parse(\"{\\\"k\\\":\\\"😀\\\"}\"); window.z =   {\"a\":\"é\"}",
        ] {
            scan_global_json_assignments(script, &mut out);
        }
    }
}
