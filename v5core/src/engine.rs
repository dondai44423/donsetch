//! The challenge engine: the site's own program, run in our process.
//!
//! V8 executes the challenge page's scripts against a bounded host layer
//! (`engine_host.js`) and one op that performs HTTP with the fetcher's own
//! identity. Nothing renders, nothing is laid out, and no browser is involved.
//! A mint returns the credentials the site itself issued; the caller binds them
//! to the identity that earned them and retries.

use crate::envelope::{self, Guard, Limits, Meter};
use crate::http::{Call, Kind, Wire};
use crate::identity::Identity;
use crate::state::StoredCookie;
use deno_core::{op2, v8, Extension, ExtensionFileSource, JsRuntime, OpState, RuntimeOptions};
use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
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
    wire: Wire,
    identity: &'static Identity,
    started: Instant,
    deadline: Instant,
    guard: Arc<Guard>,
    handle: v8::IsolateHandle,
    stop: Arc<AtomicU8>,
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
fn op_result(state: &mut OpState, #[string] json: String) -> bool {
    // One report per mint, and bounded: the host layer sends a few KB of cookies
    // and logs; a page reaching this op directly must not park megabytes here.
    let shared = state.borrow_mut::<Shared>();
    if shared.result.is_some() || json.len() > 4 * 1024 * 1024 {
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
    // The page's live jar wins over the header captured at mint start: the
    // challenge's own token must be on the request that spends it.
    // The page's jar (seeded with the fetcher's own cookies for the page host) is
    // the only source: a host the jar has nothing for gets no cookie, as in a
    // browser. `credentials: "omit"` arrives as the marker "-".
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
        Ok(response) => {
            let size = response.raw.as_ref().map_or(response.body.len(), Vec::len);
            shared.guard.record_bytes(size);
            // Inspection only (V5_ENGINE_DUMP_DIR): every byte the challenge page
            // asks for, on disk, so a program's failure can be read instead of
            // guessed at. Off by default; never part of a fetch.
            if let Ok(directory) = std::env::var("V5_ENGINE_DUMP_DIR") {
                if !directory.is_empty() {
                    static NEXT: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = std::fs::create_dir_all(&directory);
                    let mut bytes = format!("# {method} {url}\n").into_bytes();
                    bytes.extend_from_slice(
                        response.raw.as_deref().unwrap_or(response.body.as_bytes()),
                    );
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
        Err(error) => refusal(error),
    }
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

#[op2]
#[string]
fn op_random(#[smi] len: i32) -> String {
    let mut buffer = vec![0u8; len.clamp(0, 65536) as usize];
    if getrandom::getrandom(&mut buffer).is_err() {
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
        }
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// WHATWG URL parsing and setters (the `url` crate's `quirks` module implements
/// the URL Standard's API). `setter` empty parses `input` against `base`;
/// otherwise `input` is an href and `setter` names the component to set.
/// Returns the components as JSON, or "" when the input is not a URL.
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
    // A setter the standard ignores (a bad port, a host on a `data:` URL) leaves
    // the URL as it was; only `href` throws, and it reports through "".
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

/// WebCrypto on BoringSSL. The output is framed by `webcrypto::framed`: a
/// status byte, then the bytes or the reason.
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

/// V8's stack formatter for the engine. A page reads `error.stack` and can send
/// it home (AWS WAF reports every failure with its stack), so a frame from the
/// host layer or the runtime (`v5:…`, `ext:…`) is a signature no browser has: a
/// browser's own internals never appear in a page's stack. Those frames are
/// dropped before V8 formats the rest, and a page's own
/// `Error.prepareStackTrace` still sees its frames, as in Chrome.
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
            op_result(),
            op_fetch(),
            op_now(),
            op_sleep(),
            op_random(),
            op_crypto(),
            op_url(),
        ]),
        js_files: Cow::Owned(vec![ExtensionFileSource::new_computed(
            "v5:engine_host.js",
            std::sync::Arc::from(include_str!("engine_host.js")),
        )]),
        ..Default::default()
    }
}

/// What a mint produced.
#[derive(Debug, Clone, Default)]
pub struct Mint {
    pub cookies: Vec<StoredCookie>,
    pub surfaces: Vec<String>,
    pub logs: Vec<String>,
    pub redirect: Option<String>,
    pub page_status: u16,
    pub page_url: String,
    pub page_body: String,
    pub ms: f64,
    /// What the run cost, so the envelope's headroom is measured, not assumed.
    pub cost: Cost,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Cost {
    pub heap_used: usize,
    pub heap_total: usize,
    pub buffer_peak: usize,
    pub ops: u32,
    pub bytes_read: usize,
}

/// Run the page's own scripts and return the credentials it set, inside `limits`.
/// One engine runs at a time per process; the wall clock starts when this call
/// holds the slot.
#[cfg(test)]
pub fn mint_within(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
) -> Result<Mint, MintError> {
    mint_within_via(limits, url, html, identity, cookie_header, None)
}

/// `mint_within`, with the page program's requests leaving through `proxy`.
pub fn mint_within_via(
    limits: &Limits,
    url: &str,
    html: &str,
    identity: &'static Identity,
    cookie_header: Option<&str>,
    proxy: Option<&str>,
) -> Result<Mint, MintError> {
    let _slot = envelope::slot();
    envelope::init_v8().map_err(MintError::Runtime)?;
    let started = Instant::now();
    let deadline = started + limits.wall;
    let page_host = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_default();
    let guard = Guard::new(limits.clone(), &page_host);
    let wire = Wire::guarded(identity, &guard, proxy).map_err(MintError::Runtime)?;
    let page = serde_json::json!({
        "url": url,
        "html": html,
        "cookie": cookie_header.unwrap_or(""),
        "identity": identity.script_view(),
        "debug": std::env::var("V5_ENGINE_LOG").is_ok(),
        "referrer": "",
        "wasmBudget": limits.buffer_max,
    })
    .to_string();

    // V8 posts delayed tasks to the tokio runtime current at isolate creation.
    // Enter one for creation only: the HTTP client owns a separate runtime, and
    // blocking on it while another runtime is entered would panic.
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
        // V8 aborts the process when the heap is exhausted unless this callback
        // raises the limit. Terminate the program and grant room to unwind.
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
        wire,
        identity,
        started,
        deadline,
        guard: Arc::clone(&guard),
        handle: handle.clone(),
        stop: Arc::clone(&stop),
    });

    // The watchdog: when the wall clock runs out, V8 is terminated from outside.
    // A loop in the page's own program never yields to an op that could check.
    // Between clock checks it looks at the process's resident memory, the one
    // number that sees allocations V8 maps itself (WebAssembly memory) as well as
    // its heap and buffers.
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
    let outcome = runtime.execute_script("v5:mint", "globalThis.__v5Mint();");
    let _ = finished.send(());
    let _ = watchdog.join();
    drop(tokio_runtime);

    match stop.load(Ordering::SeqCst) {
        STOP_WALL => {
            return Err(MintError::WallClock {
                limit_ms: limits.wall.as_millis() as u64,
            })
        }
        // V8 reports a refused ArrayBuffer allocation as memory pressure, so the
        // meter is what tells the two caps apart.
        STOP_EXTERNAL => {
            return Err(MintError::Buffers {
                limit_mb: limits.buffer_max / (1024 * 1024),
            })
        }
        STOP_HEAP if meter.refusals() > 0 => {
            return Err(MintError::Buffers {
                limit_mb: limits.buffer_max / (1024 * 1024),
            })
        }
        STOP_HEAP => {
            return Err(MintError::Heap {
                limit_mb: limits.heap_max / (1024 * 1024),
            })
        }
        _ => {}
    }
    outcome.map_err(|error| MintError::Script(format!("engine: {error}")))?;

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
    let raw = raw.ok_or(MintError::NoResult)?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| MintError::Script(format!("engine result: {error}")))?;
    let mut mint = Mint {
        ms: value["ms"].as_f64().unwrap_or(0.0),
        redirect: value["redirect"].as_str().map(str::to_string),
        page_status: value["page"]["status"].as_u64().unwrap_or(0) as u16,
        page_url: value["page"]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        page_body: value["page"]["body"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        cost,
        ..Default::default()
    };
    for cookie in value["cookies"].as_array().cloned().unwrap_or_default() {
        let Some(name) = cookie["name"].as_str() else {
            continue;
        };
        let Some(value) = cookie["value"].as_str() else {
            continue;
        };
        mint.cookies.push(StoredCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: cookie["domain"].as_str().map(str::to_string),
            path: cookie["path"].as_str().map(str::to_string),
            expires_unix: cookie["expires"].as_i64(),
            secure: cookie["secure"].as_bool().unwrap_or(true),
            http_only: cookie["httpOnly"].as_bool().unwrap_or(false),
        });
    }
    mint.surfaces = value["surfaces"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    mint.logs = value["logs"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
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

/// True when a page is an AWS WAF challenge this engine can run: the goku-shaped
/// bundle or the legacy inline challenge. Both mint an `aws-waf-token`.
pub fn is_aws_waf_page(html: &str) -> bool {
    html.contains("gokuProps")
        || html.contains("awswaf")
        || html.contains("aws-waf-token")
        || html.contains("challenge.js")
}

/// Cloudflare's managed challenge ("Just a moment..."): the inline
/// `_cf_chl_opt` the page bootstraps from and the challenge-platform loader it
/// names. Either marker means the page carries the site's own program.
pub fn is_cloudflare_page(html: &str) -> bool {
    html.contains("cf_chl_opt") || html.contains("/cdn-cgi/challenge-platform/")
}

#[cfg(test)]
mod host_source_tests {
    /// V8 extension source must be 7-bit ASCII: deno_core asserts it in debug
    /// builds (the product's), so a stray character panics there and nowhere else.
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
}
