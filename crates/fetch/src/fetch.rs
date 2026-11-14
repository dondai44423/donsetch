//! The encounter: one URL, a portfolio, and a budget.
//!
//! Order matters and is cheap to keep: the stored winner first, then proven
//! identities by score, the vendor's known passers, untried identities, and
//! identities that only failed here last. Each attempt is one request. A wall
//! that names its vendor moves that vendor's passers forward. The loop stops as
//! soon as content verifies, and early when the class cannot be fixed by being
//! someone else (a 404, a shell, a human check no identity is known to pass).
//!
//! Around the encounter: the page cache (`cache.rs`) answers or revalidates
//! first, a remembered route (`OriginState::route`) asks the site's interface
//! before its document, and what was learned is written once per fetch.

use crate::cache::{self, PageCache, Plan};
use crate::challenge::akamai_bm_verify;
use crate::classify::{classify, Class, Evidence, Verdict, Wall};
use crate::contract::{verify, verify_response, Content, Contract};
use crate::datapath;
use crate::http::Wire;
use crate::identity::{self, Identity};
use crate::state::{State, StoredCookie};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Options {
    /// Keep the response body on the outcome (inspection runs only).
    pub capture_body: bool,
    /// Arrive like a returning visitor: a first-contact warm-up of the origin,
    /// then the page request with a referer and same-origin fetch metadata.
    pub arrival: bool,
    pub max_tries: usize,
    pub deadline: Duration,
    pub contract: Option<Contract>,
    /// Persist scores and cookies between processes.
    pub persist: bool,
    /// Force one identity and skip selection (diagnostics and contracts).
    pub identity: Option<String>,
    /// Minimum spacing between two requests to the same origin, in ms.
    pub pace_ms: i64,
    /// The envelope the challenge engine runs inside.
    pub engine: crate::envelope::Limits,
    /// The envelope the content lane runs inside (a client-rendered shell or a
    /// 2xx the contract cannot name, once per encounter).
    pub content: crate::envelope::Limits,
    /// Egress proxy URL for every request of the encounter, the engine's included.
    pub proxy: Option<String>,
    /// Read and keep pages in the page cache (`cache.rs`). Only a persisting
    /// worker caches: a cold probe neither reads nor writes it.
    pub cache: bool,
    /// Never serve a cached page without asking the server, even a fresh one.
    pub revalidate: bool,
    /// Redirects (followed or not, each hop asked of the caller's guard) and the
    /// headers a navigation carries besides the identity's own.
    pub policy: crate::http::Policy,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            capture_body: false,
            // Measured neutral on this corpus: no class changed, one extra
            // request paid. Off unless asked for.
            arrival: false,
            // Discovery is amortised across encounters and ordered by what this
            // origin's vendor has accepted before. Walls score per client class:
            // measured, okhttp beat Cloudflare where Chrome was refused, so the
            // search has to reach the whole portfolio, not its first three.
            max_tries: 6,
            deadline: Duration::from_millis(3000),
            contract: None,
            persist: true,
            identity: None,
            pace_ms: 900,
            engine: crate::envelope::Limits::from_env(),
            content: crate::envelope::Limits::content_from_env(),
            proxy: None,
            cache: true,
            revalidate: false,
            policy: crate::http::Policy::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub identity: String,
    pub transport: String,
    pub class: Class,
    pub status: u16,
    pub ms: f64,
    pub bytes: usize,
    pub version: String,
    pub title: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub url: String,
    pub host: String,
    pub final_url: String,
    pub class: Class,
    pub identity: Option<String>,
    pub status: u16,
    pub ms_total: f64,
    pub ms_first: f64,
    pub attempts: Vec<Attempt>,
    pub title: String,
    pub text_chars: usize,
    pub wall: Option<Wall>,
    pub evidence: Vec<String>,
    pub failures: Vec<String>,
    pub spent_state: bool,
    /// How the content was obtained: a page (`document`) or the site's own
    /// interface (`data`). Nothing else exists, and the difference is reported.
    pub transport: String,
    /// `fresh`: served from the page cache without a request; `revalidated`: the
    /// server confirmed the cached page is current (304); `changed`: a cached copy
    /// existed and the page fetched whole now reads differently. Absent: fetched
    /// whole with no copy to compare, or the same text again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    pub items: usize,
    /// First few item titles, so a caller can check the answer against the page.
    pub sample: Vec<String>,
    /// Their URLs: items carry ids the rendered page also links to.
    pub sample_urls: Vec<String>,
    /// The data path's answer as clean text (empty for document transports).
    pub body_text: String,
    /// The response body, only when the caller asked for it (inspection runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The final response's headers, only when the caller asked for the body.
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
    /// The body's bytes as served, only when the caller asked for the body and
    /// they are not UTF-8 text (otherwise `body` is exact).
    #[serde(skip)]
    pub bytes: Option<Vec<u8>>,
}

/// A fresh page from the cache as an outcome: no request was made.
#[allow(clippy::too_many_arguments)]
fn served(
    url: &str,
    host: String,
    entry: cache::Entry,
    body: String,
    raw: Option<Vec<u8>>,
    content: Content,
    options: &Options,
    started: Instant,
) -> Outcome {
    Outcome {
        url: url.to_string(),
        cache: Some("fresh".into()),
        host,
        final_url: entry.final_url.clone(),
        class: Class::Content,
        identity: Some(entry.identity.clone()),
        status: entry.status,
        ms_total: started.elapsed().as_secs_f64() * 1000.0,
        ms_first: 0.0,
        attempts: Vec::new(),
        title: content.title,
        text_chars: content.text_chars,
        wall: None,
        evidence: Vec::new(),
        failures: Vec::new(),
        spent_state: false,
        transport: "document".to_string(),
        items: 0,
        sample: Vec::new(),
        sample_urls: Vec::new(),
        body_text: String::new(),
        body: options.capture_body.then_some(body),
        headers: if options.capture_body {
            entry.headers.clone()
        } else {
            Vec::new()
        },
        bytes: if options.capture_body { raw } else { None },
    }
}

/// What asking a data path came to.
enum Interface {
    Answered(Box<Outcome>),
    Wait,
    Unanswered,
}

/// One fetching context: its own clients (one per identity and egress) and the
/// state it learns into. Single-threaded; `donsetch_fetch::Session` pools these.
pub struct Worker {
    state: State,
    wires: HashMap<(&'static str, Option<String>), Wire>,
    /// The egress of the fetch in progress.
    proxy: Option<String>,
    /// The redirect and header rules of the fetch in progress.
    policy: crate::http::Policy,
    /// Learn from encounters: scores, cookies, credentials.
    persist: bool,
    /// Write what was learned to the state file. Off when an owner of several
    /// workers persists their merged state itself (`donsetch_fetch::Session`).
    saves: bool,
    /// The state file as this worker last read or wrote it: what a save merges from.
    base: State,
    /// Something was learned since the last save.
    dirty: Cell<bool>,
    cache: Arc<PageCache>,
    /// The cached page this fetch may revalidate or compare against.
    pending: Option<Pending>,
    /// The document a fetch ended on, for the cache.
    captured: Option<Captured>,
}

struct Pending {
    entry: cache::Entry,
    /// Send its validators (a revalidation); otherwise it is only compared (an audit).
    conditional: bool,
}

struct Captured {
    identity: &'static str,
    final_url: String,
    status: u16,
    headers: Vec<(String, String)>,
    bytes: Vec<u8>,
    cookie: Option<String>,
    /// The cached entry a 304 confirmed.
    revalidated: Option<cache::Entry>,
}

impl Worker {
    /// A worker that learns, saves its state and caches pages on disk when
    /// `persist`, and does none of that otherwise.
    pub fn new(persist: bool) -> Self {
        let state = if persist {
            State::load()
        } else {
            State::default()
        };
        let cache = if persist {
            PageCache::for_state()
        } else {
            PageCache::in_memory()
        };
        Self::build(state, persist, persist, Arc::new(cache))
    }

    /// A worker that learns into `state` and caches into `cache`, and never
    /// writes the state to disk (its owner does).
    pub fn learning(state: State, cache: Arc<PageCache>) -> Self {
        Self::build(state, true, false, cache)
    }

    fn build(state: State, persist: bool, saves: bool, cache: Arc<PageCache>) -> Self {
        Self {
            base: state.clone(),
            state,
            wires: HashMap::new(),
            proxy: None,
            policy: crate::http::Policy::default(),
            persist,
            saves,
            dirty: Cell::new(false),
            cache,
            pending: None,
            captured: None,
        }
    }

    /// The learned state, swapped for `next`.
    pub fn swap_state(&mut self, next: State) -> State {
        std::mem::replace(&mut self.state, next)
    }

    /// Mark the state changed; the fetch writes it once, when it ends.
    fn save_state(&self) {
        self.dirty.set(true);
    }

    /// One call to a data interface as `identity`: the app's own request, not a
    /// page load, with fetch metadata from the path's headers.
    fn interface_call(
        &mut self,
        identity: &'static Identity,
        request: &datapath::Request,
        url: &str,
        cookie: Option<&str>,
        timeout: Duration,
    ) -> Option<Result<crate::http::Response, String>> {
        let wire = self.wire(identity).ok()?;
        Some(wire.subresource(
            identity,
            crate::http::Call {
                method: request.method,
                url,
                cookie,
                timeout,
                extra: &request.headers,
                body: request.body.as_deref().map(str::as_bytes),
                cap: None,
                kind: crate::http::Kind::Subresource,
            },
        ))
    }

    /// Ask the origin's own interface (a data path), as `identity`. `Wait` means
    /// the interface asked for time (a cooldown or its published interval) and the
    /// attempt is spent.
    #[allow(clippy::too_many_arguments)]
    fn ask_interface(
        &mut self,
        url: &str,
        host: &str,
        identity: &'static Identity,
        options: &Options,
        started: Instant,
        first_ms: f64,
        attempts: &mut Vec<Attempt>,
    ) -> Interface {
        let Some((path, request)) = datapath::plan(url) else {
            return Interface::Unanswered;
        };
        let host = host.to_string();
        let remaining = options.deadline.saturating_sub(started.elapsed());
        if remaining.as_millis() >= 700 {
            let data_host = url::Url::parse(&request.url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_string))
                .unwrap_or_else(|| host.clone());
            // The interface publishes its own limits; obeying them is
            // cheaper than being blocked by them.
            if let Some(wait) = self.state.cooling(&data_host) {
                attempts.push(Attempt {
                    identity: identity.name.to_string(),
                    transport: "data".to_string(),
                    class: Class::WallContract,
                    status: 429,
                    ms: 0.0,
                    bytes: 0,
                    version: String::new(),
                    title: String::new(),
                    note: Some(format!("{}: cooling down {wait} ms", path.id)),
                });
                return Interface::Wait;
            }
            // Never sleep through an interface's interval: a fetch that
            // waits eleven seconds is worse than one that says "paced".
            let debt = self.state.pace_debt(&data_host, path.min_interval_ms);
            if debt > 0 {
                attempts.push(Attempt {
                    identity: identity.name.to_string(),
                    transport: "data".to_string(),
                    class: Class::WallContract,
                    status: 0,
                    ms: 0.0,
                    bytes: 0,
                    version: String::new(),
                    title: String::new(),
                    note: Some(format!("{}: paced, {debt} ms owed", path.id)),
                });
                return Interface::Wait;
            }
            if options.pace_ms > 0 {
                self.state.pace(&data_host, options.pace_ms.min(200));
            }
            self.state.mark_call(&data_host);
            let cookie = self.state.cookie_header_for(&request.url, identity.name);
            let response = self.interface_call(
                identity,
                &request,
                &request.url,
                cookie.as_deref(),
                remaining,
            );
            let (status, body, elapsed, version, size, cooldown) = match response {
                Some(Ok(response)) => {
                    let size = response.body.len();
                    let cooldown = retry_hint(&response.headers);
                    (
                        response.status,
                        response.body,
                        response.elapsed_ms,
                        response.version,
                        size,
                        cooldown,
                    )
                }
                Some(Err(error)) => (0, error, 0.0, String::new(), 0, None),
                None => (0, String::new(), 0.0, String::new(), 0, None),
            };
            let (status, body, elapsed, version, size, cooldown) =
                (status, body, elapsed, version, size, cooldown);
            if status == 429 {
                // Whatever the interface says it needs, give it that.
                self.state
                    .set_cooldown(&data_host, cooldown.unwrap_or(60_000));
            }
            let mut items = if status == 200 {
                (path.parse)(&body, &request.url)
            } else {
                Vec::new()
            };
            // The interface answered "no such item" in its own words: that is a
            // result, not a wall to work. Without this, a deleted question fell
            // through to the page and its Cloudflare wall was reported as an
            // identity gap the fetch could not close.
            if items.len() < path.min_items && datapath::absent(&body) {
                return Interface::Answered(Box::new(Outcome {
                    url: url.to_string(),
                    cache: None,
                    host,
                    final_url: url.to_string(),
                    class: Class::HttpError,
                    identity: Some(identity.name.to_string()),
                    status,
                    ms_total: started.elapsed().as_secs_f64() * 1000.0,
                    ms_first: first_ms,
                    attempts: std::mem::take(attempts),
                    title: String::new(),
                    text_chars: 0,
                    wall: None,
                    evidence: vec![format!("{}: not found", path.id)],
                    failures: Vec::new(),
                    spent_state: false,
                    transport: "data".to_string(),
                    items: 0,
                    sample: Vec::new(),
                    sample_urls: Vec::new(),
                    body_text: String::new(),
                    body: None,
                    headers: Vec::new(),
                    bytes: None,
                }));
            }
            let (mut elapsed, mut size, mut calls) = (elapsed, size, 1);
            // A first page too short to be content, from an interface that names
            // its own next page: ask that page once, as the page itself would.
            let next = path
                .next
                .filter(|_| status == 200 && items.len() < path.min_items)
                .and_then(|next| next(&body, &request.url));
            let remaining = options.deadline.saturating_sub(started.elapsed());
            if let Some(next) = next.filter(|_| remaining.as_millis() >= 700) {
                if let Some(Ok(more)) =
                    self.interface_call(identity, &request, &next, cookie.as_deref(), remaining)
                {
                    calls += 1;
                    elapsed += more.elapsed_ms;
                    size += more.body.len();
                    if more.status == 200 {
                        for item in (path.parse)(&more.body, &next) {
                            if item.url.is_none() || !items.iter().any(|seen| seen.url == item.url)
                            {
                                items.push(item);
                            }
                        }
                    }
                }
            }
            attempts.push(Attempt {
                identity: identity.name.to_string(),
                transport: "data".to_string(),
                class: if items.len() >= path.min_items {
                    Class::Content
                } else {
                    Class::Incomplete
                },
                status,
                ms: elapsed,
                bytes: size,
                version,
                title: items
                    .first()
                    .map(|item| item.title.clone())
                    .unwrap_or_default(),
                note: Some(format!(
                    "{}: {} items (min {}){}",
                    path.id,
                    items.len(),
                    path.min_items,
                    if calls > 1 {
                        format!(", {calls} calls")
                    } else {
                        String::new()
                    }
                )),
            });
            if items.len() >= path.min_items {
                if self.persist {
                    self.state
                        .record(&host, identity.name, "content", true, elapsed);
                    // The interface answered: next time it is asked first.
                    Arc::make_mut(self.state.origins.entry(host.clone()).or_default()).route =
                        Some("data".into());
                    self.save_state();
                }
                return Interface::Answered(Box::new(Outcome {
                    url: url.to_string(),
                    cache: None,
                    host,
                    // What answered is the interface, but the caller's question is
                    // the page it named: an internal endpoint is not the
                    // document's identity.
                    final_url: url.to_string(),
                    class: Class::Content,
                    identity: Some(identity.name.to_string()),
                    status,
                    ms_total: started.elapsed().as_secs_f64() * 1000.0,
                    ms_first: first_ms,
                    attempts: std::mem::take(attempts),
                    title: path.id.to_string(),
                    text_chars: items.iter().map(|item| item.title.len()).sum(),
                    wall: None,
                    evidence: vec![format!("{}:{}", path.id, items.len())],
                    failures: Vec::new(),
                    spent_state: cookie.is_some(),
                    transport: "data".to_string(),
                    items: items.len(),
                    sample: items
                        .iter()
                        .take(5)
                        .map(|item| item.title.clone())
                        .collect(),
                    sample_urls: items
                        .iter()
                        .filter_map(|item| item.url.clone())
                        .take(5)
                        .collect(),
                    body_text: datapath::render(path, &items),
                    body: None,
                    headers: Vec::new(),
                    bytes: None,
                }));
            }
        }
        Interface::Unanswered
    }

    /// One content-lane run on a shell document: the page's own program, in the
    /// engine, as the identity that received the document and against the state
    /// it owns. A produced document that passes the contract is content, and the
    /// produced page enters the pipeline exactly where a network document does
    /// (its body is the caller's, its copy goes to the page cache). Anything
    /// else returns None and the encounter ends on the shell it already had.
    #[allow(clippy::too_many_arguments)]
    fn content_lane(
        &mut self,
        url: &str,
        host: &str,
        response: &crate::http::Response,
        contract: (&Contract, bool),
        identity: &'static Identity,
        options: &Options,
        started: Instant,
        first_ms: f64,
        attempts: &mut Vec<Attempt>,
        spent_state: bool,
    ) -> Option<Outcome> {
        let (contract, generic) = contract;
        let lane_started = Instant::now();
        // The lane spends what the encounter has left, never more: an encounter
        // that gives the lane nothing has already ended, and a bench's 3 s
        // deadline bounds the lane with the same number.
        let remaining = options.deadline.saturating_sub(started.elapsed());
        if remaining < Duration::from_millis(500) {
            attempts.push(Attempt {
                identity: identity.name.to_string(),
                transport: "program".to_string(),
                class: Class::Shell,
                status: response.status,
                ms: 0.0,
                bytes: 0,
                version: String::new(),
                title: String::new(),
                note: Some("content lane: the encounter had no budget left".to_string()),
            });
            return None;
        }
        let mut limits = options.content.clone();
        limits.wall = limits.wall.min(remaining);
        let cookie = self.state.cookie_header_for(&response.url, identity.name);
        let lane = match crate::engine::content_within_via(
            &limits,
            &response.url,
            &response.body,
            identity,
            cookie.as_deref(),
            self.proxy.as_deref(),
            200,
        ) {
            Ok(lane) => lane,
            Err(error) => {
                attempts.push(Attempt {
                    identity: identity.name.to_string(),
                    transport: "program".to_string(),
                    class: Class::Shell,
                    status: response.status,
                    ms: lane_started.elapsed().as_secs_f64() * 1000.0,
                    bytes: 0,
                    version: String::new(),
                    title: String::new(),
                    note: Some(format!("content lane: {error}")),
                });
                return None;
            }
        };
        let lane_ms = lane.ms.max(lane_started.elapsed().as_secs_f64() * 1000.0);
        // What the program stored is the identity's own state, bound to it.
        self.state
            .store_cookies(host, identity.name, lane.cookies.clone());
        // The produced document is verified by the same contract a network
        // document answers, with no header facts to lean on.
        let content = crate::contract::verify_response(
            200,
            &[],
            &lane.document,
            &response.url,
            contract,
            generic,
        );
        attempts.push(Attempt {
            identity: identity.name.to_string(),
            transport: "program".to_string(),
            class: if content.passed {
                Class::Content
            } else {
                Class::Incomplete
            },
            status: 200,
            ms: lane_ms,
            bytes: lane.document.len(),
            version: String::new(),
            title: content.title.clone(),
            note: Some(format!(
                "content lane: stop={} {} text chars, {} requests{}",
                lane.stop,
                lane.text_chars,
                lane.transcript.len(),
                if content.passed {
                    String::new()
                } else {
                    format!(", contract: {}", content.failures.join("; "))
                }
            )),
        });
        if !content.passed {
            if self.persist {
                self.save_state();
            }
            return None;
        }
        // The page cache keeps the produced page under the identity and egress
        // that produced it, through the same capture the network path uses.
        self.captured = Some(Captured {
            identity: identity.name,
            final_url: response.url.clone(),
            status: 200,
            headers: Vec::new(),
            bytes: lane.document.clone().into_bytes(),
            cookie: cookie.clone(),
            revalidated: None,
        });
        if self.persist {
            if let Some(origin) = self.state.origins.get_mut(host).map(Arc::make_mut) {
                origin.route = None;
            }
            self.save_state();
        }
        let mut outcome = answer(
            url,
            host.to_string(),
            response,
            Class::Content,
            identity.name,
            Ended {
                started,
                first_ms,
                attempts: std::mem::take(attempts),
                spent_state,
                capture: options.capture_body,
            },
        );
        outcome.title = content.title;
        outcome.text_chars = content.text_chars;
        outcome.transport = "program".to_string();
        outcome.evidence.push(format!(
            "content lane: stop={} in {:.0} ms as {} ({} ops, {} B read)",
            lane.stop, lane_ms, identity.name, lane.cost.ops, lane.cost.bytes_read
        ));
        outcome.evidence.push(format!(
            "produced document: {} text chars, {} B, {} requests",
            lane.text_chars,
            lane.document.len(),
            lane.transcript.len()
        ));
        if let Some(navigate) = &lane.navigate {
            outcome
                .evidence
                .push(format!("the page chose a navigation: {navigate}"));
        }
        if options.capture_body {
            // The body a caller inspects is the produced document, not the shell.
            outcome.body = Some(lane.document);
            outcome.headers = Vec::new();
            outcome.bytes = None;
        }
        Some(outcome)
    }

    /// One request as one named identity, no selection and no state: the probe
    /// used to build the identity matrix.
    pub fn probe_one(
        &mut self,
        identity_name: &str,
        url: &str,
        contract: &Contract,
        timeout: Duration,
    ) -> Attempt {
        let Some(identity) = identity::lookup(identity_name) else {
            return Attempt {
                identity: identity_name.to_string(),
                transport: "document".to_string(),
                class: Class::Network,
                status: 0,
                ms: 0.0,
                bytes: 0,
                version: String::new(),
                title: String::new(),
                note: Some("unknown identity".into()),
            };
        };
        let response = {
            let wire = match self.wire(identity) {
                Ok(wire) => wire,
                Err(error) => {
                    return Attempt {
                        identity: identity.name.to_string(),
                        transport: "document".to_string(),
                        class: Class::Network,
                        status: 0,
                        ms: 0.0,
                        bytes: 0,
                        version: String::new(),
                        title: String::new(),
                        note: Some(error),
                    };
                }
            };
            wire.get(url, None, identity, timeout)
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return Attempt {
                    identity: identity.name.to_string(),
                    transport: "document".to_string(),
                    class: Class::Network,
                    status: 0,
                    ms: 0.0,
                    bytes: 0,
                    version: String::new(),
                    title: String::new(),
                    note: Some(error),
                };
            }
        };
        let content = if (200..400).contains(&response.status) {
            verify(&response.body, &response.url, contract)
        } else {
            Content {
                title: String::new(),
                text_chars: 0,
                passed: false,
                failures: vec![format!("status {}", response.status)],
            }
        };
        let verdict = classify(Evidence {
            status: response.status,
            body: &response.body,
            content_ok: content.passed,
            headers: &response.headers,
        });
        let note = if content.passed {
            None
        } else if let Some(wall) = &verdict.wall {
            Some(format!("{}:{}", wall.vendor, wall.marker))
        } else {
            Some(content.failures.join("; "))
        };
        Attempt {
            identity: identity.name.to_string(),
            transport: "document".to_string(),
            class: verdict.class,
            status: response.status,
            ms: response.elapsed_ms,
            bytes: response.body.len(),
            version: response.version.clone(),
            title: content.title,
            note,
        }
    }

    fn wire(&mut self, identity: &'static Identity) -> Result<&Wire, String> {
        let key = (identity.name, self.proxy.clone());
        if !self.wires.contains_key(&key) {
            let wire = Wire::via(identity, self.proxy.as_deref())?;
            self.wires.insert(key.clone(), wire);
        }
        let wire = self.wires.get(&key).expect("wire just inserted");
        wire.set_policy(self.policy.clone());
        Ok(wire)
    }

    /// Sleep `wait`, or less when the caller stops waiting: a cancelled fetch
    /// does not hold its thread through a pause it will never spend.
    fn nap(&self, wait: Duration) {
        let until = Instant::now() + wait;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if self
                .policy
                .cancel
                .as_ref()
                .is_some_and(crate::Cancel::is_cancelled)
            {
                return;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    /// The fetch is over: its policy goes, from the worker and from every wire
    /// it keeps, so an idle worker holds none of the request's policy (its hop
    /// guard and whatever the guard captured). When its caller stopped
    /// waiting, every connection it may have left mid-request goes with the
    /// wires (a wire's runtime runs its connections only while a request is
    /// in flight).
    fn release(&mut self) {
        let policy = std::mem::take(&mut self.policy);
        if policy
            .cancel
            .as_ref()
            .is_some_and(crate::Cancel::is_cancelled)
        {
            self.wires.clear();
        } else {
            // `wire` sets the policy of the request that uses a wire before
            // every use, so the default left here is never applied.
            for wire in self.wires.values() {
                wire.set_policy(crate::http::Policy::default());
            }
        }
    }

    /// One request as `once` describes it ([`crate::Once`]).
    pub fn once(&mut self, once: &crate::Once) -> Result<crate::Reply, String> {
        let name = once.identity.as_deref().unwrap_or("chrome-local");
        let identity =
            identity::lookup(name).ok_or_else(|| format!("identity: unknown {name:?}"))?;
        let host = url::Url::parse(&once.url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
            .ok_or_else(|| format!("request: not a URL: {}", once.url))?;
        self.proxy = once.proxy.clone();
        self.policy = once.policy.clone();
        let cookie = if once.cookies {
            self.state.cookie_header_for(&once.url, identity.name)
        } else {
            None
        };
        let headers: Vec<(&str, &str)> = once
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let result = self.wire(identity).and_then(|wire| {
            wire.call(
                identity,
                crate::http::Call {
                    method: "GET",
                    url: &once.url,
                    cookie: cookie.as_deref(),
                    timeout: once.timeout,
                    extra: &headers,
                    body: None,
                    cap: once.cap,
                    kind: once.kind,
                },
            )
        });
        self.release();
        let response = result?;
        if once.cookies {
            let cookies = response.cookies.iter().map(stored).collect();
            self.state.store_cookies(&host, identity.name, cookies);
        }
        Ok(crate::Reply {
            status: response.status,
            url: response.url,
            headers: response.headers,
            bytes: response.raw.unwrap_or_else(|| response.body.into_bytes()),
            identity: identity.name,
            version: response.version,
            elapsed_ms: response.elapsed_ms,
        })
    }

    /// Fetch one URL under the policy in `options`: the page cache first, then
    /// the encounter; what was learned is written once, at the end.
    pub fn fetch(&mut self, url: &str, options: &Options) -> Outcome {
        self.policy = options.policy.clone();
        let outcome = self.through_cache(url, options);
        self.release();
        self.pending = None;
        self.captured = None;
        if self.dirty.replace(false) && self.saves {
            self.base = self.state.save_merged(&self.base);
            self.state = self.base.clone();
        }
        outcome
    }

    fn through_cache(&mut self, url: &str, options: &Options) -> Outcome {
        // A cold probe neither reads nor writes the cache: no state in, none out.
        if !(self.persist && options.persist && options.cache) {
            return self.encounter(url, options);
        }
        let Some(host) = url::Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_string))
        else {
            return self.encounter(url, options);
        };
        let started = Instant::now();
        let now = crate::state::now_unix();
        // The page is looked up as the client the encounter will send first.
        let first = match &options.identity {
            Some(forced) => forced.clone(),
            None => self
                .state
                .order(&host, &identity::names())
                .into_iter()
                .next()
                .unwrap_or_default(),
        };
        let key = cache::key(&first, options.proxy.as_deref(), url);
        let cookie = self.state.cookie_header_for(url, &first);
        let trust = match self.state.origins.get(&host) {
            Some(origin) if origin.distrust_until_unix > now => cache::Trust::Distrusted,
            Some(origin) if origin.validators_proven => cache::Trust::Proven,
            _ => cache::Trust::Unproven,
        };
        // A declared lifetime is a promise a site may not keep: news fronts change
        // inside the minute they declare. A read inside one is served without
        // asking only when the last such read was checked and found unchanged;
        // a site seen changing inside its lifetime always asks.
        let origin = self.state.origins.get(&host);
        let fresh_earned = origin
            .is_some_and(|origin| origin.fresh_earned && origin.fresh_broken_until_unix <= now);
        let mut checking_promise = false;
        // A copy that varies on the caller's referer answers only that referer.
        let navigation = cache::hash(format!("{:?}", options.policy.navigation).as_bytes());
        let stored = self.cache.get(&key).filter(|entry| {
            entry
                .vary_navigation
                .is_none_or(|context| context == navigation)
        });
        match cache::plan(stored, now, cookie.as_deref(), trust, options.revalidate) {
            Plan::Fresh(entry) if fresh_earned => {
                let contract = options.contract.clone().unwrap_or_else(Contract::generic);
                let (body, raw) = entry.document();
                let content = verify_response(
                    entry.status,
                    &entry.headers,
                    &body,
                    &entry.final_url,
                    &contract,
                    options.contract.is_none(),
                );
                if content.passed {
                    if let Some(origin) = self.state.origins.get_mut(&host).map(Arc::make_mut) {
                        origin.fresh_earned = false;
                    }
                    self.save_state();
                    return served(url, host, entry, body, raw, content, options, started);
                }
                // The stored copy does not answer this request's contract.
            }
            Plan::Fresh(entry) => {
                checking_promise = true;
                let conditional = entry.has_validators() && trust != cache::Trust::Distrusted;
                self.pending = Some(Pending { entry, conditional });
            }
            Plan::Revalidate(entry) => {
                self.pending = Some(Pending {
                    entry,
                    conditional: true,
                })
            }
            Plan::Full(previous) => {
                self.pending = previous.map(|entry| Pending {
                    entry,
                    conditional: false,
                })
            }
        }
        let mut outcome = self.encounter(url, options);
        let Some(captured) = self.captured.take() else {
            // The page is gone: a copy of it is not served or confirmed again.
            if matches!(outcome.status, 404 | 410) {
                self.cache.remove(&key);
                if let Some(pending) = &self.pending {
                    self.cache.remove(&pending.entry.key);
                }
            }
            return outcome;
        };
        let key = cache::key(captured.identity, options.proxy.as_deref(), url);
        if checking_promise {
            let kept = captured.revalidated.is_some()
                || self.pending.as_ref().is_some_and(|pending| {
                    pending.entry.text_hash == cache::text_hash(&captured.bytes)
                });
            let origin = Arc::make_mut(self.state.origins.entry(host.clone()).or_default());
            origin.fresh_earned = kept;
            if !kept {
                origin.fresh_broken_until_unix = now + cache::DISTRUST_SECS;
            }
            self.save_state();
        }
        if let Some(entry) = captured.revalidated {
            // The 304 confirmed the copy its validators came from. It is the
            // answer to this read either way; it is stored only while that copy
            // is still the stored one (a concurrent fetch may have replaced or
            // removed it since).
            if let Some(pending) = &self.pending {
                self.cache.put_confirmed(&entry, &pending.entry);
            }
            outcome.cache = Some("revalidated".into());
            return outcome;
        }
        let fresh = cache::entry(
            &key,
            url,
            &captured.final_url,
            captured.identity,
            captured.status,
            &captured.headers,
            &captured.bytes,
            captured.cookie.as_deref(),
            now,
        )
        .map(|mut entry| {
            if cache::varies_on_navigation(&captured.headers) {
                entry.vary_navigation = Some(navigation);
            }
            entry
        });
        let previous = self.pending.as_ref().map(|pending| &pending.entry);
        if previous.is_some_and(|previous| previous.text_hash != cache::text_hash(&captured.bytes))
        {
            outcome.cache = Some("changed".into());
        }
        if let (Some(previous), Some(fresh)) = (previous, fresh.as_ref()) {
            let origin = Arc::make_mut(self.state.origins.entry(host).or_default());
            if cache::validators_lied(previous, fresh) {
                origin.distrust_until_unix = now + cache::DISTRUST_SECS;
                origin.validators_proven = false;
                self.save_state();
            } else if cache::validators_tracked(previous, fresh) && !origin.validators_proven {
                origin.validators_proven = true;
                self.save_state();
            }
        }
        match fresh {
            Some(entry) => self.cache.put(&entry),
            None => self.cache.remove(&key),
        }
        outcome
    }

    /// One encounter: identities in order, each one request, until content verifies.
    /// A document whose only content is a navigation it makes on load (Reddit
    /// sends a thread's slugless address to its canonical one this way, after
    /// its gate) is followed once, as the same client with its cookies, the way
    /// the browser it claims to be would. Same host only, asked of the caller's
    /// hop guard; any other answer leaves the document as it came.
    fn follow_page_navigation(
        &mut self,
        response: crate::http::Response,
        identity: &'static Identity,
        host: &str,
        options: &Options,
        started: Instant,
    ) -> crate::http::Response {
        if !(200..300).contains(&response.status) {
            return response;
        }
        let Some(next) = page_navigation(&response.body, &response.url) else {
            return response;
        };
        // The page's own host, which a redirect may have moved off the asked one
        // (a short link): its navigation stays there.
        let page_host = url::Url::parse(&response.url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_else(|| host.to_string());
        let same_host = url::Url::parse(&next)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|next_host| next_host == page_host);
        if !same_host || same_url(&next, &response.url) {
            return response;
        }
        if let Some(guard) = &options.policy.hop {
            if guard(&response.url, &next) != crate::http::Hop::Follow {
                return response;
            }
        }
        let remaining = options.deadline.saturating_sub(started.elapsed());
        if remaining.as_millis() < 800 {
            return response;
        }
        // The answer's own cookies ride the navigation, as they would in a browser.
        self.state.store_cookies(
            &page_host,
            identity.name,
            response.cookies.iter().map(stored).collect(),
        );
        let cookie = self.state.cookie_header_for(&next, identity.name);
        let referer = response.url.clone();
        let Ok(wire) = self.wire(identity) else {
            return response;
        };
        match wire.get_arriving(&next, cookie.as_deref(), identity, remaining, &referer) {
            Ok(mut followed) if (200..300).contains(&followed.status) => {
                let mut cookies = response.cookies;
                cookies.append(&mut followed.cookies);
                followed.cookies = cookies;
                followed.elapsed_ms += response.elapsed_ms;
                followed
            }
            _ => response,
        }
    }

    /// Answer SiteGround's proof of work as `identity`
    /// ([`crate::challenge::siteground_work`]): reach the challenge page from
    /// the stub, solve it, send the answer, and ask for the stub's page again
    /// with the clearance it earned. `None` when a step fails, the deadline
    /// cannot fit the work, or the page still does not pass.
    #[allow(clippy::too_many_arguments)]
    fn answer_siteground(
        &mut self,
        url: &str,
        host: &str,
        identity: &'static Identity,
        options: &Options,
        (contract, generic): (&Contract, bool),
        response: &crate::http::Response,
        (started, first_ms): (Instant, f64),
        attempts: &mut Vec<Attempt>,
    ) -> Option<Outcome> {
        use crate::challenge::{
            siteground_answer_path, siteground_solve, siteground_stub, siteground_work,
        };
        let base = url::Url::parse(&response.url).ok()?;
        let page_host = base.host_str()?.to_ascii_lowercase();
        // Every request stays on the page's own host and is asked of the
        // caller's hop guard like any navigation.
        let same_host = |next: &str| -> Option<String> {
            let next = base.join(next).ok()?;
            let host_ok = matches!(next.scheme(), "http" | "https")
                && next.host_str().map(str::to_ascii_lowercase).as_deref()
                    == Some(page_host.as_str());
            let next = next.to_string();
            let allowed = options
                .policy
                .hop
                .as_ref()
                .is_none_or(|guard| guard(&response.url, &next) == crate::http::Hop::Follow);
            (host_ok && allowed).then_some(next)
        };
        let remaining = || options.deadline.saturating_sub(started.elapsed());
        self.state.store_cookies(
            &page_host,
            identity.name,
            response.cookies.iter().map(stored).collect(),
        );
        let work = match siteground_work(&response.body) {
            Some(work) => work,
            None => {
                let page = same_host(&siteground_stub(&response.body)?)?;
                let cookie = self.state.cookie_header_for(&page, identity.name);
                let served = self
                    .wire(identity)
                    .ok()?
                    .get_arriving(
                        &page,
                        cookie.as_deref(),
                        identity,
                        remaining(),
                        &response.url,
                    )
                    .ok()?;
                self.state.store_cookies(
                    &page_host,
                    identity.name,
                    served.cookies.iter().map(stored).collect(),
                );
                siteground_work(&served.body)?
            }
        };
        // The work must leave time for the answer and the page after it.
        let solve_by = started + options.deadline.checked_sub(Duration::from_millis(1500))?;
        let solving = Instant::now();
        let (sol, hashes) = siteground_solve(&work, solve_by)?;
        let answer_url = same_host(&siteground_answer_path(
            &work,
            &sol,
            solving.elapsed().as_millis(),
            hashes,
        ))?;
        let cookie = self.state.cookie_header_for(&answer_url, identity.name);
        let answered = self
            .wire(identity)
            .ok()?
            .get(&answer_url, cookie.as_deref(), identity, remaining())
            .ok()?;
        self.state.store_cookies(
            &page_host,
            identity.name,
            answered.cookies.iter().map(stored).collect(),
        );
        let cookie = self.state.cookie_header_for(&response.url, identity.name);
        let next = self
            .wire(identity)
            .ok()?
            .get(&response.url, cookie.as_deref(), identity, remaining())
            .ok()?;
        let content = verify_response(
            next.status,
            &next.headers,
            &next.body,
            &next.url,
            contract,
            generic,
        );
        let verdict = classify(Evidence {
            status: next.status,
            body: &next.body,
            content_ok: content.passed,
            headers: &next.headers,
        });
        attempts.push(Attempt {
            identity: identity.name.to_string(),
            transport: "document".to_string(),
            class: verdict.class,
            status: next.status,
            ms: next.elapsed_ms,
            bytes: next.body.len(),
            version: next.version.clone(),
            title: content.title.clone(),
            note: Some(format!(
                "siteground proof of work answered ({hashes} hashes)"
            )),
        });
        if verdict.class != Class::Content {
            return None;
        }
        if self.persist {
            self.state
                .record(host, identity.name, "content", true, next.elapsed_ms);
            self.save_state();
        }
        Some(Outcome {
            title: content.title,
            text_chars: content.text_chars,
            evidence: vec!["siteground proof of work".into()],
            ..answer(
                url,
                host.to_string(),
                &next,
                Class::Content,
                identity.name,
                Ended {
                    started,
                    first_ms,
                    attempts: std::mem::take(attempts),
                    spent_state: true,
                    capture: options.capture_body,
                },
            )
        })
    }

    /// Answer the `/_pow/solve` proof of work as `identity`
    /// ([`crate::challenge::pow_work`]): solve the work the page states, post
    /// the answer as the page's own `fetch`, and ask for the page again with
    /// the clearance it earned. `None` when a step fails, the deadline cannot
    /// fit the work, the server refuses the answer, or the page still does
    /// not pass.
    #[allow(clippy::too_many_arguments)]
    fn answer_pow(
        &mut self,
        url: &str,
        host: &str,
        identity: &'static Identity,
        options: &Options,
        (contract, generic): (&Contract, bool),
        response: &crate::http::Response,
        (started, first_ms): (Instant, f64),
        attempts: &mut Vec<Attempt>,
    ) -> Option<Outcome> {
        use crate::challenge::{pow_answer, pow_solve, pow_work, POW_SOLVE_PATH};
        let work = pow_work(&response.body)?;
        let base = url::Url::parse(&response.url).ok()?;
        let page_host = base.host_str()?.to_ascii_lowercase();
        let solve_url = base.join(POW_SOLVE_PATH).ok()?.to_string();
        let allowed = options
            .policy
            .hop
            .as_ref()
            .is_none_or(|guard| guard(&response.url, &solve_url) == crate::http::Hop::Follow);
        if !allowed {
            return None;
        }
        let remaining = || options.deadline.saturating_sub(started.elapsed());
        self.state.store_cookies(
            &page_host,
            identity.name,
            response.cookies.iter().map(stored).collect(),
        );
        // The work must leave time for the answer and the page after it.
        let solve_by = started + options.deadline.checked_sub(Duration::from_millis(1500))?;
        let nonce = pow_solve(&work, solve_by)?;
        let body = pow_answer(&work, nonce, &response.url);
        let origin = base.origin().ascii_serialization();
        // The page's `fetch(..., {method: "POST", cache: "no-store",
        // credentials: "same-origin"})` with a JSON body.
        let headers = [
            ("pragma", "no-cache"),
            ("cache-control", "no-cache"),
            ("content-type", "application/json"),
            ("accept", "*/*"),
            ("origin", origin.as_str()),
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-dest", "empty"),
            ("referer", response.url.as_str()),
        ];
        let cookie = self.state.cookie_header_for(&solve_url, identity.name);
        let answered = self
            .wire(identity)
            .ok()?
            .subresource(
                identity,
                crate::http::Call {
                    method: "POST",
                    url: &solve_url,
                    cookie: cookie.as_deref(),
                    timeout: remaining(),
                    extra: &headers,
                    body: Some(body.as_bytes()),
                    cap: Some(64 * 1024),
                    kind: crate::http::Kind::Subresource,
                },
            )
            .ok()?;
        self.state.store_cookies(
            &page_host,
            identity.name,
            answered.cookies.iter().map(stored).collect(),
        );
        let accepted = serde_json::from_str::<serde_json::Value>(&answered.body)
            .is_ok_and(|reply| reply["success"] == serde_json::Value::Bool(true));
        if !accepted {
            return None;
        }
        let cookie = self.state.cookie_header_for(&response.url, identity.name);
        let next = self
            .wire(identity)
            .ok()?
            .get(&response.url, cookie.as_deref(), identity, remaining())
            .ok()?;
        let content = verify_response(
            next.status,
            &next.headers,
            &next.body,
            &next.url,
            contract,
            generic,
        );
        let verdict = classify(Evidence {
            status: next.status,
            body: &next.body,
            content_ok: content.passed,
            headers: &next.headers,
        });
        attempts.push(Attempt {
            identity: identity.name.to_string(),
            transport: "document".to_string(),
            class: verdict.class,
            status: next.status,
            ms: next.elapsed_ms,
            bytes: next.body.len(),
            version: next.version.clone(),
            title: content.title.clone(),
            note: Some(format!("proof of work answered (nonce {nonce})")),
        });
        if verdict.class != Class::Content {
            return None;
        }
        if self.persist {
            self.state
                .record(host, identity.name, "content", true, next.elapsed_ms);
            self.save_state();
        }
        Some(Outcome {
            title: content.title,
            text_chars: content.text_chars,
            evidence: vec!["pow-solve proof of work".into()],
            ..answer(
                url,
                host.to_string(),
                &next,
                Class::Content,
                identity.name,
                Ended {
                    started,
                    first_ms,
                    attempts: std::mem::take(attempts),
                    spent_state: true,
                    capture: options.capture_body,
                },
            )
        })
    }

    fn encounter(&mut self, url: &str, options: &Options) -> Outcome {
        let started = Instant::now();
        self.proxy = options.proxy.clone();
        let parsed = match url::Url::parse(url) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Outcome {
                    url: url.to_string(),
                    cache: None,
                    host: String::new(),
                    final_url: url.to_string(),
                    class: Class::Network,
                    identity: None,
                    status: 0,
                    ms_total: 0.0,
                    ms_first: 0.0,
                    attempts: Vec::new(),
                    title: String::new(),
                    text_chars: 0,
                    wall: None,
                    evidence: vec![format!("bad url: {error}")],
                    failures: Vec::new(),
                    spent_state: false,
                    transport: "document".to_string(),
                    items: 0,
                    sample: Vec::new(),
                    sample_urls: Vec::new(),
                    body_text: String::new(),
                    body: None,
                    headers: Vec::new(),
                    bytes: None,
                }
            }
        };
        let host = parsed.host_str().unwrap_or_default().to_string();
        let prior = identity::names();
        let order = match &options.identity {
            Some(forced) => vec![forced.clone()],
            None => self.state.order(&host, &prior),
        };
        // What is already known about this origin decides how much it is asked in
        // one encounter. A class another identity cannot change costs one request;
        // a class we own costs two; a first contact gets three.
        let remembered = self
            .state
            .origins
            .get(&host)
            .and_then(|state| state.wall_class.clone());
        // Measured (identity matrix, 2026-11-06): shells, empty documents, program
        // walls and most interaction walls answer differently to another
        // identity, so a remembered failure still earns a second identity. A 404,
        // and an interaction wall whose vendor no identity is known to pass, do
        // not: one request.
        let remembered_vendor = self
            .state
            .origins
            .get(&host)
            .and_then(|state| state.wall_vendor.clone());
        let vendor_passable = remembered_vendor
            .map(|vendor| !self.state.vendor_order(&vendor).is_empty())
            .unwrap_or(false);
        // An HTTP error is about the URL that drew it (an agent's guessed path
        // that 404s), not the client: it does not shrink what the next URL on
        // this origin may try. An error answer still ends its own encounter.
        let budget = match remembered.as_deref() {
            Some("wall_interaction") if !vendor_passable => 1,
            Some(_) => 2,
            None => options.max_tries,
        };
        let contract = options.contract.clone().unwrap_or_else(Contract::generic);
        let generic = options.contract.is_none();

        let mut attempts: Vec<Attempt> = Vec::new();
        let mut first_ms = 0.0;
        let mut spent_state = false;
        let mut data_tried = false;
        let mut engine_tried = false;
        let mut content_tried = false;
        let mut last_body = String::new();
        let mut last_final_url = String::new();
        let mut last_headers: Vec<(String, String)> = Vec::new();
        let mut last_raw: Option<Vec<u8>> = None;
        let mut handoff_taken = false;
        let mut siteground_taken = false;
        let mut pow_taken = false;
        if options.pace_ms > 0 {
            self.state.pace(&host, options.pace_ms);
        }

        // An origin whose interface answered last time is asked there first: the
        // document it walls or leaves empty is not requested again. A path whose
        // interface answers the question *better* than the document is asked
        // first always -- a listing's document is a partial rendering of it. If
        // the interface fails, the route is forgotten and the encounter runs
        // whole; an interface that only asked for time (paced, cooling down, 429)
        // is still the route, and is asked first again once the time has passed.
        let planned = datapath::plan(url);
        let preferred = planned.as_ref().is_some_and(|(path, _)| path.preferred);
        let routed = self
            .state
            .origins
            .get(&host)
            .and_then(|origin| origin.route.clone());
        if planned.is_some() && (routed.as_deref() == Some("data") || preferred) {
            if let Some(identity) = order.first().and_then(|name| identity::lookup(name)) {
                data_tried = true;
                match self.ask_interface(
                    url,
                    &host,
                    identity,
                    options,
                    started,
                    first_ms,
                    &mut attempts,
                ) {
                    Interface::Answered(outcome) => return *outcome,
                    Interface::Wait => {}
                    Interface::Unanswered
                        if attempts.last().is_some_and(|attempt| attempt.status == 429) => {}
                    Interface::Unanswered => {
                        if let Some(origin) = self.state.origins.get_mut(&host).map(Arc::make_mut) {
                            origin.route = None;
                        }
                        self.save_state();
                    }
                }
            }
        }

        // The order is a queue: a wall that names its vendor moves the identities
        // that vendor has let through to the front of what is left.
        let mut queue: std::collections::VecDeque<String> = order.iter().cloned().collect();
        let mut tries = 0usize;
        // One interaction wall is spent before the encounter stops on it.
        let mut interaction_asked = false;
        let mut last_wall: Option<crate::classify::Wall> = None;
        // The previous attempt's answer: class, status and body hash.
        let mut last_answer: Option<(Class, u16, u64)> = None;
        let mut last_evidence: Vec<String> = Vec::new();
        // Transport failures so far, the wire classes they happened on, and
        // whether one of them was a server that went silent.
        let mut transport_failures = 0usize;
        let mut failed_wires: Vec<&'static str> = Vec::new();
        let mut stalled = false;
        while tries < budget.max(1) {
            let Some(name) = queue.pop_front() else { break };
            let index = tries;
            tries += 1;
            let name = &name;
            let Some(identity) = identity::lookup(name) else {
                continue;
            };
            let remaining = options.deadline.saturating_sub(started.elapsed());
            // Do not start another request that cannot finish inside the
            // encounter's budget: a half-finished attempt costs the same as a
            // finished one. The first request always goes out with whatever is
            // left, or a caller's short deadline would end the read unasked.
            let floor = if index == 0 { 100 } else { 1200 };
            if remaining.as_millis() < floor {
                break;
            }
            // After a server went silent on one client, the other client is
            // asked on a short clock: a host that stalls one fingerprint answers
            // another at its ordinary speed, and a host that stalls everyone is
            // not waited on twice.
            let remaining = if stalled {
                remaining.min(STALL_RETRY_MAX)
            } else {
                remaining
            };
            // Space attempts inside one encounter too: a burst of identities is
            // as suspicious as a burst of requests.
            if index > 0 {
                self.nap(Duration::from_millis(250));
            }
            // Arrival: a browser that has been on this site before does not jump
            // straight to an inner page from nowhere. One priming request to the
            // origin root, its cookies kept, costs a request once per origin.
            if options.arrival
                && index == 0
                && !self.state.seen_origin(&host)
                && parsed.path() != "/"
            {
                let origin = format!("{}://{}/", parsed.scheme(), host);
                if let Ok(wire) = self.wire(identity) {
                    let remaining = options.deadline.saturating_sub(started.elapsed());
                    if remaining.as_millis() > 800 {
                        if let Ok(prime) = wire.get(&origin, None, identity, remaining / 2) {
                            let cookies: Vec<StoredCookie> =
                                prime.cookies.iter().map(stored).collect();
                            self.state.store_cookies(&host, identity.name, cookies);
                            self.state.mark_seen(&host);
                            self.save_state();
                        }
                    }
                }
            }
            // A minted credential is renewed before the age at which this origin
            // has been seen to refuse one, so the refusal is paid once.
            self.state
                .expire_stale_credential(&host, identity.name, crate::state::now_unix());
            let credential_vendor = self
                .state
                .credentials
                .get(&host)
                .and_then(|per_identity| per_identity.get(identity.name))
                .map(|credential| credential.vendor.clone());
            let cookie_header = self.state.cookie_header_for(url, identity.name);
            if cookie_header.is_some() {
                spent_state = true;
            }
            let spent_credential = credential_vendor.is_some() && cookie_header.is_some();
            let referer = format!("{}://{}/", parsed.scheme(), host);
            let arriving = options.arrival && self.state.seen_origin(&host) && parsed.path() != "/";
            // Revisit a cached page the way a browser does: its validators, sent by
            // the client that received it.
            let conditional: Vec<(&'static str, String)> = match &self.pending {
                Some(pending)
                    if pending.conditional
                        && pending.entry.identity == identity.name
                        && !arriving =>
                {
                    pending.entry.conditionals()
                }
                _ => Vec::new(),
            };
            let pending_entry = self.pending.as_ref().map(|pending| pending.entry.clone());
            let mut validators_sent = !conditional.is_empty();
            let mut response = {
                let wire = match self.wire(identity) {
                    Ok(wire) => wire,
                    Err(error) => {
                        attempts.push(Attempt {
                            identity: identity.name.to_string(),
                            transport: "document".to_string(),
                            class: Class::Network,
                            status: 0,
                            ms: 0.0,
                            bytes: 0,
                            version: String::new(),
                            title: String::new(),
                            note: Some(error),
                        });
                        continue;
                    }
                };
                let mut attempt = if arriving {
                    wire.get_arriving(url, cookie_header.as_deref(), identity, remaining, &referer)
                } else if conditional.is_empty() {
                    wire.get(url, cookie_header.as_deref(), identity, remaining)
                } else {
                    let headers: Vec<(&str, &str)> = conditional
                        .iter()
                        .map(|(name, value)| (*name, value.as_str()))
                        .collect();
                    wire.request(
                        "GET",
                        url,
                        cookie_header.as_deref(),
                        identity,
                        remaining,
                        &headers,
                        None,
                    )
                };
                // A 304 that answers the validators but names another
                // representation (RFC 9111 §4.3.4) confirms nothing: ask once
                // more, without validators.
                if let (Ok(answer), Some(entry)) = (&attempt, pending_entry.as_ref()) {
                    if answer.status == 304
                        && validators_sent
                        && same_url(&answer.url, url)
                        && !cache::selects(entry, &answer.headers)
                    {
                        attempt = wire.get(url, cookie_header.as_deref(), identity, remaining);
                        validators_sent = false;
                    }
                }
                match attempt {
                    Ok(response) => response,
                    Err(error) => {
                        let ms = started.elapsed().as_secs_f64() * 1000.0;
                        // A hop the caller refused, a redirect loop, a caller
                        // that stopped waiting, or a failure that happens before
                        // any client is shown (the name, the route, the socket,
                        // the certificate) is the same for every identity: no
                        // other one is asked. A failure after the client spoke
                        // may be about the client: one identity of another wire
                        // class is asked, never a third.
                        transport_failures += 1;
                        failed_wires.push(identity.wire_class());
                        let final_error = error.starts_with("denied:")
                            || error.starts_with("redirect:")
                            || error.starts_with("cancelled:")
                            || transport_scope(&error) == TransportScope::Host
                            || transport_failures >= TRANSPORT_FAILURES_MAX;
                        if !final_error {
                            // A connect that timed out is a dead route or a slow
                            // handshake (badssl.com: 0.3 s RTT, 1.1 s TLS), not a
                            // server that heard the request and went silent.
                            stalled |= error.starts_with("timeout:") && !connect_phase(&error);
                            let fresh = queue.iter().position(|name| {
                                identity::lookup(name)
                                    .is_some_and(|next| !failed_wires.contains(&next.wire_class()))
                            });
                            match fresh {
                                Some(at) => {
                                    if let Some(moved) = queue.remove(at) {
                                        queue.push_front(moved);
                                    }
                                }
                                // Every wire class left has already failed here.
                                None => queue.clear(),
                            }
                        }
                        attempts.push(Attempt {
                            identity: identity.name.to_string(),
                            transport: "document".to_string(),
                            class: Class::Network,
                            status: 0,
                            ms,
                            bytes: 0,
                            version: String::new(),
                            title: String::new(),
                            note: Some(error),
                        });
                        if final_error {
                            break;
                        }
                        continue;
                    }
                }
            };
            if index == 0 {
                first_ms = response.elapsed_ms;
            }
            // The server confirmed the cached page: it is the response. Only a
            // 304 to the request that carried the validators confirms anything;
            // any other 304 (no validator sent, or after a redirect) is an
            // answer no client asked for, and no identity changes that.
            let mut revalidated: Option<cache::Entry> = None;
            if response.status == 304 && !(validators_sent && same_url(&response.url, url)) {
                attempts.push(Attempt {
                    identity: identity.name.to_string(),
                    transport: "document".to_string(),
                    class: Class::HttpError,
                    status: response.status,
                    ms: response.elapsed_ms,
                    bytes: response.body.len(),
                    version: response.version.clone(),
                    title: String::new(),
                    note: Some("unsolicited 304".to_string()),
                });
                return ended(
                    url,
                    host,
                    &response,
                    identity.name,
                    "unsolicited 304",
                    Ended {
                        started,
                        first_ms,
                        attempts,
                        spent_state,
                        capture: options.capture_body,
                    },
                );
            }
            if response.status == 304 {
                if let Some(pending) = self.pending.as_ref() {
                    let entry = cache::confirmed(
                        pending.entry.clone(),
                        &response.headers,
                        cookie_header.as_deref(),
                        crate::state::now_unix(),
                    );
                    let (body, raw) = entry.document();
                    response.status = entry.status;
                    response.body = body;
                    response.raw = raw;
                    response.headers = entry.headers.clone();
                    revalidated = Some(entry);
                }
            }
            // A redirect the wire did not follow (the caller's policy, or its hop
            // guard stopped it) is the site's answer, not a wall: it ends the
            // encounter as it is, and no identity is scored on it.
            if matches!(response.status, 301 | 302 | 303 | 307 | 308)
                && response
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("location"))
            {
                if self.persist {
                    let cookies: Vec<StoredCookie> = response.cookies.iter().map(stored).collect();
                    self.state.store_cookies(&host, identity.name, cookies);
                    self.save_state();
                }
                attempts.push(Attempt {
                    identity: identity.name.to_string(),
                    transport: "document".to_string(),
                    class: Class::HttpError,
                    status: response.status,
                    ms: response.elapsed_ms,
                    bytes: response.body.len(),
                    version: response.version.clone(),
                    title: String::new(),
                    note: Some("redirect not followed".to_string()),
                });
                return ended(
                    url,
                    host,
                    &response,
                    identity.name,
                    "redirect not followed",
                    Ended {
                        started,
                        first_ms,
                        attempts,
                        spent_state,
                        capture: options.capture_body,
                    },
                );
            }
            let response = if revalidated.is_none() {
                self.follow_page_navigation(response, identity, &host, options, started)
            } else {
                response
            };
            last_body = response.body.clone();
            last_final_url = response.url.clone();
            if options.capture_body {
                last_headers = response.headers.clone();
                last_raw = response.raw.clone();
            }
            let content: Content = verify_response(
                response.status,
                &response.headers,
                &response.body,
                &response.url,
                &contract,
                generic,
            );
            let verdict: Verdict = classify(Evidence {
                status: response.status,
                body: &response.body,
                content_ok: content.passed,
                headers: &response.headers,
            });

            // A client-rendered shell gets the lane once: a Shell verdict ends
            // the encounter here regardless, so the lane takes the last word the
            // site gave and never a later identity's turn.
            if verdict.class == Class::Shell
                && !content_tried
                && (200..300).contains(&response.status)
            {
                content_tried = true;
                if let Some(outcome) = self.content_lane(
                    url,
                    &host,
                    &response,
                    (&contract, generic),
                    identity,
                    options,
                    started,
                    first_ms,
                    &mut attempts,
                    spent_state,
                ) {
                    return outcome;
                }
            }

            // The origin's verdict on a credential the engine minted earlier: a
            // content answer extends what we know of its lifetime; a 202/401/403
            // wall ends it, and the one re-mint below replaces it.
            let credential_refused = spent_credential
                && verdict.class != Class::Content
                && (verdict.class == Class::WallProgram
                    || matches!(response.status, 202 | 401 | 403));
            if spent_credential {
                let now = crate::state::now_unix();
                if verdict.class == Class::Content {
                    self.state
                        .credential_verdict(&host, identity.name, true, now);
                } else if credential_refused {
                    self.state
                        .credential_verdict(&host, identity.name, false, now);
                }
            }
            let cookies: Vec<StoredCookie> = response.cookies.iter().map(stored).collect();
            if self.persist {
                self.state
                    .store_cookies(&host, identity.name, cookies.clone());
            }
            let class_name = class_name(verdict.class);
            if self.persist {
                self.state.record(
                    &host,
                    identity.name,
                    class_name,
                    verdict.class == Class::Content,
                    response.elapsed_ms,
                );
                if verdict.class != Class::Content {
                    self.state.record_wall(
                        &host,
                        Some(identity.name),
                        class_name,
                        verdict.wall.as_ref().map(|wall| wall.vendor.as_str()),
                        &verdict.evidence,
                    );
                }
                let upcoming = order.iter().find(|name| *name != identity.name).cloned();
                if let Some(state) = self.state.origins.get_mut(&host).map(Arc::make_mut) {
                    state.next = upcoming;
                }
            }
            attempts.push(Attempt {
                identity: identity.name.to_string(),
                transport: "document".to_string(),
                class: verdict.class,
                status: response.status,
                ms: response.elapsed_ms,
                bytes: response.body.len(),
                version: response.version.clone(),
                title: content.title.clone(),
                note: if content.passed {
                    None
                } else {
                    Some(content.failures.join("; "))
                },
            });

            last_wall = verdict.wall.clone();
            last_evidence = verdict.evidence.clone();
            // A named vendor reorders what is left: the identities it has let
            // through come next. A forced identity has nothing to reorder.
            let mut escalation: Vec<String> = Vec::new();
            if options.identity.is_none() && verdict.class != Class::Content {
                if let Some(vendor) = verdict.wall.as_ref().map(|wall| wall.vendor.clone()) {
                    escalation = self
                        .state
                        .vendor_order(&vendor)
                        .into_iter()
                        .filter(|name| queue.contains(name))
                        .collect();
                    queue.retain(|name| !escalation.contains(name));
                    for name in escalation.iter().rev() {
                        queue.push_front(name.clone());
                    }
                }
            }

            if verdict.class == Class::Content {
                if self.persist {
                    if let Some(origin) = self.state.origins.get_mut(&host).map(Arc::make_mut) {
                        origin.route = None;
                    }
                    self.save_state();
                }
                self.captured = Some(Captured {
                    identity: identity.name,
                    final_url: response.url.clone(),
                    status: response.status,
                    headers: response.headers.clone(),
                    bytes: response
                        .raw
                        .clone()
                        .unwrap_or_else(|| response.body.clone().into_bytes()),
                    cookie: cookie_header.clone(),
                    revalidated: revalidated.take(),
                });
                return Outcome {
                    title: content.title,
                    text_chars: content.text_chars,
                    wall: verdict.wall,
                    evidence: verdict.evidence,
                    ..answer(
                        url,
                        host,
                        &response,
                        verdict.class,
                        identity.name,
                        Ended {
                            started,
                            first_ms,
                            attempts,
                            spent_state,
                            capture: options.capture_body,
                        },
                    )
                };
            }

            // A native challenge handoff: the page tells the client to come back
            // with a token after a delay. Doing that as the same client is what a
            // browser without JavaScript does.
            if verdict.class != Class::Content {
                if let Some(handoff) = akamai_bm_verify(&response.body) {
                    // The page's own wait and one request after it must fit the
                    // encounter's budget; the target is the page's own address,
                    // on its own host, and is asked of the caller's hop guard like
                    // any navigation.
                    let wait = Duration::from_secs(handoff.delay_secs);
                    let target = url::Url::parse(&response.url)
                        .and_then(|base| base.join(&handoff.target))
                        .ok()
                        .filter(|next| {
                            matches!(next.scheme(), "http" | "https")
                                && next.host_str().map(str::to_ascii_lowercase)
                                    == url::Url::parse(&response.url).ok().and_then(|base| {
                                        base.host_str().map(str::to_ascii_lowercase)
                                    })
                        })
                        .map(|next| next.to_string())
                        .filter(|next| {
                            options.policy.hop.as_ref().is_none_or(|guard| {
                                guard(&response.url, next) == crate::http::Hop::Follow
                            })
                        });
                    let remaining = options.deadline.saturating_sub(started.elapsed());
                    let fits = remaining > wait + Duration::from_millis(1200);
                    if let (false, Some(target), true) = (handoff_taken, target, fits) {
                        handoff_taken = true;
                        // The answer's own cookies (`ak_bmsc`) ride the hand-off,
                        // as they would in a browser, persisted or not.
                        self.state
                            .store_cookies(&host, identity.name, cookies.clone());
                        self.nap(wait);
                        let timeout = options.deadline.saturating_sub(started.elapsed());
                        // One request to the handoff URL, same identity, same jar.
                        let cookie = self.state.cookie_header_for(&target, identity.name);
                        let handed = match self.wire(identity) {
                            Ok(wire) => wire.get(&target, cookie.as_deref(), identity, timeout),
                            Err(error) => Err(error),
                        };
                        if let Ok(next) = handed {
                            let content = verify_response(
                                next.status,
                                &next.headers,
                                &next.body,
                                &next.url,
                                &contract,
                                generic,
                            );
                            let verdict2 = classify(Evidence {
                                status: next.status,
                                body: &next.body,
                                content_ok: content.passed,
                                headers: &next.headers,
                            });
                            attempts.push(Attempt {
                                identity: identity.name.to_string(),
                                transport: "document".to_string(),
                                class: verdict2.class,
                                status: next.status,
                                ms: next.elapsed_ms,
                                bytes: next.body.len(),
                                version: next.version.clone(),
                                title: content.title.clone(),
                                note: Some(format!(
                                    "bm-verify followed after {} s",
                                    handoff.delay_secs
                                )),
                            });
                            if verdict2.class == Class::Content {
                                if self.persist {
                                    self.state.record(
                                        &host,
                                        identity.name,
                                        "content",
                                        true,
                                        next.elapsed_ms,
                                    );
                                    self.save_state();
                                }
                                return Outcome {
                                    title: content.title,
                                    text_chars: content.text_chars,
                                    evidence: vec!["akamai bm-verify".into()],
                                    ..answer(
                                        url,
                                        host,
                                        &next,
                                        Class::Content,
                                        identity.name,
                                        Ended {
                                            started,
                                            first_ms,
                                            attempts,
                                            spent_state: cookie.is_some(),
                                            capture: options.capture_body,
                                        },
                                    )
                                };
                            }
                        }
                    }
                }
            }

            // SiteGround's proof of work: the challenge page carries all of it, and
            // the answer is one request whose clearance the stub's own URL then
            // accepts. Once per encounter, as the identity that was challenged.
            if !siteground_taken
                && verdict
                    .wall
                    .as_ref()
                    .is_some_and(|wall| wall.vendor == "siteground")
            {
                siteground_taken = true;
                if let Some(outcome) = self.answer_siteground(
                    url,
                    &host,
                    identity,
                    options,
                    (&contract, generic),
                    &response,
                    (started, first_ms),
                    &mut attempts,
                ) {
                    return outcome;
                }
            }

            // The `/_pow/solve` proof of work: the page states its work, the
            // answer is one post from the page, and the clearance it earns
            // lets the same URL serve the page. Once per encounter.
            if !pow_taken
                && verdict
                    .wall
                    .as_ref()
                    .is_some_and(|wall| wall.vendor == "pow-solve")
            {
                pow_taken = true;
                if let Some(outcome) = self.answer_pow(
                    url,
                    &host,
                    identity,
                    options,
                    (&contract, generic),
                    &response,
                    (started, first_ms),
                    &mut attempts,
                ) {
                    return outcome;
                }
            }

            // The document is walled, a shell, or incomplete. The site's own data
            // interface usually answers the same question openly, and a request to
            // it is exactly what the app itself sends. An interface that is only
            // asking for time (paced, cooling down) has not answered: fall through
            // to the site's own program rather than re-asking the document, which
            // is how a rate window used to hide a page the engine could clear.
            if !data_tried && datapath::plan(url).is_some() {
                data_tried = true;
                match self.ask_interface(
                    url,
                    &host,
                    identity,
                    options,
                    started,
                    first_ms,
                    &mut attempts,
                ) {
                    Interface::Answered(outcome) => return *outcome,
                    Interface::Wait => {}
                    Interface::Unanswered => {}
                }
            }

            // AWS WAF decides on the client class: a plain client is served a
            // different page than a browser. Ask as the Chrome class first, then
            // run the site's own program in our process. One mint per encounter;
            // the credential is bound to the identity that minted it, and only
            // what the server accepts counts.
            if (verdict.class == Class::WallProgram || credential_refused) && !engine_tried {
                engine_tried = true;
                // Which vendor's program is this? AWS WAF decides by client class;
                // Cloudflare serves its managed challenge as a page of its own.
                // Both are the site's own program and both run in our engine.
                let program_vendor: Option<&'static str> = if verdict
                    .wall
                    .as_ref()
                    .map(|wall| wall.vendor == "aws-waf")
                    .unwrap_or(false)
                    || credential_vendor.as_deref() == Some("aws-waf")
                    || crate::engine::is_aws_waf_page(&response.body)
                {
                    Some("aws-waf")
                } else if verdict
                    .wall
                    .as_ref()
                    .map(|wall| wall.vendor == "cloudflare")
                    .unwrap_or(false)
                    && tries >= budget
                    && crate::engine::is_cloudflare_page(&response.body)
                {
                    // Cloudflare serves the challenge payload only to a client
                    // class it means to challenge: a header alone (`cf-mitigated`
                    // on an empty 403) names the wall but carries no program, and
                    // identity selection is what answers that. The engine runs
                    // only on a page that holds the program, and only on the last
                    // attempt the encounter allows — identity first, always.
                    Some("cloudflare")
                } else if crate::engine::is_reddit_page(&response.body) && response.status != 429 {
                    // Reddit's gate is served to every class that is not a
                    // desktop browser (those get the interactive captcha), and
                    // what it carries is the page's own program: a form whose
                    // field that program computes. Run it as the client that
                    // received it, on this first wall -- unlike AWS WAF, the
                    // answer does not depend on being re-asked as Chrome.
                    // A 429 is the origin asking for time, not a program: it is
                    // paced, never minted on.
                    Some("reddit")
                } else {
                    None
                };
                if let Some(program_vendor) = program_vendor {
                    let mut engine_identity = identity;
                    let mut engine_body = response.body.clone();
                    let mut engine_url = response.url.clone();

                    // 1. Identity before engine. AWS WAF decides by client class:
                    // a plain client is served a stub, the Chrome class the
                    // challenge page. Cloudflare does not need this hop — its
                    // vendor order already tries the identities that passed it.
                    if program_vendor == "aws-waf" && identity.family == identity::Family::Plain {
                        if let Some(chrome) = identity::lookup("chrome-local") {
                            let budget = options
                                .deadline
                                .max(Duration::from_millis(8000))
                                .saturating_sub(started.elapsed());
                            if budget.as_millis() >= 900 {
                                let cookie = self.state.cookie_header_for(url, chrome.name);
                                let second = match self.wire(chrome) {
                                    Ok(wire) => wire.get(url, cookie.as_deref(), chrome, budget),
                                    Err(error) => Err(error),
                                };
                                if let Ok(second) = second {
                                    let content = verify_response(
                                        second.status,
                                        &second.headers,
                                        &second.body,
                                        &second.url,
                                        &contract,
                                        generic,
                                    );
                                    let verdict2 = classify(Evidence {
                                        status: second.status,
                                        body: &second.body,
                                        content_ok: content.passed,
                                        headers: &second.headers,
                                    });
                                    attempts.push(Attempt {
                                        identity: chrome.name.to_string(),
                                        transport: "document".to_string(),
                                        class: verdict2.class,
                                        status: second.status,
                                        ms: second.elapsed_ms,
                                        bytes: second.body.len(),
                                        version: second.version.clone(),
                                        title: content.title.clone(),
                                        note: Some(
                                            "identity: chrome-local for aws-waf".to_string(),
                                        ),
                                    });
                                    if verdict2.class == Class::Content {
                                        if self.persist {
                                            self.state.record(
                                                &host,
                                                chrome.name,
                                                "content",
                                                true,
                                                second.elapsed_ms,
                                            );
                                            self.save_state();
                                        }
                                        return Outcome {
                                            title: content.title,
                                            text_chars: content.text_chars,
                                            evidence: vec!["identity: chrome-local".to_string()],
                                            ..answer(
                                                url,
                                                host,
                                                &second,
                                                Class::Content,
                                                chrome.name,
                                                Ended {
                                                    started,
                                                    first_ms,
                                                    attempts,
                                                    spent_state: false,
                                                    capture: options.capture_body,
                                                },
                                            )
                                        };
                                    }
                                    engine_identity = chrome;
                                    engine_body = second.body.clone();
                                    engine_url = second.url.clone();
                                }
                            }
                        }
                    }

                    // A refused credential can come back as a bare 401/403 with no
                    // challenge in it. Ask once more without it: the site's answer
                    // to a client with no credential is the page the engine needs.
                    if credential_refused && !is_program_page(program_vendor, &engine_body) {
                        let cookie = self.state.cookie_header_for(url, engine_identity.name);
                        let budget = options
                            .deadline
                            .max(Duration::from_millis(8000))
                            .saturating_sub(started.elapsed());
                        if budget.as_millis() >= 900 {
                            let clean = match self.wire(engine_identity) {
                                Ok(wire) => {
                                    wire.get(url, cookie.as_deref(), engine_identity, budget)
                                }
                                Err(error) => Err(error),
                            };
                            if let Ok(clean) = clean {
                                engine_body = clean.body.clone();
                                engine_url = clean.url.clone();
                            }
                        }
                    }

                    // 2. The site's own program, inside the mint budget this host
                    // has earned.
                    let gate = self.state.engine_gate(&host, crate::state::now_unix());
                    if let Err(reason) = &gate {
                        // The engine did not run: the status is the wall's own.
                        attempts.push(Attempt {
                            identity: engine_identity.name.to_string(),
                            transport: "document".to_string(),
                            class: Class::WallProgram,
                            status: response.status,
                            ms: 0.0,
                            bytes: 0,
                            version: String::new(),
                            title: String::new(),
                            note: Some(format!("engine: {reason}")),
                        });
                    }
                    if gate.is_ok() && is_program_page(program_vendor, &engine_body) {
                        let cookie = self.state.cookie_header_for(url, engine_identity.name);
                        let mut failure_reason: Option<String> = None;
                        match crate::engine::mint_within_via(
                            &options.engine,
                            &engine_url,
                            &engine_body,
                            engine_identity,
                            cookie.as_deref(),
                            self.proxy.as_deref(),
                        ) {
                            Ok(mint) => {
                                // The challenge's own host: the asked one, or where a
                                // redirect (a short link) took the document.
                                let page_host = url::Url::parse(&engine_url)
                                    .ok()
                                    .and_then(|parsed| {
                                        parsed.host_str().map(str::to_ascii_lowercase)
                                    })
                                    .unwrap_or_else(|| host.clone());
                                // The retry must spend what the mint produced, even
                                // when state is not persisted to disk.
                                self.state.store_cookies(
                                    &page_host,
                                    engine_identity.name,
                                    mint.cookies.clone(),
                                );
                                self.state.mint_recorded(
                                    &host,
                                    engine_identity.name,
                                    program_vendor,
                                    &mint.cookies,
                                    crate::state::now_unix(),
                                );

                                let budget = options
                                    .deadline
                                    .max(Duration::from_millis(8000))
                                    .saturating_sub(started.elapsed());
                                // Follow the navigation the page itself chose (it may
                                // be relative), but only on the challenge's own host:
                                // never an off-site redirect.
                                let retry_url: String = mint
                                    .redirect
                                    .as_deref()
                                    .and_then(|next| {
                                        url::Url::parse(next)
                                            .or_else(|_| {
                                                url::Url::parse(&engine_url)
                                                    .and_then(|base| base.join(next))
                                            })
                                            .ok()
                                    })
                                    .filter(|parsed| {
                                        parsed.host_str().map(str::to_ascii_lowercase).as_deref()
                                            == Some(page_host.as_str())
                                    })
                                    .map(|parsed| parsed.to_string())
                                    .unwrap_or_else(|| url.to_string());
                                let retry_cookie = self
                                    .state
                                    .cookie_header_for(&retry_url, engine_identity.name);
                                // The credential is spent through the fetcher's own
                                // client, as the identity that minted it.
                                let retry: Option<Result<crate::http::Response, String>> =
                                    if budget.as_millis() >= 800 {
                                        match self.wire(engine_identity) {
                                            Ok(wire) => Some(wire.get(
                                                &retry_url,
                                                retry_cookie.as_deref(),
                                                engine_identity,
                                                budget,
                                            )),
                                            Err(_) => None,
                                        }
                                    } else {
                                        None
                                    };
                                let solved_url = retry
                                    .as_ref()
                                    .and_then(|retry| retry.as_ref().ok())
                                    .map(|retry| retry.url.clone());
                                let retry = retry.map(|retry| {
                                    retry.map(|retry| {
                                        self.follow_page_navigation(
                                            retry,
                                            engine_identity,
                                            &host,
                                            options,
                                            started,
                                        )
                                    })
                                });
                                if let Some(Ok(retry)) = retry {
                                    if std::env::var("V5_ENGINE_LOG").is_ok() {
                                        eprintln!(
                                            "[engine] retry {} cookie={} -> {} ({}B)",
                                            retry_url,
                                            retry_cookie.is_some(),
                                            retry.status,
                                            retry.body.len()
                                        );
                                    }
                                    // Inspection only: the answer the mint was spent on,
                                    // beside the requests the page made.
                                    if let Some(directory) = std::env::var_os("V5_ENGINE_DUMP_DIR")
                                    {
                                        let _ = std::fs::write(
                                            std::path::Path::new(&directory)
                                                .join(format!("spend-{}.html", retry.status)),
                                            &retry.body,
                                        );
                                    }
                                    let content = verify_response(
                                        retry.status,
                                        &retry.headers,
                                        &retry.body,
                                        &retry.url,
                                        &contract,
                                        generic,
                                    );
                                    let verdict2 = classify(Evidence {
                                        status: retry.status,
                                        body: &retry.body,
                                        content_ok: content.passed,
                                        headers: &retry.headers,
                                    });
                                    attempts.push(Attempt {
                                        identity: engine_identity.name.to_string(),
                                        transport: "document".to_string(),
                                        class: verdict2.class,
                                        status: retry.status,
                                        ms: retry.elapsed_ms,
                                        bytes: retry.body.len(),
                                        version: retry.version.clone(),
                                        title: content.title.clone(),
                                        note: Some(format!(
                                            "engine: {} cookies, {} surfaces, {} ms",
                                            mint.cookies.len(),
                                            mint.surfaces.len(),
                                            mint.ms.round()
                                        )),
                                    });
                                    if verdict2.class == Class::Content {
                                        let now = crate::state::now_unix();
                                        self.state.mint_succeeded(&host);
                                        self.state.credential_verdict(
                                            &host,
                                            engine_identity.name,
                                            true,
                                            now,
                                        );
                                        // The answer to a spent challenge is
                                        // itself state: the session it opens
                                        // (cookies on the retry) is carried, so
                                        // the next read of this origin is one
                                        // request.
                                        let spent: Vec<StoredCookie> =
                                            retry.cookies.iter().map(stored).collect();
                                        self.state.store_cookies(
                                            &host,
                                            engine_identity.name,
                                            spent,
                                        );
                                        if self.persist {
                                            self.state.record(
                                                &host,
                                                engine_identity.name,
                                                "content",
                                                true,
                                                retry.elapsed_ms,
                                            );
                                            self.save_state();
                                        }
                                        return Outcome {
                                            // The document the challenge stood in
                                            // front of, or where the page then
                                            // navigated. A challenge hand-off is a
                                            // handshake, not a redirect: its solved
                                            // URL carries a single-use token and is
                                            // not the page's identity.
                                            final_url: if solved_url.as_deref()
                                                == Some(retry.url.as_str())
                                            {
                                                engine_url.clone()
                                            } else {
                                                retry.url.clone()
                                            },
                                            title: content.title,
                                            text_chars: content.text_chars,
                                            evidence: vec![format!(
                                                "engine: minted in {} ms",
                                                mint.ms.round()
                                            )],
                                            ..answer(
                                                url,
                                                host,
                                                &retry,
                                                Class::Content,
                                                engine_identity.name,
                                                Ended {
                                                    started,
                                                    first_ms,
                                                    attempts,
                                                    spent_state: true,
                                                    capture: options.capture_body,
                                                },
                                            )
                                        };
                                    }
                                    failure_reason = Some(format!(
                                        "origin answered {} to the minted credential",
                                        retry.status
                                    ));
                                }
                            }
                            Err(error) => {
                                failure_reason = Some(error.to_string());
                                // The engine did not finish: the status is the
                                // wall's own, not a made-up zero.
                                attempts.push(Attempt {
                                    identity: engine_identity.name.to_string(),
                                    transport: "document".to_string(),
                                    class: Class::WallProgram,
                                    status: response.status,
                                    ms: 0.0,
                                    bytes: 0,
                                    version: String::new(),
                                    title: String::new(),
                                    note: Some(format!("engine: {error}")),
                                });
                            }
                        }
                        // Reaching here means the mint did not end in content (the
                        // success path returns above): the host's next mint waits.
                        let reason = failure_reason
                            .unwrap_or_else(|| "the mint produced no request to spend".to_string());
                        self.state.mint_failed(
                            &host,
                            engine_identity.name,
                            &reason,
                            crate::state::now_unix(),
                        );
                        if self.persist {
                            self.save_state();
                        }
                    }
                }
            }

            // A human widget is a class decision before it is a human step: the
            // class that claims no browser is the one a site asks to run a
            // program instead (Reddit serves the desktop Chrome class its
            // captcha and every other class its JS gate). One more identity, and
            // the class that claims no browser goes first. The second human wall
            // ends the encounter: the site is asking, not the class.
            let interaction_final = verdict.class == Class::WallInteraction
                && escalation.is_empty()
                && interaction_asked;
            if verdict.class == Class::WallInteraction && !interaction_asked {
                interaction_asked = true;
                if identity.family != identity::Family::Plain {
                    let plain = queue.iter().position(|name| {
                        identity::lookup(name)
                            .is_some_and(|other| other.family == identity::Family::Plain)
                    });
                    if let Some(at) = plain {
                        if let Some(moved) = queue.remove(at) {
                            queue.push_front(moved);
                        }
                    }
                }
            }
            // A rate limit that names when to come back is about this address,
            // not this client: asking again now as someone else is the burst
            // that escalates a site. The wait is the answer.
            let told_to_wait = response.status == 429 && retry_hint(&response.headers).is_some();
            // Two identities in a row were shown the same text and no wall named
            // itself: the client is not what the site is reacting to, and the
            // next will be shown it too. Text, not bytes: a per-request nonce in
            // a script or an attribute does not make the answer different.
            let shown = (
                verdict.class,
                response.status,
                cache::text_hash(response.body.as_bytes()),
            );
            let identity_blind = verdict.wall.is_none() && last_answer == Some(shown);
            last_answer = Some(shown);
            if interaction_final
                || told_to_wait
                || identity_blind
                || matches!(
                    verdict.class,
                    Class::Shell | Class::HttpError | Class::Network
                )
            {
                if self.persist {
                    self.save_state();
                }
                return Outcome {
                    title: content.title,
                    text_chars: content.text_chars,
                    wall: verdict.wall,
                    evidence: verdict.evidence,
                    failures: content.failures,
                    ..answer(
                        url,
                        host,
                        &response,
                        verdict.class,
                        identity.name,
                        Ended {
                            started,
                            first_ms,
                            attempts,
                            spent_state,
                            capture: options.capture_body,
                        },
                    )
                };
            }
        }

        if self.persist {
            self.save_state();
        }
        // A 2xx the contract could not name gets the lane once, after every
        // identity has had its turn: identity and egress spend first, and a
        // later identity's turn is never taken by JavaScript.
        if !content_tried {
            let last_answer_row = attempts
                .iter()
                .rev()
                .find(|attempt| attempt.status != 0)
                .cloned();
            if let Some(attempt) = last_answer_row {
                if attempt.class == Class::Incomplete
                    && (200..300).contains(&attempt.status)
                    && !last_body.is_empty()
                {
                    content_tried = true;
                    if let Some(identity) = identity::lookup(&attempt.identity) {
                        let final_response = crate::http::Response {
                            status: attempt.status,
                            version: String::new(),
                            headers: last_headers.clone(),
                            body: last_body.clone(),
                            url: last_final_url.clone(),
                            elapsed_ms: attempt.ms,
                            cookies: Vec::new(),
                            raw: last_raw.clone(),
                        };
                        if let Some(outcome) = self.content_lane(
                            url,
                            &host,
                            &final_response,
                            (&contract, generic),
                            identity,
                            options,
                            started,
                            first_ms,
                            &mut attempts,
                            spent_state,
                        ) {
                            return outcome;
                        }
                    }
                }
            }
        }
        // The answer the encounter ends on is the last one a server gave: a
        // later attempt that never reached it (a network failure) does not
        // erase what an earlier one was told.
        let last = attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status != 0)
            .or(attempts.last())
            .cloned();
        Outcome {
            url: url.to_string(),
            cache: None,
            host,
            // Where the last answer came from (a redirect may have moved it).
            final_url: if last_final_url.is_empty() {
                url.to_string()
            } else {
                last_final_url
            },
            class: last
                .as_ref()
                .map(|attempt| attempt.class)
                .unwrap_or(Class::Network),
            identity: last.as_ref().map(|attempt| attempt.identity.clone()),
            status: last.as_ref().map(|attempt| attempt.status).unwrap_or(0),
            ms_total: started.elapsed().as_secs_f64() * 1000.0,
            ms_first: first_ms,
            attempts,
            title: last
                .as_ref()
                .map(|attempt| attempt.title.clone())
                .unwrap_or_default(),
            text_chars: 0,
            // The wall the last document showed: the vendor and what it asks.
            wall: last_wall,
            evidence: last_evidence,
            failures: last
                .and_then(|attempt| attempt.note)
                .map(|note| vec![note])
                .unwrap_or_default(),
            spent_state,
            transport: "document".to_string(),
            items: 0,
            sample: Vec::new(),
            sample_urls: Vec::new(),
            body_text: String::new(),
            body: options.capture_body.then(|| last_body.clone()),
            headers: last_headers,
            bytes: last_raw,
        }
    }
}

/// The request ceiling for the identity asked after a server went silent.
const STALL_RETRY_MAX: Duration = Duration::from_secs(4);

/// Transport failures one encounter spends before it ends on the last one: the
/// first client, and one of another wire class.
const TRANSPORT_FAILURES_MAX: usize = 2;

/// Whom a transport failure is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportScope {
    /// It happened before any client was shown — the name did not resolve,
    /// the proxy or the socket refused, the certificate did not verify — or
    /// the body broke a limit, so every identity meets it alike.
    Host,
    /// It happened after the client spoke (a reset stream, a handshake alert, a
    /// server gone silent): it may be about this client.
    Client,
}

/// The scope of a wire error, read from its class prefix and the transport's
/// own words (`http::describe`), which are the same on every OS.
fn transport_scope(error: &str) -> TransportScope {
    let lower = error.to_ascii_lowercase();
    let host = lower.starts_with("dns:")
        || lower.starts_with("proxy:")
        // A refused or unreachable socket: no byte of the client was sent.
        || lower.contains("tcp connect error")
        // A connect to a cleartext URL that timed out has no handshake in it to
        // be slow: the route is dead. (Over TLS it may be a slow handshake.)
        || (connect_phase(&lower) && lower.contains("timed out") && lower.contains("(http://"))
        // Trust is the host's certificate against our one store.
        || lower.contains("certificate_verify_failed")
        || lower.contains("certificate verify failed")
        || lower.contains("certificate has expired")
        || lower.contains("self signed certificate")
        || lower.contains("self-signed certificate")
        || lower.contains("hostname mismatch")
        || lower.starts_with("body: response larger than");
    if host {
        TransportScope::Host
    } else {
        TransportScope::Client
    }
}

/// The address a document navigates to on load, resolved against `base`:
/// Reddit's `shreddit-redirect` (`location.replace("<path>")`, run on init).
fn page_navigation(body: &str, base: &str) -> Option<String> {
    let at = body.find("id=\"shreddit-redirect\"")?;
    let mut end = body.len().min(at + 1024);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let window = &body[at..end];
    let call = "location.replace(";
    let rest = &window[window.find(call)? + call.len()..];
    let rest = rest
        .strip_prefix("&quot;")
        .or_else(|| rest.strip_prefix('"'))
        .or_else(|| rest.strip_prefix('\''))?;
    let target = &rest[..rest.find("&quot;").or_else(|| rest.find(['"', '\'']))?];
    let target = target.replace("&amp;", "&");
    url::Url::parse(base)
        .ok()?
        .join(&target)
        .ok()
        .filter(|next| matches!(next.scheme(), "http" | "https"))
        .map(|next| next.to_string())
}

/// Did this failure happen while connecting (TCP and TLS), before a request
/// was sent?
fn connect_phase(error: &str) -> bool {
    error.to_ascii_lowercase().contains("(connect)")
}

/// Is this body the challenge page of the vendor whose program we are about to
/// run? The page, not the header, decides: a header names the wall, only the
/// body carries something to execute.
fn is_program_page(vendor: &str, body: &str) -> bool {
    match vendor {
        "cloudflare" => crate::engine::is_cloudflare_page(body),
        "reddit" => crate::engine::is_reddit_page(body),
        _ => crate::engine::is_aws_waf_page(body),
    }
}

/// Two spellings of one URL (the wire may normalise what it was given).
fn same_url(left: &str, right: &str) -> bool {
    match (url::Url::parse(left), url::Url::parse(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// What an encounter that ends on an answer it does not judge carries along.
struct Ended {
    started: Instant,
    first_ms: f64,
    attempts: Vec<Attempt>,
    spent_state: bool,
    capture: bool,
}

/// The outcome of an encounter that ends on `response` as it is (a redirect not
/// followed, an unsolicited 304): the site's answer, not a wall and not
/// content, with nothing scored on it.
fn ended(
    url: &str,
    host: String,
    response: &crate::http::Response,
    identity: &str,
    evidence: &str,
    ended: Ended,
) -> Outcome {
    Outcome {
        evidence: vec![evidence.to_string()],
        ..answer(url, host, response, Class::HttpError, identity, ended)
    }
}

/// The outcome of an encounter that ends on `response`, as `identity` received
/// it: its address, status and timing, and its body when the caller asked for
/// it. What the answer was judged to be (title, wall, evidence) is the caller's
/// to add.
fn answer(
    url: &str,
    host: String,
    response: &crate::http::Response,
    class: Class,
    identity: &str,
    ended: Ended,
) -> Outcome {
    Outcome {
        url: url.to_string(),
        cache: None,
        host,
        final_url: response.url.clone(),
        class,
        identity: Some(identity.to_string()),
        status: response.status,
        ms_total: ended.started.elapsed().as_secs_f64() * 1000.0,
        ms_first: ended.first_ms,
        attempts: ended.attempts,
        title: String::new(),
        text_chars: 0,
        wall: None,
        evidence: Vec::new(),
        failures: Vec::new(),
        spent_state: ended.spent_state,
        transport: "document".to_string(),
        items: 0,
        sample: Vec::new(),
        sample_urls: Vec::new(),
        body_text: String::new(),
        body: ended.capture.then(|| response.body.clone()),
        headers: if ended.capture {
            response.headers.clone()
        } else {
            Vec::new()
        },
        bytes: if ended.capture {
            response.raw.clone()
        } else {
            None
        },
    }
}

/// A `Set-Cookie` as the jar keeps it.
fn stored(cookie: &crate::http::SetCookie) -> StoredCookie {
    StoredCookie {
        name: cookie.name.clone(),
        value: cookie.value.clone(),
        domain: cookie.domain.clone(),
        path: cookie.path.clone(),
        expires_unix: cookie.expires_unix,
        secure: cookie.secure,
        http_only: cookie.http_only,
    }
}

/// The longest wait an origin's own hint is taken at: past it, the hint is
/// garbled or a ban, and either way a cooldown that outlives the session is
/// not pacing.
const RETRY_HINT_MAX_MS: i64 = 3600 * 1000;

/// How long an interface asked us to wait, in ms. `Retry-After` is seconds or
/// an HTTP date (RFC 9110 §10.2.3); `X-RateLimit-Reset` is seconds to wait on
/// some APIs (Reddit) and the epoch second of the reset on others (GitHub), so
/// a value that can only be a timestamp is read as one.
fn retry_hint(headers: &[(String, String)]) -> Option<i64> {
    retry_hint_at(headers, crate::state::now_unix())
}

fn retry_hint_at(headers: &[(String, String)], now: i64) -> Option<i64> {
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if name != "x-ratelimit-reset" && name != "retry-after" {
            continue;
        }
        let value = value.trim();
        let ms = match value.parse::<f64>() {
            Ok(seconds) if !seconds.is_finite() || seconds < 0.0 => continue,
            // Past a year from the epoch it is a timestamp, not a duration.
            Ok(seconds) if seconds > 365.0 * 86_400.0 => (seconds as i64 - now) * 1000,
            Ok(seconds) => (seconds * 1000.0) as i64,
            Err(_) => match crate::cache::http_date(value) {
                Some(at) => (at - now) * 1000,
                None => continue,
            },
        };
        return Some(ms.clamp(0, RETRY_HINT_MAX_MS));
    }
    None
}

pub fn class_name(class: Class) -> &'static str {
    match class {
        Class::Content => "content",
        Class::WallState => "wall_state",
        Class::WallContract => "wall_contract",
        Class::WallIdentity => "wall_identity",
        Class::WallInteraction => "wall_interaction",
        Class::WallProgram => "wall_program",
        Class::Shell => "shell",
        Class::Incomplete => "incomplete",
        Class::Network => "network",
        Class::HttpError => "http_error",
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod transport_tests {
    use super::{transport_scope, TransportScope};

    #[test]
    fn failures_before_the_client_spoke_are_the_hosts() {
        // The transport's own words, as `http::describe` prints them (measured).
        for host in [
            "dns: error sending request for uri (https://x.invalid/): client error (Connect): dns error: failed to lookup address information: Name or service not known",
            "connect: error sending request for uri (http://127.0.0.1:9/): client error (Connect): tcp connect error: Connection refused (os error 111)",
            "tls: error sending request for uri (https://expired.badssl.com/): client error (Connect): [CERTIFICATE_VERIFY_FAILED]: [CERTIFICATE_VERIFY_FAILED]",
            "proxy: 407 Proxy Authentication Required",
            "timeout: error sending request for uri (http://example.org:81/): client error (Connect): client error (Connect): operation timed out",
            "body: response larger than 67108864 bytes",
        ] {
            assert_eq!(transport_scope(host), TransportScope::Host, "{host}");
        }
    }

    #[test]
    fn failures_after_the_client_spoke_may_be_about_it() {
        // bestbuy and united reset the transparent client's stream and serve
        // Chrome (identity matrix, 2026-11-07): another wire class is asked.
        for client in [
            "request: error sending request for uri (https://www.united.com/en/us): client error (SendRequest): connection error",
            "timeout: error sending request for uri (https://a.test/): operation timed out",
            "tls: error sending request for uri (https://a.test/): client error (Connect): [SSLV3_ALERT_HANDSHAKE_FAILURE]",
            "connect: error sending request for uri (https://a.test/): connection reset by peer",
            // A TLS connect that timed out: a dead route, or a slow handshake
            // that another (smaller) ClientHello may finish.
            "timeout: error sending request for uri (https://example.com:8443/): client error (Connect): client error (Connect): operation timed out",
        ] {
            assert_eq!(transport_scope(client), TransportScope::Client, "{client}");
        }
    }
}

#[cfg(test)]
mod retry_hint_tests {
    use super::retry_hint_at;

    fn hint(name: &str, value: &str) -> Option<i64> {
        retry_hint_at(&[(name.to_string(), value.to_string())], 1_800_000_000)
    }

    #[test]
    fn a_wait_is_read_in_every_shape_origins_send_it() {
        assert_eq!(hint("Retry-After", "120"), Some(120_000));
        assert_eq!(hint("x-ratelimit-reset", "7.5"), Some(7_500));
        // GitHub: the epoch second of the reset, 90 s from now.
        assert_eq!(hint("X-RateLimit-Reset", "1800000090"), Some(90_000));
        // RFC 9110 HTTP date, 30 s from now; one already past is no wait.
        assert_eq!(
            hint("Retry-After", "Fri, 15 Jan 2027 08:00:30 GMT"),
            Some(30_000)
        );
        assert_eq!(
            hint("Retry-After", "Thu, 01 Jan 2026 00:00:00 GMT"),
            Some(0)
        );
        // Garbled or hostile values are capped or ignored, never a lockout.
        assert_eq!(hint("Retry-After", "86400"), Some(3_600_000));
        assert_eq!(hint("Retry-After", "soon"), None);
        assert_eq!(hint("Retry-After", "-5"), None);
        assert_eq!(hint("Retry-After", "NaN"), None);
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::page_navigation;

    #[test]
    fn reddits_redirect_shell_names_its_target() {
        let body = "<div id=\"shreddit-redirect\"><ac-call nonce=\"n\" delay=\"0\" \
                    method=\"location.replace(&quot;/r/rust/comments/1xpfpc5/is_it/?a=1&amp;b=2&quot;)\" \
                    target=\"window\" trigger=\"init\"></ac-call></div>";
        assert_eq!(
            page_navigation(
                body,
                "https://www.reddit.com/r/rust/comments/1xpfpc5/?solution=x"
            )
            .as_deref(),
            Some("https://www.reddit.com/r/rust/comments/1xpfpc5/is_it/?a=1&b=2")
        );
        // A page that only mentions location.replace navigates nowhere.
        assert_eq!(
            page_navigation(
                "<script>location.replace('/elsewhere')</script>",
                "https://a.test/"
            ),
            None
        );
        // Never off the web's schemes.
        let javascript = "<div id=\"shreddit-redirect\"><ac-call method=\"location.replace(&quot;javascript:alert(1)&quot;)\"></ac-call></div>";
        assert_eq!(page_navigation(javascript, "https://a.test/"), None);
    }
}
