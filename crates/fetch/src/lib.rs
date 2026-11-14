//! The fetch layer as a library: carried identity and state, and a challenge
//! engine that runs the site's own program in-process (V8) when a wall demands it.
//! No browser is linked and none is launched.
//!
//! The entry point is [`Session`]: `Session::fetch(&Request) -> Outcome`. It is
//! `Send + Sync` and blocking; an async caller runs it under
//! `tokio::task::spawn_blocking`. Concurrent calls each get a [`fetch::Worker`]
//! (its own clients and a snapshot of the learned state) and fold what they
//! learned back into the session's state, which is written to disk once per
//! call when the session persists.

mod ca_blocklist;
pub mod cache;
mod challenge;
pub mod classify;
pub mod contract;
mod datapath;
pub mod engine;
pub mod envelope;
pub mod fetch;
mod http;
pub mod identity;
mod mine;
pub mod state;
mod webcrypto;

#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod content_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod fixture_tests;
#[cfg(test)]
mod redirect_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod revalidation_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod testkit;

pub use classify::{Class, Wall};
pub use contract::Contract;
pub use fetch::{Attempt, Outcome};
pub use http::{
    add_trust_pem, default_hop, proxy_url, Cancel, Hop, HopGuard, Kind, Policy, RequestStart,
    ResponseHead, CANCELLED, DOCUMENT_BODY_MAX, REDIRECTS_MAX,
};

use std::sync::Mutex;
use std::time::Duration;

/// One URL to fetch and the policy for it.
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    /// Force one identity by name (`identity::names()`); `None` lets the session
    /// choose from what this origin and its vendor have accepted before.
    pub identity: Option<String>,
    /// Egress proxy URL (`http://user:pass@host:port`, `socks5://...`); the
    /// challenge engine leaves through it too. `None` is a direct connection,
    /// whatever proxy the environment names.
    pub proxy: Option<String>,
    /// Spend and earn learned state (scores, cookies, credentials). Off is a
    /// cold probe: nothing is read from or written to the session's state.
    pub learn: bool,
    /// Wall-clock budget for the encounter, challenge included.
    pub deadline: Duration,
    /// What the answer must contain; `None` accepts any verified document.
    pub contract: Option<Contract>,
    /// Keep the final response's body, headers and bytes on the outcome.
    pub keep_body: bool,
    /// Minimum spacing between two requests to one origin.
    pub pace: Duration,
    /// Use the page cache: a fresh page is served without a request, a stale
    /// one is revalidated. Ignored on a cold probe (`learn: false`).
    pub cache: bool,
    /// Ask the server even when the cached page is fresh.
    pub revalidate: bool,
    /// Redirects (followed or not; every hop asked of the caller's guard before
    /// it is dialed) and the headers a navigation carries besides the identity's
    /// own (a link's `referer` and `sec-fetch-site`).
    pub policy: Policy,
    /// The explicit private-egress opt-out for the engine's network guard.
    /// Production passes `false`; a deliberate private read passes `true`.
    pub allow_private: bool,
}

impl Request {
    /// A learning, direct fetch that keeps the body.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            identity: None,
            proxy: None,
            learn: true,
            deadline: Duration::from_millis(15_000),
            contract: None,
            keep_body: true,
            pace: Duration::from_millis(900),
            cache: true,
            revalidate: false,
            policy: Policy::default(),
            allow_private: false,
        }
    }
}

/// One request exactly as described: no identity search, no page cache, no
/// engine. For a resource the caller already knows how to ask for (an API, a
/// search page, a file), where the answer is the reply, whatever it is.
#[derive(Clone, Debug)]
pub struct Once {
    pub url: String,
    /// The identity to present (`identity::names()`); `None` is `chrome-local`,
    /// the reference browser's own.
    pub identity: Option<String>,
    pub proxy: Option<String>,
    /// A page load or a page's own request (an API call, a script, an image):
    /// they carry different fetch metadata, and a subresource sends none of a
    /// navigation's own headers.
    pub kind: Kind,
    /// Sent over the identity's own headers (a subresource's fetch metadata, a
    /// conditional, a referer). On `plain`, a `user-agent` here is the client.
    pub headers: Vec<(String, String)>,
    /// Spend and keep this identity's cookies for the URL's host.
    pub cookies: bool,
    /// The whole request, redirects included.
    pub timeout: Duration,
    /// Refuse a body larger than this (default: [`DOCUMENT_BODY_MAX`]).
    pub cap: Option<usize>,
    pub policy: Policy,
}

impl Once {
    /// A GET navigation as `chrome-local`, no cookies, direct, 15 s.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            identity: None,
            proxy: None,
            kind: Kind::Navigate,
            headers: Vec::new(),
            cookies: false,
            timeout: Duration::from_secs(15),
            cap: None,
            policy: Policy {
                request_max: Duration::MAX,
                ..Policy::default()
            },
        }
    }
}

/// What one request came back with.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    /// The URL that answered (after the redirects followed).
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub bytes: Vec<u8>,
    pub identity: &'static str,
    pub version: String,
    pub elapsed_ms: f64,
}

/// The state file as this session last read or wrote it, and which version of
/// the file that was.
#[derive(Default)]
struct Disk {
    base: state::State,
    stamp: Option<state::FileStamp>,
}

/// A fetching session: learned state and the page cache shared by every call,
/// workers pooled.
pub struct Session {
    state: Mutex<state::State>,
    /// The state file as last read or written: what a save merges from, so two
    /// processes on one file keep each other's learning.
    disk: Mutex<Disk>,
    idle: Mutex<Vec<fetch::Worker>>,
    cache: std::sync::Arc<cache::PageCache>,
    /// The state file, when the session persists.
    path: Option<std::path::PathBuf>,
}

impl Session {
    /// `persist` keeps state at the default path ([`state::state_path`]): loaded
    /// now, written after every learning call. Otherwise state and page cache
    /// live in memory, for the session's life.
    pub fn new(persist: bool) -> Self {
        match persist.then(state::state_path).flatten() {
            Some(path) => Self::at(path),
            None => Self::build(None),
        }
    }

    /// A persisting session whose state is the file `path`; its page cache is
    /// the `pages` directory beside it. Processes sharing one path merge what
    /// they learn.
    pub fn at(path: impl Into<std::path::PathBuf>) -> Self {
        Self::build(Some(path.into()))
    }

    fn build(path: Option<std::path::PathBuf>) -> Self {
        let state = path
            .as_deref()
            .map(state::State::load_from)
            .unwrap_or_default();
        let cache = match path.as_deref().and_then(std::path::Path::parent) {
            Some(dir) => cache::PageCache::on_disk(dir.join("pages")),
            None => cache::PageCache::in_memory(),
        };
        let stamp = path.as_deref().and_then(state::FileStamp::of);
        Self {
            disk: Mutex::new(Disk {
                base: state.clone(),
                stamp,
            }),
            state: Mutex::new(state),
            idle: Mutex::new(Vec::new()),
            cache: std::sync::Arc::new(cache),
            path,
        }
    }

    /// The page cache this session reads and writes.
    pub fn cache(&self) -> &cache::PageCache {
        &self.cache
    }

    /// The cached copy of `url` as `identity` through `proxy` stores it, if any.
    pub fn cache_entry(
        &self,
        identity: &str,
        proxy: Option<&str>,
        url: &str,
    ) -> Option<cache::Entry> {
        self.cache.get(&cache::key(identity, proxy, url))
    }

    /// Forget every cached page (learned state is kept).
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Fetch one URL. Blocking: call it from a thread, or `spawn_blocking`.
    pub fn fetch(&self, request: &Request) -> Outcome {
        let mut options = fetch::Options {
            capture_body: request.keep_body,
            deadline: request.deadline,
            contract: request.contract.clone(),
            persist: request.learn,
            identity: request.identity.clone(),
            proxy: request.proxy.clone(),
            pace_ms: request.pace.as_millis() as i64,
            cache: request.cache,
            revalidate: request.revalidate,
            policy: request.policy.clone(),
            allow_private: request.allow_private,
            ..fetch::Options::default()
        };
        // The engine shares the encounter's deadline: a mint never outlives it.
        options.engine.wall = options.engine.wall.min(request.deadline);

        let mut worker = lock(&self.idle).pop().unwrap_or_else(|| {
            fetch::Worker::learning(state::State::default(), std::sync::Arc::clone(&self.cache))
        });
        let before = if request.learn {
            lock(&self.state).clone()
        } else {
            state::State::default()
        };
        worker.swap_state(before.clone());
        let outcome = worker.fetch(&request.url, &options);
        let after = worker.swap_state(state::State::default());
        if request.learn {
            self.learn(&before, &after);
        }
        lock(&self.idle).push(worker);
        outcome
    }

    /// One request as described by `once`. Blocking. With `once.cookies`, the
    /// identity's cookies for the host are sent and what the reply sets is kept
    /// (and written, when the session persists). Errors are one line whose
    /// prefix names the class: `timeout:`, `dns:`, `tls:`, `proxy:`, `connect:`,
    /// `body:`, `request:`, `redirect:`, `denied:`, `identity:`.
    pub fn once(&self, once: &Once) -> Result<Reply, String> {
        let mut worker = lock(&self.idle).pop().unwrap_or_else(|| {
            fetch::Worker::learning(state::State::default(), std::sync::Arc::clone(&self.cache))
        });
        let before = if once.cookies {
            lock(&self.state).clone()
        } else {
            state::State::default()
        };
        worker.swap_state(before.clone());
        let reply = worker.once(once);
        let after = worker.swap_state(state::State::default());
        if once.cookies {
            self.learn(&before, &after);
        }
        lock(&self.idle).push(worker);
        reply
    }

    /// Fold what one call learned (`before` → `after`) into the shared state,
    /// and write it when the session persists. Saves are taken in turn (the
    /// disk lock, always before the state's), but the file is written outside
    /// the shared state's lock, so other calls start while one saves; what the
    /// merge brings back (another process's learning, pruning) is folded in
    /// after, over whatever they learned meanwhile.
    fn learn(&self, before: &state::State, after: &state::State) {
        let Some(path) = &self.path else {
            lock(&self.state).absorb(before, after);
            return;
        };
        let mut disk = lock(&self.disk);
        let snapshot = {
            let mut shared = lock(&self.state);
            shared.absorb(before, after);
            shared.clone()
        };
        let (merged, stamp) = snapshot.save_merged_known(&disk.base, path, disk.stamp);
        lock(&self.state).absorb(&snapshot, &merged);
        *disk = Disk {
            base: merged,
            stamp,
        };
    }

    /// The names of the cookies `identity` holds for `host` (unexpired). A
    /// caller reads whether a session-shaped cookie exists, never its value.
    pub fn cookie_names(&self, host: &str, identity: &str) -> Vec<String> {
        lock(&self.state)
            .spendable(&host.to_ascii_lowercase(), identity)
            .into_iter()
            .map(|cookie| cookie.name)
            .collect()
    }

    /// A copy of what the session has learned so far.
    pub fn state(&self) -> state::State {
        lock(&self.state).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
