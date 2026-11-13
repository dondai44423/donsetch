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
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("donsetch-update/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(policy.connect)
        .redirect(reqwest::redirect::Policy::limited(8));
    builder = match &route.proxy {
        Some(url) => builder.proxy(reqwest::Proxy::all(url).map_err(|e| format!("proxy: {e}"))?),
        None if route.from_environment => builder,
        None => builder.no_proxy(),
    };
    builder.build().map_err(|e| e.to_string())
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

async fn attempt(
    client: &reqwest::Client,
    source: &Source,
    buf: &mut Vec<u8>,
    policy: &Policy,
    patient: bool,
    progress: &mut impl FnMut(usize),
) -> Result<Attempt, String> {
    let (url, accept) = source.resolve(client).await?;
    let mut request = client.get(&url);
    if let Some(accept) = accept {
        request = request.header("Accept", accept);
    }
    if !buf.is_empty() {
        request = request.header("Range", format!("bytes={}-", buf.len()));
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
        progress(buf.len());
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
    mut progress: impl FnMut(usize),
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
                match attempt(&client, source, &mut buf, policy, patient, &mut progress).await {
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
                    let start = if ignore_range {
                        0
                    } else {
                        text.lines()
                            .find_map(|l| l.strip_prefix("range: bytes="))
                            .and_then(|r| r.trim_end_matches('-').trim().parse::<u64>().ok())
                            .unwrap_or(0)
                    } as usize;
                    seen.store(start as u64, Ordering::SeqCst);
                    let rest = &body[start..];
                    let header = if start > 0 {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {}-{}/{}\r\nconnection: close\r\n\r\n",
                            rest.len(),
                            start,
                            body.len() - 1,
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
}
