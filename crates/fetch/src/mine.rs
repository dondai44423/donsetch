//! Mined request contracts: the requests an application's own program made to
//! assemble its content, stored per (host, identity, URL shape) and replayed as
//! plain HTTP before the content lane runs again.
//!
//! A lane run leaves a wire transcript. Mining finds the response array whose
//! values the produced document shows, and stores the request that delivered it
//! as a template: the document's variable components (path segments, query
//! values) become `{{n}}` placeholders. A later encounter fills them from its
//! own document URL and replays the request as the same identity, on the same
//! jar and wire, without JavaScript.
//!
//! The contract is checked structurally on every replay: the array is there,
//! its items carry the same field paths, and at least `min_items` of them have
//! a title. It is never a text diff against the mined values, because content
//! changes; it is a promise that the same request still returns the same shape
//! of answer. On a break (an error status, a missing array, too few items) the
//! contract is dropped and the lane runs again, which mines a fresh one.

use crate::datapath::Item;
use crate::engine::TranscriptEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlueprintKind {
    #[default]
    Collection,
    EntityJson,
    HtmlFragment,
}

pub struct ReplayPayload {
    pub html: String,
    pub title: String,
    pub text_chars: usize,
    pub items: usize,
    pub sample: Vec<String>,
    pub sample_urls: Vec<String>,
}

/// One mined request contract. The field paths are relative to an item object
/// in the response array, never to values: values change, shapes do not.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MinedContract {
    /// What kind of structured answer the mined endpoint returns.
    #[serde(default)]
    pub kind: BlueprintKind,
    /// The document host this was mined on.
    pub host: String,
    /// The identity whose wire made the request; a contract is bound to it.
    pub identity: String,
    /// The document URL shape, e.g. `/profile/*` or `/?q=*`.
    pub shape: String,
    pub method: String,
    /// The request URL with `{{n}}` placeholders for document components.
    pub url: String,
    /// The app's own request headers, filtered to request-level ones and with
    /// the document URL and components templated.
    pub headers: Vec<(String, String)>,
    /// The request body, templated when it is UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The body is base64 (worked out at replay), not the UTF-8 text above.
    pub body_b64: bool,
    /// The object keys leading to the array of items.
    pub array_path: Vec<String>,
    /// The top-level keys every item object carries, for a shape check.
    pub item_keys: Vec<String>,
    /// The field (within an item) that carries its URL, when one was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_field: Option<Vec<String>>,
    /// The field that carries its title.
    pub title_field: Vec<String>,
    /// Further fields joined into the item's detail.
    pub detail_fields: Vec<Vec<String>>,
    /// Fewer items than this and the replay is a break, not content.
    pub min_items: usize,
    /// The item titles and URLs the lane's produced document showed: evidence
    /// for the equivalence check, never the replay's test.
    pub sample: Vec<String>,
    pub sample_urls: Vec<String>,
    pub mined_unix: i64,
    pub uses: u32,
    pub last_ok_unix: i64,
    pub failures: u32,
}

/// A request ready to leave the wire: URL, headers and body.
type RequestParts = (String, Vec<(String, String)>, Option<Vec<u8>>);

impl MinedContract {
    /// The slot values this URL supplies for `{{0}}`, `{{1}}`, … in the
    /// contract's shape; `None` when the URL does not have that shape.
    pub fn fill(&self, url: &str) -> Option<Vec<String>> {
        let parts = UrlParts::parse(url)?;
        let shape = UrlParts::parse(&self.shape)?;
        if shape.path.len() != parts.path.len() || shape.query.len() != parts.query.len() {
            return None;
        }
        let mut values = Vec::new();
        for (want, got) in shape.path.iter().zip(&parts.path) {
            if want == "*" {
                values.push(got.clone());
            } else if want != got {
                return None;
            }
        }
        for ((want_key, want_value), (got_key, got_value)) in shape.query.iter().zip(&parts.query) {
            if want_key != got_key {
                return None;
            }
            if want_value == "*" {
                values.push(got_value.clone());
            } else if want_value != got_value {
                return None;
            }
        }
        Some(values)
    }

    /// The request URL, headers and body with the document's components filled
    /// in, or `None` when the URL does not match the contract's shape.
    pub fn materialize(&self, url: &str, document_url: &str) -> Option<RequestParts> {
        let values = self.fill(url)?;
        let request_url = fill_url(&self.url, document_url, &values);
        let safe_doc: String = document_url
            .chars()
            .filter(|c| *c != '\r' && *c != '\n')
            .collect();
        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let mut out = value.replace("{{doc}}", &safe_doc);
                for (index, slot) in values.iter().enumerate() {
                    let clean: String = slot.chars().filter(|c| *c != '\r' && *c != '\n').collect();
                    out = out.replace(&format!("{{{{{index}}}}}"), &clean);
                }
                (name.clone(), out)
            })
            .collect();
        let is_form = headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type")
                && value
                    .to_ascii_lowercase()
                    .contains("application/x-www-form-urlencoded")
        });
        let body = self.body.as_ref().map(|body| {
            if self.body_b64 {
                crate::http::unbase64(body).unwrap_or_default()
            } else {
                fill_body(body, document_url, &values, is_form).into_bytes()
            }
        });
        Some((request_url, headers, body))
    }

    /// Items from a replayed response body, or `None` when the answer no longer
    /// has the mined shape. The values are new; the structure is the contract.
    pub fn extract(&self, body: &str, request_url: &str) -> Option<Vec<Item>> {
        let value: Value = serde_json::from_str(body).ok()?;
        let array = self.find_array(&value)?;
        let mut items: Vec<Item> = Vec::new();
        for element in array {
            let Some(object) = element.as_object() else {
                continue;
            };
            if !self.has_shape(object) {
                continue;
            }
            let title = plain(
                self.string_at(&Value::Object(object.clone()), &self.title_field)
                    .unwrap_or_default()
                    .trim(),
            );
            if title.is_empty() {
                continue;
            }
            let url = self
                .url_field
                .as_ref()
                .and_then(|field| self.string_at(&Value::Object(object.clone()), field))
                .and_then(|raw| resolve_url(&raw, request_url))
                .filter(|url| items.iter().all(|item| item.url.as_deref() != Some(url)));
            let mut detail: Vec<String> = Vec::new();
            for field in &self.detail_fields {
                if let Some(value) = self.string_at(&Value::Object(object.clone()), field) {
                    let value = plain(value.trim());
                    if !value.is_empty() && value != title {
                        detail.push(value.chars().take(160).collect());
                    }
                }
            }
            items.push(Item {
                title: title.chars().take(300).collect(),
                detail: detail.join(" · "),
                url,
            });
            if items.len() >= 100 {
                break;
            }
        }
        (items.len() >= self.min_items).then_some(items)
    }

    /// Extract a full replay payload (`Collection`, `EntityJson`, or `HtmlFragment`)
    /// from a replayed HTTP response body. Returns `None` on any structural break.
    pub fn extract_payload(
        &self,
        body: &str,
        request_url: &str,
        document_url: &str,
        host: &str,
    ) -> Option<ReplayPayload> {
        match self.kind {
            BlueprintKind::Collection => {
                let items = self.extract(body, request_url)?;
                let text_chars: usize = items
                    .iter()
                    .map(|item| item.title.chars().count() + item.detail.chars().count())
                    .sum();
                let title = items
                    .first()
                    .map(|item| item.title.clone())
                    .unwrap_or_else(|| format!("replay {host}"));
                let sample = items
                    .iter()
                    .take(5)
                    .map(|item| item.title.clone())
                    .collect();
                let sample_urls = items
                    .iter()
                    .filter_map(|item| item.url.clone())
                    .take(5)
                    .collect();
                let html = render_html(host, &items);
                Some(ReplayPayload {
                    html,
                    title,
                    text_chars: text_chars.max(1),
                    items: items.len(),
                    sample,
                    sample_urls,
                })
            }
            BlueprintKind::EntityJson => {
                let value: Value = serde_json::from_str(body).ok()?;
                let obj = crate::synthesize::unwrap_entity_object(&value)?;
                if !shares_shape(obj, &self.item_keys) {
                    return None;
                }
                let (html, title, text_chars) =
                    crate::synthesize::render_entity_replay_html(&value, document_url)?;
                Some(ReplayPayload {
                    html,
                    sample: vec![title.clone()],
                    sample_urls: Vec::new(),
                    title,
                    text_chars,
                    items: 1,
                })
            }
            BlueprintKind::HtmlFragment => {
                let trimmed = body.trim();
                if !trimmed.starts_with('<') {
                    return None;
                }
                let stripped = crate::contract::strip_noise(trimmed);
                let parsed = scraper::Html::parse_fragment(&stripped);
                let plain_text = crate::contract::normalize(
                    &parsed.root_element().text().collect::<Vec<_>>().join(" "),
                );
                let text_chars = plain_text.chars().count();
                if text_chars < 120 {
                    return None;
                }
                let heading_sel = scraper::Selector::parse("h1, h2, title").ok();
                let title = heading_sel
                    .as_ref()
                    .and_then(|sel| parsed.select(sel).next())
                    .map(|el| crate::contract::normalize(&el.text().collect::<Vec<_>>().join(" ")))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("replay {host}"));
                let html = format!(
                    "<!DOCTYPE html><html><head><title>{}</title></head><body><main>{}</main></body></html>",
                    escape_html_text(&title),
                    trimmed
                );
                Some(ReplayPayload {
                    html,
                    sample: vec![title.clone()],
                    sample_urls: Vec::new(),
                    title,
                    text_chars,
                    items: 1,
                })
            }
        }
    }

    /// The array the contract names, or one elsewhere in the JSON that still
    /// has the mined item shape (an app may wrap its answer differently).
    fn find_array<'a>(&self, value: &'a Value) -> Option<&'a Vec<Value>> {
        if let Some(found) = path_get(value, &self.array_path).and_then(Value::as_array) {
            return Some(found);
        }
        fn walk<'a>(value: &'a Value, keys: &[String], best: &mut Option<&'a Vec<Value>>) {
            match value {
                Value::Array(elements) => {
                    if elements.iter().any(|element| {
                        element
                            .as_object()
                            .is_some_and(|object| shares_shape(object, keys))
                    }) && best
                        .map(|found| elements.len() > found.len())
                        .unwrap_or(true)
                    {
                        *best = Some(elements);
                    }
                    for element in elements {
                        walk(element, keys, best);
                    }
                }
                Value::Object(map) => {
                    for child in map.values() {
                        walk(child, keys, best);
                    }
                }
                _ => {}
            }
        }
        let mut best = None;
        walk(value, &self.item_keys, &mut best);
        best
    }

    fn has_shape(&self, object: &serde_json::Map<String, Value>) -> bool {
        shares_shape(object, &self.item_keys)
    }

    fn string_at(&self, value: &Value, path: &[String]) -> Option<String> {
        let found = path_get(value, path)?;
        match found {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            Value::Bool(flag) => Some(flag.to_string()),
            _ => None,
        }
    }
}

/// The URL split the shape speaks: decoded path segments and query pairs, in
/// the order they appear.
struct UrlParts {
    path: Vec<String>,
    query: Vec<(String, String)>,
}

impl UrlParts {
    fn parse(raw: &str) -> Option<Self> {
        // A contract's shape is a path and query, not an absolute URL; parse
        // it against a throwaway base so the same splitter reads both.
        let absolute;
        let url = if raw.starts_with("http://") || raw.starts_with("https://") {
            url::Url::parse(raw).ok()?
        } else {
            absolute = format!("https://shape.invalid{raw}");
            url::Url::parse(&absolute).ok()?
        };
        let path = url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .map(percent_decode)
                    .collect()
            })
            .unwrap_or_default();
        let query = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        Some(Self { path, query })
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[index + 1]), hex_val(bytes[index + 2])) {
                out.push((hi << 4) | lo);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_decode(text: &str) -> String {
    percent_decode(&text.replace('+', " "))
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn escape_json_str(text: &str) -> String {
    match serde_json::to_string(text) {
        Ok(quoted) if quoted.len() >= 2 => quoted[1..quoted.len() - 1].to_string(),
        _ => text.to_string(),
    }
}

fn fill_url(template_url: &str, document_url: &str, values: &[String]) -> String {
    let mut out = if let Some(rest) = template_url.strip_prefix("{{doc}}") {
        format!("{document_url}{rest}")
    } else {
        template_url.to_string()
    };
    if out.contains("{{doc}}") {
        out = out.replace("{{doc}}", &encode_uri_component(document_url));
    }
    for (index, value) in values.iter().enumerate() {
        out = out.replace(&format!("{{{{{index}}}}}"), &encode_uri_component(value));
    }
    out
}

fn fill_body(template_body: &str, document_url: &str, values: &[String], is_form: bool) -> String {
    let trimmed = template_body.trim_start();
    let is_json = trimmed.starts_with('{') || trimmed.starts_with('[');
    let encode = |raw: &str| -> String {
        if is_json {
            escape_json_str(raw)
        } else if is_form {
            encode_uri_component(raw)
        } else {
            raw.to_string()
        }
    };
    let mut out = template_body.replace("{{doc}}", &encode(document_url));
    for (index, value) in values.iter().enumerate() {
        out = out.replace(&format!("{{{{{index}}}}}"), &encode(value));
    }
    out
}

/// The document's shape, with the components that appear in the app's request
/// replaced by `*`; and the request URL/body/headers with those components
/// replaced by `{{n}}`.
struct Templated {
    shape: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    body_b64: bool,
}

fn template(document_url: &str, entry: &TranscriptEntry) -> Option<Templated> {
    let doc = UrlParts::parse(document_url)?;
    let request_body = entry
        .request_body
        .as_ref()
        .and_then(|body| crate::http::unbase64(body))
        .unwrap_or_default();
    let body_text = String::from_utf8(request_body).ok();

    // Which document components actually parameterize the app's request? Only
    // components whose substitution changes the request URL, kept headers, or
    // UTF-8 body become `*` wildcards in `shape`; otherwise static endpoints on
    // parameterized pages (e.g. `GET /api/featured` on `/products/12345`) would
    // wildcard `12345` with zero `{{n}}` slots and poison `/products/99999`.
    let mut values: Vec<String> = Vec::new();
    let mut path_shape: Vec<String> = Vec::new();
    for (index, segment) in doc.path.iter().enumerate() {
        let candidate = if index == 0 {
            parameter_segment(segment) && distinctive(segment)
        } else {
            distinctive(segment) || parameter_segment(segment)
        };
        if candidate && component_substitutes(entry, document_url, body_text.as_deref(), segment) {
            path_shape.push("*".into());
            if !values.contains(segment) {
                values.push(segment.clone());
            }
        } else {
            path_shape.push(segment.clone());
        }
    }
    let mut query_shape: Vec<(String, String)> = Vec::new();
    for (key, value) in &doc.query {
        if value.chars().count() >= 2
            && component_substitutes(entry, document_url, body_text.as_deref(), value)
        {
            query_shape.push((key.clone(), "*".into()));
            if !values.contains(value) {
                values.push(value.clone());
            }
        } else {
            query_shape.push((key.clone(), value.clone()));
        }
    }
    // A home page has no components; the shape is the path itself.
    let mut shape = format!("/{}", path_shape.join("/"));
    if !query_shape.is_empty() {
        shape.push('?');
        shape.push_str(
            &query_shape
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        );
    }

    let mut url = entry.url.replace(document_url, "{{doc}}");
    for (index, value) in values.iter().enumerate() {
        if value.chars().count() >= 2 {
            url = replace_component(&url, value, &format!("{{{{{index}}}}}"));
        }
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in &entry.request_headers {
        if !keep_header(name) {
            continue;
        }
        let mut value = value.replace(document_url, "{{doc}}");
        if !matches!(
            name.to_ascii_lowercase().as_str(),
            "origin"
                | "referer"
                | "accept"
                | "content-type"
                | "sec-fetch-site"
                | "sec-fetch-mode"
                | "sec-fetch-dest"
        ) {
            for (index, component) in values.iter().enumerate() {
                if component.chars().count() >= 3 {
                    value = value.replace(component, &format!("{{{{{index}}}}}"));
                }
            }
        }
        if let Some(existing) = headers
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
        {
            existing.1 = value;
        } else {
            headers.push((name.clone(), value));
        }
    }
    let (body, body_b64) = match body_text {
        Some(mut text) => {
            text = text.replace(document_url, "{{doc}}");
            for (index, component) in values.iter().enumerate() {
                if component.chars().count() >= 3 {
                    let slot = format!("{{{{{index}}}}}");
                    text = text.replace(component, &slot);
                    let escaped = escape_json_str(component);
                    if escaped != *component {
                        text = text.replace(&escaped, &slot);
                    }
                }
            }
            (Some(text), false)
        }
        None => (
            entry.request_body.clone().filter(|body| !body.is_empty()),
            true,
        ),
    };
    Some(Templated {
        shape,
        url,
        headers,
        body,
        body_b64,
    })
}

fn component_substitutes(
    entry: &TranscriptEntry,
    document_url: &str,
    body_text: Option<&str>,
    value: &str,
) -> bool {
    if value.chars().count() < 2 {
        return false;
    }
    let base_url = entry.url.replace(document_url, "{{doc}}");
    if replace_component(&base_url, value, "\x00SLOT\x00") != base_url {
        return true;
    }
    if value.chars().count() >= 3 {
        if entry.request_headers.iter().any(|(name, header_val)| {
            keep_header(name)
                && !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "origin"
                        | "referer"
                        | "accept"
                        | "content-type"
                        | "sec-fetch-site"
                        | "sec-fetch-mode"
                        | "sec-fetch-dest"
                )
                && header_val.replace(document_url, "{{doc}}").contains(value)
        }) {
            return true;
        }
        if let Some(text) = body_text {
            let scrubbed = text.replace(document_url, "{{doc}}");
            if scrubbed.contains(value) {
                return true;
            }
            let escaped = escape_json_str(value);
            if escaped != value && scrubbed.contains(&escaped) {
                return true;
            }
        }
    }
    false
}

fn distinctive(value: &str) -> bool {
    let chars = value.chars().count();
    chars >= 8
        || value.chars().any(|c| c.is_ascii_digit())
        || value.chars().any(|c| !c.is_alphanumeric())
}

fn parameter_segment(value: &str) -> bool {
    if value.chars().count() < 2 {
        return false;
    }
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "docs"
            | "latest"
            | "overview"
            | "api"
            | "examples"
            | "styling"
            | "helm"
            | "npm"
            | "search"
            | "items"
            | "reviews"
            | "settings"
            | "packages"
            | "package"
            | "providers"
            | "components"
            | "profile"
            | "users"
    )
}

fn replace_affix_token(segment: &str, value: &str, placeholder: &str) -> Option<String> {
    if value.chars().count() < 3 {
        return None;
    }
    let decoded = percent_decode(segment);
    let mut start = 0usize;
    while let Some(rel) = decoded[start..].find(value) {
        let idx = start + rel;
        let end = idx + value.len();
        let prev_delim = idx == 0
            || matches!(
                decoded.as_bytes()[idx - 1],
                b'-' | b'_' | b'.' | b':' | b'@'
            );
        let next_delim = end == decoded.len()
            || matches!(decoded.as_bytes()[end], b'-' | b'_' | b'.' | b':' | b'@');
        if prev_delim && next_delim && (idx > 0 || end < decoded.len()) {
            let mut out = String::with_capacity(decoded.len() + placeholder.len());
            out.push_str(&decoded[..idx]);
            out.push_str(placeholder);
            out.push_str(&decoded[end..]);
            return Some(out);
        }
        start = idx + value.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Replace a whole path segment, query value, or token-bounded segment affix
/// (such as `material-button.html` -> `material-{{0}}.html`).
fn replace_component(url: &str, value: &str, placeholder: &str) -> String {
    let Some((head, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let mut out = format!("{head}://{authority}");
    let (path_and_query, fragment) = match tail.find('#') {
        Some(at) => (&tail[..at], &tail[at..]),
        None => (tail, ""),
    };
    let (path, query) = match path_and_query.find('?') {
        Some(at) => (&path_and_query[..at], Some(&path_and_query[at + 1..])),
        None => (path_and_query, None),
    };
    let path = path
        .split('/')
        .map(|segment| {
            if percent_decode(segment) == value {
                placeholder.to_string()
            } else if let Some(affixed) = replace_affix_token(segment, value, placeholder) {
                affixed
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    out.push_str(&path);
    if let Some(query) = query {
        out.push('?');
        out.push_str(
            &query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((key, raw)) if query_decode(raw) == value => {
                        format!("{key}={placeholder}")
                    }
                    _ => pair.to_string(),
                })
                .collect::<Vec<_>>()
                .join("&"),
        );
    }
    out.push_str(fragment);
    out
}

fn keep_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "cookie"
            | "host"
            | "content-length"
            | "connection"
            | "accept-encoding"
            | "user-agent"
            | "accept-language"
            | "sec-ch-ua"
            | "sec-ch-ua-mobile"
            | "sec-ch-ua-platform"
            | "priority"
            | "pragma"
            | "cache-control"
            | "if-none-match"
            | "if-modified-since"
    )
}

fn resolve_url(raw: &str, request_url: &str) -> Option<String> {
    let raw = raw.trim();
    if let Ok(url) = url::Url::parse(raw) {
        if matches!(url.scheme(), "http" | "https") {
            return Some(url.to_string());
        }
        return None;
    }
    if !raw.starts_with('/') {
        return None;
    }
    let base = url::Url::parse(request_url).ok()?;
    base.join(raw).ok().map(|url| url.to_string())
}

fn path_get<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut here = value;
    for key in path {
        here = here.get(key)?;
    }
    Some(here)
}

fn shares_shape(object: &serde_json::Map<String, Value>, keys: &[String]) -> bool {
    if keys.is_empty() {
        return true;
    }
    let present = keys
        .iter()
        .filter(|key| object.contains_key(key.as_str()))
        .count();
    present * 2 >= keys.len()
}

/// Every string leaf in a JSON value, with the path that reaches it.
fn leaves<'a>(value: &'a Value, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, &'a str)>) {
    match value {
        Value::String(text) => out.push((path.clone(), text)),
        Value::Object(map) => {
            for (key, child) in map {
                path.push(key.clone());
                leaves(child, path, out);
                path.pop();
            }
        }
        Value::Array(elements) => {
            for child in elements {
                leaves(child, path, out);
            }
        }
        _ => {}
    }
}

fn arrays_of_objects<'a>(
    value: &'a Value,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, &'a Vec<Value>)>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                path.push(key.clone());
                arrays_of_objects(child, path, out);
                path.pop();
            }
        }
        Value::Array(elements) => {
            if !elements.is_empty() && elements.iter().all(Value::is_object) {
                out.push((path.clone(), elements));
            }
            for child in elements {
                arrays_of_objects(child, path, out);
            }
        }
        _ => {}
    }
}

/// Mine a contract from a lane run's produced document and wire transcript.
/// `None` when no JSON response can be tied to what the document shows: an app
/// that renders nothing from a data response has nothing to replay.
pub fn mine(
    document: &str,
    document_url: &str,
    identity: &str,
    transcript: &[TranscriptEntry],
) -> Option<MinedContract> {
    use scraper::{Html, Selector};

    let parsed = Html::parse_document(&crate::contract::strip_noise(document));
    let text =
        crate::contract::normalize(&parsed.root_element().text().collect::<Vec<_>>().join(" "));
    let lower_text = text.to_lowercase();
    let hrefs: BTreeSet<String> = parsed
        .select(&Selector::parse("[href], [src]").expect("static selector"))
        .filter_map(|element| {
            element
                .value()
                .attr("href")
                .or_else(|| element.value().attr("src"))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_lowercase())
        })
        .collect();
    let text = lower_text;

    let mut best: Option<(usize, usize, MinedContract)> = None;
    let mut best_singular: Option<(usize, MinedContract)> = None;
    for entry in transcript {
        if !(200..300).contains(&entry.status) {
            continue;
        }
        let Some(body) = entry.body.as_deref() else {
            continue;
        };
        if let Some((score, singular)) =
            mine_entity_or_fragment(document_url, identity, entry, body, &text)
        {
            if best_singular.as_ref().is_none_or(|(prev, _)| score > *prev) {
                best_singular = Some((score, singular));
            }
        }
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            continue;
        };
        let mut arrays = Vec::new();
        arrays_of_objects(&value, &mut Vec::new(), &mut arrays);
        for (array_path, elements) in arrays {
            let mut matched = 0usize;
            for element in elements {
                let mut field_leaves = Vec::new();
                leaves(element, &mut Vec::new(), &mut field_leaves);
                let hit = field_leaves.iter().any(|(_, value)| {
                    let value = value.trim();
                    (value.chars().count() >= 6 && text.contains(&value.to_lowercase()))
                        || (value.starts_with("http") && hrefs.contains(&value.to_lowercase()))
                });
                if hit {
                    matched += 1;
                }
            }
            if matched == 0 {
                continue;
            }
            let Some(contract) = contract_from(
                document_url,
                identity,
                entry,
                &array_path,
                elements,
                &text,
                &hrefs,
            ) else {
                continue;
            };
            let is_param = contract.shape.contains('*');
            let score = (usize::from(is_param), matched, elements.len());
            let better = best
                .as_ref()
                .map(|(best_matched, best_len, best_contract)| {
                    let prev = (
                        usize::from(best_contract.shape.contains('*')),
                        *best_matched,
                        *best_len,
                    );
                    score > prev
                })
                .unwrap_or(true);
            if better {
                best = Some((matched, elements.len(), contract));
            }
        }
    }
    match (best, best_singular) {
        (Some((matched, _, collection)), Some((singular_score, singular))) => {
            let singular_param = singular.shape.contains('*');
            let collection_param = collection.shape.contains('*');
            let sub_array_on_entity = singular_param
                && !collection.array_path.is_empty()
                && !matches!(
                    collection
                        .array_path
                        .last()
                        .map(|s| s.to_ascii_lowercase())
                        .as_deref(),
                    Some(
                        "items"
                            | "results"
                            | "hits"
                            | "nodes"
                            | "edges"
                            | "records"
                            | "entries"
                            | "feed"
                            | "posts"
                            | "statuses"
                            | "jobs"
                            | "packages"
                            | "articles"
                            | "threads"
                    )
                );
            if singular_param
                && (!collection_param
                    || matched <= 2
                    || singular_score >= 1_200
                    || sub_array_on_entity)
            {
                Some(singular)
            } else {
                Some(collection)
            }
        }
        (Some((_, _, collection)), None) => Some(collection),
        (None, Some((_, singular))) => Some(singular),
        (None, None) => None,
    }
}

fn mine_entity_or_fragment(
    document_url: &str,
    identity: &str,
    entry: &TranscriptEntry,
    body: &str,
    lower_doc_text: &str,
) -> Option<(usize, MinedContract)> {
    let templated = template(document_url, entry)?;
    if !templated.shape.contains('*') {
        return None;
    }
    let host = url::Url::parse(document_url).ok()?.host_str()?.to_string();
    let trimmed = body.trim();
    let now = crate::state::now_unix();

    // Case 1: Parameterized HTML fragment endpoint (e.g. Angular Material docs).
    if trimmed.starts_with('<') && !trimmed.to_ascii_lowercase().starts_with("<!doctype") {
        let stripped = crate::contract::strip_noise(trimmed);
        let parsed = scraper::Html::parse_fragment(&stripped);
        let frag_text =
            crate::contract::normalize(&parsed.root_element().text().collect::<Vec<_>>().join(" "));
        let chars = frag_text.chars().count();
        if chars >= 180 {
            let probe: String = frag_text.to_lowercase().chars().take(48).collect();
            if probe.len() >= 20 && lower_doc_text.contains(probe.trim()) {
                return Some((
                    chars.min(6_000) + 1_500,
                    MinedContract {
                        kind: BlueprintKind::HtmlFragment,
                        host,
                        identity: identity.to_string(),
                        shape: templated.shape,
                        method: entry.method.clone(),
                        url: templated.url,
                        headers: templated.headers,
                        body: templated.body,
                        body_b64: templated.body_b64,
                        min_items: 1,
                        mined_unix: now,
                        ..Default::default()
                    },
                ));
            }
        }
    }

    // Case 2: Parameterized single-entity JSON endpoint (e.g. Bundlephobia, ArtifactHub).
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return None;
    };
    let obj = crate::synthesize::unwrap_entity_object(&value)?;
    let scalar_count = obj
        .values()
        .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
        .count();
    if scalar_count < 3 {
        return None;
    }
    let (_html, title, text_chars) =
        crate::synthesize::render_entity_replay_html(&value, document_url)?;
    let has_rich_prose = [
        "readme",
        "content",
        "markdown",
        "descriptionHtml",
        "overview",
    ]
    .iter()
    .any(|k| {
        obj.get(*k)
            .and_then(Value::as_str)
            .is_some_and(|s| s.len() >= 200)
    });
    let has_metrics = (obj.contains_key("size") && obj.contains_key("gzip"))
        || (obj.contains_key("version") && obj.contains_key("description"))
        || (obj.contains_key("description") && scalar_count >= 4 && text_chars >= 120);
    if !has_rich_prose && !has_metrics {
        return None;
    }
    let matched_doc = (!title.is_empty()
        && title.chars().count() >= 2
        && lower_doc_text.contains(&title.to_lowercase()))
        || obj.values().filter_map(Value::as_str).any(|s| {
            let s = s.trim();
            s.chars().count() >= 8 && lower_doc_text.contains(&s.to_lowercase())
        });
    if !matched_doc {
        return None;
    }
    let item_keys: Vec<String> = obj.keys().take(32).cloned().collect();
    let score = text_chars.min(8_000) + usize::from(has_rich_prose) * 1_500 + scalar_count * 80;
    Some((
        score,
        MinedContract {
            kind: BlueprintKind::EntityJson,
            host,
            identity: identity.to_string(),
            shape: templated.shape,
            method: entry.method.clone(),
            url: templated.url,
            headers: templated.headers,
            body: templated.body,
            body_b64: templated.body_b64,
            item_keys,
            min_items: 1,
            sample: vec![title],
            mined_unix: now,
            ..Default::default()
        },
    ))
}

fn contract_from(
    document_url: &str,
    identity: &str,
    entry: &TranscriptEntry,
    array_path: &[String],
    elements: &[Value],
    text: &str,
    hrefs: &BTreeSet<String>,
) -> Option<MinedContract> {
    let templated = template(document_url, entry)?;
    let host = url::Url::parse(document_url).ok()?.host_str()?.to_string();

    // Per leaf path: how many items matched this path, and how long the values
    // are. A title is the field that matched most often and reads longest; a
    // URL is the field whose values the document links to.
    let mut counts: Vec<(Vec<String>, usize, usize, usize)> = Vec::new(); // path, hits, chars, items
    let mut matched_objects = 0usize;
    for element in elements {
        let Some(object) = element.as_object() else {
            continue;
        };
        let mut field_leaves = Vec::new();
        leaves(element, &mut Vec::new(), &mut field_leaves);
        let mut any = false;
        for (path, value) in field_leaves {
            let value = value.trim();
            let hit = (value.chars().count() >= 6 && text.contains(&value.to_lowercase()))
                || (value.starts_with("http") && hrefs.contains(&value.to_lowercase()));
            if !hit {
                continue;
            }
            any = true;
            match counts.iter_mut().find(|(known, _, _, _)| *known == path) {
                Some(entry) => {
                    entry.1 += 1;
                    entry.2 += value.chars().count();
                    entry.3 += 1;
                }
                None => counts.push((path, 1, value.chars().count(), 1)),
            }
        }
        if any {
            matched_objects += 1;
        }
        let _ = object;
    }
    if counts.is_empty() || matched_objects == 0 {
        return None;
    }

    let key_score = |path: &[String]| -> i32 {
        let last = path
            .last()
            .map(|key| key.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(
            last.as_str(),
            "title" | "name" | "headline" | "subject" | "story_title"
        ) {
            // The name a reader would call the item: never a body paragraph.
            5
        } else if matches!(
            last.as_str(),
            "url" | "link" | "permalink" | "href" | "uri" | "web_url" | "story_url"
        ) {
            3
        } else if matches!(
            last.as_str(),
            "text" | "body" | "caption" | "label" | "excerpt" | "summary" | "description"
        ) {
            // The item's prose is detail unless the page showed no title.
            2
        } else if matches!(
            last.as_str(),
            "author"
                | "username"
                | "handle"
                | "points"
                | "score"
                | "date"
                | "created_at"
                | "published"
                | "comments"
        ) {
            1
        } else {
            0
        }
    };
    let mut ordered = counts.clone();
    ordered.sort_by_key(|(path, hits, chars, _)| (key_score(path), *hits, *chars / (*hits).max(1)));
    ordered.reverse();
    // A title is text the document showed, not the item's own URL. A field the
    // page's own rendering missed (an empty highlight span) is still the app's
    // data in the content-bearing response: prefer a title-named field present
    // in the items, then a matched text field, and use the URL only when that
    // is all the page rendered. The replay's structural check is the guard.
    let mut present: Vec<(Vec<String>, usize)> = Vec::new();
    for element in elements {
        let mut field_leaves = Vec::new();
        leaves(element, &mut Vec::new(), &mut field_leaves);
        let mut seen = BTreeSet::new();
        for (path, _) in field_leaves {
            if seen.insert(path.clone()) {
                match present.iter_mut().find(|(known, _)| *known == path) {
                    Some(entry) => entry.1 += 1,
                    None => present.push((path, 1)),
                }
            }
        }
    }
    let title_field = present
        .iter()
        .filter(|(path, count)| key_score(path) >= 5 && *count * 2 >= elements.len())
        .max_by_key(|(path, count)| (key_score(path), *count))
        .map(|(path, _)| path.clone())
        .or_else(|| {
            ordered
                .iter()
                .find(|(path, _, _, _)| key_score(path) >= 2 && !looks_url(path, elements))
                .map(|(path, _, _, _)| path.clone())
        })
        .or_else(|| {
            ordered
                .iter()
                .find(|(path, _, _, _)| !looks_url(path, elements))
                .map(|(path, _, _, _)| path.clone())
        })
        .or_else(|| ordered.first().map(|(path, _, _, _)| path.clone()))
        .expect("ordered is not empty");

    // The URL field is the one whose values are links in the produced
    // document, not just text that happened to match.
    let mut url_field: Option<Vec<String>> = None;
    let mut url_hits = 0;
    for (path, _, _, _) in &counts {
        let mut hits = 0;
        for element in elements {
            let mut field_leaves = Vec::new();
            leaves(element, &mut Vec::new(), &mut field_leaves);
            if let Some((_, value)) = field_leaves.iter().find(|(known, _)| known == path) {
                let value = value.trim().to_lowercase();
                if value.starts_with("http") && hrefs.contains(&value) {
                    hits += 1;
                }
            }
        }
        if hits > url_hits {
            url_hits = hits;
            url_field = Some(path.clone());
        }
    }

    let mut detail_fields: Vec<Vec<String>> = Vec::new();
    for (path, _, _, _) in &ordered {
        if path == &title_field || Some(path) == url_field.as_ref() || looks_url(path, elements) {
            continue;
        }
        detail_fields.push(path.clone());
        if detail_fields.len() == 3 {
            break;
        }
    }

    let mut item_keys: BTreeSet<String> = BTreeSet::new();
    let mut first = true;
    for element in elements {
        let Some(object) = element.as_object() else {
            continue;
        };
        if first {
            item_keys.extend(object.keys().cloned());
            first = false;
        } else {
            item_keys.retain(|key| object.contains_key(key));
        }
    }

    let min_items = matched_objects.clamp(1, 3);
    let sample: Vec<String> = elements
        .iter()
        .filter_map(|element| {
            let object = element.as_object()?;
            let value = Value::Object(object.clone());
            let title = string_at(&value, &title_field)?;
            (!title.trim().is_empty()).then(|| title.chars().take(160).collect())
        })
        .take(5)
        .collect();
    let sample_urls: Vec<String> = elements
        .iter()
        .filter_map(|element| {
            let object = element.as_object()?;
            let value = Value::Object(object.clone());
            let raw = string_at(&value, url_field.as_ref()?)?;
            resolve_url(&raw, &entry.url)
        })
        .take(5)
        .collect();
    let now = crate::state::now_unix();
    Some(MinedContract {
        kind: BlueprintKind::Collection,
        host,
        identity: identity.to_string(),
        shape: templated.shape,
        method: entry.method.clone(),
        url: templated.url,
        headers: templated.headers,
        body: templated.body,
        body_b64: templated.body_b64,
        array_path: array_path.to_vec(),
        item_keys: item_keys.into_iter().take(32).collect(),
        url_field,
        title_field,
        detail_fields,
        min_items,
        sample,
        sample_urls,
        mined_unix: now,
        uses: 0,
        last_ok_unix: 0,
        failures: 0,
    })
}

/// True when most values at this path are URLs: a field of links is never a
/// title, and a URL repeated as detail is noise.
fn looks_url(path: &[String], elements: &[Value]) -> bool {
    let mut seen = 0;
    let mut urls = 0;
    for element in elements {
        let mut field_leaves = Vec::new();
        leaves(element, &mut Vec::new(), &mut field_leaves);
        if let Some((_, value)) = field_leaves.iter().find(|(known, _)| known == path) {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            seen += 1;
            if value.starts_with("http://")
                || value.starts_with("https://")
                || value.starts_with('/')
                || value.contains("://")
            {
                urls += 1;
            }
        }
    }
    seen > 0 && urls * 2 > seen
}

/// Text as a reader sees it: the JSON may carry markup (an app's highlighted
/// field), and the record should not.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn string_at(value: &Value, path: &[String]) -> Option<String> {
    let found = path_get(value, path)?;
    match found {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// The records as the caller reads them: the same markdown the hand-written
/// data paths render, under the contract's own name.
#[allow(dead_code)]
pub fn render(host: &str, items: &[Item]) -> String {
    crate::datapath::render_items(&format!("replay {host}"), items)
}

/// Render replayed items as semantic HTML with `<a href="...">` links so
/// downstream readability and crawl link discovery work seamlessly.
pub fn render_html(host: &str, items: &[Item]) -> String {
    let heading = format!("replay {host}");
    let mut out = format!(
        "<!DOCTYPE html><html><head><title>{}</title></head><body><main><h1>{}</h1><ul>",
        escape_html_text(&heading),
        escape_html_text(&heading)
    );
    for item in items {
        out.push_str("<li><article><h2>");
        if let Some(url) = &item.url {
            out.push_str(&format!(
                "<a href=\"{}\">{}</a>",
                escape_html_attr(url),
                escape_html_text(&item.title)
            ));
        } else {
            out.push_str(&escape_html_text(&item.title));
        }
        out.push_str("</h2>");
        if !item.detail.is_empty() {
            out.push_str(&format!("<p>{}</p>", escape_html_text(&item.detail)));
        }
        out.push_str("</article></li>");
    }
    out.push_str("</ul></main></body></html>");
    out
}

fn escape_html_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_html_attr(s: &str) -> String {
    escape_html_text(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(method: &str, url: &str, body: &str, request_body: Option<&str>) -> TranscriptEntry {
        TranscriptEntry {
            method: method.into(),
            url: url.into(),
            status: 200,
            bytes: body.len(),
            kind: "empty".into(),
            body: Some(body.into()),
            request_headers: vec![
                ("accept".into(), "application/json".into()),
                ("x-app".into(), "1".into()),
            ],
            request_body: request_body.map(|text| crate::http::base64(text.as_bytes())),
        }
    }

    #[test]
    fn a_feed_is_mined_and_replayed_with_its_ids_filled() {
        let body = r#"{"hits":[
            {"title":"Why Discord is switching from Go to Rust","url":"https://example.com/a","author":"bob","points":42},
            {"title":"Rust in the Linux kernel","url":"https://example.com/b","author":"ana","points":7}
        ]}"#;
        let document = r#"<html><body><a href="https://example.com/a">Why Discord is switching from Go to Rust</a><a href="https://example.com/b">Rust in the Linux kernel</a></body></html>"#;
        let transcript = vec![entry(
            "POST",
            "https://api.example.com/search?q=rust",
            body,
            Some(r#"{"query":"rust"}"#),
        )];
        let contract = mine(
            document,
            "https://site.example/search?q=rust",
            "chrome",
            &transcript,
        )
        .expect("mined");
        assert_eq!(contract.shape, "/search?q=*");
        assert_eq!(contract.method, "POST");
        assert!(contract.url.contains("q={{0}}"));
        assert_eq!(contract.title_field, vec!["title".to_string()]);
        assert_eq!(
            contract.url_field,
            Some(vec!["url".to_string()]),
            "the linked field is the URL"
        );
        let (url, _headers, body) = contract
            .materialize(
                "https://site.example/search?q=go",
                "https://site.example/search?q=go",
            )
            .expect("materialized");
        assert_eq!(url, "https://api.example.com/search?q=go");
        assert!(String::from_utf8_lossy(&body.expect("body")).contains("\"go\""));
        let items = contract
            .extract(
                r#"{"hits":[{"title":"A new answer","url":"https://example.com/c","author":"cy","points":1},{"title":"Another answer","url":"https://example.com/d","author":"di","points":2}]}"#,
                &url,
            )
            .expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url.as_deref(), Some("https://example.com/c"));
    }

    #[test]
    fn a_break_is_not_content() {
        let body = r#"{"hits":[{"title":"A real title that is long enough","url":"https://example.com/a"}]}"#;
        let document = r#"<html><body><a href="https://example.com/a">A real title that is long enough</a></body></html>"#;
        let transcript = vec![entry("GET", "https://api.example.com/items", body, None)];
        let contract =
            mine(document, "https://site.example/", "chrome", &transcript).expect("mined");
        assert!(
            contract
                .extract(r#"{"other":[]}"#, "https://api.example.com/items")
                .is_none(),
            "a changed shape is a break"
        );
        assert!(
            contract
                .extract(r#"{"hits":[]}"#, "https://api.example.com/items")
                .is_none(),
            "too few items is a break"
        );
    }

    #[test]
    fn a_shape_that_does_not_match_is_not_filled() {
        let contract = MinedContract {
            shape: "/profile/*".into(),
            ..Default::default()
        };
        assert_eq!(
            contract.fill("https://bsky.app/profile/alice"),
            Some(vec!["alice".into()])
        );
        assert_eq!(contract.fill("https://bsky.app/feed"), None);
        assert_eq!(
            contract.fill("https://bsky.app/profile/alice/replies"),
            None
        );
    }

    #[test]
    fn get_request_without_component_keeps_literal_shape_instead_of_wildcarding() {
        let body = r#"{"items":[{"title":"Featured item shown on every page","url":"https://shop.example/f1"},{"title":"Second featured item on every page","url":"https://shop.example/f2"}]}"#;
        let document = r#"<html><body><a href="https://shop.example/f1">Featured item shown on every page</a><a href="https://shop.example/f2">Second featured item on every page</a></body></html>"#;
        let transcript = vec![entry(
            "GET",
            "https://shop.example/api/featured.json",
            body,
            None,
        )];
        let contract = mine(
            document,
            "https://shop.example/products/12345?tab=reviews",
            "chrome",
            &transcript,
        )
        .expect("mined");
        assert_eq!(
            contract.shape, "/products/12345?tab=reviews",
            "unrelated GET must not wildcard path or query components"
        );
        assert!(
            contract
                .fill("https://shop.example/products/99999?tab=specs")
                .is_none(),
            "distinct product URL must not match a static endpoint contract"
        );
    }

    #[test]
    fn path_plus_and_ampersand_round_trip_and_materialize_escapes_url_and_json() {
        let body = r#"{"hits":[{"title":"C++ R&D reference guide","url":"https://example.com/cpp"},{"title":"Async Rust systems guide","url":"https://example.com/rust"}]}"#;
        let document = r#"<html><body><a href="https://example.com/cpp">C++ R&D reference guide</a><a href="https://example.com/rust">Async Rust systems guide</a></body></html>"#;
        let transcript = vec![entry(
            "POST",
            "https://api.example.com/lang/c++/r%26d?q=rust%20async",
            body,
            Some(r#"{"topic":"c++","sub":"r&d","query":"rust async"}"#),
        )];
        let contract = mine(
            document,
            "https://site.example/lang/c++/r%26d?q=rust+async",
            "chrome",
            &transcript,
        )
        .expect("mined");
        assert_eq!(contract.shape, "/lang/*/*?q=*");
        let (req_url, _headers, req_body) = contract
            .materialize(
                "https://site.example/lang/f%23/a%2Fb?q=hello+world%26admin%3D1%23frag",
                "https://site.example/lang/f%23/a%2Fb?q=hello+world%26admin%3D1%23frag",
            )
            .expect("materialized");
        assert_eq!(
            req_url,
            "https://api.example.com/lang/f%23/a%2Fb?q=hello%20world%26admin%3D1%23frag"
        );
        let parsed: Value =
            serde_json::from_slice(&req_body.expect("json body")).expect("valid json");
        assert_eq!(parsed["topic"], "f#");
        assert_eq!(parsed["sub"], "a/b");
        assert_eq!(parsed["query"], "hello world&admin=1#frag");

        // Quotes and newlines in filled values keep JSON body valid.
        let (_, _, quoted_body) = contract
            .materialize(
                "https://site.example/lang/c++/r%26d?q=say+%22hi%22%0Anow",
                "https://site.example/lang/c++/r%26d?q=say+%22hi%22%0Anow",
            )
            .expect("materialized quoted");
        let parsed_q: Value = serde_json::from_slice(&quoted_body.expect("json body"))
            .expect("valid json with quotes");
        assert_eq!(parsed_q["query"], "say \"hi\"\nnow");
    }

    #[test]
    fn mines_affix_html_fragment_and_ignores_origin_header_domain_collision() {
        let frag = format!(
            "<div class=\"docs-markdown\"><h2>Button overview</h2><p>{}</p></div>",
            "Angular Material buttons are native button or anchor elements enhanced with styling. "
                .repeat(4)
        );
        let doc = format!("<html><body><main>{frag}</main></body></html>");
        let transcript = vec![entry(
            "GET",
            "https://material.angular.dev/docs-content/overviews/material/button/material-button.html",
            &frag,
            None,
        )];
        let mined = mine(
            &doc,
            "https://material.angular.dev/components/button/overview",
            "plain",
            &transcript,
        )
        .expect("mined html fragment");
        assert_eq!(mined.kind, BlueprintKind::HtmlFragment);
        assert_eq!(mined.shape, "/components/*/overview");
        let (sib_url, _, _) = mined
            .materialize(
                "https://material.angular.dev/components/dialog/overview",
                "https://material.angular.dev/components/dialog/overview",
            )
            .expect("sibling materialized");
        assert_eq!(
            sib_url,
            "https://material.angular.dev/docs-content/overviews/material/dialog/material-dialog.html"
        );

        // Negative: when a profile handle equals the host name (`bsky.app`),
        // `origin: https://bsky.app` must NOT wildcard `/profile/*` on an opaque DID URL.
        let mut did_entry = entry(
            "GET",
            "https://public.api.bsky.app/xrpc/app.bsky.feed.getAuthorFeed?actor=did:plc:12345",
            r#"{"feed":[{"post":{"text":"Official announcement post from our engineering team"}}]}"#,
            None,
        );
        did_entry.request_headers = vec![
            ("origin".into(), "https://bsky.app".into()),
            ("referer".into(), "https://bsky.app/".into()),
        ];
        let bsky_doc =
            "<html><body><p>Official announcement post from our engineering team</p></body></html>";
        let bsky_mined = mine(
            bsky_doc,
            "https://bsky.app/profile/bsky.app",
            "plain",
            &[did_entry],
        )
        .expect("mined literal");
        assert_eq!(bsky_mined.shape, "/profile/bsky.app");
    }
}
