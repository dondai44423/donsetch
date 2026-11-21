//! A download that survives a throttled or stalled route: what `donsetch -u`
//! fetches its release tarball with.
//!
//! GitHub's release-asset CDN sometimes serves one connection at a few tens of
//! KB/s for hours while every other host is fast. A single request then spins
//! until its timeout. This downloader watches the rate instead: a route that
//! moves fewer than [`Policy::min_per_window`] bytes in a window is abandoned
//! (keeping what it delivered), and the next route resumes from that byte with
//! a `Range` request. Routes are tried in this order:
//!
//!   1. the release download URL, directly (or through the configured proxy);
//!   2. the same URL through each HTTP proxy lane in `[proxy] pool`;
//!   3. the GitHub API's asset URL (a different front door), the same way.
//!
//! If every route was too slow, a second, patient pass takes whichever route is
//! left and finishes the job at whatever speed it has. The caller still checks
//! the SHA-256, so a resume that spliced two different files cannot install.

use std::time::{Duration, Instant};

/// How a route is judged and how long the whole job may take.
#[derive(Clone, Debug)]
pub(crate) struct Policy {
    /// Connection setup and redirects are not judged for this long.
    pub warmup: Duration,
    /// The rate is judged once per window.
    pub window: Duration,
    /// Fewer bytes than this in a window and the route is too slow.
    pub min_per_window: u64,
    /// A patient pass gives up on a route after this long without data.
    pub stall: Duration,
    /// Everything, all routes, all passes.
    pub overall: Duration,
    pub connect: Duration,
}

impl Policy {
    /// 64 KiB/s is the floor: a 40 MB tarball at that rate takes ten minutes,
    /// and every other route measured on the line that prompted this was ten
    /// to a hundred times faster.
    pub(crate) fn standard() -> Self {
        Policy {
            warmup: Duration::from_secs(4),
            window: Duration::from_secs(5),
            min_per_window: 5 * 64 * 1024,
            stall: Duration::from_secs(30),
            overall: Duration::from_secs(20 * 60),
            connect: Duration::from_secs(15),
        }
    }
}

/// Where the bytes come from.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// A plain URL.
    Url { label: String, url: String },
    /// A release asset resolved through the GitHub API at attempt time, so a
    /// healthy primary route never pays for the lookup.
    GithubAsset {
        repo: String,
        tag: String,
        name: String,
    },
}

impl Source {
    fn label(&self) -> &str {
        match self {
            Source::Url { label, .. } => label,
            Source::GithubAsset { .. } => "GitHub API asset",
        }
    }

    async fn resolve(
        &self,
        client: &reqwest::Client,
    ) -> Result<(String, Option<&'static str>), String> {
        match self {
            Source::Url { url, .. } => Ok((url.clone(), None)),
            Source::GithubAsset { repo, tag, name } => {
                let api = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
                let body: serde_json::Value = client
                    .get(&api)
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await
                    .map_err(|e| format!("release lookup: {e}"))?
                    .error_for_status()
                    .map_err(|e| format!("release lookup: {e}"))?
                    .json()
                    .await
                    .map_err(|e| format!("release lookup: {e}"))?;
                let url = body["assets"]
                    .as_array()
                    .and_then(|assets| assets.iter().find(|a| a["name"] == name.as_str()))
                    .and_then(|a| a["url"].as_str())
                    .ok_or_else(|| format!("release lookup: no asset named {name}"))?;
                Ok((url.to_string(), Some("application/octet-stream")))
            }
        }
    }
}

/// How a request leaves this machine.
#[derive(Clone, Debug)]
pub(crate) struct Route {
    pub label: String,
    /// `None` is the ambient route: the environment's proxies, if the config
    /// allows them, otherwise direct.
    pub proxy: Option<String>,
    /// Whether the ambient route may read proxy variables from the environment.
    pub from_environment: bool,
}

/// The routes this install is configured for: its explicit or ambient route
/// first, then every HTTP proxy lane in the pool. SOCKS lanes are left to the
/// fetch layer, which owns that transport.
pub(crate) fn configured_routes() -> Vec<Route> {
    let cfg = crate::config::cfg();
    let p = &cfg.proxy;
    let explicit = [p.https.as_str(), p.all.as_str()]
        .into_iter()
        .find(|s| !s.trim().is_empty());
    let mut routes = vec![match explicit {
        Some(url) => Route {
            label: "configured proxy".into(),
            proxy: Some(url.to_string()),
            from_environment: false,
        },
        None => Route {
            label: "direct".into(),
            proxy: None,
            from_environment: p.from_environment,
        },
    }];
    for entry in &p.pool {
        if let Ok(proxy) = crate::egress::proxy::Proxy::parse(entry)
            && proxy.is_http_connect()
        {
            routes.push(Route {
                label: format!("proxy lane {}", proxy.id()),
                proxy: Some(proxy.to_url()),
                from_environment: false,
            });
        }
    }
    routes
}

fn client_for(route: &Route, policy: &Policy) -> Result<reqwest::Client, String> {
    // Force HTTP/1.1 with TCP_NODELAY so the 8 parallel range workers open 8
    // independent TCP connections instead of multiplexing onto one HTTP/2
    // socket with hyper's 64 KiB default stream window (which caps throughput
    // at ~250 KB/s on high-RTT links).
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("donsetch-update/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(policy.connect)
        .http1_only()
        .tcp_nodelay(true)
        .pool_max_idle_per_host(PARALLEL_CHUNKS + 2)
        .redirect(reqwest::redirect::Policy::limited(8));
    builder = match &route.proxy {
        Some(url) => builder.proxy(reqwest::Proxy::all(url).map_err(|e| format!("proxy: {e}"))?),
        None if route.from_environment => builder,
        None => builder.no_proxy(),
    };
    builder.build().map_err(|e| e.to_string())
}

/// When GitHub's release redirect points to `release-assets.githubusercontent.com`
/// (Fastly proxy), its `jwt` query token names the underlying Azure Blob host
/// (`"path": "releaseassetproduction.blob.core.windows.net"`) alongside the
/// signed SAS query string. On ISPs with congested Fastly peering, fetching
/// directly from that official Azure Blob origin can be 10–30× faster while
/// carrying the exact same signed asset (still SHA-256 verified by the caller).
fn direct_azure_blob_url(resolved: &str) -> Option<String> {
    use base64::Engine as _;
    let parsed = url::Url::parse(resolved).ok()?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("release-assets.githubusercontent.com")
    {
        return None;
    }
    let jwt = parsed
        .query_pairs()
        .find_map(|(k, v)| (k == "jwt").then(|| v.into_owned()))?;
    let payload_b64 = jwt.split('.').nth(1)?.trim_end_matches('=');
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let blob_host = payload.get("path")?.as_str()?;
    // Strict allowlist: only official Azure Blob hostnames, never arbitrary hosts.
    if !blob_host.ends_with(".blob.core.windows.net")
        || blob_host.len() <= ".blob.core.windows.net".len()
        || !blob_host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    {
        return None;
    }
    let mut direct = parsed;
    direct.set_host(Some(blob_host)).ok()?;
    Some(direct.into())
}

fn no_redirect_client_for(route: &Route, policy: &Policy) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("donsetch-update/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(policy.connect)
        .http1_only()
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none());
    builder = match &route.proxy {
        Some(url) => builder.proxy(reqwest::Proxy::all(url).map_err(|e| format!("proxy: {e}"))?),
        None if route.from_environment => builder,
        None => builder.no_proxy(),
    };
    builder.build().map_err(|e| e.to_string())
}

/// Follow up to a few 3xx hops on `github.com` / `api.github.com` without ever
/// connecting to `release-assets.githubusercontent.com` first, so we get the
/// signed redirect target in ~500 ms instead of waiting for a throttled Fastly
/// handshake before racing the direct Azure Blob origin.
async fn peek_github_redirect(
    route: &Route,
    policy: &Policy,
    url: &str,
    accept: Option<&str>,
) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "github.com" && host != "api.github.com" {
        return None;
    }
    let client = no_redirect_client_for(route, policy).ok()?;
    let mut current = url.to_string();
    let timeout = policy.connect.min(Duration::from_secs(8));
    for _ in 0..4 {
        let mut req = client.head(&current);
        if let Some(accept) = accept {
            req = req.header("Accept", accept);
        }
        let resp = tokio::time::timeout(timeout, req.send()).await.ok()?.ok()?;
        if !resp.status().is_redirection() {
            return None;
        }
        let loc = resp.headers().get("location")?.to_str().ok()?;
        let next = url::Url::parse(&current).ok()?.join(loc).ok()?;
        let next_str = next.to_string();
        if direct_azure_blob_url(&next_str).is_some() {
            return Some(next_str);
        }
        current = next_str;
    }
    None
}

async fn fetch_probe_slice(
    client: &reqwest::Client,
    url: &str,
    accept: Option<&str>,
    from: u64,
    to_inclusive: u64,
    timeout: Duration,
) -> Result<(Vec<u8>, Option<u64>), String> {
    let want = (to_inclusive - from + 1) as usize;
    let mut req = client
        .get(url)
        .header("Range", format!("bytes={from}-{to_inclusive}"));
    if let Some(accept) = accept {
        req = req.header("Accept", accept);
    }
    let mut resp = tokio::time::timeout(timeout, req.send())
        .await
        .map_err(|_| "probe timeout".to_string())?
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if status != 206 {
        return Err(format!("probe HTTP {status}"));
    }
    let total = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(total_from_content_range);
    let expected = total
        .map(|t| (t.saturating_sub(from) as usize).min(want))
        .unwrap_or(want);
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected {
        match tokio::time::timeout(timeout, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let take = chunk.len().min(expected - out.len());
                out.extend_from_slice(&chunk[..take]);
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err("probe stall".into()),
        }
    }
    if out.len() == expected {
        Ok((out, total))
    } else {
        Err(format!("short probe: {} of {expected} bytes", out.len()))
    }
}

/// Race a small 256 KiB ranged probe across `primary` (Fastly CDN) and `alt`
/// (direct Azure Blob origin). Whichever completes the slice first wins both
/// the route, the total asset length, and the already-fetched bytes so nothing
/// is wasted.
async fn race_fastest_origin(
    client: &reqwest::Client,
    primary: &str,
    alt: &str,
    accept: Option<&str>,
    start: u64,
    total_hint: Option<u64>,
    timeout: Duration,
) -> (String, Option<(Vec<u8>, Option<u64>)>) {
    let probe_end = match total_hint {
        Some(total) => (start + 256 * 1024).min(total).saturating_sub(1),
        None => start + 256 * 1024 - 1,
    };
    let f_primary = Box::pin(fetch_probe_slice(
        client, primary, accept, start, probe_end, timeout,
    ));
    let f_alt = Box::pin(fetch_probe_slice(
        client, alt, accept, start, probe_end, timeout,
    ));
    match futures_util::future::select(f_primary, f_alt).await {
        futures_util::future::Either::Left((Ok(res), _)) => (primary.to_string(), Some(res)),
        futures_util::future::Either::Right((Ok(res), _)) => (alt.to_string(), Some(res)),
        futures_util::future::Either::Left((Err(_), other)) => match other.await {
            Ok(res) => (alt.to_string(), Some(res)),
            Err(_) => (primary.to_string(), None),
        },
        futures_util::future::Either::Right((Err(_), other)) => match other.await {
            Ok(res) => (primary.to_string(), Some(res)),
            Err(_) => (primary.to_string(), None),
        },
    }
}

enum Attempt {
    Done,
    /// Delivered bytes, but under the floor.
    TooSlow(u64),
}

/// Parse the total size out of `Content-Range: bytes 100-199/200`.
fn total_from_content_range(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// Connections per asset. A single stream from the release CDN is throttled to
/// tens of KB/s on some networks (measured: 4-28 KB/s while the same file came
/// down at 200+ KB/s over sixteen connections); parallel ranges are the
/// difference between a 25-minute upgrade and a three-minute one.
pub(crate) const PARALLEL_CHUNKS: usize = 8;
/// Assets below this go down the streaming path: range setup would cost more
/// than it saves.
const PARALLEL_FLOOR: u64 = 2 * 1024 * 1024;

/// Live download telemetry passed to UI progress consumers.
pub struct Progress<'a> {
    pub bytes: usize,
    pub total: Option<u64>,
    pub lanes: Option<[f32; PARALLEL_CHUNKS]>,
    pub route: &'a str,
}

enum Parallel {
    Done,
    /// The route does not do ranges (or the asset is small): stream it instead.
    Unsupported,
    /// Delivered bytes, but under the floor.
    TooSlow(u64),
}

/// One closed range, resumed inside the segment on a stall, writing arrived
/// bytes incrementally into `slot` so an abandoned route still leaves a
/// contiguous resumable prefix in segment order.
async fn fetch_segment(
    client: &reqwest::Client,
    url: &str,
    accept: Option<&str>,
    range: (u64, u64),
    stall: Duration,
    slot: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    counters: (
        std::sync::Arc<std::sync::atomic::AtomicU64>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    ),
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let (delivered, lane_got) = counters;
    let (from, to) = range;
    let want = (to.saturating_sub(from)) as usize;
    let mut got = 0usize;
    let mut tries = 0;
    while got < want {
        tries += 1;
        if tries > 4 {
            return Err(format!("range {from}-{}: gave up at {got} bytes", to - 1));
        }
        let at = from + got as u64;
        let mut request = client
            .get(url)
            .header("Range", format!("bytes={at}-{}", to - 1));
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }
        let mut response = request.send().await.map_err(|e| e.to_string())?;
        if response.status().as_u16() != 206 {
            return Err(format!(
                "range {from}-{}: HTTP {}",
                to - 1,
                response.status().as_u16()
            ));
        }
        while let Ok(Ok(Some(chunk))) = tokio::time::timeout(stall, response.chunk()).await {
            let take = chunk.len().min(want - got);
            if take > 0 {
                slot.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(&chunk[..take]);
                got += take;
                lane_got.fetch_add(take as u64, Ordering::Relaxed);
                delivered.fetch_add(take as u64, Ordering::Relaxed);
            }
            if got >= want {
                break;
            }
        }
    }
    Ok(())
}

/// The ranged path. Ask for everything from `buf.len()` on, split what is missing
/// across connections, monitor aggregate throughput per `policy.window`, and
/// append only the contiguous completed prefix so `buf` stays resumable for the
/// next route.
#[allow(clippy::too_many_arguments)]
async fn attempt_parallel(
    client: &reqwest::Client,
    route: &Route,
    url: &str,
    accept: Option<&str>,
    route_label: &str,
    buf: &mut Vec<u8>,
    policy: &Policy,
    patient: bool,
    progress: &mut impl FnMut(Progress<'_>),
) -> Result<Parallel, String> {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    let mut start = buf.len() as u64;

    // Fast path for GitHub releases: read the 302 Location from github.com
    // without touching Fastly first, then race Fastly vs the signed Azure Blob
    // origin for the first 256 KiB (and total length) in one round trip.
    let (active_url, total) = if let Some(fastly_url) =
        peek_github_redirect(route, policy, url, accept).await
        && let Some(azure_url) = direct_azure_blob_url(&fastly_url)
    {
        let probe_timeout = policy.connect.min(Duration::from_secs(10));
        let (winner, prefetched) = race_fastest_origin(
            client,
            &fastly_url,
            &azure_url,
            None,
            start,
            None,
            probe_timeout,
        )
        .await;
        if let Some((bytes, Some(total))) = prefetched {
            buf.extend_from_slice(&bytes);
            start = buf.len() as u64;
            progress(Progress {
                bytes: buf.len(),
                total: Some(total),
                lanes: None,
                route: route_label,
            });
            if start >= total {
                return Ok(Parallel::Done);
            }
            (winner, total)
        } else {
            return Ok(Parallel::Unsupported);
        }
    } else {
        // Generic path: 1-byte ranged probe resolves redirects once and reads
        // Content-Range without triggering a full-file transfer we would drop.
        let mut probe = client
            .get(url)
            .header("Range", format!("bytes={start}-{start}"));
        if let Some(accept) = accept {
            probe = probe.header("Accept", accept);
        }
        let response = probe.send().await.map_err(|e| e.to_string())?;
        let resolved_url = response.url().to_string();
        let total = match response.status().as_u16() {
            206 => response
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(total_from_content_range),
            // A server that ignores the Range answers 200: stream it instead.
            200 => None,
            other => return Err(format!("HTTP {other}")),
        };
        drop(response);
        let Some(total) = total else {
            return Ok(Parallel::Unsupported);
        };
        if total <= start {
            return Ok(Parallel::Done);
        }
        if start == 0 && total - start < PARALLEL_FLOOR {
            return Ok(Parallel::Unsupported);
        }
        (resolved_url, total)
    };

    let remaining = total - start;
    // Split large assets into ~2.5–5 MiB chunks across 8 persistent HTTP/1.1
    // workers: large enough for TCP cwnd ramp-up, small enough for work stealing.
    let chunk = if remaining >= 8 * 1024 * 1024 {
        (remaining / 16).clamp(2 * 1024 * 1024, 5 * 1024 * 1024)
    } else {
        (remaining / PARALLEL_CHUNKS as u64).max(256 * 1024)
    };
    let mut segments = Vec::new();
    let mut at = start;
    while at < total {
        let end = (at + chunk).min(total);
        segments.push((at, end));
        at = end;
    }
    let segments = Arc::new(segments);
    let begun = Instant::now();
    let delivered = Arc::new(AtomicU64::new(0));
    let slots: Arc<Vec<Arc<Mutex<Vec<u8>>>>> = Arc::new(
        segments
            .iter()
            .map(|&(from, to)| Arc::new(Mutex::new(Vec::with_capacity((to - from) as usize))))
            .collect(),
    );
    let worker_count = PARALLEL_CHUNKS.min(segments.len());
    let share = (remaining / worker_count.max(1) as u64).max(1);
    let lane_bytes: Vec<(Arc<AtomicU64>, u64)> = (0..worker_count)
        .map(|_| (Arc::new(AtomicU64::new(0)), share))
        .collect();
    let next_chunk = Arc::new(AtomicUsize::new(0));
    let mut join_set = tokio::task::JoinSet::new();
    for (worker_got_cell, _) in &lane_bytes {
        let client = client.clone();
        let url = active_url.clone();
        let accept = accept.map(str::to_string);
        let stall = policy.stall;
        let delivered = Arc::clone(&delivered);
        let worker_got = Arc::clone(worker_got_cell);
        let segments = Arc::clone(&segments);
        let slots = Arc::clone(&slots);
        let next_chunk = Arc::clone(&next_chunk);
        join_set.spawn(async move {
            let mut failed: Option<(usize, String)> = None;
            loop {
                let idx = next_chunk.fetch_add(1, Ordering::Relaxed);
                if idx >= segments.len() {
                    break;
                }
                let range = segments[idx];
                let slot = Arc::clone(&slots[idx]);
                let res = fetch_segment(
                    &client,
                    &url,
                    accept.as_deref(),
                    range,
                    stall,
                    slot,
                    (Arc::clone(&delivered), Arc::clone(&worker_got)),
                )
                .await;
                if let Err(err) = res {
                    failed = Some((idx, err));
                    break;
                }
            }
            failed
        });
    }

    let snapshot_lanes = |lane_bytes: &[(Arc<AtomicU64>, u64)]| -> [f32; PARALLEL_CHUNKS] {
        let mut out = [1.0f32; PARALLEL_CHUNKS];
        for (i, slot) in out.iter_mut().enumerate() {
            if let Some((got, want)) = lane_bytes.get(i) {
                *slot = (got.load(Ordering::Relaxed) as f32 / *want as f32).clamp(0.0, 1.0);
            } else if !lane_bytes.is_empty() {
                *slot = 1.0;
            } else {
                *slot = 0.0;
            }
        }
        out
    };

    let mut errors: Vec<Option<String>> = vec![None; segments.len()];
    let mut too_slow_rate: Option<u64> = None;
    let mut timed_out = false;
    let mut window_start = begun + policy.warmup;
    let mut window_base = 0u64;
    // Wake every ~60ms for smooth live UI telemetry while checking throughput
    // windows against `window_start + policy.window`.
    let tick_step = Duration::from_millis(60).min(policy.window.max(Duration::from_millis(25)));

    while !join_set.is_empty() {
        let wait = if !patient && begun.elapsed() < policy.warmup {
            (policy.warmup - begun.elapsed()).min(tick_step)
        } else {
            tick_step
        };
        tokio::select! {
            joined = join_set.join_next() => {
                match joined {
                    Some(Ok(Some((idx, err)))) => errors[idx] = Some(err),
                    Some(Ok(None)) => {}
                    Some(Err(err)) => {
                        if !err.is_cancelled() {
                            errors[0].get_or_insert_with(|| err.to_string());
                        }
                    }
                    None => break,
                }
            }
            () = tokio::time::sleep(wait) => {}
        }

        let now_bytes = delivered.load(Ordering::Relaxed);
        progress(Progress {
            bytes: start as usize + now_bytes as usize,
            total: Some(total),
            lanes: Some(snapshot_lanes(&lane_bytes)),
            route: route_label,
        });

        if begun.elapsed() > policy.overall {
            timed_out = true;
            join_set.abort_all();
            break;
        }
        if !patient && Instant::now() >= window_start {
            let elapsed_win = Instant::now().saturating_duration_since(window_start);
            if elapsed_win >= policy.window {
                let gained = now_bytes.saturating_sub(window_base);
                if gained < policy.min_per_window {
                    let secs = elapsed_win.as_secs_f64().max(0.001);
                    too_slow_rate = Some((gained as f64 / secs) as u64);
                    join_set.abort_all();
                    break;
                }
                window_start = Instant::now();
                window_base = now_bytes;
            }
        }
    }

    let mut appended = 0usize;
    let mut failure: Option<String> = None;
    for (idx, &(from, to)) in segments.iter().enumerate() {
        let want = (to.saturating_sub(from)) as usize;
        let bytes = slots[idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !bytes.is_empty() {
            appended += bytes.len();
            buf.extend_from_slice(&bytes);
        }
        if bytes.len() < want {
            if let Some(err) = errors[idx].take() {
                failure.get_or_insert(err);
            }
            break;
        }
    }
    progress(Progress {
        bytes: buf.len(),
        total: Some(total),
        lanes: Some(snapshot_lanes(&lane_bytes)),
        route: route_label,
    });

    if buf.len() as u64 == total {
        return Ok(Parallel::Done);
    }
    if let Some(rate) = too_slow_rate {
        return Ok(Parallel::TooSlow(rate));
    }
    if timed_out {
        return Err(format!(
            "ranged download exceeded {}s at {} of {total} bytes",
            policy.overall.as_secs(),
            buf.len()
        ));
    }
    let seconds = begun.elapsed().as_secs_f64().max(0.001);
    let rate = (appended as f64 / seconds) as u64;
    let floor = (policy.min_per_window as f64 / policy.window.as_secs_f64().max(0.001)) as u64;
    if !patient && rate < floor {
        return Ok(Parallel::TooSlow(rate));
    }
    Err(failure
        .unwrap_or_else(|| format!("ranged download ended at {} of {total} bytes", buf.len())))
}

async fn attempt(
    client: &reqwest::Client,
    route: &Route,
    source: &Source,
    buf: &mut Vec<u8>,
    policy: &Policy,
    patient: bool,
    progress: &mut impl FnMut(Progress<'_>),
) -> Result<Attempt, String> {
    let route_label = route.label.as_str();
    let (url, accept) = source.resolve(client).await?;
    // Parallel ranges first: a single stream is throttled per connection on some
    // networks, and that is the shape that made upgrades crawl.
    match attempt_parallel(
        client,
        route,
        &url,
        accept,
        route_label,
        buf,
        policy,
        patient,
        progress,
    )
    .await?
    {
        Parallel::Done => return Ok(Attempt::Done),
        Parallel::TooSlow(rate) => return Ok(Attempt::TooSlow(rate)),
        Parallel::Unsupported => {}
    }
    let mut request = client.get(&url);
    if let Some(accept) = accept {
        request = request.header("Accept", accept);
    }
    let resumed_from = buf.len() as u64;
    if resumed_from > 0 {
        request = request.header("Range", format!("bytes={resumed_from}-"));
    }
    let mut response = request.send().await.map_err(|e| e.to_string())?;
    let total = match response.status().as_u16() {
        206 => response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(total_from_content_range),
        200 => {
            // The server ignored the Range (or this is the first request).
            buf.clear();
            response.content_length()
        }
        status => return Err(format!("HTTP {status}")),
    };

    let started = Instant::now();
    let mut window: Option<(Instant, u64)> = None;
    // A route being judged gets one window to produce anything at all.
    let wait = if patient {
        policy.stall
    } else {
        policy.window * 2
    };
    loop {
        let next = match tokio::time::timeout(wait, response.chunk()).await {
            Ok(next) => next.map_err(|e| e.to_string())?,
            Err(_) if patient => {
                return Err(format!("no data for {}s", policy.stall.as_secs()));
            }
            Err(_) => return Ok(Attempt::TooSlow(0)),
        };
        let Some(chunk) = next else { break };
        buf.extend_from_slice(&chunk);
        progress(Progress {
            bytes: buf.len(),
            total,
            lanes: None,
            route: route_label,
        });
        if patient || started.elapsed() < policy.warmup {
            continue;
        }
        let (since, bytes) = window.get_or_insert((Instant::now(), 0));
        *bytes += chunk.len() as u64;
        if since.elapsed() >= policy.window {
            if *bytes < policy.min_per_window {
                let rate = *bytes / since.elapsed().as_secs().max(1);
                return Ok(Attempt::TooSlow(rate));
            }
            window = None;
        }
    }
    match total {
        Some(total) if buf.len() as u64 != total => {
            Err(format!("the body ended at {} of {total} bytes", buf.len()))
        }
        _ => Ok(Attempt::Done),
    }
}

/// Download from the first route that can finish it, resuming across routes.
/// `note` hears each route that was abandoned and why.
pub(crate) async fn download(
    sources: &[Source],
    routes: &[Route],
    policy: &Policy,
    mut progress: impl FnMut(Progress<'_>),
    mut note: impl FnMut(&str),
) -> Result<Vec<u8>, String> {
    let begun = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for patient in [false, true] {
        for source in sources {
            for route in routes {
                if begun.elapsed() > policy.overall {
                    return Err(format!(
                        "gave up after {}s: {}",
                        policy.overall.as_secs(),
                        failures.join("; ")
                    ));
                }
                let what = format!("{} via {}", source.label(), route.label);
                let client = match client_for(route, policy) {
                    Ok(c) => c,
                    Err(e) => {
                        failures.push(format!("{what}: {e}"));
                        continue;
                    }
                };
                match attempt(
                    &client,
                    route,
                    source,
                    &mut buf,
                    policy,
                    patient,
                    &mut progress,
                )
                .await
                {
                    Ok(Attempt::Done) => return Ok(buf),
                    Ok(Attempt::TooSlow(rate)) => {
                        note(&format!(
                            "{what} is too slow ({} KB/s); {} KB kept, trying another route",
                            rate / 1024,
                            buf.len() / 1024
                        ));
                        failures.push(format!("{what}: too slow"));
                    }
                    Err(e) => {
                        note(&format!("{what} failed ({e}); trying another route"));
                        failures.push(format!("{what}: {e}"));
                    }
                }
            }
        }
    }
    Err(format!("every route failed: {}", failures.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A file server with a speed limit. It honours `Range: bytes=N-` and
    /// records the start of the last range it was asked for.
    async fn serve(
        body: Arc<Vec<u8>>,
        chunk: usize,
        pause: Duration,
        ignore_range: bool,
    ) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let last_start = Arc::new(AtomicU64::new(0));
        let seen = last_start.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let range = if ignore_range {
                        None
                    } else {
                        text.lines()
                            .find_map(|l| l.strip_prefix("range: bytes="))
                            .map(|spec| {
                                let spec = spec.trim();
                                let (first, last) = spec.split_once('-').unwrap_or((spec, ""));
                                let start = first
                                    .trim()
                                    .parse::<usize>()
                                    .unwrap_or(0)
                                    .min(body.len() - 1);
                                let end = last
                                    .trim()
                                    .parse::<usize>()
                                    .map(|end| end.min(body.len() - 1))
                                    .unwrap_or(body.len() - 1);
                                (start, end.max(start))
                            })
                    };
                    let (start, end) = range.unwrap_or((0, body.len() - 1));
                    seen.store(start as u64, Ordering::SeqCst);
                    let rest = &body[start..=end];
                    let header = if range.is_some() {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {}-{}/{}\r\nconnection: close\r\n\r\n",
                            rest.len(),
                            start,
                            end,
                            body.len()
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            rest.len()
                        )
                    };
                    if stream.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    for piece in rest.chunks(chunk) {
                        if stream.write_all(piece).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(pause).await;
                    }
                });
            }
        });
        (format!("http://{addr}/asset.tar.gz"), last_start)
    }

    fn fast_policy() -> Policy {
        Policy {
            warmup: Duration::ZERO,
            window: Duration::from_millis(300),
            min_per_window: 20_000,
            stall: Duration::from_secs(5),
            overall: Duration::from_secs(30),
            connect: Duration::from_secs(5),
        }
    }

    fn direct() -> Route {
        Route {
            label: "direct".into(),
            proxy: None,
            from_environment: false,
        }
    }

    fn url(label: &str, url: String) -> Source {
        Source::Url {
            label: label.into(),
            url,
        }
    }

    fn payload() -> Arc<Vec<u8>> {
        Arc::new((0..400_000u32).map(|i| (i % 251) as u8).collect())
    }

    /// The first source crawls at 1 KB per 40 ms (25 KB/s, the measured
    /// throttle); the second is fast. The download abandons the first after a
    /// window, keeps what it got, and the second resumes from that byte.
    #[tokio::test]
    async fn a_crawling_route_is_abandoned_and_the_next_one_resumes_its_bytes() {
        let body = payload();
        let (slow, _) = serve(body.clone(), 1024, Duration::from_millis(40), false).await;
        let (fast, fast_start) = serve(body.clone(), 16 * 1024, Duration::ZERO, false).await;
        let mut notes = Vec::new();
        let got = download(
            &[url("slow", slow), url("fast", fast)],
            &[direct()],
            &fast_policy(),
            |_| {},
            |n| notes.push(n.to_string()),
        )
        .await
        .expect("the fast route finishes it");
        assert_eq!(got, *body, "the spliced download is byte-identical");
        assert!(
            fast_start.load(Ordering::SeqCst) > 0,
            "the fast route was asked to resume, not to start over"
        );
        assert!(notes.iter().any(|n| n.contains("too slow")), "{notes:?}");
    }

    /// A server that ignores `Range` answers 200 with the whole file: the
    /// download restarts cleanly instead of appending a second copy.
    #[tokio::test]
    async fn a_server_that_ignores_range_restarts_instead_of_appending() {
        let body = payload();
        let (slow, _) = serve(body.clone(), 1024, Duration::from_millis(40), false).await;
        let (fast, _) = serve(body.clone(), 16 * 1024, Duration::ZERO, true).await;
        let got = download(
            &[url("slow", slow), url("fast", fast)],
            &[direct()],
            &fast_policy(),
            |_| {},
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(got.len(), body.len());
        assert_eq!(got, *body);
    }

    /// When every route is slow the patient pass finishes the job anyway.
    #[tokio::test]
    async fn when_every_route_is_slow_the_patient_pass_still_finishes() {
        let body: Arc<Vec<u8>> = Arc::new((0..60_000u32).map(|i| (i % 251) as u8).collect());
        let (slow, _) = serve(body.clone(), 1024, Duration::from_millis(25), false).await;
        let mut notes = Vec::new();
        let got = download(
            &[url("only", slow)],
            &[direct()],
            &fast_policy(),
            |_| {},
            |n| notes.push(n.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(got, *body);
        assert!(
            notes.iter().any(|n| n.contains("too slow")),
            "the first pass judged it: {notes:?}"
        );
    }

    /// A healthy route is never second-guessed and the fallback is never asked.
    #[tokio::test]
    async fn a_healthy_route_finishes_without_touching_the_fallback() {
        let body = payload();
        let (fast, _) = serve(body.clone(), 32 * 1024, Duration::ZERO, false).await;
        let (spare, spare_start) = serve(body.clone(), 1024, Duration::ZERO, false).await;
        let mut notes = Vec::new();
        let got = download(
            &[url("fast", fast), url("spare", spare)],
            &[direct()],
            &fast_policy(),
            |_| {},
            |n| notes.push(n.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(got, *body);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(spare_start.load(Ordering::SeqCst), 0);
    }

    /// The witness: the real v5.1.0 asset, through this downloader and the real
    /// network, against its published checksum. Run it by hand
    /// (`just t-native cli::download::tests::live`); it needs the internet.
    #[tokio::test]
    #[ignore = "live network: downloads a 39 MB release asset"]
    async fn live_release_asset_matches_its_published_checksum() {
        use sha2::{Digest, Sha256};
        let base = "https://github.com/dondai44423/donsetch/releases/download/v5.1.0";
        let name = "donsetch-linux-x64.tar.gz";
        let sources = [
            url("GitHub release", format!("{base}/{name}")),
            Source::GithubAsset {
                repo: "dondai44423/donsetch".into(),
                tag: "v5.1.0".into(),
                name: name.into(),
            },
        ];
        let mut notes = Vec::new();
        let began = Instant::now();
        let bytes = download(
            &sources,
            &configured_routes(),
            &Policy::standard(),
            |_| {},
            |n| notes.push(n.to_string()),
        )
        .await
        .expect("download");
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let published = reqwest::get(format!("{base}/{name}.sha256"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        println!(
            "{} bytes in {:.1}s, notes: {notes:?}",
            bytes.len(),
            began.elapsed().as_secs_f32()
        );
        assert_eq!(published.split_whitespace().next().unwrap(), digest);
    }

    #[test]
    fn content_range_totals_parse() {
        assert_eq!(total_from_content_range("bytes 100-199/200"), Some(200));
        assert_eq!(total_from_content_range("bytes */200"), Some(200));
        assert_eq!(total_from_content_range("bytes 0-1/*"), None);
    }

    /// A throttled asset comes down over several connections: the whole file, in
    /// order, with progress reporting what is held.
    #[tokio::test]
    async fn a_throttled_asset_finishes_over_parallel_ranges() {
        // 2.4 MB is over the ranged floor; 4 KiB per 4 ms is ~1 MB/s per
        // connection, so one stream would take ~2.4 s and eight segments finish
        // in well under the stall.
        let body: Arc<Vec<u8>> = Arc::new((0..2_400_000u32).map(|i| (i % 251) as u8).collect());
        let (url_text, _seen) = serve(body.clone(), 4096, Duration::from_millis(4), false).await;
        let policy = fast_policy();
        let client = client_for(&direct(), &policy).unwrap();
        let mut buf = Vec::new();
        let mut reported = 0;
        let mut saw_lanes = false;
        let outcome = attempt_parallel(
            &client,
            &direct(),
            &url_text,
            None,
            "direct",
            &mut buf,
            &policy,
            false,
            &mut |p| {
                reported = p.bytes;
                if p.lanes.is_some() {
                    saw_lanes = true;
                }
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, Parallel::Done),
            "the ranged path should finish the file"
        );
        assert_eq!(buf.len(), body.len());
        assert_eq!(&buf[..], &body[..], "the segments assemble in order");
        assert_eq!(reported, buf.len(), "progress reports what is held");
        assert!(saw_lanes, "parallel download exposes live lane telemetry");
    }

    /// A server that ignores `Range` is streamed instead: the ranged path steps
    /// aside rather than failing the upgrade.
    #[tokio::test]
    async fn a_server_without_ranges_streams_instead() {
        let body = payload();
        let (url_text, _seen) = serve(body.clone(), 16 * 1024, Duration::ZERO, true).await;
        let policy = fast_policy();
        let client = client_for(&direct(), &policy).unwrap();
        let mut buf = Vec::new();
        let outcome = attempt(
            &client,
            &direct(),
            &url("direct", url_text),
            &mut buf,
            &policy,
            false,
            &mut |_| {},
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, Attempt::Done),
            "the streaming path should finish"
        );
        assert_eq!(&buf[..], &body[..]);
    }

    /// Large (>= PARALLEL_FLOOR) assets on a throttled primary route must fail
    /// over after one window instead of awaiting all eight range tasks to
    /// completion, while keeping segment 0's prefix for the fast fallback.
    #[tokio::test]
    async fn a_crawling_parallel_route_fails_over_within_a_window_and_resumes() {
        let body: Arc<Vec<u8>> = Arc::new((0..2_400_000u32).map(|i| (i % 251) as u8).collect());
        // 256 bytes every 80 ms per stream (~3.2 KB/s * 8 = ~25.6 KB/s, ~7.6 KB
        // per 300 ms window < 20 KB floor; finishing on slow would take ~90 s).
        let (slow, _) = serve(body.clone(), 256, Duration::from_millis(80), false).await;
        let (fast, fast_start) = serve(body.clone(), 64 * 1024, Duration::ZERO, false).await;
        let mut notes = Vec::new();
        let started = Instant::now();
        let got = download(
            &[url("slow-cdn", slow), url("fast-mirror", fast)],
            &[direct()],
            &fast_policy(),
            |_| {},
            |n| notes.push(n.to_string()),
        )
        .await
        .expect("fast mirror finishes the >=2 MiB asset");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "parallel slow route must fail over promptly, took {:?}",
            started.elapsed()
        );
        assert_eq!(got.len(), body.len());
        assert_eq!(got, *body);
        assert!(
            fast_start.load(Ordering::SeqCst) > 0,
            "fast route should resume after the contiguous prefix kept from segment 0"
        );
        assert!(notes.iter().any(|n| n.contains("too slow")), "{notes:?}");
    }

    #[test]
    fn direct_azure_blob_url_extracts_verified_blob_host_only() {
        use base64::Engine as _;
        let encode = |json: &str| {
            format!(
                "eyJ0eXAiOiJKV1QifQ.{}.sig",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes())
            )
        };
        let valid_jwt = encode(r#"{"path":"releaseassetproduction.blob.core.windows.net"}"#);
        let fastly = format!(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&jwt={valid_jwt}"
        );
        assert_eq!(
            direct_azure_blob_url(&fastly).as_deref(),
            Some(
                format!(
                    "https://releaseassetproduction.blob.core.windows.net/github-production-release-asset/1/2?sp=r&jwt={valid_jwt}"
                )
                .as_str()
            )
        );

        // Negative: non-Azure host in JWT path must be rejected.
        let evil_jwt = encode(r#"{"path":"evil.example.com"}"#);
        let evil_url = format!("https://release-assets.githubusercontent.com/a/b?jwt={evil_jwt}");
        assert_eq!(direct_azure_blob_url(&evil_url), None);

        // Negative: unrelated CDN host is untouched.
        assert_eq!(
            direct_azure_blob_url(&format!("https://example.com/a/b?jwt={valid_jwt}")),
            None
        );
    }

    #[tokio::test]
    async fn race_fastest_origin_picks_faster_server_and_keeps_bytes() {
        let body: Arc<Vec<u8>> = Arc::new((0..512_000u32).map(|i| (i % 251) as u8).collect());
        let (slow, _) = serve(body.clone(), 4096, Duration::from_millis(30), false).await;
        let (fast, _) = serve(body.clone(), 64 * 1024, Duration::ZERO, false).await;
        let policy = fast_policy();
        let client = client_for(&direct(), &policy).unwrap();
        let (winner, res) = race_fastest_origin(
            &client,
            &slow,
            &fast,
            None,
            0,
            Some(body.len() as u64),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(winner, fast);
        let (bytes, total) = res.expect("winning origin returns prefetched slice");
        assert_eq!(total, Some(body.len() as u64));
        assert_eq!(bytes.len(), 256 * 1024);
        assert_eq!(&bytes[..], &body[..256 * 1024]);
    }
}
