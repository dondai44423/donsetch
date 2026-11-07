//! `web_screenshot` MCP tool (issue #171).
//!
//! One rendered PNG of a page, captured through the locally installed
//! browser in a throwaway headless profile. No new trust surface: the
//! URL and the page's own HTTP requests pass the same guards (see
//! `browser::HeadlessSession::guard_requests`), one launch per call, and
//! the bytes stay in this process for the caller's token budget to
//! decide on.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Value, json};

use super::Daemon;
use super::errors::{policy_error_value, tool_error, tool_error_structured};
use crate::error::FetchError;
use crate::fetch::guards::{ensure_url_safe, validate_url_basic};

const WAIT_MS_MAX: u64 = 5000;

/// A guard or render error as the tool result: a rule denial keeps its
/// rule, kind and the operator's message through `policy_error_value`;
/// anything else is the plain text `wrap` builds from the error.
fn fetch_failure(error: &FetchError, url: &str, wrap: impl FnOnce(&FetchError) -> String) -> Value {
    if matches!(error, FetchError::Denied { .. }) {
        return policy_error_value(error, url, None);
    }
    tool_error(wrap(error))
}

/// Viewport is the cheap default (matches CLI --full-page SetTrue:
/// omitted flag = viewport). Omitting full_page on MCP must NOT
/// silently mean full-page: that is the expensive path and it made
/// CLI/MCP disagree on the same call shape.
fn full_page_arg(args: &Value) -> bool {
    args.get("full_page")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The whole-call budget: `deadline_ms` with the fetch tool's clamp
/// (500-600000 ms) and default (60s), so the two tools agree on what
/// a deadline means. V12: the value used to be a fixed 60s the
/// caller could not change.
fn deadline_arg(args: &Value) -> Duration {
    Duration::from_millis(
        args.get("deadline_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000)
            .clamp(500, 600_000),
    )
}

pub async fn web_screenshot_tool(
    _daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<super::ToolCtx>,
) -> Value {
    let started = std::time::Instant::now();
    let budget = deadline_arg(args);
    let url_in = match args.get("url").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => {
            return tool_error("web_screenshot needs a url string");
        }
    };
    let full_page = full_page_arg(args);
    let wait_ms = args
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .min(WAIT_MS_MAX);
    // Cloned before the work future takes `url_in` for its debug block.
    let deadline_error_url = url_in.clone();

    let target = match validate_url_basic(&url_in) {
        Ok(u) => u,
        Err(e) => return fetch_failure(&e, &url_in, ToString::to_string),
    };
    let host = target.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return tool_error("web_screenshot: the url has no host");
    }
    // The capture observes the caller's budget end to end: launch,
    // load and shoot all run under the tool's deadline and
    // notifications/cancelled (the launch is killed on drop).
    let work = async {
        let inner: Result<serde_json::Value, serde_json::Value> = async {
            let target = ensure_url_safe(target.as_str())
                .await
                .map_err(|error| fetch_failure(&error, &url_in, ToString::to_string))?;
            let (png, final_url) =
                crate::browser::capture_page_png(target.as_str(), full_page, wait_ms)
                    .await
                    .map_err(|error| match error {
                        FetchError::Browser(e) => tool_error(format!("web_screenshot: {e}")),
                        // The browser's request guard refused the page's
                        // own document: answered as the URL's check is.
                        refused => fetch_failure(&refused, &url_in, ToString::to_string),
                    })?;
            let filename = format!("capture-{}.png", crate::handles::random_base62(16));
            let path = crate::paths::resolve_screenshot_path(&filename).map_err(tool_error)?;
            crate::browser::save_screenshot(&path, &png)
                .map_err(|e| tool_error(format!("web_screenshot: saving capture failed: {e}")))?;
            let b64 = BASE64_STANDARD.encode(&png);
            Ok(json!({
                "content": [
                    {
                        "type": "image",
                        "data": b64,
                        "mimeType": "image/png"
                    },
                    {
                        "type": "text",
                        "text": format!(
                            "Captured {final_url} ({} view, {} PNG bytes)",
                            if full_page { "full-page" } else { "viewport" },
                            png.len()
                        )
                    }
                ],
                "structuredContent": {
                    "ok": true,
                    "url": final_url,
                    "full_page": full_page,
                    "bytes": png.len(),
                    "path": path
                },
                "isError": false,
                "_meta": {
                    "com.donsetch/screenshot-debug": {
                        "requested_url": url_in
                    }
                }
            }))
        }
        .await;
        match inner {
            Ok(v) | Err(v) => v,
        }
    };
    super::run_with_budget(
        work,
        Some(budget.saturating_sub(started.elapsed())),
        ctx.as_mut(),
        || {
            tool_error_structured(
                format!(
                    "web_screenshot: deadline exceeded after {}ms",
                    budget.as_millis()
                ),
                "transient",
                Some(json!({
                    "code": "deadline.hit",
                    "url": deadline_error_url,
                    "next_action": "raise deadline_ms or wait_ms, or capture the viewport instead of the full page",
                })),
            )
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{deadline_arg, fetch_failure, full_page_arg};
    use crate::fetch::guards::validate_url_basic_with_policy;
    use crate::rules::{RuleAction, RuleSet, RulesSection, UrlRule};
    use serde_json::json;
    use std::time::Duration;

    fn deny_example_com() -> RuleSet {
        let mut section = RulesSection::default();
        section.url.insert(
            "example.com".to_string(),
            UrlRule {
                action: RuleAction::Deny,
                message: Some("ask the human operator to download it".to_string()),
                reason: Some("ip_ban".to_string()),
                ..Default::default()
            },
        );
        RuleSet::compile(&section).unwrap()
    }

    // The first guard of web_screenshot is the sync validate_url_basic.
    // A rule denial there must reach the caller as the structured policy
    // error (rule, code, the operator's message), not as plain text.
    #[test]
    fn a_denied_url_surfaces_as_the_policy_error() {
        let rules = deny_example_com();
        let url = "https://www.example.com/page";
        let error = validate_url_basic_with_policy(url, false, &rules).unwrap_err();
        let value = fetch_failure(&error, url, ToString::to_string);
        assert_eq!(value["structuredContent"]["ok"], false);
        let structured = &value["structuredContent"];
        assert_eq!(structured["code"], "policy.denied.ip_ban");
        assert_eq!(structured["rule"], "example.com");
        assert_eq!(
            structured["next_action"],
            "ask the human operator to download it"
        );
    }

    // The negative case: a host no rule names passes the guard, and an
    // error that is not a denial keeps the plain-text shape.
    #[test]
    fn other_guard_errors_stay_plain_text() {
        let rules = deny_example_com();
        assert!(validate_url_basic_with_policy("https://example.org/", false, &rules).is_ok());
        let url = "ftp://example.org/";
        let error = validate_url_basic_with_policy(url, false, &rules).unwrap_err();
        let value = fetch_failure(&error, url, ToString::to_string);
        assert_eq!(value["structuredContent"]["ok"], false);
        assert!(
            !value["structuredContent"]["code"]
                .as_str()
                .unwrap_or("")
                .starts_with("policy.denied"),
            "{value}"
        );
    }

    #[test]
    fn omitted_full_page_defaults_to_viewport_not_full_page() {
        // CLI --full-page is SetTrue (omitted = false). MCP omitting
        // the field used to default true: same call shape, different
        // expensive path. Viewport is the cheap default on both.
        assert!(!full_page_arg(&json!({})), "omit = viewport");
        assert!(!full_page_arg(&json!({"full_page": false})));
        assert!(full_page_arg(&json!({"full_page": true})));
    }

    // V12: deadline_ms mirrors web_fetch: default 60s, clamp
    // 500-600000 ms, so both tools bound a call the same way.
    #[test]
    fn deadline_ms_defaults_and_clamps_like_fetch() {
        assert_eq!(deadline_arg(&json!({})), Duration::from_secs(60));
        assert_eq!(
            deadline_arg(&json!({"deadline_ms": 4000})),
            Duration::from_secs(4)
        );
        assert_eq!(
            deadline_arg(&json!({"deadline_ms": 100})),
            Duration::from_millis(500)
        );
        assert_eq!(
            deadline_arg(&json!({"deadline_ms": 10_000_000})),
            Duration::from_secs(600)
        );
    }

    // ── The browser's request guard, against the real browser ──
    //
    // Two loopback origins owned by the test: an allowed one on 127.0.0.1
    // and one on 127.0.0.2 that a local rule denies (Linux answers on all
    // of 127.0.0.0/8). The denied origin answers every request and logs
    // its path, so the rule is the only thing that can keep the browser
    // off it. Each test installs the process config, so it depends on
    // nextest's one process per test. Run with
    // `just t-native web_screenshot_tool::tests::real_browser`.
    #[cfg(target_os = "linux")]
    mod real_browser {
        use crate::rules::{RuleAction, RulesMode, UrlRule};
        use serde_json::{Value, json};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        const FIXTURE_MESSAGE: &str = "the fixture host is off limits";

        type Log = Arc<Mutex<Vec<String>>>;

        fn install_fixture_config() {
            let mut loaded = crate::config::load().unwrap();
            // The browser resolves as the product's does (DONGHOST_CHROME,
            // browser.chromium_path, discovery). Both origins are loopback,
            // so private egress is on: only the rule refuses 127.0.0.2.
            loaded.config.fetch.allow_private_egress = true;
            loaded.config.proxy.from_environment = false;
            loaded.config.rules.mode = RulesMode::Enforce;
            loaded.config.rules.url.insert(
                "127.0.0.2".to_string(),
                UrlRule {
                    action: RuleAction::Deny,
                    message: Some(FIXTURE_MESSAGE.to_string()),
                    reason: Some("fixture".to_string()),
                    ..Default::default()
                },
            );
            crate::config::install_loaded(loaded).unwrap();
        }

        /// A loopback HTTP origin on `ip`: logs each request's path and
        /// answers with `route(path)`, a status line tail with headers,
        /// and a body. Returns the origin's base URL and its log.
        async fn origin<F>(ip: &str, route: F) -> (String, Log)
        where
            F: Fn(&str) -> (String, String) + Send + Sync + 'static,
        {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let log = Log::default();
            let seen = Arc::clone(&log);
            let route = Arc::new(route);
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let seen = Arc::clone(&seen);
                    let route = Arc::clone(&route);
                    tokio::spawn(async move {
                        // The whole head, a 100 KB URL included: closing
                        // on unread request bytes resets the connection.
                        let mut head = Vec::new();
                        let mut buf = [0u8; 16 * 1024];
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 256 * 1024
                        {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => head.extend_from_slice(&buf[..n]),
                            }
                        }
                        let path = String::from_utf8_lossy(&head)
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        seen.lock().unwrap().push(path.clone());
                        let (status, body) = route(&path);
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            (base, log)
        }

        async fn denied_origin() -> (String, Log) {
            origin("127.0.0.2", |_| text("the denied host answered")).await
        }

        fn text(body: &str) -> (String, String) {
            (
                "200 OK\r\nContent-Type: text/plain".to_string(),
                body.to_string(),
            )
        }

        fn html(body: String) -> (String, String) {
            ("200 OK\r\nContent-Type: text/html".to_string(), body)
        }

        fn redirect(location: String) -> (String, String) {
            (format!("302 Found\r\nLocation: {location}"), String::new())
        }

        async fn screenshot(daemon: &Arc<crate::mcp::server::Daemon>, url: String) -> Value {
            super::super::web_screenshot_tool(daemon, &json!({ "url": url, "wait_ms": 500 }), None)
                .await
        }

        /// What the denied origin received, once late requests had time
        /// to land.
        async fn reached(log: &Log) -> Vec<String> {
            tokio::time::sleep(Duration::from_millis(500)).await;
            log.lock().unwrap().clone()
        }

        #[tokio::test]
        #[ignore = "requires an installed Chromium; owns two loopback HTTP origins and a private browser"]
        async fn subresources_from_a_denied_host_fail_and_the_capture_still_succeeds() {
            install_fixture_config();
            let (denied, denied_log) = denied_origin().await;
            let (allowed, allowed_log) = origin("127.0.0.1", move |path| match path {
                "/page" => html(format!(
                    "<!doctype html><title>allowed</title><p>An allowed page.</p>\
                     <img src=\"/allowed-img\"><img src=\"{denied}/img-probe\">\
                     <script>fetch(\"{denied}/fetch-probe\").catch(() => {{}})</script>"
                )),
                "/allowed-img" => text("allowed"),
                _ => text("no such fixture"),
            })
            .await;
            let daemon = Arc::new(crate::mcp::server::Daemon::new().await.unwrap());

            let shot = screenshot(&daemon, format!("{allowed}/page")).await;
            let structured = &shot["structuredContent"];
            let denied_requests = reached(&denied_log).await;
            // Negative: the capture succeeds, and the allowed host's own
            // subresource still loads.
            assert_eq!(structured["ok"], true, "{structured}");
            assert_eq!(structured["url"], format!("{allowed}/page"));
            let served = allowed_log.lock().unwrap().clone();
            assert!(served.iter().any(|p| p == "/allowed-img"), "{served:?}");
            assert_eq!(
                denied_requests,
                Vec::<String>::new(),
                "the denied host was reached; capture: {structured}"
            );
        }

        #[tokio::test]
        #[ignore = "requires an installed Chromium; owns two loopback HTTP origins and a private browser"]
        async fn a_redirect_into_a_denied_host_refuses_the_capture_with_the_rule() {
            install_fixture_config();
            let (denied, denied_log) = denied_origin().await;
            let (allowed, _) = origin("127.0.0.1", move |path| match path {
                "/redirect" => redirect(format!("{denied}/redirect-probe")),
                "/hop" => redirect("/landing".to_string()),
                "/landing" => html("<!doctype html><p>Landed.</p>".to_string()),
                _ => text("no such fixture"),
            })
            .await;
            let daemon = Arc::new(crate::mcp::server::Daemon::new().await.unwrap());

            // Negative: a redirect that stays on the allowed host is followed.
            let followed = screenshot(&daemon, format!("{allowed}/hop")).await;
            let structured = &followed["structuredContent"];
            assert_eq!(structured["ok"], true, "{structured}");
            assert_eq!(structured["url"], format!("{allowed}/landing"));

            let refused = screenshot(&daemon, format!("{allowed}/redirect")).await;
            let structured = &refused["structuredContent"];
            assert_eq!(
                reached(&denied_log).await,
                Vec::<String>::new(),
                "the denied host was reached; capture: {structured}"
            );
            assert_eq!(structured["ok"], false, "{structured}");
            assert_eq!(structured["code"], "policy.denied.fixture", "{structured}");
            assert_eq!(structured["rule"], "127.0.0.2");
            assert_eq!(structured["next_action"], FIXTURE_MESSAGE);
        }

        #[tokio::test]
        #[ignore = "requires an installed Chromium; owns two loopback HTTP origins and a private browser"]
        async fn a_page_navigating_into_a_denied_host_refuses_the_capture_with_the_rule() {
            install_fixture_config();
            let (denied, denied_log) = denied_origin().await;
            let (allowed, _) = origin("127.0.0.1", move |path| match path {
                "/leave" => html(format!(
                    "<!doctype html><p>Leaving.</p>\
                     <script>location.replace(\"{denied}/navigation-probe\")</script>"
                )),
                _ => text("no such fixture"),
            })
            .await;
            let daemon = Arc::new(crate::mcp::server::Daemon::new().await.unwrap());

            let refused = screenshot(&daemon, format!("{allowed}/leave")).await;
            let structured = &refused["structuredContent"];
            assert_eq!(
                reached(&denied_log).await,
                Vec::<String>::new(),
                "the denied host was reached; capture: {structured}"
            );
            assert_eq!(structured["ok"], false, "{structured}");
            assert_eq!(structured["code"], "policy.denied.fixture", "{structured}");
            assert_eq!(structured["rule"], "127.0.0.2");
        }

        /// The capture's structured result and first text, for messages.
        fn outcome(shot: &Value) -> String {
            format!(
                "{} {}",
                shot["structuredContent"], shot["content"][0]["text"]
            )
        }

        // A page can cancel requests while they wait for the guard. Chrome
        // then rejects their answers; the capture goes on.
        #[tokio::test]
        #[ignore = "requires an installed Chromium; owns two loopback HTTP origins and a private browser"]
        async fn a_page_cancelling_its_own_requests_still_captures() {
            install_fixture_config();
            let (allowed, _) = origin("127.0.0.1", |path| match path {
                "/cancel" => html(
                    "<!doctype html><p>Cancelling.</p><script>\
                     const stop = new AbortController();\
                     for (let i = 0; i < 150; i++) \
                       fetch('/fetch/' + i, { signal: stop.signal }).catch(() => {});\
                     setTimeout(() => stop.abort(), 5);\
                     </script>"
                        .to_string(),
                ),
                _ => text("answered"),
            })
            .await;
            let daemon = Arc::new(crate::mcp::server::Daemon::new().await.unwrap());

            let shot = screenshot(&daemon, format!("{allowed}/cancel")).await;
            assert_eq!(shot["structuredContent"]["ok"], true, "{}", outcome(&shot));
        }

        // A page can start far more requests at once than the guard checks
        // at once. Each waits its turn and loads.
        #[tokio::test]
        #[ignore = "requires an installed Chromium; owns two loopback HTTP origins and a private browser"]
        async fn a_page_with_a_thousand_images_still_captures() {
            install_fixture_config();
            let mut page = String::from("<!doctype html><p>A thousand images.</p>");
            for i in 0..1000 {
                page.push_str(&format!("<img src=\"/img/{i}\" width=1 height=1>"));
            }
            let (allowed, allowed_log) = origin("127.0.0.1", move |path| match path {
                "/many" => html(page.clone()),
                _ => text("pixel"),
            })
            .await;
            let daemon = Arc::new(crate::mcp::server::Daemon::new().await.unwrap());

            let shot = screenshot(&daemon, format!("{allowed}/many")).await;
            assert_eq!(shot["structuredContent"]["ok"], true, "{}", outcome(&shot));
            let images: std::collections::HashSet<String> = allowed_log
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.starts_with("/img/"))
                .cloned()
                .collect();
            assert_eq!(images.len(), 1000);
        }

        // A long URL is checked like any other: to the allowed host it
        // loads, to the denied host it is blocked, and the capture goes on.
        #[tokio::test]
        #[ignore = "requires an installed Chromium; owns two loopback HTTP origins and a private browser"]
        async fn a_long_request_url_is_checked_like_any_other() {
            install_fixture_config();
            let (denied, denied_log) = denied_origin().await;
            let query = "a".repeat(100_000);
            let (allowed, allowed_log) = origin("127.0.0.1", move |path| match path {
                "/long" => html(format!(
                    "<!doctype html><p>Long URLs.</p>\
                     <img src=\"/long-probe?{query}\"><img src=\"{denied}/long-probe?{query}\">"
                )),
                _ => text("no such fixture"),
            })
            .await;
            let daemon = Arc::new(crate::mcp::server::Daemon::new().await.unwrap());

            let shot = screenshot(&daemon, format!("{allowed}/long")).await;
            let short = |paths: Vec<String>| -> Vec<String> {
                paths.iter().map(|p| p.chars().take(24).collect()).collect()
            };
            let denied_requests = short(reached(&denied_log).await);
            assert_eq!(shot["structuredContent"]["ok"], true, "{}", outcome(&shot));
            let served = short(allowed_log.lock().unwrap().clone());
            assert!(
                served.iter().any(|p| p.starts_with("/long-probe?a")),
                "{served:?}"
            );
            assert_eq!(
                denied_requests,
                Vec::<String>::new(),
                "the denied host was reached"
            );
        }
    }
}
