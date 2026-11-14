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

/// One mined request contract. The field paths are relative to an item object
/// in the response array, never to values: values change, shapes do not.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MinedContract {
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
        let fill = |text: &str| -> String {
            let mut out = text.replace("{{doc}}", document_url);
            for (index, value) in values.iter().enumerate() {
                out = out.replace(&format!("{{{{{index}}}}}"), value);
            }
            out
        };
        let request_url = fill(&self.url);
        let headers = self
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), fill(value)))
            .collect();
        let body = self.body.as_ref().map(|body| {
            if self.body_b64 {
                crate::http::unbase64(body).unwrap_or_default()
            } else {
                fill(body).into_bytes()
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
    url::form_urlencoded::parse(format!("v={text}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| text.to_string())
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
    let body_text = String::from_utf8(request_body.clone()).ok();
    let header_text: String = entry
        .request_headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\n"))
        .collect();

    // Which document components are variable? A query value is the caller's
    // question; a path segment has to look like an identifier (a digit, a
    // separator or a long token) so a word like `about` stays a literal.
    let mut values: Vec<String> = Vec::new();
    let mut path_shape: Vec<String> = Vec::new();
    for segment in &doc.path {
        let seen = appears(
            entry,
            &request_body,
            body_text.as_deref(),
            &header_text,
            segment,
        );
        if seen && distinctive(segment) {
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
        let seen = appears(
            entry,
            &request_body,
            body_text.as_deref(),
            &header_text,
            value,
        );
        if seen && value.chars().count() >= 2 {
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
        for (index, component) in values.iter().enumerate() {
            if component.chars().count() >= 3 {
                value = value.replace(component, &format!("{{{{{index}}}}}"));
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
                    text = text.replace(component, &format!("{{{{{index}}}}}"));
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

fn appears(
    entry: &TranscriptEntry,
    request_body: &[u8],
    body_text: Option<&str>,
    header_text: &str,
    value: &str,
) -> bool {
    if value.chars().count() < 2 {
        return false;
    }
    if entry.url.contains(value) {
        return true;
    }
    if header_text.contains(value) {
        return true;
    }
    body_text.is_some_and(|text| text.contains(value)) || {
        // A binary body is not searched; it cannot carry a document component
        // in a form this contract can fill.
        request_body.is_empty()
    }
}

fn distinctive(value: &str) -> bool {
    let chars = value.chars().count();
    chars >= 8
        || value.chars().any(|c| c.is_ascii_digit())
        || value.chars().any(|c| !c.is_alphanumeric())
}

/// Replace a whole path segment or query value, not a substring of a longer
/// one; the rest of the URL is left exactly as the app sent it.
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
                    Some((key, raw)) if percent_decode(raw) == value => {
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
    for entry in transcript {
        if !(200..300).contains(&entry.status) {
            continue;
        }
        let Some(body) = entry.body.as_deref() else {
            continue;
        };
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
            let score = (matched, elements.len());
            let better = best
                .as_ref()
                .map(|(best_matched, best_len, _)| {
                    score.0 > *best_matched || (score.0 == *best_matched && score.1 > *best_len)
                })
                .unwrap_or(true);
            if !better {
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
            best = Some((score.0, score.1, contract));
        }
    }
    best.map(|(_, _, contract)| contract)
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
pub fn render(host: &str, items: &[Item]) -> String {
    crate::datapath::render_items(&format!("replay {host}"), items)
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
}
