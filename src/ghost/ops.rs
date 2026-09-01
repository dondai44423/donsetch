//! Solve + Render — the ghost's two jobs.

use std::time::{Duration, Instant};

use crate::detect::walls::{self, Verdict};
use crate::error::FetchError;
use crate::ghost::cache::CookieRecord;

use super::Ghost;

pub struct SolveResult {
    /// Clearance + session cookies with real expiry.
    pub cookies: Vec<CookieRecord>,
    /// Wall vendor detected during the challenge ("cloudflare",
    /// "datadome", etc.) — feeds the domain profile.
    #[allow(dead_code)]
    pub vendor: Option<String>,
    /// Last DOM snapshot — fallback content if tier 1 with
    /// harvested cookies still gets refused.
    #[allow(dead_code)]
    pub html: String,
    pub took: Duration,
}

pub enum SolveOutcome {
    Solved(SolveResult),
    /// Interactive captcha — human/service territory.
    /// Honest dead end, no solving service by design.
    CaptchaWalled,
    TimedOut,
}

/// Clearance cookie names worth noting (not exhaustive —
/// a ContentOk verdict is the real success signal).
const CLEARANCE_NAMES: &[&str] = &[
    "cf_clearance",
    "datadome",
    "_px3",
    "ak_bmsc",
    "bm_sz",
    "reese84",
];

/// Cookie-banner / consent-modal dismisser + lazy-load kicker.
/// Clicks buttons whose labels look like consent actions and
/// common modal close controls, then scrolls to trigger
/// deferred content fetches.
const DISMISS_MODALS_JS: &str = r#"(() => {
  const labels = /^(accept all|accept|agree|i agree|allow all|allow cookies|got it|ok|okay|continue|reject all|close)$/i;
  for (const el of document.querySelectorAll('button,a[role="button"],[role="button"],input[type="submit"]')) {
    const t = (el.innerText || el.value || '').trim();
    if (t && t.length < 40 && labels.test(t)) { try { el.click(); } catch(e){} }
  }
  for (const el of document.querySelectorAll('[aria-label="Close"],[class*="modal" i] [class*="close" i]')) {
    try { el.click(); } catch(e){}
  }
  window.scrollTo(0, document.body.scrollHeight * 0.6);
})();"#;

/// SOLVE mode: navigate into a wall, wait for the
/// challenge to clear, harvest everything.
///
/// Oracle = multi-signal with stability: interstitials
/// are small, markers live in small pages, and the
/// clearance cookie must corroborate for known vendors.
/// A "clear" verdict must hold for TWO polls (300ms
/// apart) — challenge pages re-navigate after pass and
/// a mid-redirect snapshot can fake ContentOk.
pub async fn solve(
    ghost: &mut Ghost,
    url: &str,
    timeout: Duration,
) -> Result<SolveOutcome, FetchError> {
    let start = Instant::now();
    ghost.navigate(url).await?;
    // No resource blocking — challenges need resources to load.
    let _ = ghost
        .cdp
        .call(
            Some(&ghost.session),
            "Network.enable",
            serde_json::json!({}),
        )
        .await;

    let mut clicked = false;
    let mut clear_streak = 0u8;
    let mut poll_ms = 200u64; // fast early, back off later
    let mut vendor: Option<String> = None;

    while start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        let html = match ghost.outer_html().await {
            Ok(h) => h,
            Err(_) => continue, // mid-navigation, poll again
        };
        // Mid-navigation guard: about:blank is tiny and
        // marker-free — it would fake a clear streak.
        // Require a real URL and real bytes before any
        // clear vote counts.
        let cur = ghost.current_url().await.unwrap_or_default();
        let navigated = !cur.is_empty() && !cur.starts_with("about:") && html.len() > 500;
        if !navigated {
            continue;
        }
        let verdict = walls::detect(200, &[], html.as_bytes());

        // Interstitials are tiny (CF ~5-15KB, DataDome
        // ~1.5KB, PX ~10KB). ≥30KB + markers = real page
        // that mentions the vendor (nowsecure case).
        let small = html.len() < 30_000;
        let marker_hit = matches!(verdict, Verdict::Challenge(_) | Verdict::Blocked);
        let challenged = small && marker_hit;

        if challenged {
            if vendor.is_none()
                && let Verdict::Challenge(v) = &verdict
            {
                vendor = Some(format!("{v:?}").to_lowercase());
            }
            clear_streak = 0;
        } else {
            // Clear candidate: big page, or small page
            // with NO markers (e.g. CF cleared to a thin
            // landing). Corroborate with clearance cookie
            // for extra certainty; hold for 2 polls.
            clear_streak += 1;
            if clear_streak >= 2 {
                let cookies =
                    tokio::time::timeout(std::time::Duration::from_secs(5), ghost.cookies())
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                ghost.touch();
                return Ok(SolveOutcome::Solved(SolveResult {
                    cookies,
                    vendor,
                    html,
                    took: start.elapsed(),
                }));
            }
        }

        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!(
                "[ghost] t={:.0?} html={}B verdict={:?} challenged={} streak={}",
                start.elapsed(),
                html.len(),
                verdict,
                challenged,
                clear_streak,
            );
            if start.elapsed() < Duration::from_millis(1600) {
                // char-safe: byte 1200 can split a multi-byte char
                let mut cut = html.len().min(1200);
                while !html.is_char_boundary(cut) {
                    cut -= 1;
                }
                eprintln!("[ghost] html: {}", &html[..cut]);
            }
        }

        // Captcha walls: honest dead end.
        let lower = html.to_lowercase();
        if small
            && (lower.contains("hcaptcha.com")
                || lower.contains("g-recaptcha")
                || lower.contains("www.google.com/recaptcha")
                || lower.contains("captcha-delivery.com/captcha")
                || lower.contains("px-captcha"))
        {
            return Ok(SolveOutcome::CaptchaWalled);
        }

        // Turnstile-style checkbox: find the actual iframe position
        // via JS and click at its center. Fixed coordinates miss
        // because Turnstile renders at different positions per site.
        if !clicked
            && small
            && (lower.contains("challenges.cloudflare.com")
                || lower.contains("turnstile")
                || lower.contains("verify you are human"))
        {
            // Use Runtime.evaluate to find the Turnstile iframe's
            // bounding rect, then click at its center.
            let turnstile_pos = ghost
                .cdp
                .call(
                    Some(&ghost.session),
                    "Runtime.evaluate",
                    serde_json::json!({
                        "expression": "(() => { const f = document.querySelector('iframe[src*=challenges.cloudflare], iframe[src*=turnstile], cf-turnstile > div > iframe'); if (f) { const r = f.getBoundingClientRect(); return JSON.stringify({x: r.x + r.width/2, y: r.y + r.height/2}); } return null; })()",
                        "returnByValue": true
                    }),
                )
                .await
                .ok()
                .and_then(|v| v.get("result").and_then(|r| r.get("value")).cloned())
                .and_then(|v| v.as_str().map(String::from))
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
            let (x, y) = if let Some(ref coords) = turnstile_pos {
                (
                    coords.get("x").and_then(|v| v.as_f64()).unwrap_or(480.0),
                    coords.get("y").and_then(|v| v.as_f64()).unwrap_or(420.0),
                )
            } else {
                (480.0, 420.0)
            };
            let _ = ghost.click(x, y).await;
            clicked = true;
        }

        // Adaptive backoff: 300ms for the first 4s,
        // then settle to 750ms.
        if start.elapsed() > Duration::from_secs(5) {
            poll_ms = 500;
        }
    }
    Ok(SolveOutcome::TimedOut)
}

/// Tier-2 result: the browser's rendered DOM plus everything
/// harvested from the session.
pub struct GhostPage {
    /// Live DOM after content-grade settle.
    pub html: String,
    /// Session + clearance cookies with real expiry.
    pub cookies: Vec<CookieRecord>,
    /// Challenge vendor seen during the wait (if any).
    pub vendor: Option<String>,
    /// Interactive captcha — honest dead end.
    pub captcha: bool,
    #[allow(dead_code)]
    pub took: Duration,
}

/// Unified tier-2 fetch. Success oracle = CONTENT QUALITY,
/// not wall-clear:
///
/// 1. Challenge page still up → keep waiting (or dead-end on
///    captcha). Turnstile-style checkboxes clicked once.
/// 2. Consent interstitial → one click through, keep waiting.
/// 3. Not challenged → require a SUBSTANTIVE, STABLE DOM
///    (visible text ≥ 1200 chars, scripts excluded; length
///    drift < 1% across two polls).
/// 4. Then capture DOM + cookies.
///
/// A cleared-but-empty shell never satisfies this oracle —
/// that's the flaw that made tier 2 return SPA shells.
pub async fn ghost_fetch(
    ghost: &mut Ghost,
    url: &str,
    timeout: Duration,
) -> Result<GhostPage, FetchError> {
    let start = Instant::now();
    ghost.navigate(url).await?;
    // No resource blocking — challenges verify that resources
    // (images, fonts) load. Blocking them breaks Cloudflare
    // and DataDome challenge solving. The speed cost is small
    // (a few extra KB of fonts/images) vs the reliability gain.

    let mut clicked_challenge = 0u8; // turnstile clicks spent (max 3)
    let mut last_click_at = std::time::Instant::now() - Duration::from_secs(10);
    let mut clicked_consent = false;
    let mut kicked = false;
    let mut vendor: Option<String> = None;
    let mut settle_streak = 0u8;
    let mut dead_streak = 0u32; // consecutive polls: DOM static + visible < 80
    let mut prev_len = 0usize;
    let mut html = String::new();

    while start.elapsed() < timeout {
        let poll = if start.elapsed() > Duration::from_secs(5) {
            500
        } else {
            200
        };
        tokio::time::sleep(Duration::from_millis(poll)).await;
        html = match ghost.outer_html().await {
            Ok(h) => h,
            Err(e) => {
                if std::env::var_os("DONGHOST_DEBUG").is_some() {
                    eprintln!(
                        "[ghost_fetch] outer_html err at t={:?}: {e}",
                        start.elapsed()
                    );
                }
                continue;
            }
        };
        // Mid-navigation guard.
        let cur = ghost.current_url().await.unwrap_or_default();
        if cur.is_empty() || cur.starts_with("about:") || html.len() < 500 {
            continue;
        }
        let cur_len = html.len();
        let lower = html.to_lowercase();

        // Interactive captcha: honest dead end.
        if cur_len < 30_000
            && (lower.contains("hcaptcha.com")
                || lower.contains("g-recaptcha")
                || lower.contains("www.google.com/recaptcha")
                || lower.contains("captcha-delivery.com/captcha")
                || lower.contains("px-captcha"))
        {
            return Ok(GhostPage {
                html,
                cookies: Vec::new(),
                vendor,
                captcha: true,
                took: start.elapsed(),
            });
        }

        // Challenge still up → wait it out (click turnstile once).
        // Use detect_dom_smart (not detect_dom) — real pages with
        // embedded challenge widgets (Turnstile on a contact form)
        // have challenge markers but also substantial visible text.
        // detect_dom alone would classify these as Challenge on every
        // poll, preventing the content oracle from ever running.
        // detect_dom_smart checks visible text first: ≥ 80 visible
        // chars = real content, skip challenge check.
        let verdict = walls::detect_dom_smart(html.as_bytes());
        let challenged = matches!(verdict, Verdict::Challenge(_) | Verdict::Blocked);
        if challenged {
            if vendor.is_none()
                && let Verdict::Challenge(v) = &verdict
            {
                vendor = Some(format!("{v:?}").to_lowercase());
            }
            settle_streak = 0;
            // Turnstile widget: the iframe renders LATE (after the
            // widget JS boots) and repositions per layout — a
            // one-shot click fired before it attached always missed.
            // Re-find the geometry and click again every few polls,
            // up to 3 attempts total.
            if clicked_challenge < 3
                && last_click_at.elapsed() > Duration::from_secs(4)
                && (lower.contains("challenges.cloudflare.com")
                    || lower.contains("turnstile")
                    || lower.contains("verify you are human"))
            {
                let turnstile_pos = ghost
                    .cdp
                    .call(
                        Some(&ghost.session),
                        "Runtime.evaluate",
                        serde_json::json!({
                            "expression": "(() => { const f = document.querySelector('iframe[src*=challenges.cloudflare], iframe[src*=turnstile], cf-turnstile > div > iframe'); if (f) { const r = f.getBoundingClientRect(); return JSON.stringify({x: r.x + r.width/2, y: r.y + r.height/2}); } return null; })()",
                            "returnByValue": true
                        }),
                    )
                    .await
                    .ok()
                    .and_then(|v| v.get("result").and_then(|r| r.get("value")).cloned())
                    .and_then(|v| v.as_str().map(String::from))
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                let (x, y) = if let Some(ref coords) = turnstile_pos {
                    (
                        coords.get("x").and_then(|v| v.as_f64()).unwrap_or(480.0),
                        coords.get("y").and_then(|v| v.as_f64()).unwrap_or(420.0),
                    )
                } else {
                    (480.0, 420.0)
                };
                let _ = ghost.click(x, y).await;
                clicked_challenge += 1;
                last_click_at = std::time::Instant::now();
            }
            prev_len = cur_len;
            continue;
        }

        // Consent interstitial (Google / EU GDPR walls): click
        // through once. Not a wall detector case — the page is
        // "ContentOk" but it's the consent form, not content.
        if !clicked_consent && lower.contains("before you continue") {
            let _ = ghost.click(560.0, 430.0).await;
            let _ = ghost
                .cdp
                .call(
                    Some(&ghost.session),
                    "Runtime.evaluate",
                    serde_json::json!({"expression": DISMISS_MODALS_JS}),
                )
                .await;
            clicked_consent = true;
            prev_len = cur_len;
            continue;
        }

        // Still hydrating: aria-busy spinners, modal-hidden
        // body. Not a wall — the SPA just hasn't loaded its
        // content yet. Kick it once (dismiss consent modals +
        // scroll to trigger lazy fetches) and keep waiting;
        // never settle on a skeleton page.
        //
        // NOTE: "skeleton" in CSS class names is NOT a reliable
        // loading signal — Amazon, React apps, and many CSS
        // frameworks use "skeleton" in class names even after
        // the page has fully hydrated. Using it here causes the
        // ghost to never settle on pages with real content.
        // The content-quality oracle (visible >= 80) is the
        // reliable signal: if the page has 80+ visible chars
        // that are stable, it's real content, regardless of
        // CSS class names.
        let loading = lower.matches("aria-busy=\"true\"").take(3).count() >= 3
            || lower.contains("<body aria-hidden=\"true\">");
        if loading {
            settle_streak = 0;
            prev_len = cur_len;
            if !kicked {
                kicked = true;
                let _ = ghost
                    .cdp
                    .call(
                        Some(&ghost.session),
                        "Runtime.evaluate",
                        serde_json::json!({"expression": DISMISS_MODALS_JS}),
                    )
                    .await;
            }
            continue;
        }

        // Content-quality oracle: substantive AND stable.
        // The key metric is VISIBLE text length, not total HTML
        // size. SPA shells have < 10 visible chars; the smallest
        // real pages (example.com, httpbin) have 80+. A page with
        // 80+ visible chars that's stable across 2 polls is real
        // content. The old cur_len >= 4_000 gate was too strict
        // for simple pages — ghost_fetch ran the full 20s timeout
        // on example.com (visible=122, cur_len=559) instead of
        // returning immediately.
        //
        // SPA hydration guard: small DOMs (< 50KB) need more
        // time before settling. SPAs download, parse, and execute
        // JS bundles asynchronously — a stable 8KB DOM at t=400ms
        // is a SvelteKit/React shell, not a complete page. Wait
        // at least 3 seconds for the DOM to grow before settling.
        // Large DOMs (≥ 50KB) are already rendered — settle fast.
        // Measured: crates.io shell 8KB (needs ~3s), Discourse
        // shell 30KB (needs ~2-3s), both grow to 100-400KB after
        // hydration. example.com 559B (already complete at t=0).
        let visible = visible_text_len(&html);
        let substantive = visible >= 80;
        let stable = prev_len > 0 && cur_len.abs_diff(prev_len) < cur_len / 100 + 64;
        let min_settle = if cur_len < 50_000 {
            Duration::from_secs(4)
        } else {
            Duration::ZERO
        };
        let past_min = start.elapsed() >= min_settle;
        if substantive && stable && past_min {
            settle_streak += 1;
            if settle_streak >= 2 {
                let cookies =
                    tokio::time::timeout(std::time::Duration::from_secs(5), ghost.cookies())
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                ghost.touch();
                return Ok(GhostPage {
                    html,
                    cookies,
                    vendor,
                    captcha: false,
                    took: start.elapsed(),
                });
            }
        } else {
            settle_streak = 0;
        }

        // Dead DOM early exit: if the DOM is static (not changing)
        // AND has < 80 visible chars for several seconds, it's a dead
        // page — a block/challenge page that the ghost can't solve.
        // Don't waste 20s waiting; exit early and let the caller
        // handle it. This saves 12s on Amazon-type pages.
        //
        // Tolerance: allow small DOM changes (< 100 bytes) so
        // dynamic ads/trackers don't reset the streak.
        //
        // Adaptive threshold: tiny DOMs (< 5KB) exit faster (8 polls
        // ≈ 1.6s early / 4s late) — a 2KB DOM with 0 visible chars is
        // clearly dead. Larger DOMs get 15 polls (≈ 3s / 7.5s).
        if prev_len > 0 && visible < 80 && cur_len.abs_diff(prev_len) < 100 {
            dead_streak += 1;
        } else {
            dead_streak = 0;
        }
        let dead_threshold = if cur_len < 5000 { 8 } else { 15 };
        if dead_streak >= dead_threshold {
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!(
                    "[ghost_fetch] dead DOM: static + visible<80 for {dead_streak} polls, exiting early"
                );
            }
            break;
        }

        prev_len = cur_len;

        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!(
                "[ghost_fetch] t={:.0?} html={}B visible={} challenged=false streak={} dead={}",
                start.elapsed(),
                cur_len,
                visible,
                settle_streak,
                dead_streak,
            );
        }
    }

    // Timeout: return whatever rendered — partial beats none,
    // the caller's extraction yield decides success.
    // Timeout: return whatever rendered — partial beats none,
    // the caller's extraction yield decides success.
    //
    // BUT: if the final DOM is still a challenge/wall page, flag it
    // as captcha so ghost_escalate doesn't extract and cache the
    // interstitial as content (the Indeed false-positive bug).
    // Use detect_dom_smart — a real page with an embedded challenge
    // widget (Turnstile) has challenge markers but also visible text.
    // detect_dom alone would flag it as captcha; detect_dom_smart
    // checks visible text first.
    let final_verdict = walls::detect_dom_smart(html.as_bytes());
    if matches!(final_verdict, Verdict::Challenge(_) | Verdict::Blocked) {
        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!(
                "[ghost_fetch] timeout on challenge page ({:?}), flagging as captcha",
                final_verdict
            );
        }
        return Ok(GhostPage {
            html,
            cookies: Vec::new(),
            vendor,
            captcha: true,
            took: start.elapsed(),
        });
    }
    let cookies = tokio::time::timeout(std::time::Duration::from_secs(5), ghost.cookies())
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    ghost.touch();
    Ok(GhostPage {
        html,
        cookies,
        vendor,
        captcha: false,
        took: start.elapsed(),
    })
}

/// Fast visible-text estimate: strip tags + script/style bodies,
/// count non-whitespace. No lowercasing, no DOM — byte scan.
fn visible_text_len(html: &str) -> usize {
    let b = html.as_bytes();
    let mut n = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'<' => {
                // Skip script/style bodies entirely.
                if starts_ci(&b[i + 1..], b"script") {
                    i = find_ci(b, b"</script", i + 7)
                        .map(|p| p + 9)
                        .unwrap_or(b.len());
                } else if starts_ci(&b[i + 1..], b"style") {
                    i = find_ci(b, b"</style", i + 6)
                        .map(|p| p + 8)
                        .unwrap_or(b.len());
                } else {
                    while i < b.len() && b[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            c if !c.is_ascii_whitespace() => {
                n += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    n
}

fn starts_ci(b: &[u8], pat: &[u8]) -> bool {
    b.len() >= pat.len()
        && b[..pat.len()]
            .iter()
            .zip(pat)
            .all(|(a, p)| a.to_ascii_lowercase() == *p)
}

fn find_ci(b: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= b.len() || needle.is_empty() || b.len() < needle.len() {
        return None;
    }
    (from..=b.len() - needle.len()).find(|&p| starts_ci(&b[p..], needle))
}

/// RENDER mode: execute a JS shell, return the live DOM.
/// Success = outerHTML length stable across two polls —
/// robust for SPAs, no Network domain needed.
pub async fn render(ghost: &mut Ghost, url: &str, timeout: Duration) -> Result<String, FetchError> {
    let start = Instant::now();
    ghost.navigate(url).await?;
    let mut prev_len = 0usize;
    let mut stable = 0u8;
    let mut html = String::new();

    while start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(600)).await;
        html = match ghost.outer_html().await {
            Ok(h) => h,
            Err(_) => continue,
        };
        let len = html.len();
        if len > 4000 && len.abs_diff(prev_len) < len / 100 + 64 {
            stable += 1;
            if stable >= 2 {
                ghost.touch();
                return Ok(html);
            }
        } else {
            stable = 0;
        }
        prev_len = len;
    }
    ghost.touch();
    // Timeout: return whatever rendered — partial beats none.
    if html.is_empty() {
        Err(FetchError::ghost("render produced no DOM"))
    } else {
        Ok(html)
    }
}

/// Fingerprint self-test: navigate our local page,
/// read results back from the DOM (no Runtime ever).
pub async fn selftest(ghost: &mut Ghost) -> Result<String, FetchError> {
    // Lives in the system temp dir, never the persistent profile:
    // a hard-killed daemon used to leave selftest-<pid>.html
    // litter inside the shared profile root.
    let page = std::env::temp_dir().join(format!("donsetch-selftest-{}.html", std::process::id()));
    std::fs::write(&page, include_str!("selftest.html"))
        .map_err(|e| FetchError::ghost(format!("selftest: {e}")))?;
    let url = format!("file://{}", page.display());
    ghost.navigate_raw(&url, false).await?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(html) = ghost.outer_html().await {
            // Body holds the JSON; title mirrors it. Take
            // the LAST occurrence (body) to skip <title>.
            if let Some(a) = html.rfind("{\"webdriver\"")
                && let Some(b) = html[a..].find("</body>")
            {
                let json = html[a..a + b].replace("&quot;", "\"");
                let _ = std::fs::remove_file(&page);
                return Ok(json);
            }
        }
    }
    let _ = std::fs::remove_file(&page);
    Err(FetchError::ghost("selftest timed out"))
}

/// Does a cookie list contain a known clearance name?
pub fn has_clearance(cookies: &[CookieRecord]) -> bool {
    cookies
        .iter()
        .any(|c| CLEARANCE_NAMES.contains(&c.name.as_str()))
}

#[cfg(test)]
mod ghost_fetch_tests {
    use super::*;

    #[test]
    fn visible_text_strips_scripts_and_tags() {
        let html = r#"<html><head><script>var x = 123456789;</script><style>.a{color:red}</style></head><body><p>Hello world this is real content that should be counted.</p></body></html>"#;
        let v = visible_text_len(html);
        assert!(v > 20 && v < 100, "got {v}");
    }

    #[test]
    fn visible_text_shell_is_tiny() {
        let html = r#"<html><head><script src="app.js"></script></head><body><div id="root"></div></body></html>"#;
        assert!(visible_text_len(html) < 10);
    }
}
