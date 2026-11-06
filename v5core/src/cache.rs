//! The page cache: a document fetched once is revalidated, not refetched, and
//! never served once the site has changed it.
//!
//! HTTP caching (RFC 9111) for a private client, made strict:
//!
//! * **Fresh** (no request): only inside a freshness lifetime the server itself
//!   declared (`max-age`, `Expires`), minus the response's age, and never longer
//!   than [`FRESH_CAP_SECS`], and only to the cookie it was confirmed under. No
//!   heuristic freshness: a page that says nothing is never served without asking.
//! * **Revalidate** (one small request): a conditional request, sent the way
//!   Chrome revisits a page (`If-None-Match`, `If-Modified-Since`). The server's
//!   `304` is the proof the copy is current; anything else replaces it.
//! * **Audit** (one full request): validators can lie (an `ETag` computed from a
//!   template, a `Last-Modified` from a build). A site is trusted as far as it has
//!   proven: until it has been seen to change its text *and* its validators
//!   together, every [`AUDIT_AFTER_UNPROVEN`] revalidations are followed by a full
//!   fetch; once proven, every [`AUDIT_AFTER`], and always [`AUDIT_SECS`] after the
//!   last full fetch. A site caught returning unchanged validators for changed
//!   text is not revalidated again for [`DISTRUST_SECS`]: it pays full fetches,
//!   and its pages stay correct.
//!
//! Only verified content is stored: a wall, an error or a truncated body never is.
//! Entries are keyed by identity, egress and URL, so a representation is only ever
//! reused by the client that received it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// The longest a declared freshness lifetime is trusted without asking.
pub const FRESH_CAP_SECS: i64 = 60;
/// Revalidations before a full fetch checks a proven site's validators.
pub const AUDIT_AFTER: u32 = 4;
/// The same for a site whose validators have not yet been seen to track a change.
pub const AUDIT_AFTER_UNPROVEN: u32 = 2;
/// The longest a representation is reused without a full fetch.
pub const AUDIT_SECS: i64 = 3600;
/// How long a site caught with lying validators is fetched whole.
pub const DISTRUST_SECS: i64 = 7 * 24 * 3600;
/// A `Last-Modified` this close to the response's `Date` may hide a change made in
/// the same second (RFC 9110 §8.8.2.2); it is not used as a validator.
const WEAK_DATE_SECS: i64 = 60;
/// Largest body stored.
pub const ENTRY_MAX: usize = 8 << 20;
/// Total bytes kept; the least recently used entries go first.
pub const TOTAL_MAX: usize = 256 << 20;
/// Entries kept in memory when the cache has no directory.
const MEMORY_ENTRIES: usize = 512;

/// Response headers never kept with a body: cookies (a stored page never sets
/// one again), connection-level fields, and the transfer encoding of a body the
/// cache holds decoded. Everything else is the page's own metadata and comes
/// back with it, as served.
const NEVER_KEPT: &[&str] = &[
    "set-cookie",
    "set-cookie2",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "content-encoding",
];

fn kept(name: &str) -> bool {
    !NEVER_KEPT
        .iter()
        .any(|never| name.eq_ignore_ascii_case(never))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub url: String,
    pub final_url: String,
    pub identity: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// When the server last confirmed this representation (a 200 or a 304).
    pub confirmed_unix: i64,
    /// Served without asking until then (0: always ask).
    pub fresh_until_unix: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// The request's cookie, hashed, when the response varies on it.
    pub vary_cookie: Option<u64>,
    /// The cookie this copy was last confirmed under, hashed, whatever the
    /// response's Vary says: many sites vary on a cookie without saying so, and a
    /// copy is served without asking only to the cookie it was confirmed for.
    #[serde(default)]
    pub cookie: u64,
    /// The caller's navigation context (referer and what follows from it),
    /// hashed, when the response varies on it: served only to the same context.
    #[serde(default)]
    pub vary_navigation: Option<u64>,
    /// The visible text, hashed: what an audit compares.
    pub text_hash: u64,
    /// The whole body, hashed: what a read from disk checks.
    pub body_hash: u64,
    /// Revalidations since the last full fetch.
    pub revalidations: u32,
    pub full_unix: i64,
    #[serde(skip)]
    pub body: Vec<u8>,
}

impl Entry {
    pub fn has_validators(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }

    /// The conditional headers of a revisit, in Chrome's order.
    pub fn conditionals(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        if let Some(tag) = &self.etag {
            headers.push(("if-none-match", tag.clone()));
        }
        if let Some(date) = &self.last_modified {
            headers.push(("if-modified-since", date.clone()));
        }
        headers
    }

    /// The body as the fetch path holds it: text, plus the bytes when not UTF-8.
    pub fn document(&self) -> (String, Option<Vec<u8>>) {
        match String::from_utf8(self.body.clone()) {
            Ok(text) => (text, None),
            Err(error) => (
                String::from_utf8_lossy(error.as_bytes()).into_owned(),
                Some(error.into_bytes()),
            ),
        }
    }
}

/// How far an origin's validators are believed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Caught lying: never revalidated.
    Distrusted,
    /// Not yet seen to change.
    Unproven,
    /// Seen to change its validators with its text.
    Proven,
}

/// What to do with a cached representation for this request.
#[derive(Debug)]
pub enum Plan {
    /// Serve it; no request.
    Fresh(Entry),
    /// Ask whether it changed.
    Revalidate(Entry),
    /// Fetch whole. `Some` keeps the old copy for comparison (an audit).
    Full(Option<Entry>),
}

/// The plan for `entry` now. `cookie` is the cookie the request will carry;
/// `revalidate` refuses a fresh serve (the caller wants the server asked).
pub fn plan(
    entry: Option<Entry>,
    now: i64,
    cookie: Option<&str>,
    trust: Trust,
    revalidate: bool,
) -> Plan {
    let Some(entry) = entry else {
        return Plan::Full(None);
    };
    if entry.vary_cookie.is_some()
        && entry.vary_cookie != Some(hash(cookie.unwrap_or_default().as_bytes()))
    {
        return Plan::Full(None);
    }
    // A clock that went backwards proves nothing about age.
    let sane = now >= entry.confirmed_unix;
    let same_cookie = entry.cookie == hash(cookie.unwrap_or_default().as_bytes());
    if sane && !revalidate && same_cookie && now < entry.fresh_until_unix {
        return Plan::Fresh(entry);
    }
    let audit_after = if trust == Trust::Proven {
        AUDIT_AFTER
    } else {
        AUDIT_AFTER_UNPROVEN
    };
    let audit_due =
        entry.revalidations >= audit_after || !sane || now - entry.full_unix >= AUDIT_SECS;
    if trust == Trust::Distrusted || !entry.has_validators() || audit_due {
        return Plan::Full(Some(entry));
    }
    Plan::Revalidate(entry)
}

/// How a response may be stored.
#[derive(Debug, PartialEq)]
pub struct Policy {
    pub store: bool,
    pub fresh_secs: i64,
    pub vary_cookie: bool,
}

/// RFC 9111 for a private cache, without heuristics.
pub fn policy(headers: &[(String, String)], now: i64) -> Policy {
    let field = |name: &str| -> Vec<&str> {
        headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    };
    let directives: Vec<String> = field("cache-control")
        .iter()
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    let has = |name: &str| {
        directives
            .iter()
            .any(|token| token == name || token.starts_with(&format!("{name}=")))
    };
    let vary: Vec<String> = field("vary")
        .iter()
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    if has("no-store") || vary.iter().any(|name| name == "*") {
        return Policy {
            store: false,
            fresh_secs: 0,
            vary_cookie: false,
        };
    }
    let date = field("date").first().and_then(|value| http_date(value));
    let age = field("age")
        .first()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let max_age = directives
        .iter()
        .find_map(|token| token.strip_prefix("max-age="))
        .map(|value| value.trim_matches('"').parse::<i64>().ok());
    let lifetime = match max_age {
        // A malformed max-age is stale (RFC 9111 §4.2.1).
        Some(parsed) => parsed.unwrap_or(0),
        None => match (field("expires").first(), date) {
            (Some(expires), Some(date)) => http_date(expires).map(|at| at - date).unwrap_or(0),
            _ => 0,
        },
    };
    let apparent = date.map(|date| (now - date).max(0)).unwrap_or(0);
    // HTTP/1.0's Pragma: no-cache counts when no Cache-Control speaks.
    let pragma = directives.is_empty()
        && field("pragma")
            .iter()
            .any(|value| value.to_ascii_lowercase().contains("no-cache"));
    let fresh = if has("no-cache") || pragma {
        0
    } else {
        (lifetime - age.max(apparent)).clamp(0, FRESH_CAP_SECS)
    };
    Policy {
        store: true,
        fresh_secs: fresh,
        vary_cookie: vary.iter().any(|name| name == "cookie"),
    }
}

/// The entry for a verified 200, or `None` when it may not be stored.
#[allow(clippy::too_many_arguments)]
pub fn entry(
    key: &str,
    url: &str,
    final_url: &str,
    identity: &str,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    cookie: Option<&str>,
    now: i64,
) -> Option<Entry> {
    if status != 200 || body.len() > ENTRY_MAX {
        return None;
    }
    let policy = policy(headers, now);
    if !policy.store {
        return None;
    }
    let field = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_string())
    };
    let etag = field("etag").filter(|tag| valid_etag(tag));
    let date = field("date")
        .and_then(|value| http_date(&value))
        .unwrap_or(now);
    let last_modified = field("last-modified")
        .filter(|value| http_date(value).is_some_and(|at| date - at >= WEAK_DATE_SECS));
    let kept = headers
        .iter()
        .filter(|(name, _)| kept(name))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();
    Some(Entry {
        key: key.to_string(),
        url: url.to_string(),
        final_url: final_url.to_string(),
        identity: identity.to_string(),
        status,
        headers: kept,
        confirmed_unix: now,
        fresh_until_unix: if policy.fresh_secs > 0 {
            now + policy.fresh_secs
        } else {
            0
        },
        etag,
        last_modified,
        vary_cookie: policy
            .vary_cookie
            .then(|| hash(cookie.unwrap_or_default().as_bytes())),
        cookie: hash(cookie.unwrap_or_default().as_bytes()),
        vary_navigation: None,
        text_hash: text_hash(body),
        body_hash: hash(body),
        revalidations: 0,
        full_unix: now,
        body: body.to_vec(),
    })
}

/// The request fields a caller sets per call: a response whose `Vary` names
/// one is a copy for that context only. Every other field a document request
/// carries is fixed by the identity, which is part of the key.
const NAVIGATION_FIELDS: &[&str] = &["referer", "sec-fetch-site", "origin"];

/// Whether `headers` say the representation depends on the caller's navigation
/// context (RFC 9111 §4.1).
pub fn varies_on_navigation(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("vary"))
        .flat_map(|(_, value)| value.split(','))
        .any(|name| {
            NAVIGATION_FIELDS
                .iter()
                .any(|field| name.trim().eq_ignore_ascii_case(field))
        })
}

/// A 304 confirmed `entry`: its headers take the 304's updated fields (RFC 9111
/// §3.2: every field it carries replaces the stored one, `Content-Type` and the
/// fields never kept excepted) and its freshness restarts from now.
pub fn confirmed(
    mut entry: Entry,
    headers: &[(String, String)],
    cookie: Option<&str>,
    now: i64,
) -> Entry {
    entry.cookie = hash(cookie.unwrap_or_default().as_bytes());
    let updated: Vec<String> = headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .filter(|name| kept(name) && name != "content-type")
        .collect();
    entry
        .headers
        .retain(|(known, _)| !updated.contains(&known.to_ascii_lowercase()));
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if updated.contains(&lower) {
            entry.headers.push((lower, value.clone()));
        }
    }
    let policy = policy(&entry.headers, now);
    if let Some(tag) = entry
        .headers
        .iter()
        .find(|(name, _)| name == "etag")
        .map(|(_, value)| value.trim().to_string())
    {
        if valid_etag(&tag) {
            entry.etag = Some(tag);
        }
    }
    entry.confirmed_unix = now;
    entry.fresh_until_unix = if policy.store && policy.fresh_secs > 0 {
        now + policy.fresh_secs
    } else {
        0
    };
    entry.revalidations += 1;
    entry
}

/// Does a 304 select `entry` (RFC 9111 §4.3.4)? A strong validator in the 304
/// selects only a stored strong one of the same value; a weak one selects by
/// weak comparison; a `Last-Modified` must equal the stored one; two `ETag`s
/// are no answer. A 304 with no validator selects the one stored copy.
pub fn selects(entry: &Entry, headers: &[(String, String)]) -> bool {
    let tags: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("etag"))
        .map(|(_, value)| value.trim())
        .collect();
    match tags.as_slice() {
        [] => {}
        [tag] => {
            let Some(stored) = entry.etag.as_deref() else {
                return false;
            };
            let opaque = |tag: &str| tag.strip_prefix("W/").unwrap_or(tag).to_string();
            return if tag.starts_with("W/") {
                opaque(tag) == opaque(stored)
            } else {
                !stored.starts_with("W/") && *tag == stored
            };
        }
        _ => return false,
    }
    match headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("last-modified"))
    {
        Some((_, date)) => entry.last_modified.as_deref() == Some(date.trim()),
        None => true,
    }
}

/// A full fetch that answered with the same validators as `old` but different
/// text: the site's validators do not track its content.
pub fn validators_lied(old: &Entry, new: &Entry) -> bool {
    let same_tag = old.etag.is_some() && old.etag == new.etag;
    let same_date = old.etag.is_none()
        && new.etag.is_none()
        && old.last_modified.is_some()
        && old.last_modified == new.last_modified;
    (same_tag || same_date) && old.text_hash != new.text_hash
}

/// A full fetch whose validators and text both changed from `old`: the site's
/// validators track its content.
pub fn validators_tracked(old: &Entry, new: &Entry) -> bool {
    let tag_moved = old.etag.is_some() && new.etag.is_some() && old.etag != new.etag;
    let date_moved = old.last_modified.is_some()
        && new.last_modified.is_some()
        && old.last_modified != new.last_modified;
    (tag_moved || date_moved) && old.text_hash != new.text_hash
}

/// The cache key: one client (identity and egress) and one URL.
pub fn key(identity: &str, proxy: Option<&str>, url: &str) -> String {
    match proxy {
        // The lane is named by a hash: proxy credentials are never written down.
        Some(proxy) => format!("{identity}|{:016x}|{url}", hash(proxy.as_bytes())),
        None => format!("{identity}|direct|{url}"),
    }
}

// ----- storage ---------------------------------------------------------------

/// Entries on disk (one file each, written atomically) or, without a directory,
/// in memory. Shared by every worker of a session.
pub struct PageCache {
    dir: Option<PathBuf>,
    memory: Mutex<HashMap<String, Entry>>,
    /// Disk bytes in use, counted once and then kept up to date.
    disk_bytes: Mutex<Option<u64>>,
    /// Held across a read-compare-write, so a late confirmation never lands
    /// over a version a concurrent fetch stored (or removed) meanwhile.
    writes: Mutex<()>,
}

impl PageCache {
    pub fn on_disk(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            memory: Mutex::new(HashMap::new()),
            disk_bytes: Mutex::new(None),
            writes: Mutex::new(()),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            dir: None,
            memory: Mutex::new(HashMap::new()),
            disk_bytes: Mutex::new(None),
            writes: Mutex::new(()),
        }
    }

    /// Next to the learned state when it persists, in memory otherwise.
    pub fn for_state() -> Self {
        match crate::state::state_path().and_then(|path| path.parent().map(|dir| dir.join("pages")))
        {
            Some(dir) => Self::on_disk(dir),
            None => Self::in_memory(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Entry> {
        let Some(dir) = &self.dir else {
            return lock(&self.memory).get(key).cloned();
        };
        let path = dir.join(file_name(key));
        let raw = std::fs::read(&path).ok()?;
        match decode(&raw).filter(|entry| entry.key == key) {
            Some(entry) => Some(entry),
            None => {
                // Corrupt, foreign or a hash collision: it is not this page.
                self.delete(key);
                None
            }
        }
    }

    pub fn put(&self, entry: &Entry) {
        let _writing = lock(&self.writes);
        self.store(entry);
    }

    /// Store `confirmed` (a 304's renewal of `snapshot`) only while the stored
    /// copy is still `snapshot`'s version: a confirmation that arrives after a
    /// concurrent fetch replaced the page, or found it gone, confirms nothing
    /// that is still there. Returns whether it was stored.
    pub fn put_confirmed(&self, confirmed: &Entry, snapshot: &Entry) -> bool {
        let _writing = lock(&self.writes);
        let current = self.get(&confirmed.key);
        let same_version = current.is_some_and(|current| {
            current.body_hash == snapshot.body_hash && current.full_unix == snapshot.full_unix
        });
        if same_version {
            self.store(confirmed);
        }
        same_version
    }

    fn store(&self, entry: &Entry) {
        let Some(dir) = &self.dir else {
            let mut memory = lock(&self.memory);
            if memory.len() >= MEMORY_ENTRIES && !memory.contains_key(&entry.key) {
                if let Some(oldest) = memory
                    .values()
                    .min_by_key(|known| known.confirmed_unix)
                    .map(|known| known.key.clone())
                {
                    memory.remove(&oldest);
                }
            }
            memory.insert(entry.key.clone(), entry.clone());
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = dir.join(file_name(&entry.key));
        let previous = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let encoded = encode(entry);
        if !crate::state::write_replacing(&path, &encoded) {
            return;
        }
        let total = {
            let mut bytes = lock(&self.disk_bytes);
            let current = bytes.get_or_insert_with(|| directory_bytes(dir));
            *current = current.saturating_sub(previous) + encoded.len() as u64;
            *current
        };
        if total > TOTAL_MAX as u64 {
            self.evict(dir);
        }
    }

    pub fn remove(&self, key: &str) {
        let _writing = lock(&self.writes);
        self.delete(key);
    }

    fn delete(&self, key: &str) {
        match &self.dir {
            Some(dir) => {
                let path = dir.join(file_name(key));
                let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                if std::fs::remove_file(&path).is_ok() {
                    if let Some(bytes) = lock(&self.disk_bytes).as_mut() {
                        *bytes = bytes.saturating_sub(size);
                    }
                }
            }
            None => {
                lock(&self.memory).remove(key);
            }
        }
    }

    /// Forget every page.
    pub fn clear(&self) {
        lock(&self.memory).clear();
        if let Some(dir) = &self.dir {
            for file in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if file.path().extension().is_some_and(|ext| ext == "page") {
                    let _ = std::fs::remove_file(file.path());
                }
            }
            *lock(&self.disk_bytes) = Some(0);
        }
    }

    /// Down to three quarters of the budget, least recently written first.
    fn evict(&self, dir: &PathBuf) {
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|file| {
                let meta = file.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), file.path()))
            })
            .collect();
        files.sort();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        let target = (TOTAL_MAX as u64) * 3 / 4;
        for (_, size, path) in files {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= size;
            }
        }
        *lock(&self.disk_bytes) = Some(total);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn directory_bytes(dir: &PathBuf) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|file| file.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

fn file_name(key: &str) -> String {
    format!("{:016x}.page", hash(key.as_bytes()))
}

/// One line of JSON metadata, then the body's bytes.
fn encode(entry: &Entry) -> Vec<u8> {
    let mut out = serde_json::to_vec(entry).unwrap_or_default();
    out.push(b'\n');
    out.extend_from_slice(&entry.body);
    out
}

fn decode(raw: &[u8]) -> Option<Entry> {
    let split = raw.iter().position(|byte| *byte == b'\n')?;
    let mut entry: Entry = serde_json::from_slice(&raw[..split]).ok()?;
    entry.body = raw[split + 1..].to_vec();
    (hash(&entry.body) == entry.body_hash).then_some(entry)
}

// ----- helpers ---------------------------------------------------------------

/// FNV-1a, 64-bit: stable across builds and platforms, unlike `std`'s hasher.
pub fn hash(bytes: &[u8]) -> u64 {
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        state ^= *byte as u64;
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state
}

/// The visible text, hashed: scripts, styles, tags and whitespace removed, so a
/// per-request nonce in an attribute or a script does not count as a change.
pub fn text_hash(body: &[u8]) -> u64 {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_ascii_lowercase();
    let mut visible = String::with_capacity(text.len() / 2);
    let mut cursor = 0;
    let bytes = text.as_bytes();
    while cursor < bytes.len() {
        if bytes[cursor] == b'<' {
            let rest = &lower[cursor..];
            let skip_to = ["script", "style", "noscript", "template"]
                .iter()
                .find(|tag| rest[1..].starts_with(*tag))
                .and_then(|tag| rest.find(&format!("</{tag}")).map(|end| cursor + end));
            let from = skip_to.unwrap_or(cursor);
            cursor = lower[from..]
                .find('>')
                .map(|end| from + end + 1)
                .unwrap_or(bytes.len());
            visible.push(' ');
        } else {
            let next = lower[cursor..]
                .find('<')
                .map(|at| cursor + at)
                .unwrap_or(bytes.len());
            visible.push_str(&text[cursor..next]);
            cursor = next;
        }
    }
    let normalized: Vec<&str> = visible.split_whitespace().collect();
    hash(normalized.join(" ").as_bytes())
}

fn valid_etag(tag: &str) -> bool {
    let opaque = tag.strip_prefix("W/").unwrap_or(tag);
    opaque.len() >= 2
        && opaque.starts_with('"')
        && opaque.ends_with('"')
        && !opaque[1..opaque.len() - 1].contains('"')
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) as Unix seconds. The two
/// obsolete formats are not accepted: a date that does not parse is no date.
pub fn http_date(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts.as_slice() else {
        return None;
    };
    if *zone != "GMT" {
        return None;
    }
    let day: i64 = day.parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| name == month)? as i64
        + 1;
    let year: i64 = year.parse().ok()?;
    let clock: Vec<i64> = time
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    let [hour, minute, second] = clock.as_slice() else {
        return None;
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    const NOW: i64 = 1_793_000_000;

    fn date(at: i64) -> String {
        // Inverse of `http_date` for the tests' own instants.
        let days = at.div_euclid(86_400);
        let seconds = at.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
        let names = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        format!(
            "Thu, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
            names[month as usize - 1],
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    }

    #[test]
    fn http_dates_round_trip() {
        assert_eq!(
            http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(http_date(&date(NOW)), Some(NOW));
        assert_eq!(http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
    }

    #[test]
    fn freshness_is_only_what_the_server_declared_and_capped() {
        let now_date = date(NOW);
        assert_eq!(
            policy(&headers(&[("date", &now_date)]), NOW).fresh_secs,
            0,
            "no heuristic freshness"
        );
        assert_eq!(
            policy(&headers(&[("cache-control", "public, max-age=30")]), NOW).fresh_secs,
            30
        );
        assert_eq!(
            policy(&headers(&[("cache-control", "max-age=86400")]), NOW).fresh_secs,
            FRESH_CAP_SECS
        );
        assert_eq!(
            policy(
                &headers(&[("cache-control", "max-age=30"), ("age", "25")]),
                NOW
            )
            .fresh_secs,
            5
        );
        assert_eq!(
            policy(&headers(&[("cache-control", "max-age=30, no-cache")]), NOW).fresh_secs,
            0
        );
        assert_eq!(
            policy(&headers(&[("cache-control", "max-age=oops")]), NOW).fresh_secs,
            0
        );
        let expires = date(NOW + 20);
        assert_eq!(
            policy(&headers(&[("date", &now_date), ("expires", &expires)]), NOW).fresh_secs,
            20
        );
        assert_eq!(
            policy(&headers(&[("date", &now_date), ("expires", "0")]), NOW).fresh_secs,
            0
        );
        assert!(!policy(&headers(&[("cache-control", "no-store")]), NOW).store);
        assert!(!policy(&headers(&[("vary", "Accept-Encoding, *")]), NOW).store);
        assert!(policy(&headers(&[("vary", "Cookie")]), NOW).vary_cookie);
        assert_eq!(
            policy(
                &headers(&[
                    ("pragma", "no-cache"),
                    ("expires", &expires),
                    ("date", &now_date)
                ]),
                NOW
            )
            .fresh_secs,
            0
        );
        assert_eq!(
            policy(
                &headers(&[("pragma", "no-cache"), ("cache-control", "max-age=30")]),
                NOW
            )
            .fresh_secs,
            30,
            "Cache-Control speaks first"
        );
    }

    #[test]
    fn a_last_modified_too_close_to_its_date_is_not_a_validator() {
        let now_date = date(NOW);
        let recent = date(NOW - 5);
        let old = date(NOW - 3600);
        let made = |modified: &str| {
            entry(
                "k",
                "u",
                "u",
                "chrome",
                200,
                &headers(&[("date", &now_date), ("last-modified", modified)]),
                b"x",
                None,
                NOW,
            )
            .unwrap()
        };
        assert_eq!(made(&recent).last_modified, None);
        assert_eq!(made(&old).last_modified.as_deref(), Some(old.as_str()));
    }

    #[test]
    fn only_a_full_200_within_the_size_cap_is_stored() {
        let plain = headers(&[("etag", "\"a\"")]);
        assert!(entry("k", "u", "u", "chrome", 203, &plain, b"x", None, NOW).is_none());
        assert!(entry(
            "k",
            "u",
            "u",
            "chrome",
            200,
            &plain,
            &vec![b'x'; ENTRY_MAX + 1],
            None,
            NOW
        )
        .is_none());
        assert_eq!(
            entry(
                "k",
                "u",
                "u",
                "chrome",
                200,
                &headers(&[("etag", "unquoted")]),
                b"x",
                None,
                NOW
            )
            .unwrap()
            .etag,
            None
        );
        assert_eq!(
            entry(
                "k",
                "u",
                "u",
                "chrome",
                200,
                &headers(&[("etag", "W/\"a\"")]),
                b"x",
                None,
                NOW
            )
            .unwrap()
            .etag
            .as_deref(),
            Some("W/\"a\"")
        );
    }

    #[test]
    fn the_plan_follows_freshness_validators_trust_and_audits() {
        let fresh = entry(
            "k",
            "u",
            "u",
            "chrome",
            200,
            &headers(&[("cache-control", "max-age=30"), ("etag", "\"a\"")]),
            b"x",
            None,
            NOW,
        )
        .unwrap();
        assert!(matches!(
            plan(Some(fresh.clone()), NOW + 10, None, Trust::Proven, false),
            Plan::Fresh(_)
        ));
        assert!(
            matches!(
                plan(Some(fresh.clone()), NOW + 10, None, Trust::Proven, true),
                Plan::Revalidate(_)
            ),
            "asked to revalidate"
        );
        assert!(matches!(
            plan(Some(fresh.clone()), NOW + 31, None, Trust::Proven, false),
            Plan::Revalidate(_)
        ));
        assert!(
            matches!(
                plan(
                    Some(fresh.clone()),
                    NOW + 31,
                    None,
                    Trust::Distrusted,
                    false
                ),
                Plan::Full(Some(_))
            ),
            "a lying site pays full fetches"
        );
        assert!(
            matches!(
                plan(Some(fresh.clone()), NOW - 100, None, Trust::Proven, false),
                Plan::Full(Some(_))
            ),
            "a clock that went back"
        );
        assert!(matches!(
            plan(
                Some(fresh.clone()),
                NOW + AUDIT_SECS,
                None,
                Trust::Proven,
                false
            ),
            Plan::Full(Some(_))
        ));
        let mut worn = fresh.clone();
        worn.revalidations = AUDIT_AFTER;
        assert!(matches!(
            plan(Some(worn.clone()), NOW + 31, None, Trust::Proven, false),
            Plan::Full(Some(_))
        ));
        worn.revalidations = AUDIT_AFTER_UNPROVEN;
        assert!(
            matches!(
                plan(Some(worn.clone()), NOW + 31, None, Trust::Unproven, false),
                Plan::Full(Some(_))
            ),
            "an unproven site is audited sooner"
        );
        assert!(matches!(
            plan(Some(worn), NOW + 31, None, Trust::Proven, false),
            Plan::Revalidate(_)
        ));
        let bare = entry("k", "u", "u", "chrome", 200, &[], b"x", None, NOW).unwrap();
        assert!(
            matches!(
                plan(Some(bare), NOW + 1, None, Trust::Proven, false),
                Plan::Full(Some(_))
            ),
            "nothing to revalidate with"
        );
        assert!(matches!(
            plan(None, NOW, None, Trust::Proven, false),
            Plan::Full(None)
        ));
    }

    #[test]
    fn a_response_that_varies_on_cookie_is_reused_only_with_that_cookie() {
        let varied = entry(
            "k",
            "u",
            "u",
            "chrome",
            200,
            &headers(&[("vary", "Cookie"), ("etag", "\"a\"")]),
            b"x",
            Some("s=1"),
            NOW,
        )
        .unwrap();
        assert!(matches!(
            plan(
                Some(varied.clone()),
                NOW + 1,
                Some("s=1"),
                Trust::Proven,
                false
            ),
            Plan::Revalidate(_)
        ));
        assert!(matches!(
            plan(Some(varied), NOW + 1, Some("s=2"), Trust::Proven, false),
            Plan::Full(None)
        ));
    }

    #[test]
    fn a_fresh_copy_is_served_without_asking_only_under_the_cookie_it_was_confirmed_for() {
        // No Vary: Cookie, yet a login or a region cookie changes what many sites send.
        let stored = entry(
            "k",
            "u",
            "u",
            "chrome",
            200,
            &headers(&[("cache-control", "max-age=30"), ("etag", "\"a\"")]),
            b"x",
            Some("s=1"),
            NOW,
        )
        .unwrap();
        assert!(matches!(
            plan(
                Some(stored.clone()),
                NOW + 1,
                Some("s=1"),
                Trust::Proven,
                false
            ),
            Plan::Fresh(_)
        ));
        assert!(matches!(
            plan(
                Some(stored.clone()),
                NOW + 1,
                Some("s=2"),
                Trust::Proven,
                false
            ),
            Plan::Revalidate(_)
        ));
        assert!(matches!(
            plan(Some(stored.clone()), NOW + 1, None, Trust::Proven, false),
            Plan::Revalidate(_)
        ));
        // Confirmed under the new cookie, it is that cookie's copy.
        let reconfirmed = confirmed(
            stored,
            &headers(&[("cache-control", "max-age=30")]),
            Some("s=2"),
            NOW + 2,
        );
        assert!(matches!(
            plan(
                Some(reconfirmed),
                NOW + 3,
                Some("s=2"),
                Trust::Proven,
                false
            ),
            Plan::Fresh(_)
        ));
    }

    #[test]
    fn a_304_restarts_freshness_and_takes_new_validators() {
        let stored = entry(
            "k",
            "u",
            "u",
            "chrome",
            200,
            &headers(&[("etag", "\"a\""), ("content-type", "text/html")]),
            b"x",
            None,
            NOW,
        )
        .unwrap();
        let updated = confirmed(
            stored,
            &headers(&[
                ("etag", "\"b\""),
                ("cache-control", "max-age=10"),
                ("set-cookie", "x=1"),
                ("content-type", "text/plain"),
            ]),
            None,
            NOW + 100,
        );
        assert_eq!(updated.etag.as_deref(), Some("\"b\""));
        assert_eq!(updated.fresh_until_unix, NOW + 110);
        assert_eq!(updated.revalidations, 1);
        assert!(updated.headers.iter().all(|(name, _)| name != "set-cookie"));
        assert!(
            updated
                .headers
                .contains(&("content-type".into(), "text/html".into())),
            "a 304 does not change the body's type"
        );
    }

    #[test]
    fn unchanged_validators_over_changed_text_are_caught() {
        let made = |body: &[u8], tag: &str| {
            entry(
                "k",
                "u",
                "u",
                "chrome",
                200,
                &headers(&[("etag", tag)]),
                body,
                None,
                NOW,
            )
            .unwrap()
        };
        let old = made(b"<p>Price 10</p><script>n=1</script>", "\"a\"");
        assert!(
            !validators_lied(&old, &made(b"<p>Price 10</p><script>n=2</script>", "\"a\"")),
            "a script nonce is not content"
        );
        assert!(validators_lied(&old, &made(b"<p>Price 12</p>", "\"a\"")));
        assert!(
            !validators_lied(&old, &made(b"<p>Price 12</p>", "\"b\"")),
            "a new tag is the server telling the truth"
        );
        assert!(validators_tracked(&old, &made(b"<p>Price 12</p>", "\"b\"")));
        assert!(
            !validators_tracked(&old, &made(b"<p>Price 10</p>", "\"b\"")),
            "a new tag over the same text proves nothing"
        );
    }

    #[test]
    fn disk_entries_round_trip_and_corruption_is_a_miss() {
        let dir = std::env::temp_dir().join(format!("v5cache-{}-{}", std::process::id(), NOW));
        let cache = PageCache::on_disk(dir.clone());
        let stored = entry(
            "chrome|direct|https://a.test/",
            "https://a.test/",
            "https://a.test/",
            "chrome",
            200,
            &headers(&[("etag", "\"a\"")]),
            "héllo".as_bytes(),
            None,
            NOW,
        )
        .unwrap();
        cache.put(&stored);
        assert_eq!(cache.get(&stored.key), Some(stored.clone()));
        assert_eq!(
            cache.get(&stored.key).unwrap().document(),
            ("héllo".to_string(), None)
        );
        let path = dir.join(file_name(&stored.key));
        let mut raw = std::fs::read(&path).unwrap();
        raw.truncate(raw.len() - 2);
        std::fs::write(&path, raw).unwrap();
        assert_eq!(cache.get(&stored.key), None, "a torn body is not the page");
        assert!(!path.exists());
        cache.put(&stored);
        cache.clear();
        assert_eq!(cache.get(&stored.key), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_separate_clients_and_never_hold_proxy_credentials() {
        assert_ne!(
            key("chrome", None, "https://a.test/"),
            key("firefox", None, "https://a.test/")
        );
        let proxied = key(
            "chrome",
            Some("http://user:secret@p.test:8080"),
            "https://a.test/",
        );
        assert!(!proxied.contains("secret"));
        assert_ne!(proxied, key("chrome", None, "https://a.test/"));
    }
}
