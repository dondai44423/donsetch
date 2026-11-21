//! The site's own program, run in our process.
//!
//! Two modes of one engine. A **mint** executes a challenge page's scripts
//! against a bounded host layer (`engine_host.js`) and one op that performs HTTP
//! with the fetcher's own identity; it returns the credentials the site itself
//! issued. A **content run** executes an application page's scripts against
//! native V8 ES modules (`ContentModuleLoader`), multiplexed async subresource
//! I/O (`op_fetch_async`), and a data-plane DOM, returning the document the app
//! assembled. Nothing renders, nothing is laid out, and no browser is involved.

use crate::envelope::{self, Guard, Limits, Meter};
use crate::http::{Call, Kind, OwnedCall, Wire, WireHandle};
use crate::identity::Identity;
use crate::state::StoredCookie;
use deno_core::error::ModuleLoaderError;
use deno_core::{
    op2, v8, Extension, ExtensionFileSource, JsBuffer, JsRuntime, ModuleLoadOptions,
    ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleResolveResponse, ModuleSource,
    ModuleSourceCode, ModuleSpecifier, ModuleType, OpState, PollEventLoopOptions,
    RequestedModuleType, ResolutionKind, RuntimeOptions,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Why V8 was stopped from outside.
const STOP_NONE: u8 = 0;
const STOP_WALL: u8 = 1;
const STOP_HEAP: u8 = 2;
const STOP_EXTERNAL: u8 = 3;

/// How often the watchdog checks the clock and the process's memory.
const WATCH_EVERY: Duration = Duration::from_millis(20);

/// Why a mint produced nothing. Each names the bound that stopped it, so a caller
/// can tell a hostile or broken program from a wall the engine could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// The wall-clock budget ran out and V8 was terminated.
    WallClock { limit_ms: u64 },
    /// The V8 heap reached its ceiling and V8 was terminated.
    Heap { limit_mb: usize },
    /// The program asked for more ArrayBuffer memory than the cap allows.
    Buffers { limit_mb: usize },
    /// The program threw out of the host layer.
    Script(String),
    /// The engine could not start.
    Runtime(String),
    /// The program finished without reporting.
    NoResult,
}

impl fmt::Display for MintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WallClock { limit_ms } => {
                write!(f, "wall-clock cap of {limit_ms} ms exceeded, V8 terminated")
            }
            Self::Heap { limit_mb } => {
                write!(f, "heap cap of {limit_mb} MB reached, V8 terminated")
            }
            Self::Buffers { limit_mb } => {
                write!(
                    f,
                    "typed-array memory cap of {limit_mb} MB reached, V8 terminated"
                )
            }
            Self::Script(message) => write!(f, "{message}"),
            Self::Runtime(message) => write!(f, "{message}"),
            Self::NoResult => write!(f, "no result"),
        }
    }
}

struct Shared {
    page: String,
    result: Option<String>,
    /// The largest report the host may post: a mint reports cookies and logs, a
    /// content run reports the document.
    result_cap: usize,
    wire: Wire,
    wire_handle: WireHandle,
    identity: &'static Identity,
    started: Instant,
    deadline: Instant,
    guard: Arc<Guard>,
    handle: v8::IsolateHandle,
    stop: Arc<AtomicU8>,
    /// The preload board of a content run: script bytes fetched while the
    /// isolate boots, waited on per URL by the host or module loader.
    prefetch: Option<Arc<PrefetchBoard>>,
    /// Inline `<script type="module">` sources registered for native V8 compilation.
    inline_modules: Arc<Mutex<HashMap<String, String>>>,
    /// Number of ES modules currently being loaded asynchronously by V8.
    active_loads: Arc<AtomicUsize>,
}

impl Shared {
    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Stop V8 now. Used when an op finds the wall clock already spent: the
    /// watchdog would do the same, this only closes the gap.
    fn expire(&self) {
        let _ =
            self.stop
                .compare_exchange(STOP_NONE, STOP_WALL, Ordering::SeqCst, Ordering::SeqCst);
        self.handle.terminate_execution();
    }
}

fn refusal(reason: impl fmt::Display) -> String {
    serde_json::json!({ "error": reason.to_string() }).to_string()
}

/// Headers a page may not set: they belong to the connection or to the identity.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
];

/// The host passes headers as `[[name, value], ...]` (a page's own `fetch`) or as
/// one `{name: value}` object (its script loader).
fn header_pairs(raw: &str) -> Option<Vec<(String, String)>> {
    if raw.is_empty() {
        return Some(Vec::new());
    }
    match serde_json::from_str::<serde_json::Value>(raw).ok()? {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| {
                let pair = item.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_string(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        serde_json::Value::Object(map) => map
            .into_iter()
            .map(|(name, value)| Some((name, value.as_str()?.to_string())))
            .collect(),
        _ => None,
    }
}

#[op2]
#[string]
fn op_page(state: &mut OpState) -> String {
    state.borrow::<Shared>().page.clone()
}

#[op2(fast)]
fn op_register_inline_module(state: &mut OpState, #[string] url: String, #[string] code: String) {
    let shared = state.borrow::<Shared>();
    if let Ok(mut map) = shared.inline_modules.lock() {
        if map.len() < 128 {
            map.insert(url, code);
        }
    }
}

/// Write `text` as a JSON string literal.
fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn is_hydration_comment(text: &str) -> bool {
    matches!(text, "$?" | "/$" | "$" | "$!" | "[" | "]" | "")
        || text.starts_with("?lit$")
        || text.starts_with("ng-container")
}

/// One parsed node, in the host's tree format: an element is
/// `[tag, {attributes}, ...children]`, a text node is a JSON string, and in
/// template mode (or framework hydration anchors) a comment node is `["#comment", {}, text]`.
fn write_node(out: &mut String, node: ego_tree::NodeRef<'_, scraper::Node>, keep_comments: bool) {
    use scraper::Node;
    match node.value() {
        Node::Element(element) => {
            out.push('[');
            write_json_string(out, element.name());
            out.push_str(",{");
            let mut first = true;
            for (name, value) in element.attrs.iter() {
                if !first {
                    out.push(',');
                }
                first = false;
                match &name.prefix {
                    Some(prefix) => {
                        write_json_string(out, &format!("{}:{}", &**prefix, &*name.local))
                    }
                    None => write_json_string(out, &name.local),
                }
                out.push(':');
                write_json_string(out, value);
            }
            out.push('}');
            let in_template = keep_comments || element.name().eq_ignore_ascii_case("template");
            for child in node.children() {
                if matches!(child.value(), Node::Fragment) {
                    for frag_child in child.children() {
                        let include = match frag_child.value() {
                            Node::Element(_) | Node::Text(_) => true,
                            Node::Comment(c) => in_template || is_hydration_comment(&c.comment),
                            _ => false,
                        };
                        if !include {
                            continue;
                        }
                        out.push(',');
                        write_node(out, frag_child, in_template);
                    }
                    continue;
                }
                let include = match child.value() {
                    Node::Element(_) | Node::Text(_) => true,
                    Node::Comment(c) => in_template || is_hydration_comment(&c.comment),
                    _ => false,
                };
                if !include {
                    continue;
                }
                out.push(',');
                write_node(out, child, in_template);
            }
            out.push(']');
        }
        Node::Text(text) => write_json_string(out, &text.text),
        Node::Comment(comment) if keep_comments || is_hydration_comment(&comment.comment) => {
            out.push_str("[\"#comment\",{},");
            write_json_string(out, &comment.comment);
            out.push(']');
        }
        _ => {}
    }
}

fn find_element<'a>(
    node: ego_tree::NodeRef<'a, scraper::Node>,
    name: &str,
) -> Option<ego_tree::NodeRef<'a, scraper::Node>> {
    if let scraper::Node::Element(element) = node.value() {
        if element.name() == name {
            return Some(node);
        }
    }
    for child in node.children() {
        if let Some(hit) = find_element(child, name) {
            return Some(hit);
        }
    }
    None
}

/// The largest tree the op will hand the host. A document past this is an
/// honest failure (the run reports it), never a truncated DOM.
const PARSE_TREE_CAP: usize = 24 * 1024 * 1024;

/// Parse an HTML document (`mode` = `document`), an `innerHTML` fragment
/// (`mode` = `fragment`), or a `<template>` fragment (`mode` = `template`)
/// into the host's tree format.
fn parse_tree(html: &str, mode: &str) -> String {
    use scraper::{Html, Node};
    let mut out = String::with_capacity(html.len() + html.len() / 2);
    if mode == "template" {
        let fragment = Html::parse_fragment(html);
        let root = fragment.root_element();
        out.push('[');
        let mut first = true;
        for child in root.children() {
            if matches!(
                child.value(),
                Node::Element(_) | Node::Text(_) | Node::Comment(_)
            ) {
                if !first {
                    out.push(',');
                }
                first = false;
                write_node(&mut out, child, true);
            }
        }
        out.push(']');
        if out.len() > PARSE_TREE_CAP {
            return "{\"error\":\"parsed tree exceeds the lane cap\"}".to_string();
        }
        return out;
    }

    let document = Html::parse_document(html);
    let root = document.tree.root();
    if mode == "fragment" {
        let host = find_element(root, "body").unwrap_or(root);
        out.push('[');
        let mut first = true;
        for child in host.children() {
            if matches!(child.value(), Node::Element(_) | Node::Text(_)) {
                if !first {
                    out.push(',');
                }
                first = false;
                write_node(&mut out, child, false);
            }
        }
        out.push(']');
    } else {
        match find_element(root, "html") {
            Some(html_node) => write_node(&mut out, html_node, false),
            None => {
                out.push_str("[[\"html\",{},");
                out.push_str("[\"head\",{},");
                out.push_str("[\"body\",{}");
                for child in root.children() {
                    if matches!(child.value(), Node::Element(_) | Node::Text(_)) {
                        out.push(',');
                        write_node(&mut out, child, false);
                    }
                }
                out.push_str("]]]");
            }
        }
    }
    if out.len() > PARSE_TREE_CAP {
        return "{\"error\":\"parsed tree exceeds the lane cap\"}".to_string();
    }
    out
}

/// Parse an HTML document or fragment into the host's tree format. The content
/// lane materializes the result as its DOM; nothing renders.
#[op2]
#[string]
fn op_parse_html(#[string] html: String, #[string] mode: String) -> String {
    parse_tree(&html, &mode)
}

/// The preload board's wait: the entry for `url` once it is published, or "" on
/// timeout or when the scanner never took it (the host then goes live).
#[op2]
#[string]
fn op_prefetch_wait(state: &mut OpState, #[string] url: String, #[smi] ms: i32) -> String {
    let shared = state.borrow::<Shared>();
    let Some(board) = &shared.prefetch else {
        return String::new();
    };
    let remaining = shared.remaining();
    if remaining.is_zero() {
        return String::new();
    }
    let timeout = Duration::from_millis(ms.clamp(0, 5_000) as u64).min(remaining);
    match board.take_wait(&url, timeout) {
        Some(entry) => entry.to_string(),
        None => String::new(),
    }
}

#[op2(fast)]
fn op_result(state: &mut OpState, #[string] json: String) -> bool {
    let shared = state.borrow_mut::<Shared>();
    if shared.result.is_some() || json.len() > shared.result_cap {
        return false;
    }
    shared.result = Some(json);
    true
}

#[op2]
#[string]
fn op_fetch(
    state: &mut OpState,
    #[string] method: String,
    #[string] url: String,
    #[string] headers: String,
    #[buffer] body: &[u8],
    #[string] cookie: String,
    #[string] kind: String,
) -> String {
    let shared = state.borrow::<Shared>();
    let remaining = shared.remaining();
    if remaining.is_zero() {
        shared.expire();
        return refusal("wall-clock cap exceeded");
    }
    let limits = &shared.guard.limits;
    if !shared.guard.take_op() {
        return refusal(format!(
            "blocked: more than {} requests in one mint",
            limits.ops_max
        ));
    }
    let method = method.to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST" | "HEAD") {
        return refusal(format!("blocked: method {method}"));
    }
    if let Err(reason) = shared.guard.check_url(&url) {
        return refusal(reason);
    }
    if body.len() > limits.request_body_max {
        return refusal(format!(
            "blocked: request body over {} bytes",
            limits.request_body_max
        ));
    }
    let Some(pairs) = header_pairs(&headers) else {
        return refusal("blocked: malformed headers");
    };
    if pairs.len() > 64
        || pairs
            .iter()
            .any(|(name, value)| name.len() > 256 || value.len() > 16 * 1024)
    {
        return refusal("blocked: header set too large");
    }
    let extra: Vec<(&str, &str)> = pairs
        .iter()
        .filter(|(name, _)| !FORBIDDEN_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let cookie = if cookie.is_empty() || cookie == "-" {
        None
    } else {
        Some(cookie.as_str())
    };
    let body = if body.is_empty() { None } else { Some(body) };
    let timeout = limits.op_timeout.min(remaining);
    let cap = shared.guard.body_allowance();
    if cap == 0 {
        return refusal("blocked: response byte budget spent");
    }
    let kind = if kind == "navigate" {
        Kind::Navigate
    } else {
        Kind::Subresource
    };
    match shared.wire.call(
        shared.identity,
        Call {
            method: &method,
            url: &url,
            cookie,
            timeout,
            extra: &extra,
            body,
            cap: Some(cap),
            kind,
        },
    ) {
        Ok(response) => format_op_response(&shared.guard, &method, &url, response),
        Err(error) => refusal(error),
    }
}

/// Multiplexed async subresource/API fetch for the content lane. Multiple
/// concurrent `fetch()` and async `XMLHttpRequest` calls run in parallel on
/// the Tokio runtime over pooled HTTP/2 connections without blocking V8.
#[op2]
#[string]
#[allow(clippy::too_many_arguments)]
async fn op_fetch_async(
    state: Rc<RefCell<OpState>>,
    #[string] method: String,
    #[string] url: String,
    #[string] headers: String,
    #[buffer] body: JsBuffer,
    #[string] cookie: String,
    #[string] kind: String,
    #[smi] content_mode: i32,
) -> String {
    if content_mode != 0 && crate::hydrate::is_telemetry_url(&url) {
        return serde_json::json!({
            "status": 200,
            "url": url,
            "headers": [["content-type", "application/json"]],
            "body": "{}",
            "shortCircuited": true,
        })
        .to_string();
    }

    let (wire_handle, identity, guard, deadline, prefetch) = {
        let state_ref = state.borrow();
        let shared = state_ref.borrow::<Shared>();
        let remaining = shared.remaining();
        if remaining.is_zero() {
            shared.expire();
            return refusal("wall-clock cap exceeded");
        }
        (
            shared.wire_handle.clone(),
            shared.identity,
            Arc::clone(&shared.guard),
            shared.deadline,
            shared.prefetch.clone(),
        )
    };

    let method = method.to_ascii_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "HEAD" | "PUT" | "DELETE" | "PATCH" | "OPTIONS"
    ) {
        return refusal(format!("blocked: method {method}"));
    }

    // Check preload board first for bodyless GET requests (0 extra op charged).
    let body_bytes: &[u8] = body.as_ref();
    if method == "GET" && body_bytes.is_empty() {
        if let Some(board) = &prefetch {
            let wait = Duration::from_millis(1_500)
                .min(deadline.saturating_duration_since(Instant::now()));
            if let Some(prefetched) = board.take_async(&url, wait).await {
                return prefetched.to_string();
            }
        }
    }

    let limits = &guard.limits;
    if !guard.take_op() {
        return refusal(format!(
            "blocked: more than {} requests in one mint",
            limits.ops_max
        ));
    }
    if let Err(reason) = guard.check_url(&url) {
        return refusal(reason);
    }
    if body_bytes.len() > limits.request_body_max {
        return refusal(format!(
            "blocked: request body over {} bytes",
            limits.request_body_max
        ));
    }
    let Some(pairs) = header_pairs(&headers) else {
        return refusal("blocked: malformed headers");
    };
    if pairs.len() > 64
        || pairs
            .iter()
            .any(|(name, value)| name.len() > 256 || value.len() > 16 * 1024)
    {
        return refusal("blocked: header set too large");
    }
    let extra: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(name, _)| !FORBIDDEN_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
        .collect();
    let cookie_opt = if cookie.is_empty() || cookie == "-" {
        None
    } else {
        Some(cookie)
    };
    let req_body = if body_bytes.is_empty() {
        None
    } else {
        Some(body_bytes.to_vec())
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return refusal("wall-clock cap exceeded");
    }
    let timeout = limits.op_timeout.min(remaining);
    let cap = guard.body_allowance();
    if cap == 0 {
        return refusal("blocked: response byte budget spent");
    }
    let kind = if kind == "navigate" {
        Kind::Navigate
    } else {
        Kind::Subresource
    };

    match wire_handle
        .call_async(
            identity,
            OwnedCall {
                method: method.clone(),
                url: url.clone(),
                cookie: cookie_opt,
                timeout,
                extra,
                body: req_body,
                cap: Some(cap),
                kind,
            },
        )
        .await
    {
        Ok(response) => format_op_response(&guard, &method, &url, response),
        Err(error) => refusal(error),
    }
}

fn format_op_response(
    guard: &Guard,
    method: &str,
    url: &str,
    response: crate::http::Response,
) -> String {
    let size = response.raw.as_ref().map_or(response.body.len(), Vec::len);
    guard.record_bytes(size);
    if let Ok(directory) = std::env::var("V5_ENGINE_DUMP_DIR") {
        if !directory.is_empty() {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = std::fs::create_dir_all(&directory);
            let mut bytes = format!("# {method} {url}\n").into_bytes();
            bytes.extend_from_slice(response.raw.as_deref().unwrap_or(response.body.as_bytes()));
            let _ = std::fs::write(
                std::path::Path::new(&directory).join(format!("{index:03}.txt")),
                bytes,
            );
        }
    }
    let mut reply = serde_json::json!({
        "status": response.status,
        "url": response.url,
        "headers": response.headers,
        "body": response.body,
    });
    if let Some(raw) = &response.raw {
        reply["body_b64"] = serde_json::Value::String(crate::http::base64(raw));
    }
    reply.to_string()
}

#[op2(fast)]
fn op_now(state: &mut OpState) -> f64 {
    state.borrow::<Shared>().started.elapsed().as_secs_f64() * 1000.0
}

#[op2(fast)]
fn op_sleep(state: &mut OpState, #[smi] ms: i32) {
    let shared = state.borrow::<Shared>();
    let remaining = shared.remaining();
    if remaining.is_zero() {
        shared.expire();
        return;
    }
    let bounded = Duration::from_millis(ms.clamp(0, 5000) as u64).min(remaining);
    std::thread::sleep(bounded);
}

/// Non-blocking async sleep for `contentPumpAsync`, allowing concurrent HTTP/2
/// fetches and ES module graph downloads to make progress while timers wait.
#[op2]
async fn op_sleep_async(state: Rc<RefCell<OpState>>, #[smi] ms: i32) {
    let bounded = {
        let state_ref = state.borrow();
        let shared = state_ref.borrow::<Shared>();
        let remaining = shared.remaining();
        if remaining.is_zero() {
            shared.expire();
            return;
        }
        Duration::from_millis(ms.clamp(0, 2_000) as u64).min(remaining)
    };
    if !bounded.is_zero() {
        tokio::time::sleep(bounded).await;
    }
}

#[op2]
#[string]
fn op_random(#[smi] len: i32) -> String {
    let mut buffer = vec![0u8; len.clamp(0, 65536) as usize];
    if getrandom::fill(&mut buffer).is_err() {
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
        }
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[op2]
#[string]
fn op_url(
    #[string] input: String,
    #[string] base: String,
    #[string] setter: String,
    #[string] value: String,
) -> String {
    use url::{quirks, Url};
    let parsed = if setter.is_empty() {
        if base.is_empty() {
            Url::parse(&input)
        } else {
            Url::parse(&base).and_then(|base| base.join(&input))
        }
    } else {
        Url::parse(&input)
    };
    let Ok(mut url) = parsed else {
        return String::new();
    };
    let applied = match setter.as_str() {
        "" => Ok(()),
        "href" => quirks::set_href(&mut url, &value).map_err(|_| ()),
        "protocol" => quirks::set_protocol(&mut url, &value),
        "username" => quirks::set_username(&mut url, &value),
        "password" => quirks::set_password(&mut url, &value),
        "host" => quirks::set_host(&mut url, &value),
        "hostname" => quirks::set_hostname(&mut url, &value),
        "port" => quirks::set_port(&mut url, &value),
        "pathname" => {
            quirks::set_pathname(&mut url, &value);
            Ok(())
        }
        "search" => {
            quirks::set_search(&mut url, &value);
            Ok(())
        }
        "hash" => {
            quirks::set_hash(&mut url, &value);
            Ok(())
        }
        _ => Err(()),
    };
    if applied.is_err() && setter == "href" {
        return String::new();
    }
    serde_json::json!({
        "href": quirks::href(&url),
        "origin": quirks::origin(&url),
        "protocol": quirks::protocol(&url),
        "username": quirks::username(&url),
        "password": quirks::password(&url),
        "host": quirks::host(&url),
        "hostname": quirks::hostname(&url),
        "port": quirks::port(&url),
        "pathname": quirks::pathname(&url),
        "search": quirks::search(&url),
        "hash": quirks::hash(&url),
    })
    .to_string()
}

#[op2]
#[buffer]
fn op_crypto(
    #[string] operation: String,
    #[string] params: String,
    #[buffer] key: &[u8],
    #[buffer] data: &[u8],
) -> Vec<u8> {
    crate::webcrypto::framed(&operation, &params, key, data)
}

#[op2(fast)]
fn op_active_loads(state: &mut OpState) -> u32 {
    state
        .try_borrow::<Shared>()
        .map_or(0, |s| s.active_loads.load(Ordering::SeqCst) as u32)
}

fn page_stack_trace<'s, 'i>(
    scope: &mut v8::PinScope<'s, 'i>,
    error: v8::Local<'s, v8::Value>,
    callsites: v8::Local<'s, v8::Array>,
) -> v8::Local<'s, v8::Value> {
    let mut kept: Vec<v8::Local<v8::Value>> = Vec::with_capacity(callsites.length() as usize);
    let key = v8::String::new(scope, "getFileName").expect("static string");
    for index in 0..callsites.length() {
        let Some(site) = callsites.get_index(scope, index) else {
            continue;
        };
        let host_frame = site
            .to_object(scope)
            .and_then(|object| {
                let getter = object.get(scope, key.into())?;
                let getter = v8::Local::<v8::Function>::try_from(getter).ok()?;
                let name = getter.call(scope, object.into(), &[])?;
                if name.is_string() {
                    Some(name.to_rust_string_lossy(scope))
                } else {
                    None
                }
            })
            .is_some_and(|name| name.starts_with("v5:") || name.starts_with("ext:"));
        if !host_frame {
            kept.push(site);
        }
    }
    let kept = v8::Array::new_with_elements(scope, &kept);
    deno_core::error::prepare_stack_trace_callback(scope, error, kept)
}

fn extension() -> Extension {
    Extension {
        name: "v5_engine",
        ops: Cow::Owned(vec![
            op_page(),
            op_register_inline_module(),
            op_result(),
            op_parse_html(),
            op_prefetch_wait(),
            op_fetch(),
            op_fetch_async(),
            op_now(),
            op_sleep(),
            op_sleep_async(),
            op_random(),
            op_crypto(),
            op_url(),
            op_active_loads(),
        ]),
        js_files: Cow::Owned(vec![ExtensionFileSource::new_computed(
            "v5:engine_host.js",
            std::sync::Arc::from(include_str!("engine_host.js")),
        )]),
        ..Default::default()
    }
}

// ----- Native V8 ES Module Loader for Content Mode ---------------------------

type ModuleFetchCell = Arc<tokio::sync::OnceCell<Result<(String, String), String>>>;

struct ActiveLoadGuard(Arc<AtomicUsize>);

impl Drop for ActiveLoadGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ContentModuleLoader {
    base_url: String,
    page_url: String,
    page_host: String,
    origin: String,
    import_map: HashMap<String, String>,
    inline_modules: Arc<Mutex<HashMap<String, String>>>,
    active_loads: Arc<AtomicUsize>,
    fetched: Arc<tokio::sync::Mutex<HashMap<String, ModuleFetchCell>>>,
    prewarmed_count: Arc<AtomicUsize>,
    prefetch: Option<Arc<PrefetchBoard>>,
    wire: WireHandle,
    identity: &'static Identity,
    guard: Arc<Guard>,
    cookie_header: Option<String>,
    deadline: Instant,
    op_timeout: Duration,
    logs: Arc<Mutex<Vec<String>>>,
}

impl ContentModuleLoader {
    #[allow(clippy::too_many_arguments)]
    fn new(
        base_url: String,
        page_url: String,
        import_map: HashMap<String, String>,
        inline_modules: Arc<Mutex<HashMap<String, String>>>,
        active_loads: Arc<AtomicUsize>,
        prefetch: Option<Arc<PrefetchBoard>>,
        wire: WireHandle,
        identity: &'static Identity,
        guard: Arc<Guard>,
        cookie_header: Option<String>,
        deadline: Instant,
        op_timeout: Duration,
        logs: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        let parsed = url::Url::parse(&page_url).ok();
        let page_host = parsed
            .as_ref()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        let origin = parsed
            .as_ref()
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_default();
        Self {
            base_url,
            page_url,
            page_host,
            origin,
            import_map,
            inline_modules,
            active_loads,
            fetched: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            prewarmed_count: Arc::new(AtomicUsize::new(0)),
            prefetch,
            wire,
            identity,
            guard,
            cookie_header,
            deadline,
            op_timeout,
            logs,
        }
    }

    fn resolve_specifier(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleSpecifier, String> {
        let spec = specifier.trim();
        if let Some(mapped) = self.import_map.get(spec) {
            if let Ok(url) = ModuleSpecifier::parse(mapped) {
                return Ok(url);
            }
        }
        for (prefix, target) in &self.import_map {
            if prefix.ends_with('/') {
                if let Some(rest) = spec.strip_prefix(prefix.as_str()) {
                    let combined = format!("{target}{rest}");
                    if let Ok(url) = ModuleSpecifier::parse(&combined) {
                        return Ok(url);
                    }
                }
            }
        }
        if (spec.starts_with("https://") || spec.starts_with("http://"))
            && ModuleSpecifier::parse(spec).is_ok()
        {
            return ModuleSpecifier::parse(spec).map_err(|e| e.to_string());
        }
        let ref_base = if referrer.starts_with("https://") || referrer.starts_with("http://") {
            referrer
        } else {
            &self.base_url
        };
        if spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../") {
            return ModuleSpecifier::parse(ref_base)
                .and_then(|b| b.join(spec))
                .map_err(|e| e.to_string());
        }
        Err(format!(
            "bare module specifier {spec:?} was not mapped by an importmap"
        ))
    }

    fn push_log(logs: &Mutex<Vec<String>>, msg: String) {
        if let Ok(mut list) = logs.lock() {
            if list.len() < 400 {
                list.push(msg);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_module_dedup(
    url: String,
    fetched: Arc<tokio::sync::Mutex<HashMap<String, ModuleFetchCell>>>,
    prefetch: Option<Arc<PrefetchBoard>>,
    wire: WireHandle,
    identity: &'static Identity,
    guard: Arc<Guard>,
    page_host: String,
    page_url: String,
    origin: String,
    cookie_header: Option<String>,
    deadline: Instant,
    op_timeout: Duration,
    logs: Arc<Mutex<Vec<String>>>,
) -> Result<(String, String), String> {
    let cell = {
        let mut map = fetched.lock().await;
        Arc::clone(
            map.entry(url.clone())
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
        )
    };

    cell.get_or_init(|| async move {
        if let Some(board) = &prefetch {
            let wait = Duration::from_millis(1_500)
                .min(deadline.saturating_duration_since(Instant::now()));
            if let Some(entry) = board.take_async(&url, wait).await {
                let status = entry["status"].as_u64().unwrap_or(0) as u16;
                if (200..300).contains(&status) {
                    let final_url = entry["url"].as_str().unwrap_or(&url).to_string();
                    let body = entry["body"].as_str().unwrap_or_default().to_string();
                    return Ok((final_url, body));
                }
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("wall-clock cap exceeded".to_string());
        }
        if !guard.take_op() {
            return Err(format!(
                "blocked: more than {} requests in one run",
                guard.limits.ops_max
            ));
        }
        guard.check_url(&url)?;
        let cap = guard.body_allowance();
        if cap == 0 {
            return Err("blocked: response byte budget spent".to_string());
        }
        let same_origin = url::Url::parse(&url)
            .ok()
            .and_then(|p| p.host_str().map(str::to_ascii_lowercase))
            .as_deref()
            == Some(page_host.as_str());
        let cookie = if same_origin { cookie_header } else { None };
        let site = if same_origin {
            "same-origin"
        } else {
            "cross-site"
        };
        let referer = if same_origin {
            page_url
        } else {
            format!("{origin}/")
        };
        let mut extra = vec![
            ("sec-fetch-site".to_string(), site.to_string()),
            ("sec-fetch-mode".to_string(), "cors".to_string()),
            ("sec-fetch-dest".to_string(), "script".to_string()),
            ("referer".to_string(), referer),
            ("accept".to_string(), "*/*".to_string()),
        ];
        if !same_origin && !origin.is_empty() {
            extra.push(("origin".to_string(), origin));
        }
        let timeout = op_timeout.min(remaining);
        match wire
            .call_async(
                identity,
                OwnedCall {
                    method: "GET".to_string(),
                    url: url.clone(),
                    cookie,
                    timeout,
                    extra,
                    body: None,
                    cap: Some(cap),
                    kind: Kind::Subresource,
                },
            )
            .await
        {
            Ok(resp) => {
                let size = resp.raw.as_ref().map_or(resp.body.len(), Vec::len);
                guard.record_bytes(size);
                if !(200..300).contains(&resp.status) {
                    let msg = format!("module fetch {} -> status {}", url, resp.status);
                    ContentModuleLoader::push_log(&logs, msg.clone());
                    return Err(msg);
                }
                Ok((resp.url, resp.body))
            }
            Err(err) => {
                let msg = format!("module fetch {} -> {}", url, err);
                ContentModuleLoader::push_log(&logs, msg.clone());
                Err(msg)
            }
        }
    })
    .await
    .clone()
}

/// Fast O(N) byte scanner for static ESM specifiers (`from "..."`, `import "..."`)
/// so child chunks start downloading over HTTP/2 while V8 compiles the parent.
fn scan_static_specifiers(code: &str, base_url: &str) -> Vec<String> {
    const CAP: usize = 24;
    let Ok(base) = url::Url::parse(base_url) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let bytes = code.as_bytes();
    let mut idx = 0usize;
    while idx + 7 < bytes.len() && out.len() < CAP {
        let rest = &code[idx..];
        let matched = if let Some(after_from) = rest.strip_prefix("from") {
            let trimmed = after_from.trim_start_matches([' ', '\t']);
            if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                Some((trimmed, idx + (rest.len() - trimmed.len())))
            } else {
                None
            }
        } else if let Some(after_import) = rest.strip_prefix("import") {
            let prev_ok = idx == 0
                || !bytes[idx - 1].is_ascii_alphanumeric()
                    && bytes[idx - 1] != b'_'
                    && bytes[idx - 1] != b'$'
                    && bytes[idx - 1] != b'.';
            let trimmed = after_import.trim_start_matches([' ', '\t']);
            if prev_ok && (trimmed.starts_with('"') || trimmed.starts_with('\'')) {
                Some((trimmed, idx + (rest.len() - trimmed.len())))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((quoted, next_pos)) = matched {
            let q = quoted.as_bytes()[0];
            if let Some(end) = quoted[1..].find(q as char) {
                let spec = &quoted[1..1 + end];
                if (spec.starts_with("./")
                    || spec.starts_with("../")
                    || spec.starts_with('/')
                    || spec.starts_with("https://")
                    || spec.starts_with("http://"))
                    && !spec.ends_with(".css")
                    && spec.len() <= 512
                {
                    if let Ok(resolved) = base.join(spec) {
                        let s = resolved.to_string();
                        if !crate::hydrate::is_telemetry_url(&s) && seen.insert(s.clone()) {
                            out.push(s);
                        }
                    }
                }
                idx = next_pos + end + 2;
                continue;
            }
        }
        idx += 1 + rest.chars().next().map_or(0, |c| c.len_utf8() - 1);
    }
    out
}

impl ModuleLoader for ContentModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        match self.resolve_specifier(specifier, referrer) {
            Ok(resolved) => Ok(resolved),
            Err(err) => {
                Self::push_log(
                    &self.logs,
                    format!(
                        "module specifier {}: {err}",
                        &specifier[..specifier.len().min(80)]
                    ),
                );
                Err(ModuleLoaderError::type_error(err))
            }
        }
    }

    fn import_meta_resolve(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        self.resolve_specifier(specifier, referrer)
            .map_err(ModuleLoaderError::type_error)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let spec_str = module_specifier.to_string();

        // 1. Registered inline `<script type="module">` source.
        if let Ok(map) = self.inline_modules.lock() {
            if let Some(code) = map.get(&spec_str).cloned() {
                return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                    ModuleType::JavaScript,
                    ModuleSourceCode::String(code.into()),
                    module_specifier,
                    None,
                )));
            }
        }

        // 2. Short-circuit third-party telemetry & CSS imports in Content Mode.
        if crate::hydrate::is_telemetry_url(&spec_str) {
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String("export default {};\n".to_string().into()),
                module_specifier,
                None,
            )));
        }
        if module_specifier
            .path()
            .to_ascii_lowercase()
            .ends_with(".css")
            && options.requested_module_type == RequestedModuleType::None
        {
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String("export default \"\";\n".to_string().into()),
                module_specifier,
                None,
            )));
        }

        let spec = module_specifier.clone();
        let fetched = Arc::clone(&self.fetched);
        let prewarmed = Arc::clone(&self.prewarmed_count);
        let prefetch = self.prefetch.clone();
        let wire = self.wire.clone();
        let identity = self.identity;
        let guard = Arc::clone(&self.guard);
        let page_host = self.page_host.clone();
        let page_url = self.page_url.clone();
        let origin = self.origin.clone();
        let cookie_header = self.cookie_header.clone();
        let deadline = self.deadline;
        let op_timeout = self.op_timeout;
        let logs = Arc::clone(&self.logs);
        let req_type = options.requested_module_type;
        self.active_loads.fetch_add(1, Ordering::SeqCst);
        let load_guard = ActiveLoadGuard(Arc::clone(&self.active_loads));

        ModuleLoadResponse::Async(Box::pin(async move {
            let _load_guard = load_guard;
            let (final_url, code) = fetch_module_dedup(
                spec_str.clone(),
                Arc::clone(&fetched),
                prefetch.clone(),
                wire.clone(),
                identity,
                Arc::clone(&guard),
                page_host.clone(),
                page_url.clone(),
                origin.clone(),
                cookie_header.clone(),
                deadline,
                op_timeout,
                Arc::clone(&logs),
            )
            .await
            .map_err(ModuleLoaderError::generic)?;

            // Speculatively pre-warm child static imports on the Tokio runtime
            // while V8 parses and compiles this module.
            if prewarmed.load(Ordering::Relaxed) < 32 {
                for child_url in scan_static_specifiers(&code, &final_url) {
                    if prewarmed.fetch_add(1, Ordering::Relaxed) >= 32 {
                        break;
                    }
                    let fetched_c = Arc::clone(&fetched);
                    let prefetch_c = prefetch.clone();
                    let wire_c = wire.clone();
                    let guard_c = Arc::clone(&guard);
                    let page_host_c = page_host.clone();
                    let page_url_c = page_url.clone();
                    let origin_c = origin.clone();
                    let cookie_c = cookie_header.clone();
                    let logs_c = Arc::clone(&logs);
                    tokio::task::spawn_local(async move {
                        let _ = fetch_module_dedup(
                            child_url,
                            fetched_c,
                            prefetch_c,
                            wire_c,
                            identity,
                            guard_c,
                            page_host_c,
                            page_url_c,
                            origin_c,
                            cookie_c,
                            deadline,
                            op_timeout,
                            logs_c,
                        )
                        .await;
                    });
                }
            }

            let module_type = match req_type {
                RequestedModuleType::Json => ModuleType::Json,
                RequestedModuleType::Text => ModuleType::Text,
                RequestedModuleType::Bytes => ModuleType::Bytes,
                _ => ModuleType::JavaScript,
            };
            let found_spec = ModuleSpecifier::parse(&final_url).unwrap_or_else(|_| spec.clone());
            Ok(ModuleSource::new_with_redirect(
                module_type,
                ModuleSourceCode::String(code.into()),
                &spec,
                &found_spec,
                None,
            ))
        }))
    }
}

/// What a mint produced.
#[derive(Debug, Clone, Default)]
pub struct Mint {
    pub cookies: Vec<StoredCookie>,
    pub surfaces: Vec<String>,
    pub logs: Vec<String>,
    pub redirect: Option<String>,
    pub ms: f64,
    pub cost: Cost,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Cost {
    pub heap_used: usize,
    pub heap_total: usize,
    pub buffer_peak: usize,
    pub ops: u32,
    pub bytes_read: usize,
}

#[cfg(test)]
pub fn mint_within(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
) -> Result<Mint, MintError> {
    mint_within_via_policy(limits, url, html, identity, cookie_header, None, true)
}

fn document_text_len(html: &str) -> usize {
    use scraper::Node;
    fn walk(node: ego_tree::NodeRef<'_, Node>, total: &mut usize) {
        for child in node.children() {
            match child.value() {
                Node::Text(text) => {
                    *total += text.text.chars().filter(|c| !c.is_whitespace()).count();
                }
                Node::Element(element) => {
                    if matches!(element.name(), "script" | "style" | "noscript" | "template") {
                        continue;
                    }
                    if element.attrs.iter().any(|(key, value)| {
                        &*key.local == "hidden" || (&*key.local == "aria-hidden" && value == "true")
                    }) {
                        continue;
                    }
                    walk(child, total);
                }
                _ => walk(child, total),
            }
        }
    }
    let document = scraper::Html::parse_document(html);
    let mut total = 0usize;
    walk(document.tree.root(), &mut total);
    total
}

#[derive(Default)]
struct PrefetchState {
    pending: HashSet<String>,
    entries: HashMap<String, serde_json::Value>,
}

struct PrefetchBoard {
    state: Mutex<PrefetchState>,
    ready: std::sync::Condvar,
    async_ready: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
}

impl PrefetchBoard {
    fn new() -> Self {
        Self {
            state: Mutex::new(PrefetchState::default()),
            ready: std::sync::Condvar::new(),
            async_ready: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn register(&self, url: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.insert(url.to_string());
        }
    }

    fn finish_one(&self, url: &str, entry: Option<serde_json::Value>) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(url);
            if let Some(entry) = entry {
                state.entries.insert(url.to_string(), entry);
            }
            drop(state);
            self.ready.notify_all();
            self.async_ready.notify_waiters();
        }
    }

    fn take_wait(&self, url: &str, timeout: Duration) -> Option<serde_json::Value> {
        let limit = Instant::now() + timeout;
        let mut state = self.state.lock().ok()?;
        if !state.pending.contains(url) && !state.entries.contains_key(url) {
            return None;
        }
        while state.pending.contains(url) && !self.is_closed() {
            let left = limit.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self.ready.wait_timeout(state, left).ok()?.0;
        }
        state.entries.remove(url)
    }

    async fn take_async(&self, url: &str, timeout: Duration) -> Option<serde_json::Value> {
        let limit = Instant::now() + timeout;
        loop {
            let notified = self.async_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().ok()?;
                if let Some(entry) = state.entries.remove(url) {
                    return Some(entry);
                }
                if !state.pending.contains(url) || self.is_closed() {
                    return None;
                }
            }
            let left = limit.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            if tokio::time::timeout(left, notified).await.is_err() {
                return self.state.lock().ok()?.entries.remove(url);
            }
        }
    }

    fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.ready.notify_all();
        self.async_ready.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Start the preload pass: scan the document's own first-party/app script and
/// modulepreload set (resolving `<base href>`), then fetch it on worker threads
/// while V8 initializes.
#[allow(clippy::too_many_arguments)]
fn start_prefetch(
    html: &str,
    base_url: &str,
    page_url: &str,
    identity: &'static Identity,
    guard: Arc<Guard>,
    proxy: Option<String>,
    op_timeout: Duration,
    cookie_header: Option<String>,
    deadline: Instant,
) -> Option<Arc<PrefetchBoard>> {
    const PREFETCH_MAX: usize = 32;
    const PREFETCH_WORKERS: usize = 6;
    const PREFETCH_TIMEOUT: Duration = Duration::from_millis(2_500);
    const PREFETCH_BODY_MAX: usize = 16 * 1024 * 1024;
    const PREFETCH_TOTAL_MAX: usize = 32 * 1024 * 1024;

    let Ok(base) = url::Url::parse(base_url) else {
        return None;
    };
    let page_parsed = url::Url::parse(page_url).ok();
    let page_host = page_parsed
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let origin = page_parsed
        .as_ref()
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|| base.origin().ascii_serialization());
    let document = scraper::Html::parse_document(html);
    let mut targets: Vec<(String, &'static str, &'static str)> = Vec::new();
    let mut seen = HashSet::new();
    for node in document.tree.root().descendants() {
        let scraper::Node::Element(element) = node.value() else {
            continue;
        };
        let attr = |name: &str| {
            element
                .attrs
                .iter()
                .find(|(key, _)| &*key.local == name)
                .map(|(_, value)| value.to_string())
        };
        let target = match element.name() {
            "script" => {
                let is_mod = attr("type")
                    .map(|t| t.trim().eq_ignore_ascii_case("module"))
                    .unwrap_or(false);
                attr("src").map(|src| (src, "script", if is_mod { "cors" } else { "no-cors" }))
            }
            "link" => {
                let rel = attr("rel").unwrap_or_default().to_ascii_lowercase();
                if rel.split_whitespace().any(|part| part == "modulepreload") {
                    attr("href").map(|href| (href, "script", "cors"))
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some((target, dest, fetch_mode)) = target else {
            continue;
        };
        let Ok(resolved) = base.join(&target) else {
            continue;
        };
        if !matches!(resolved.scheme(), "http" | "https") {
            continue;
        }
        let text = resolved.to_string();
        if crate::hydrate::is_telemetry_url(&text) {
            continue;
        }
        if seen.insert(text.clone()) {
            targets.push((text, dest, fetch_mode));
        }
        if targets.len() >= PREFETCH_MAX {
            break;
        }
    }
    if targets.is_empty() {
        return None;
    }
    if deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(300) {
        return None;
    }
    let board = Arc::new(PrefetchBoard::new());
    for (url, _, _) in &targets {
        board.register(url);
    }
    let page_host = Arc::new(page_host);
    let origin = Arc::new(origin);
    let page_url = Arc::new(page_url.to_string());
    let cookie_header = Arc::new(cookie_header);
    let proxy = Arc::new(proxy);
    let targets = Arc::new(targets);
    let total = Arc::new(AtomicUsize::new(0));
    let workers = PREFETCH_WORKERS.min(targets.len());
    for worker in 0..workers {
        let board = Arc::clone(&board);
        let guard = Arc::clone(&guard);
        let targets = Arc::clone(&targets);
        let page_host = Arc::clone(&page_host);
        let origin = Arc::clone(&origin);
        let page_url = Arc::clone(&page_url);
        let cookie_header = Arc::clone(&cookie_header);
        let proxy = Arc::clone(&proxy);
        let total = Arc::clone(&total);
        std::thread::spawn(move || {
            let wire = match Wire::guarded(identity, &guard, proxy.as_deref()) {
                Ok(wire) => wire,
                Err(_) => {
                    for index in (worker..targets.len()).step_by(workers) {
                        board.finish_one(&targets[index].0, None);
                    }
                    return;
                }
            };
            for index in (worker..targets.len()).step_by(workers) {
                let (url, dest, fetch_mode) = &targets[index];
                if board.is_closed() || !guard.take_op() {
                    break;
                }
                if guard.check_url(url).is_err() {
                    board.finish_one(url, None);
                    continue;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining < Duration::from_millis(150) {
                    break;
                }
                let cap = guard.body_allowance();
                if cap == 0 {
                    break;
                }
                let timeout = PREFETCH_TIMEOUT.min(op_timeout).min(remaining);
                let same_origin = url::Url::parse(url)
                    .ok()
                    .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
                    .as_deref()
                    == Some(page_host.as_str());
                let cookie = if same_origin {
                    cookie_header.as_deref()
                } else {
                    None
                };
                let site = if same_origin {
                    "same-origin"
                } else {
                    "cross-site"
                };
                let referer = if same_origin {
                    page_url.as_str().to_string()
                } else {
                    format!("{}/", origin.as_str())
                };
                let mut extra: Vec<(&str, &str)> = vec![
                    ("sec-fetch-site", site),
                    ("sec-fetch-mode", fetch_mode),
                    ("sec-fetch-dest", dest),
                    ("referer", &referer),
                    ("accept", "*/*"),
                ];
                if !same_origin && *fetch_mode == "cors" {
                    extra.push(("origin", origin.as_str()));
                }
                let call = Call {
                    method: "GET",
                    url,
                    cookie,
                    timeout,
                    extra: &extra,
                    body: None,
                    cap: Some(cap),
                    kind: Kind::Subresource,
                };
                match wire.call(identity, call) {
                    Ok(response) => {
                        let size = response.raw.as_ref().map_or(response.body.len(), Vec::len);
                        guard.record_bytes(size);
                        if size > PREFETCH_BODY_MAX {
                            board.finish_one(url, None);
                            continue;
                        }
                        if total.fetch_add(size, Ordering::Relaxed) + size > PREFETCH_TOTAL_MAX {
                            board.finish_one(url, None);
                            continue;
                        }
                        let mut entry = serde_json::json!({
                            "status": response.status,
                            "url": response.url,
                            "headers": response.headers,
                            "body": response.body,
                        });
                        if let Some(raw) = &response.raw {
                            entry["body_b64"] = serde_json::Value::String(crate::http::base64(raw));
                        }
                        board.finish_one(url, Some(entry));
                    }
                    Err(_error) => board.finish_one(url, None),
                }
            }
            for index in (worker..targets.len()).step_by(workers) {
                board.finish_one(&targets[index].0, None);
            }
        });
    }
    Some(board)
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Content {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub document: String,
    pub stop: String,
    pub text_chars: usize,
    pub visited: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate: Option<String>,
    pub transcript: Vec<TranscriptEntry>,
    pub cookies: Vec<StoredCookie>,
    pub surfaces: Vec<String>,
    pub logs: Vec<String>,
    pub ms: f64,
    pub cost: Cost,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TranscriptEntry {
    pub method: String,
    pub url: String,
    pub status: u16,
    pub bytes: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub request_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
}

pub const CONTENT_DOCUMENT_CAP: usize = 16 * 1024 * 1024;
const CONTENT_RESULT_CAP: usize = 24 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Mint,
    Content,
}

pub fn mint_within_via(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
    proxy: Option<&str>,
) -> Result<Mint, MintError> {
    mint_within_via_policy(limits, url, html, identity, cookie_header, proxy, false)
}

pub fn mint_within_via_policy(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
    proxy: Option<&str>,
    allow_private: bool,
) -> Result<Mint, MintError> {
    let (value, cost, _) = run_within(
        Mode::Mint,
        limits,
        url,
        html,
        identity,
        cookie_header,
        proxy,
        0,
        allow_private,
    )?;
    mint_from(value, cost)
}

pub fn content_within_via(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
    proxy: Option<&str>,
    text_target: usize,
) -> Result<Content, MintError> {
    content_within_via_policy(
        limits,
        url,
        html,
        identity,
        cookie_header,
        proxy,
        text_target,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn content_within_via_policy(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
    proxy: Option<&str>,
    text_target: usize,
    allow_private: bool,
) -> Result<Content, MintError> {
    let (value, cost, loader_logs) = run_within(
        Mode::Content,
        limits,
        url,
        html,
        identity,
        cookie_header,
        proxy,
        text_target,
        allow_private,
    )?;
    content_from(value, cost, loader_logs)
}

#[allow(clippy::too_many_arguments)]
fn run_within(
    mode: Mode,
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
    proxy: Option<&str>,
    text_target: usize,
    allow_private: bool,
) -> Result<(serde_json::Value, Cost, Vec<String>), MintError> {
    let _slot = envelope::slot();
    envelope::init_v8().map_err(MintError::Runtime)?;
    let started = Instant::now();
    let deadline = started + limits.wall;
    let page_host = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_default();
    let guard = Guard::with_policy(limits.clone(), &page_host, allow_private);
    let wire = Wire::guarded(identity, &guard, proxy).map_err(MintError::Runtime)?;
    let wire_handle = wire.handle();

    let (base_url, is_shell) = if mode == Mode::Content {
        let base = crate::hydrate::extract_base_href(html, url);
        let shell = crate::hydrate::assess_html_shell(html, url).is_unhydrated_shell;
        (base, shell)
    } else {
        (url.to_string(), false)
    };

    let should_run_content =
        mode == Mode::Content && (is_shell || document_text_len(html) < text_target);
    let prefetch = if should_run_content {
        start_prefetch(
            html,
            &base_url,
            url,
            identity,
            Arc::clone(&guard),
            proxy.map(str::to_string),
            limits.op_timeout,
            cookie_header.map(str::to_string),
            deadline,
        )
    } else {
        None
    };

    let debug = std::env::var("V5_ENGINE_LOG").is_ok();
    let page = match mode {
        Mode::Mint => serde_json::json!({
            "url": url,
            "html": html,
            "cookie": cookie_header.unwrap_or(""),
            "identity": identity.script_view(),
            "debug": debug,
            "referrer": "",
            "wasmBudget": limits.buffer_max,
        }),
        Mode::Content => serde_json::json!({
            "url": url,
            "baseUrl": base_url,
            "html": html,
            "cookie": cookie_header.unwrap_or(""),
            "identity": identity.script_view(),
            "debug": debug,
            "referrer": "",
            "wasmBudget": limits.buffer_max,
            "mode": "content",
            "textTarget": text_target,
            "forceHydrate": is_shell,
            "deadlineMs": limits.wall.as_millis() as u64,
            "documentCap": CONTENT_DOCUMENT_CAP,
            "prefetch": prefetch.is_some(),
        }),
    }
    .to_string();
    let result_cap = match mode {
        Mode::Mint => 4 * 1024 * 1024,
        Mode::Content => CONTENT_RESULT_CAP,
    };

    let inline_modules = Arc::new(Mutex::new(HashMap::new()));
    let active_loads = Arc::new(AtomicUsize::new(0));
    let loader_logs = Arc::new(Mutex::new(Vec::new()));
    let module_loader: Option<Rc<dyn ModuleLoader>> = if mode == Mode::Content {
        let import_map = crate::hydrate::extract_import_map(html, &base_url);
        Some(Rc::new(ContentModuleLoader::new(
            base_url,
            url.to_string(),
            import_map,
            Arc::clone(&inline_modules),
            Arc::clone(&active_loads),
            prefetch.clone(),
            wire_handle.clone(),
            identity,
            Arc::clone(&guard),
            cookie_header.map(str::to_string),
            deadline,
            limits.op_timeout,
            Arc::clone(&loader_logs),
        )))
    } else {
        None
    };

    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| MintError::Runtime(format!("engine runtime: {error}")))?;
    let meter = Meter::new(limits.buffer_max);
    let params = v8::CreateParams::default()
        .heap_limits(0, limits.heap_max)
        .array_buffer_allocator(envelope::buffer_allocator(&meter));
    let mut runtime = {
        let _entered = tokio_runtime.enter();
        JsRuntime::new(RuntimeOptions {
            module_loader,
            extensions: vec![extension()],
            create_params: Some(params),
            ..Default::default()
        })
    };

    runtime
        .v8_isolate()
        .set_prepare_stack_trace_callback(page_stack_trace);
    let handle = runtime.v8_isolate().thread_safe_handle();
    let stop = Arc::new(AtomicU8::new(STOP_NONE));
    {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        let room = limits.heap_max;
        runtime.add_near_heap_limit_callback(move |current, _initial| {
            let _ = stop.compare_exchange(STOP_NONE, STOP_HEAP, Ordering::SeqCst, Ordering::SeqCst);
            handle.terminate_execution();
            current.saturating_add(room)
        });
    }
    runtime.op_state().borrow_mut().put(Shared {
        page,
        result: None,
        result_cap,
        prefetch: prefetch.clone(),
        wire,
        wire_handle,
        identity,
        started,
        deadline,
        guard: Arc::clone(&guard),
        handle: handle.clone(),
        stop: Arc::clone(&stop),
        inline_modules,
        active_loads,
    });

    let rss_ceiling = envelope::resident_kb()
        .map(|now| now + (limits.heap_max + limits.buffer_max + envelope::RSS_SLACK) / 1024);
    let (finished, waiting) = mpsc::channel::<()>();
    let watchdog = {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match waiting.recv_timeout(left.min(WATCH_EVERY)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let reason = if Instant::now() >= deadline {
                        Some(STOP_WALL)
                    } else if matches!(
                        (rss_ceiling, envelope::resident_kb()),
                        (Some(ceiling), Some(now)) if now > ceiling
                    ) {
                        Some(STOP_EXTERNAL)
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        let _ = stop.compare_exchange(
                            STOP_NONE,
                            reason,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        handle.terminate_execution();
                        break;
                    }
                }
                _ => break,
            }
        })
    };

    let outcome: Result<(), String> = match mode {
        Mode::Mint => runtime
            .execute_script("v5:mint", "globalThis.__v5Mint();")
            .map(|_| ())
            .map_err(|error| format!("engine: {error}")),
        Mode::Content => {
            let local = tokio::task::LocalSet::new();
            tokio_runtime.block_on(local.run_until(async {
                let promise = runtime
                    .execute_script("v5:content", "globalThis.__v5ContentAsync();")
                    .map_err(|error| format!("engine: {error}"))?;
                let resolve = runtime.resolve(promise);
                runtime
                    .with_event_loop_promise(resolve, PollEventLoopOptions::default())
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("engine: {error}"))
            }))
        }
    };

    let _ = finished.send(());
    let _ = watchdog.join();
    drop(tokio_runtime);

    match stop.load(Ordering::SeqCst) {
        STOP_WALL => {
            if let Some(board) = &prefetch {
                board.close();
            }
            return Err(MintError::WallClock {
                limit_ms: limits.wall.as_millis() as u64,
            });
        }
        STOP_EXTERNAL => {
            if let Some(board) = &prefetch {
                board.close();
            }
            return Err(MintError::Buffers {
                limit_mb: limits.buffer_max / (1024 * 1024),
            });
        }
        STOP_HEAP if meter.refusals() > 0 => {
            if let Some(board) = &prefetch {
                board.close();
            }
            return Err(MintError::Buffers {
                limit_mb: limits.buffer_max / (1024 * 1024),
            });
        }
        STOP_HEAP => {
            if let Some(board) = &prefetch {
                board.close();
            }
            return Err(MintError::Heap {
                limit_mb: limits.heap_max / (1024 * 1024),
            });
        }
        _ => {}
    }
    outcome.map_err(MintError::Script)?;

    let mut cost = Cost {
        buffer_peak: meter.peak(),
        ops: guard.ops_used(),
        bytes_read: guard.bytes_read(),
        ..Default::default()
    };
    {
        let statistics = runtime.v8_isolate().get_heap_statistics();
        cost.heap_used = statistics.used_heap_size();
        cost.heap_total = statistics.total_heap_size();
    }
    let raw = {
        let state = runtime.op_state();
        let state = state.borrow();
        let shared = state.borrow::<Shared>();
        shared.result.clone()
    };
    if let Some(board) = &prefetch {
        board.close();
    }
    let raw = raw.ok_or(MintError::NoResult)?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| MintError::Script(format!("engine result: {error}")))?;
    let extra_logs = loader_logs.lock().map(|l| l.clone()).unwrap_or_default();
    Ok((value, cost, extra_logs))
}

fn stored_cookies(value: &serde_json::Value) -> Vec<StoredCookie> {
    let mut cookies = Vec::new();
    for cookie in value["cookies"].as_array().cloned().unwrap_or_default() {
        let Some(name) = cookie["name"].as_str() else {
            continue;
        };
        let Some(cookie_value) = cookie["value"].as_str() else {
            continue;
        };
        cookies.push(StoredCookie {
            name: name.to_string(),
            value: cookie_value.to_string(),
            domain: cookie["domain"].as_str().map(str::to_string),
            path: cookie["path"].as_str().map(str::to_string),
            expires_unix: cookie["expires"].as_i64(),
            secure: cookie["secure"].as_bool().unwrap_or(true),
            http_only: cookie["httpOnly"].as_bool().unwrap_or(false),
        });
    }
    cookies
}

fn string_list(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn mint_from(value: serde_json::Value, cost: Cost) -> Result<Mint, MintError> {
    let mut mint = Mint {
        ms: value["ms"].as_f64().unwrap_or(0.0),
        redirect: value["redirect"].as_str().map(str::to_string),
        cost,
        ..Default::default()
    };
    mint.cookies = stored_cookies(&value);
    mint.surfaces = string_list(&value["surfaces"]);
    mint.logs = string_list(&value["logs"]);
    if std::env::var("V5_ENGINE_LOG").is_ok() {
        for line in &mint.logs {
            eprintln!("[engine] {}", line.replace('\n', "\\n").replace('\r', ""));
        }
        eprintln!(
            "[engine] cost: heap {} KB used / {} KB committed, buffers peak {} KB, {} ops, {} KB read",
            mint.cost.heap_used / 1024,
            mint.cost.heap_total / 1024,
            mint.cost.buffer_peak / 1024,
            mint.cost.ops,
            mint.cost.bytes_read / 1024
        );
    }
    Ok(mint)
}

fn content_from(
    value: serde_json::Value,
    cost: Cost,
    extra_logs: Vec<String>,
) -> Result<Content, MintError> {
    if let Some(error) = value["error"].as_str() {
        return Err(MintError::Script(error.to_string()));
    }
    let mut content = Content {
        document: value["document"].as_str().unwrap_or_default().to_string(),
        stop: value["stop"].as_str().unwrap_or("quiescent").to_string(),
        text_chars: value["textChars"].as_u64().unwrap_or(0) as usize,
        visited: value["visited"].as_str().unwrap_or_default().to_string(),
        navigate: value["navigate"].as_str().map(str::to_string),
        ms: value["ms"].as_f64().unwrap_or(0.0),
        cost,
        ..Default::default()
    };
    content.cookies = stored_cookies(&value);
    content.surfaces = string_list(&value["surfaces"]);
    content.logs = string_list(&value["logs"]);
    content.logs.extend(extra_logs);
    for entry in value["transcript"].as_array().cloned().unwrap_or_default() {
        content.transcript.push(TranscriptEntry {
            method: entry["method"].as_str().unwrap_or_default().to_string(),
            url: entry["url"].as_str().unwrap_or_default().to_string(),
            status: entry["status"].as_u64().unwrap_or(0) as u16,
            bytes: entry["bytes"].as_u64().unwrap_or(0) as usize,
            kind: entry["kind"].as_str().unwrap_or_default().to_string(),
            body: entry["body"].as_str().map(str::to_string),
            request_headers: entry["reqHeaders"]
                .as_array()
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|pair| {
                            Some((
                                pair.get(0)?.as_str()?.to_string(),
                                pair.get(1)?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            request_body: entry["reqBody"].as_str().map(str::to_string),
        });
    }
    if std::env::var("V5_ENGINE_LOG").is_ok() {
        for line in &content.logs {
            eprintln!("[engine] {}", line.replace('\n', "\\n").replace('\r', ""));
        }
        eprintln!(
            "[engine] content: stop={} {} text chars, {} requests, cost: heap {} KB used, {} ops, {} KB read",
            content.stop,
            content.text_chars,
            content.transcript.len(),
            content.cost.heap_used / 1024,
            content.cost.ops,
            content.cost.bytes_read / 1024
        );
    }
    Ok(content)
}

pub fn is_aws_waf_page(html: &str) -> bool {
    html.contains("gokuProps")
        || html.contains("awswaf")
        || html.contains("aws-waf-token")
        || html.contains("challenge.js")
}

pub fn is_cloudflare_page(html: &str) -> bool {
    html.contains("cf_chl_opt") || html.contains("/cdn-cgi/challenge-platform/")
}

pub fn is_reddit_page(html: &str) -> bool {
    html.contains("jsc_token")
}

#[cfg(test)]
mod host_source_tests {
    #[test]
    fn the_host_layer_is_ascii() {
        let source = include_str!("engine_host.js");
        let stray = source
            .lines()
            .enumerate()
            .find(|(_, line)| !line.is_ascii())
            .map(|(index, line)| format!("line {}: {line}", index + 1));
        assert_eq!(stray, None);
    }

    #[test]
    fn the_parse_tree_is_the_host_format() {
        let tree = super::parse_tree(
            "<div id=a>hi<!-- a comment between nodes --><span class='x'>yo</span></div>",
            "document",
        );
        assert!(tree.starts_with("[\"html\",{"), "{tree}");
        assert!(
            tree.contains("[\"div\",{\"id\":\"a\"},\"hi\",[\"span\",{\"class\":\"x\"},\"yo\"]]"),
            "{tree}"
        );
        let fragment = super::parse_tree("<p>a</p>text", "fragment");
        assert_eq!(fragment, "[[\"p\",{},\"a\"],\"text\"]");
    }
}
