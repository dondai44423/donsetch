//! P1 · the wire: browser-grade transport + ONE cookie jar shared by the
//! runtime and every request. The client runs on a dedicated worker thread
//! with its own tokio runtime; the JS bridge calls it synchronously.

use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc as tokio_mpsc;

use crate::engine::floor_char_boundary;
use crate::state::{now_secs, to_unix};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReqKind {
    Document,
    Script,
    Xhr,
    Image,
}

#[derive(Clone, Debug)]
pub struct WireReq {
    pub method: String,
    pub url: String,
    pub kind: ReqKind,
    pub extra_headers: Vec<(String, String)>,
    pub body: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WireResp {
    pub status: u16,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub ms: u128,
}

struct Job {
    req: WireReq,
    reply: std_mpsc::Sender<Result<WireResp, String>>,
}

// ── the one jar ──────────────────────────────────────────────────────────

/// One cookie with the lifetime state a persistent session needs. Storage is
/// flat: identity = name+domain (Path/Secure/SameSite are not modeled — the
/// documented tier-1 approximation), and every cookie the wire keeps is a
/// cookie the runtime sees (one jar, no quarantine).
#[derive(Clone, Debug)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    /// unix seconds; `None` = session cookie (no Max-Age/Expires given)
    pub expires: Option<u64>,
    pub created: u64,
    pub updated: u64,
}

#[derive(Default, Debug)]
pub struct Jar {
    pub cookies: Vec<Cookie>,
    /// Persistence (phase 2 · WARM SESSIONS): `None` = in-memory only
    /// (DONCORE_JAR=off, or no usable state root).
    pub store: Option<crate::state::JarStore>,
    /// (name, domain) deleted this run — tombstones for the merge-on-save,
    /// so a deletion survives even though the cookie is gone from memory.
    deleted: Vec<(String, String)>,
    dirty: bool,
    last_persist: Option<Instant>,
    revision: u64,
}

/// Cookie domain match (approximates RFC 6265): a cookie keyed to `dom` is
/// sent to `dom` itself and to subdomains of it. Host-only cookies are keyed
/// to their exact host and (v1 approximation) also reach that host's
/// subdomains.
fn dom_match(dom: &str, host: &str) -> bool {
    dom.is_empty() || dom == host || host.ends_with(&format!(".{dom}"))
}

/// True when a host is an IP literal (v4/v6; brackets tolerated).
fn is_ip_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.parse::<std::net::IpAddr>().is_ok()
}

/// Chrome's cap on cookie lifetime (400 days) and on one name+value pair
/// (bytes); the newest 1000 cookies are what a save keeps.
const MAX_COOKIE_LIFE: u64 = 400 * 86_400;
const MAX_PAIR_BYTES: usize = 4096;
const MAX_STORED_COOKIES: usize = 1000;

struct ParsedCookie {
    name: String,
    value: String,
    domain: String,
    delete: bool,
    expires: Option<u64>,
}

/// One Set-Cookie / document.cookie string → a cookie operation (upsert or
/// delete), or None when the cookie must be ignored. RFC 6265 §5.3 shape:
/// Max-Age wins over Expires; <= 0 or an already-past Expires deletes; a
/// Domain attribute must domain-match the sender (a bare TLD-shaped value is
/// refused); an oversized pair is dropped.
fn parse_cookie(sc: &str, host: &str) -> Option<ParsedCookie> {
    let mut parts = sc.split(';').map(str::trim);
    let first = parts.next().unwrap_or("");
    let (n, v) = first.split_once('=')?;
    let name = n.trim().to_string();
    let value = v.trim().to_string();
    if name.is_empty() || name.len() + value.len() > MAX_PAIR_BYTES {
        return None;
    }
    let now = now_secs();
    let mut dom = host.to_string();
    let mut max_age: Option<i64> = None;
    let mut expires_at: Option<u64> = None;
    for p in parts {
        let Some((k, val)) = p.split_once('=') else {
            continue;
        };
        let val = val.trim();
        if k.trim().eq_ignore_ascii_case("domain") {
            let d = val.trim_start_matches('.').to_ascii_lowercase();
            if d.is_empty() {
                continue;
            }
            // RFC 6265 §5.1.3: suffix matching applies to HOST NAMES only —
            // for an IP-literal host the Domain attribute must equal the host
            // exactly (Chrome rejects `Domain=0.1` on 127.0.0.1).
            if is_ip_host(host) {
                if d != host {
                    return None;
                }
            } else if !dom_match(&d, host) || (!d.contains('.') && host.contains('.')) {
                return None;
            }
            dom = d;
        } else if k.trim().eq_ignore_ascii_case("max-age") {
            max_age = val.parse::<i64>().ok();
        } else if k.trim().eq_ignore_ascii_case("expires") {
            expires_at = httpdate::parse_http_date(val).ok().map(to_unix);
        }
    }
    let mut delete = false;
    let expires = if let Some(ma) = max_age {
        if ma <= 0 {
            delete = true;
            None
        } else {
            Some(now.saturating_add((ma as u64).min(MAX_COOKIE_LIFE)))
        }
    } else if let Some(e) = expires_at {
        if e <= now {
            delete = true;
            None
        } else {
            Some(e.min(now.saturating_add(MAX_COOKIE_LIFE)))
        }
    } else {
        None
    };
    Some(ParsedCookie {
        name,
        value,
        domain: dom,
        delete,
        expires,
    })
}

impl Jar {
    /// Set-Cookie headers from a response: parse, apply, persist (the store
    /// debounces — a burst of sets is one write).
    pub fn store_response_cookies(&mut self, url: &str, set_cookies: &[String]) {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        for sc in set_cookies {
            if let Some(p) = parse_cookie(sc, &host) {
                if std::env::var("DONCORE_DEBUG").is_ok() && !p.delete {
                    eprintln!("[jar] {host} <- domain={} {}={}", p.domain, p.name, p.value);
                }
                self.apply(p);
            }
        }
    }

    /// `document.cookie` setter: one cookie string, the same grammar.
    /// (Attributes used to be discarded — a JS-set cookie with an expiry must
    /// not become immortal, and one without attributes stays a session
    /// cookie.)
    pub fn set_js_cookie(&mut self, v: &str, host: &str) {
        if let Some(p) = parse_cookie(v, host) {
            self.apply(p);
        }
    }

    fn apply(&mut self, p: ParsedCookie) {
        let now = now_secs();
        self.cookies
            .retain(|c| c.expires.map(|e| e > now).unwrap_or(true));
        if p.delete {
            self.cookies
                .retain(|c| !(c.name == p.name && c.domain == p.domain));
            self.deleted.push((p.name, p.domain));
        } else if let Some(c) = self
            .cookies
            .iter_mut()
            .find(|c| c.name == p.name && c.domain == p.domain)
        {
            let created = c.created;
            *c = Cookie {
                name: p.name,
                value: p.value,
                domain: p.domain,
                expires: p.expires,
                created,
                updated: now,
            };
        } else {
            self.cookies.push(Cookie {
                name: p.name,
                value: p.value,
                domain: p.domain,
                expires: p.expires,
                created: now,
                updated: now,
            });
        }
        self.revision += 1;
        self.dirty = true;
        self.persist_if_dirty(false);
    }

    /// Attaches the disk store and loads the previous session's cookies
    /// (dead entries are dropped; when the disk held any, the jar is marked
    /// dirty so the next save rewrites a cleaned file).
    pub fn attach_store(&mut self, store: crate::state::JarStore) {
        let (loaded, dropped) = store.read_cookies();
        let before = loaded.len();
        for c in loaded {
            if !self
                .cookies
                .iter()
                .any(|x| x.name == c.name && x.domain == c.domain)
            {
                self.cookies.push(c);
            }
        }
        cap_cookies(&mut self.cookies);
        if dropped > 0 || self.cookies.len() != before {
            self.dirty = true;
        }
        self.store = Some(store);
    }

    /// Writes the jar when dirty; `force` bypasses the debounce (process
    /// exit). A failed write keeps dirty + tombstones so the next attempt
    /// retries — a deletion must never be lost to a bad disk moment.
    pub fn persist_if_dirty(&mut self, force: bool) {
        if self.store.is_none() || !self.dirty {
            return;
        }
        if !force {
            if let Some(t) = self.last_persist {
                if t.elapsed().as_millis() < 300 {
                    return;
                }
            }
        }
        let now = now_secs();
        self.cookies
            .retain(|c| c.expires.map(|e| e > now).unwrap_or(true));
        cap_cookies(&mut self.cookies);
        let written = match &self.store {
            Some(store) => store.write_cookies(&self.cookies, &self.deleted).is_ok(),
            None => false,
        };
        if written {
            self.deleted.clear();
            self.dirty = false;
        }
        self.last_persist = Some(Instant::now());
    }

    pub fn get(&self, name: &str) -> Option<String> {
        let now = now_secs();
        self.cookies
            .iter()
            .rev()
            .find(|c| c.name == name && c.expires.map(|e| e > now).unwrap_or(true))
            .map(|c| c.value.clone())
    }

    pub fn all(&self, host: Option<&str>) -> Vec<(String, String)> {
        let now = now_secs();
        self.cookies
            .iter()
            .filter(|c| c.expires.map(|e| e > now).unwrap_or(true))
            .filter(|c| match host {
                None => true,
                Some(h) => dom_match(&c.domain, h),
            })
            .map(|c| (c.name.clone(), c.value.clone()))
            .collect()
    }

    pub fn header_for(&self, url: &str) -> Option<String> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        let pairs: Vec<String> = self
            .all(Some(&host))
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Monotonic cookie-change counter (add/update/delete) — the engine's
    /// bounce trigger, value-aware (a warm clearance refresh counts even when
    /// the cookie count is unchanged).
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// Keeps the newest MAX_STORED_COOKIES (by last update).
fn cap_cookies(cookies: &mut Vec<Cookie>) {
    if cookies.len() > MAX_STORED_COOKIES {
        cookies.sort_by(|a, b| b.updated.cmp(&a.updated));
        cookies.truncate(MAX_STORED_COOKIES);
    }
}

// ── the wire ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Wire {
    tx: tokio_mpsc::UnboundedSender<Job>,
    pub jar: Arc<Mutex<Jar>>,
}

/// Chrome-matching persona constants (154 = the version installed on this
/// machine; branded "Chromium" exactly as the local build reports).
pub const UA_154: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36";
pub const SEC_CH_UA_154: &str = "\"Not A(Brand\";v=\"99\", \"Chromium\";v=\"154\"";

/// Brings a wreq-util emulation up to the Chrome 154 wire reality:
/// exact header values, the missing Chrome headers, and Chrome's output
/// order. TLS/H2 settings stay from the preset (no newer preset exists).
pub fn tune_emulation(mut emu: wreq::Emulation) -> wreq::Emulation {
    use wreq::header::{HeaderValue, OrigHeaderMap};
    emu.headers.insert("user-agent", HeaderValue::from_static(UA_154));
    emu.headers.insert("sec-ch-ua", HeaderValue::from_static(SEC_CH_UA_154));
    emu.headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
    emu.headers.insert("sec-ch-ua-platform", HeaderValue::from_static("\"Linux\""));
    emu.headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
    emu.headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
    emu.headers.insert("accept-encoding", HeaderValue::from_static("gzip, deflate, br, zstd"));
    emu.headers.insert("priority", HeaderValue::from_static("u=0, i"));
    let mut orig = OrigHeaderMap::new();
    for name in [
        "sec-ch-ua",
        "sec-ch-ua-mobile",
        "sec-ch-ua-platform",
        "upgrade-insecure-requests",
        "user-agent",
        "accept",
        "sec-fetch-site",
        "sec-fetch-mode",
        "sec-fetch-user",
        "sec-fetch-dest",
        "accept-encoding",
        "accept-language",
        "priority",
    ] {
        orig.insert(name);
    }
    emu.orig_headers = orig;
    emu
}

impl Wire {
    /// Spawns the worker thread. `emulation` is the tuned wreq emulation.
    pub fn new(
        emulation: wreq::Emulation,
        resolve: Vec<(String, std::net::SocketAddr)>,
    ) -> Self {
        let jar = Arc::new(Mutex::new(Jar::default()));
        let (tx, mut rx) = tokio_mpsc::unbounded_channel::<Job>();
        let jar_worker = Arc::clone(&jar);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio worker runtime");
            let mut builder = wreq::Client::builder()
                .emulation(emulation)
                .redirect(wreq::redirect::Policy::limited(10))
                // Unreachable/blackholed hosts must fail at the structural
                // pace, not the OS SYN pace: the 25s total stays the outer
                // bound, connect (TCP+TLS) gets 3s — draw #2 burned 25s in
                // connect on xbabe/adups/kompoz2; every observed live wire
                // completed in <1.3s.
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(25));
            for (host, addr) in resolve {
                builder = builder.resolve(host, addr);
            }
            let client = Arc::new(builder.build().expect("wreq client"));
            rt.block_on(async move {
                // Browser-faithful job concurrency: a page's scripts fetch in
                // parallel (≈6 per host is the browser default); the serial
                // worker queued every script RTT back-to-back, which was the
                // whole cost of the script-heavy rows (twitch/stockx/
                // huntington were init-bound in sweep #5). Single jobs behave
                // exactly as before.
                let sem = Arc::new(tokio::sync::Semaphore::new(6));
                while let Some(job) = rx.recv().await {
                    let client = Arc::clone(&client);
                    let jar = Arc::clone(&jar_worker);
                    let sem = Arc::clone(&sem);
                    tokio::spawn(async move {
                        let _permit = sem.acquire_owned().await.ok();
                        let resp = do_request(&client, &jar, &job.req).await;
                        let _ = job.reply.send(resp);
                    });
                }
            });
        });
        Self { tx, jar }
    }

    /// Issue several requests at once — all enter the queue immediately and
    /// the worker runs them with browser-like concurrency — then collect the
    /// replies in order. Used for a document's initial script batch: a
    /// browser overlaps those fetches; serial per-script RTTs were the whole
    /// cost of the script-heavy rows.
    pub fn fetch_many(&self, reqs: Vec<WireReq>) -> Vec<Result<WireResp, String>> {
        let mut waiting: Vec<Option<std_mpsc::Receiver<Result<WireResp, String>>>> =
            Vec::with_capacity(reqs.len());
        for req in reqs {
            let (rtx, rrx) = std_mpsc::channel();
            if self.tx.send(Job { req, reply: rtx }).is_ok() {
                waiting.push(Some(rrx));
            } else {
                waiting.push(None);
            }
        }
        waiting
            .into_iter()
            .map(|r| -> Result<WireResp, String> {
                match r {
                    Some(rrx) => rrx
                        .recv()
                        .map_err(|_| "wire worker dropped the job".to_string())?,
                    None => Err("wire worker gone".to_string()),
                }
            })
            .collect()
    }

    /// Blocking fetch (called from the JS thread or the engine).
    pub fn fetch(&self, req: WireReq) -> Result<WireResp, String> {
        let (rtx, rrx) = std_mpsc::channel();
        self.tx
            .send(Job { req, reply: rtx })
            .map_err(|_| "wire worker gone".to_string())?;
        rrx.recv().map_err(|_| "wire worker dropped the job".to_string())?
    }
}

async fn do_request(
    client: &wreq::Client,
    jar: &Arc<Mutex<Jar>>,
    req: &WireReq,
) -> Result<WireResp, String> {
    let t0 = Instant::now();
    let mut builder = client.request(
        req.method
            .parse()
            .map_err(|_| format!("bad method {}", req.method))?,
        &req.url,
    );
    match req.kind {
        ReqKind::Document => {}
        ReqKind::Script => {
            builder = builder
                .header("Sec-Fetch-Dest", "script")
                .header("Accept", "*/*");
        }
        ReqKind::Xhr => {
            builder = builder.header("Sec-Fetch-Dest", "empty").header("Accept", "*/*");
        }
        ReqKind::Image => {
            builder = builder
                .header("Sec-Fetch-Dest", "image")
                .header("Accept", "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8");
        }
    }
    for (k, v) in &req.extra_headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let cookie = {
        let jar = jar.lock().unwrap_or_else(|p| p.into_inner());
        jar.header_for(&req.url)
    };
    if std::env::var("DONCORE_DEBUG").is_ok() {
        let t_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        eprintln!(
            "[wire] t={t_ms} {} {} cookie={:?}",
            req.method,
            &req.url[..floor_char_boundary(&req.url, req.url.len().min(90))],
            cookie.as_deref().map(|c| &c[floor_char_boundary(c, c.len().saturating_sub(60))..]).unwrap_or("-")
        );
    }
    if let Some(c) = cookie {
        builder = builder.header("Cookie", c);
    }
    if let Some(body) = &req.body {
        builder = builder.body(body.clone());
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status().as_u16();
    let final_url = resp.uri().to_string();
    if std::env::var("DONCORE_DEBUG").is_ok() {
        eprintln!(
            "[wire] <- {status} blen={} {}",
            req.body.as_ref().map(|b| b.len()).unwrap_or(0),
            &final_url[..floor_char_boundary(&final_url, final_url.len().min(110))]
        );
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut set_cookies: Vec<String> = Vec::new();
    for (name, value) in resp.headers().iter() {
        let v = value.to_str().unwrap_or("").to_string();
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            set_cookies.push(v.clone());
        }
        headers.push((name.as_str().to_string(), v));
    }
    {
        let mut jar = jar.lock().unwrap_or_else(|p| p.into_inner());
        jar.store_response_cookies(&final_url, &set_cookies);
    }
    let body = resp
        .text()
        .await
        .map_err(|e| format!("body read: {e}"))?;
    if std::env::var("DONCORE_DEBUG").is_ok() {
        // Wire forensics (DEBUG only): request bodies and non-2xx response
        // bodies land in scratch/ — enough to read what a rejected
        // challenge submission actually carried (g2/DataDome, 2026-10-18).
        let t_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = std::fs::create_dir_all("scratch");
        if let Some(b) = &req.body {
            if !b.is_empty() {
                let _ = std::fs::write(format!("scratch/wire-req-{t_ms}.txt"), b);
            }
        }
        if !body.is_empty() && (status >= 400 || req.method.eq_ignore_ascii_case("POST")) {
            let _ = std::fs::write(format!("scratch/wire-resp-{t_ms}.txt"), &body);
        }
    }
    Ok(WireResp {
        status,
        final_url,
        headers,
        body,
        ms: t0.elapsed().as_millis(),
    })
}
