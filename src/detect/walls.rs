//! Vendor-aware wall detection → honest verdicts.
//!
//! A 200 is never trusted on its own: challenge interstitials are
//! frequently served as 200 with a tiny JS shell. Detection runs on
//! status + headers + (decompressed) body markers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Cloudflare,
    DataDome,
    Akamai,
    Kasada,
    PerimeterX,
    Imperva,
    Sucuri,
    Wordfence,
    Generic,
}

#[allow(dead_code)] // full verdict surface used by MCP layer
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Real content, safe to use.
    ContentOk,
    /// Bot-wall challenge page (maybe JS-less cookie challenge, maybe
    /// full JS challenge). Vendor identified when possible.
    Challenge(Vendor),
    /// Hard block page (no path forward at this tier).
    Blocked,
    /// Login required.
    AuthWall,
    /// Paywalled.
    Paywall,
    /// 404 or content-less page dressed as success.
    SoftNotFound,
}

/// Whether an HTTP denial warrants a browser observation. Explicit tier and
/// deadline checks belong to the caller; this policy is shared by fetch/crawl.
pub fn browser_recovery(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    verdict: Verdict,
) -> bool {
    if matches!(status, 401 | 402 | 404)
        || matches!(
            verdict,
            Verdict::AuthWall | Verdict::Paywall | Verdict::SoftNotFound
        )
    {
        return false;
    }
    if let Some(ct) = header(headers, "content-type") {
        let mime = ct.split(';').next().unwrap_or_default().trim();
        if !["text/html", "text/plain", "application/xhtml+xml"]
            .iter()
            .any(|allowed| mime.eq_ignore_ascii_case(allowed))
        {
            return false;
        }
    } else {
        // A missing MIME type does not turn API JSON or binary denial data
        // into a browser document. Inspect only a bounded prefix.
        let prefix = &body[..body.len().min(512)];
        let text = String::from_utf8_lossy(prefix);
        if text.trim_start().starts_with(['{', '['])
            || prefix.starts_with(b"%PDF-")
            || prefix
                .iter()
                .any(|b| *b == 0 || (*b < 0x20 && !b.is_ascii_whitespace()))
        {
            return false;
        }
    }
    if content_gate(body).is_some() {
        return false;
    }
    matches!(verdict, Verdict::Challenge(_)) || (status == 403 && verdict == Verdict::Blocked)
}

#[cfg(test)]
mod stealth_v3_tests {
    use super::*;

    #[test]
    fn stealth_v3_kasada_bootstrap_is_not_content_or_not_found() {
        let body = b"<html><body><script>window.KPSDK={};KPSDK.start=Date.now();</script><script src='/sensor/ips.js?x-kpsdk-im=opaque'></script><iframe src='javascript:;' style='display:none'></iframe></body></html>";
        assert!(matches!(detect_dom_smart(body), Verdict::Challenge(_)));
        let article = format!(
            "<article><h1>Kasada integration</h1><p>{}</p></article><script>window.KPSDK={{}};</script><script src='/ips.js'></script>",
            "Public documentation of an embedded monitoring script. ".repeat(40)
        );
        assert_eq!(detect_dom_smart(article.as_bytes()), Verdict::ContentOk);
        assert_eq!(
            detect_dom_smart(b"<p>KPSDK and ips.js are filenames in this example.</p>"),
            Verdict::ContentOk
        );
    }

    #[test]
    fn stealth_v3_plain_html_403_can_receive_one_browser_observation() {
        let body = b"<html><h1>Forbidden</h1><p>Request denied.</p></html>";
        let verdict = detect(403, &[], body);
        assert_eq!(verdict, Verdict::Blocked);
        assert!(browser_recovery(403, &[], body, verdict));
    }

    #[test]
    fn stealth_v3_http_denial_negative_cases_stay_terminal() {
        for status in [401, 402, 404, 429, 500, 502, 503] {
            let body = b"Request denied";
            assert!(
                !browser_recovery(status, &[], body, detect(status, &[], body)),
                "status={status}"
            );
        }
        for (content_type, body) in [
            (
                "application/json",
                b"{\"error\":\"permission denied\"}".as_slice(),
            ),
            ("application/pdf", b"%PDF-1.7".as_slice()),
            (
                "text/html",
                b"<title>Sign in</title><form><input type='password'></form>".as_slice(),
            ),
        ] {
            let headers = vec![("Content-Type".into(), content_type.into())];
            assert!(
                !browser_recovery(403, &headers, body, detect(403, &headers, body)),
                "type={content_type}"
            );
        }
        let headers = vec![("Retry-After".into(), "60".into())];
        assert!(!browser_recovery(
            429,
            &headers,
            b"Too many requests",
            Verdict::Blocked
        ));
    }

    #[test]
    fn stealth_v3_late_interactive_wall_is_terminal_but_article_is_content() {
        let html = format!(
            "<title>Prove your humanity</title><script>{}</script><h1>Prove your humanity</h1><form><div class='g-recaptcha'></div></form>",
            "x".repeat(350_000)
        );
        assert!(matches!(
            detect_dom_smart(html.as_bytes()),
            Verdict::Challenge(_)
        ));
        assert!(
            human_captcha(html.as_bytes()),
            "late widget must be found in a large wall"
        );
        let article = format!(
            "<article><h1>CAPTCHA tutorial</h1><p>{}</p><div class='g-recaptcha'></div></article>",
            "This tutorial explains integration of a contact widget. ".repeat(40)
        );
        assert_eq!(detect_dom_smart(article.as_bytes()), Verdict::ContentOk);
    }

    #[test]
    fn stealth_v3_human_touch_gate_is_not_an_article_with_help_text() {
        let help = "Complete the verification shown below to continue browsing. The following advice explains how the browser can display the verification widget. ".repeat(40);
        let wall = format!(
            "<title>It needs a human touch</title><h1>It needs a human touch</h1><article><div id='px-captcha'><iframe></iframe></div><p>{help}</p></article>"
        );
        assert!(matches!(
            detect_dom_smart(wall.as_bytes()),
            Verdict::Challenge(Vendor::PerimeterX)
        ));
        assert!(human_captcha(wall.as_bytes()));
        let article = format!(
            "<title>Design needs a human touch</title><article><h1>Design needs a human touch</h1><p>{help}</p><div id='px-captcha'></div></article>"
        );
        assert_eq!(detect_dom_smart(article.as_bytes()), Verdict::ContentOk);
        let inactive = format!(
            "<title>Ordinary research article</title><h1>Ordinary research article</h1><script>let inactive = '<h1>It needs a human touch</h1><div id=px-captcha></div>';</script><article><p>{help}</p></article>"
        );
        assert_eq!(detect_dom_smart(inactive.as_bytes()), Verdict::ContentOk);
        let quoted = format!(
            "<title>It needs a human touch</title><style>#px-captcha{{display:none}}</style><script>let template = '<div id=px-captcha></div>';</script><article><h1>Research notes</h1><p>{help}</p></article>"
        );
        assert_eq!(detect_dom_smart(quoted.as_bytes()), Verdict::ContentOk);
    }
}

#[cfg(test)]
mod report_audit_tests {
    use super::*;

    #[test]
    fn report_audit_google_human_captcha_never_satisfies_the_content_oracle() {
        let help =
            "Our systems have detected unusual traffic from your computer network. ".repeat(20);
        let html = format!(
            "<script src='https://www.google.com/recaptcha/api.js'></script><form id='captcha-form' action='index'><div class='g-recaptcha'></div></form><p>{help}</p>"
        );
        assert!(visible_text_count(html.as_bytes()) > 1000);
        assert_eq!(
            detect_dom_smart(html.as_bytes()),
            Verdict::Challenge(Vendor::Generic)
        );
        assert!(human_captcha(html.as_bytes()));
        let tutorial = format!("<article><h1>Google CAPTCHA integration</h1>{html}</article>");
        assert_eq!(detect_dom_smart(tutorial.as_bytes()), Verdict::ContentOk);
    }

    #[test]
    fn report_audit_soft_notfound_and_auth_gates_are_not_content() {
        for html in [
            "<html><title>404: This page could not be found.</title><body><h1>404</h1><p>This page could not be found.</p></body></html>",
            "<html><title>Homepage Hero Thumbnail (1).jpg</title><body><h1>404: This page could not be found.</h1><p>This page could not be found.</p></body></html>",
        ] {
            assert_eq!(detect(200, &[], html.as_bytes()), Verdict::SoftNotFound);
        }
        let login = "<html><title>LinkedIn: Log In or Sign Up</title><body><h1>Welcome to your professional community</h1><form><input type='password'><button>Sign in</button></form></body></html>";
        assert_eq!(detect(200, &[], login.as_bytes()), Verdict::AuthWall);
        assert_eq!(
            detect_dom_smart(
                "<html><title>Login – Vercel</title><body><h1>Log in to Vercel</h1></body></html>"
                    .as_bytes()
            ),
            Verdict::AuthWall
        );
        // Vercel appends its title after ~340 KiB of styles/flight data;
        // the first DOM scan contains only the branded H1 and login form.
        let late_title = format!(
            "<html><head></head><body><h1>Log in to Vercel</h1><form><input type='email'></form><script>{}</script><title>Login – Vercel</title></body></html>",
            "x".repeat(350_000)
        );
        assert_eq!(detect_dom_smart(late_title.as_bytes()), Verdict::AuthWall);
        assert_eq!(detect_dom_smart(b"<h1>Log in to Vercel with CI</h1><article><p>A practical authentication tutorial with working code and deployment instructions for continuous integration.</p></article>"), Verdict::ContentOk);
        assert_eq!(detect_dom_smart(b"<article><h1>Log in to Vercel with CI</h1><p>A practical authentication tutorial with working code and deployment instructions for continuous integration.</p><form><input type='email'></form></article>"), Verdict::ContentOk);
        let late_form = format!(
            "<title>Cloudflare Dashboard | Manage Your Account</title><style>{}</style><h1>Sign in to Cloudflare</h1><form action='/login'><input type='password'><button>Sign in</button></form>",
            "x".repeat(90_000)
        );
        assert_eq!(detect_dom_smart(late_form.as_bytes()), Verdict::AuthWall);
        assert_eq!(detect(200, &[], late_form.as_bytes()), Verdict::AuthWall);
        let instagram = format!(
            "<title>Instagram</title><script>{}</script><form id='login_form'><input type='password'><button>Log in</button></form>",
            "x".repeat(450_000)
        );
        assert_eq!(detect(200, &[], instagram.as_bytes()), Verdict::AuthWall);
        assert_eq!(detect_dom_smart(b"<title>Login form tutorial</title><article><h1>Authentication examples</h1><p>A public tutorial with examples of client and server authentication.</p><form id='login_form'><input type='password'><button>Log in</button></form></article>"), Verdict::ContentOk);
        let article = "<html><title>How to handle a 404 or sign in</title><body><article><h1>Authentication guide</h1><p>Our tutorial explains the message 404: This page could not be found.</p><form><input type='password'></form></article></body></html>";
        assert_eq!(
            detect(200, &[], article.as_bytes()),
            Verdict::ContentOk,
            "quoted errors and embedded sign-in forms must remain content"
        );
    }
}

pub fn detect(status: u16, headers: &[(String, String)], body: &[u8]) -> Verdict {
    let server = header(headers, "server").unwrap_or_default().to_lowercase();
    let cf_ray = header(headers, "cf-ray").is_some();
    // cf-mitigated: challenge is Cloudflare's explicit challenge
    // declaration header on block responses (glassdoor 2026-09).
    let cf_mitigated = header(headers, "cf-mitigated")
        .map(|v| v.to_lowercase().contains("challenge"))
        .unwrap_or(false);
    let is_cf = server.contains("cloudflare") || cf_ray || cf_mitigated;
    // Challenge markers live in the title/head : scanning
    // the whole body false-positives on articles that merely
    // MENTION a vendor (a Wikipedia page about Akamai).
    // Error statuses (403/429/503) get the wide window: their
    // bodies ARE block pages, and vendors like Cloudflare put
    // the markers at the BOTTOM of a large bilingual shell
    // (glassdoor's 403: nearest marker at byte 126k). Content
    // pages keep the narrow window. `true` for error statuses
    // below mirrors the call sites in this fn.
    let wide = matches!(status, 403 | 429 | 503);
    let scan = &body[..body.len().min(if wide { 256 * 1024 } else { 64 * 1024 })];
    let text = String::from_utf8_lossy(scan).to_lowercase();

    match status {
        401 | 402 => return Verdict::AuthWall,
        404 => return Verdict::SoftNotFound,
        403 | 429 | 503 => {
            return classify_wall(&text, headers, is_cf, status, true);
        }
        _ => {}
    }

    if (200..300).contains(&status) {
        // Binary bodies (PDFs, images, archives) never carry HTML
        // challenge markers : marker-scanning their lossy-decoded
        // bytes is how an arXiv PDF behind Cloudflare ("attention
        // required" occurring inside the paper text, plus a cf-ray
        // header) got false-flagged as Blocked at HTTP 200. Bot
        // walls speak HTML; if the body is a PDF or another binary
        // format, wall detection has nothing to say. Honest
        // verdicts for these come from the binary guard (reject)
        // or DonSheet (PDF parse) downstream.
        if body.starts_with(b"%PDF-") || crate::fetch::guards::is_binary_body(body) {
            return Verdict::ContentOk;
        }
        if let Some(verdict) = content_gate(body) {
            return verdict;
        }
        // Interstitials dressed as 200. Body markers only
        // count on SMALL pages: interstitials are tiny,
        // while real pages (a Bing SERP, an article about
        // Cloudflare) mention vendors in passing : the
        // lesson the ghost oracle learned first.
        let allow_body_markers = scan.len() < 32 * 1024;
        let v = classify_wall(&text, headers, is_cf, status, allow_body_markers);
        if v != Verdict::ContentOk {
            return v;
        }
        // Title/structure-based interstitial detection: catches the
        // modern CF class ("Performing security verification") whose
        // bodies don't always carry the classic script markers in the
        // first bytes but always carry the boilerplate title.
        if let Some(vid) = detect_interstitial(body) {
            return Verdict::Challenge(vid);
        }
        return Verdict::ContentOk;
    }

    // Any other status (4xx/5xx not specifically handled above)
    // is a server error, not content. Previously this fell through
    // to ContentOk, causing 400/500/502 etc. to be treated as
    // successful fetches : the agent would trust error pages as
    // real content.
    Verdict::Blocked
}

/// Detect wall from a ghost-rendered DOM (no HTTP headers).
/// Always checks body markers : the DOM is already rendered,
/// so challenge markers in the HTML are real, not false
/// positives from CSS class names mentioning a vendor.
/// Scans first 64KB (challenge markers live in <head>).
///
/// Unlike `detect`, this doesn't gate body markers on page size:
/// ghost DOMs are rendered, so large DOMs with challenge markers
/// are genuinely challenged (Amazon's 51KB block page).
/// Also strips <style>/<script> before checking for "skeleton"
/// and other markers that appear in CSS class names.
pub fn detect_dom(body: &[u8]) -> Verdict {
    let scan = &body[..body.len().min(64 * 1024)];
    let text = String::from_utf8_lossy(scan).to_lowercase();
    classify_wall(&text, &[], false, 200, true)
}

/// Smart DOM detection for ghost-rendered pages: considers
/// visible text content before challenge markers.
///
/// A real page with an embedded challenge widget (Cloudflare
/// Turnstile on a contact form, DataDome monitoring script on
/// a Forbes article) contains challenge markers but also has
/// substantial visible text. `detect_dom` alone would classify
/// these as Challenge, causing the ghost to never settle and
/// eventually return captcha=true.
///
/// This function first checks visible text: if the page has
/// ≥ 80 non-whitespace chars outside scripts/styles, it's real
/// content : return ContentOk regardless of challenge markers.
/// Only when the page is visually empty (< 80 visible chars)
/// does it fall back to `detect_dom` for challenge detection.
///
/// Challenge interstitials (CF, DataDome, PX) always have
/// < 80 visible chars : they're mostly JS/HTML structure.
/// The Amazon 51KB block page has ~50 visible chars.
/// Real pages have 80+ visible chars even when they embed
/// challenge widgets in a small section.
pub fn detect_dom_smart(body: &[u8]) -> Verdict {
    if let Some(verdict) = content_gate(body) {
        return verdict;
    }
    // Interstitials first: the ≥80-visible-chars override below
    // must never whitewash a challenge page. Modern CF interstitials
    // ("Performing security verification") carry 300-400 chars of
    // vendor boilerplate : enough to pass the old visible-text gate
    // and get served as content.
    if let Some(v) = detect_interstitial(body) {
        return Verdict::Challenge(v);
    }
    let visible = visible_text_count(body);
    if visible >= 80 {
        return Verdict::ContentOk;
    }
    detect_dom(body)
}

/// Error and authentication headings, excluding scripts' inactive route templates.
pub fn content_gate(body: &[u8]) -> Option<Verdict> {
    // SPA style/bootstrap data can precede the login form by hundreds of KiB.
    // Wide DOM parsing is only needed for pages with little visible content;
    // substantive articles keep the inexpensive heading window.
    let scan = &body[..body.len().min(512 * 1024)];
    let scan = if scan.len() > 64 * 1024 && visible_text_count(scan) > 1200 {
        &scan[..64 * 1024]
    } else {
        scan
    };
    let text = String::from_utf8_lossy(scan);
    let lower = text.to_lowercase();
    if !["404", "not found", "log in", "login", "sign in"]
        .iter()
        .any(|needle| lower.contains(needle))
        || crate::extract::nesting::max_nesting(&text) > crate::extract::nesting::MAX_NESTING
    {
        return None;
    }
    let doc = scraper::Html::parse_document(&text);
    let title = doc
        .select(&scraper::Selector::parse("title").unwrap())
        .next()
        .map(crate::extract::inline::visible_text)
        .unwrap_or_default()
        .to_lowercase();
    let h1 = doc
        .select(&scraper::Selector::parse("h1").unwrap())
        .next()
        .map(crate::extract::inline::visible_text)
        .unwrap_or_default()
        .to_lowercase();
    if [&title, &h1].iter().any(|heading| {
        heading.as_str() == "404"
            || heading.starts_with("404:")
            || heading.starts_with("404 -")
            || heading.as_str() == "page not found"
            || heading.as_str() == "this page could not be found."
    }) {
        return Some(Verdict::SoftNotFound);
    }
    if doc.select(&scraper::Selector::parse("form#login_form input[type='password'], form[name='login-form'] input[type='password']").unwrap()).next().is_some()
        && doc.select(&scraper::Selector::parse("article").unwrap()).next().is_none() {
        return Some(Verdict::AuthWall);
    }
    let auth_heading = [
        "sign in",
        "log in",
        "login",
        "sign in to continue",
        "log in to continue",
        "log in or sign up",
    ];
    let title_suffix = title.rsplit(':').next().unwrap_or(&title).trim();
    let title_prefix = [" – ", " - ", " | "]
        .iter()
        .find_map(|separator| title.split_once(separator).map(|(prefix, _)| prefix.trim()));
    if auth_heading.contains(&title_suffix)
        || auth_heading.contains(&h1.as_str())
        || title_prefix.is_some_and(|prefix| auth_heading.contains(&prefix))
        || (["sign in to ", "log in to ", "login to "]
            .iter()
            .any(|prefix| h1.starts_with(prefix))
            && doc
                .select(&scraper::Selector::parse("article").unwrap())
                .next()
                .is_none()
            && doc
                .select(&scraper::Selector::parse("form").unwrap())
                .next()
                .is_some())
    {
        return Some(Verdict::AuthWall);
    }
    None
}

/// Interstitial titles/phrases that vendor challenge pages use in
/// `<title>` / `<h1>`. Real pages virtually never title themselves
/// these : a page ABOUT Cloudflare has its own title.
const INTERSTITIAL_TITLES: &[&str] = &[
    "you've been blocked by network security",
    "you are blocked by network security",
    "just a moment",
    "performing security verification",
    "checking your browser",
    "verifying your browser",
    "attention required",
    "verify you are human",
    "verify that you are human",
    "verifying you are human",
    "prove your humanity",
    "security check",
    "needs to review the security",
    "one more step",
    "checking if the site connection is secure",
    "please wait...",
    "access denied",
];

/// Challenge-page script/iframe markers (URL fragments, not prose).
const INTERSTITIAL_MARKERS: &[&str] = &[
    "challenge-platform",
    "cf-chl",
    "captcha-delivery.com",
    "px-captcha",
    "_Incapsula_Resource",
];

/// Strong interstitial detection: a page whose TITLE or first H1 is
/// vendor challenge boilerplate, or a near-empty DOM (< 400 visible
/// chars) that loads a challenge script and has no form. Returns the
/// vendor when the page is an interstitial.
///
/// This is the layer that keeps the ghost oracle honest: a rendered
/// "Just a moment..." page must never satisfy the content-quality
/// oracle, no matter how many visible chars its boilerplate carries.
pub fn detect_interstitial(body: &[u8]) -> Option<Vendor> {
    let scan = &body[..body.len().min(96 * 1024)];
    let text = String::from_utf8_lossy(scan).to_lowercase();

    // Google's human CAPTCHA contains over 1000 visible chars of help text.
    // Its actual form is decisive; the text-count oracle must not serve it.
    if text.contains("unusual traffic") && text.contains("recaptcha") {
        let doc = scraper::Html::parse_document(&text);
        if doc
            .select(&scraper::Selector::parse("form#captcha-form").unwrap())
            .next()
            .is_some()
            && doc
                .select(&scraper::Selector::parse("article").unwrap())
                .next()
                .is_none()
        {
            return Some(Vendor::Generic);
        }
    }

    // Title/H1 route: strongest signal, immune to visible-text counts.
    if interstitial_heading(&text) {
        return Some(vendor_from_markers(&text).unwrap_or(Vendor::Generic));
    }

    // Near-empty route: tiny visible text + challenge script + no
    // form (a login/contact page with a Turnstile widget has BOTH a
    // form and real visible text : it must not match).
    // `challenges.cloudflare.com` is not a marker on its own: it also
    // serves Turnstile's public embed (`/turnstile/v0/api.js`), which
    // a short real page keeps loading after the challenge is passed
    // (scrapingcourse.com's solved page). It counts only with a
    // widget on the page, the bare `cf-turnstile` shell.
    let turnstile_shell =
        text.contains("challenges.cloudflare.com") && text.contains("cf-turnstile");
    let visible = visible_text_count(body);
    if visible < 80
        && text.contains("<script")
        && text.contains("kpsdk")
        && text.contains("/ips.js")
        && !text.contains("<article")
    {
        return Some(Vendor::Kasada);
    }
    if visible < 400
        && (turnstile_shell || INTERSTITIAL_MARKERS.iter().any(|m| text.contains(m)))
        && !text.contains("<form")
        && !text.contains("<input")
    {
        return Some(vendor_from_markers(&text).unwrap_or(Vendor::Generic));
    }
    None
}

/// True when the DOM carries an interactive captcha widget
/// (reCAPTCHA, hCaptcha, Turnstile, PerimeterX, DataDome, GeeTest,
/// Arkose). Distinguishes "a human must solve this" from "a
/// challenge page that never finished": different recoveries, and
/// different failure codes (issue #282's `wall.captcha` vs
/// `wall.challenge_unsolved`).
pub fn interactive_captcha(body: &[u8]) -> bool {
    let scan = &body[..body.len().min(512 * 1024)];
    let text = String::from_utf8_lossy(scan).to_lowercase();
    [
        "recaptcha",
        "hcaptcha",
        "cf-turnstile",
        "px-captcha",
        "captcha-delivery",
        "geetest",
        "arkoselabs",
        "funcaptcha",
    ]
    .iter()
    .any(|m| text.contains(m))
}

/// A widget requiring an interactive human answer, rather than a challenge
/// the browser can clear with its own first-party state. Call only after the
/// page was classified as a captcha, never on ordinary embedded widgets.
pub fn human_captcha(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&body[..body.len().min(512 * 1024)]).to_lowercase();
    [
        "hcaptcha",
        "recaptcha",
        "px-captcha",
        "captcha-delivery",
        "geetest",
        "arkoselabs",
        "funcaptcha",
    ]
    .iter()
    .any(|m| text.contains(m))
}

/// Extracted-text challenge test: the TEXT an extractor produced is
/// an unsolved interstitial, not content.
///
/// The DOM-level `detect_interstitial` misses shapes with a form or
/// a long title (reddit's "Prove your humanity" page: a real
/// recaptcha form and 300 chars of prose-like vendor boilerplate),
/// so the extraction step shipped it as ContentOk (issue #282 case
/// B). Detection here is marker + size: an interstitial's own words
/// are a handful of lines, so a long article that merely mentions a
/// challenge phrase stays content.
pub fn challenge_text(text: &str) -> bool {
    if text.chars().count() > 1200 {
        return false;
    }
    let lower = text.to_lowercase();
    lower.lines().any(|line| {
        let line = line.trim().trim_start_matches('#').trim();
        INTERSTITIAL_TITLES.iter().any(|m| line.starts_with(m))
            || line.starts_with("complete the challenge below")
            || line.starts_with("enable javascript and cookies to continue")
    })
}

/// Short access screens, not articles quoting a login or block message.
pub fn text_wall(text: &str) -> Option<Verdict> {
    if text.chars().count() > 1200 {
        return None;
    }
    let lower = text.to_lowercase();
    if lower.contains("blocked by network security")
        && (lower.contains("reddit account") || lower.contains("developer token"))
    {
        return Some(Verdict::Blocked);
    }
    if lower.lines().any(|line| {
        let heading = line.trim().trim_start_matches('#').trim();
        [
            "log in to continue",
            "login to continue",
            "sign in to continue",
        ]
        .iter()
        .any(|phrase| heading.starts_with(phrase))
    }) && text.chars().count() < 600
    {
        return Some(Verdict::AuthWall);
    }
    challenge_text(text).then_some(Verdict::Challenge(Vendor::Generic))
}

/// Inspect both the title and first H1, including text inside nested tags.
fn interstitial_heading(lower_text: &str) -> bool {
    for (open, close) in [("<title", "</title>"), ("<h1", "</h1>")] {
        let Some(start) = lower_text.find(open) else {
            continue;
        };
        if let Some(content_start) = lower_text[start..].find('>') {
            let from = start + content_start + 1;
            if let Some(end) = lower_text[from..].find(close) {
                let t = &lower_text[from..from + end];
                // Strip nested tags inside the title (h1 can wrap spans).
                let mut inside_tag = false;
                let cleaned: String = t
                    .chars()
                    .filter(|c| match c {
                        '<' => {
                            inside_tag = true;
                            false
                        }
                        '>' => {
                            inside_tag = false;
                            false
                        }
                        _ => !inside_tag,
                    })
                    .collect();
                // A PX human gate can put thousands of translated help
                // characters inside <article>. Require the observed gate
                // heading and a real widget element, not an inactive script.
                if cleaned.trim() == "it needs a human touch"
                    && scraper::Html::parse_document(lower_text)
                        .select(&scraper::Selector::parse("#px-captcha").unwrap())
                        .next()
                        .is_some()
                {
                    return true;
                }
                if INTERSTITIAL_TITLES.iter().any(|m| cleaned.contains(m)) {
                    return true;
                }
            }
        }
    }
    false
}

fn vendor_from_markers(lower_text: &str) -> Option<Vendor> {
    if lower_text.contains("challenges.cloudflare.com")
        || lower_text.contains("challenge-platform")
        || lower_text.contains("cf-chl")
        || lower_text.contains("cloudflare")
        || lower_text.contains("turnstile")
    {
        return Some(Vendor::Cloudflare);
    }
    if lower_text.contains("captcha-delivery.com") || lower_text.contains("datadome") {
        return Some(Vendor::DataDome);
    }
    if lower_text.contains("px-captcha") || lower_text.contains("perimeterx") {
        return Some(Vendor::PerimeterX);
    }
    if lower_text.contains("_incapsula_resource") || lower_text.contains("incapsula") {
        return Some(Vendor::Imperva);
    }
    None
}

/// Fast visible-text estimate: strip tags + script/style/noscript
/// bodies, count non-whitespace characters. No lowercasing, no
/// DOM : byte scan. Shared with callers that need shell evidence
/// (a big body with almost no visible text is a JS shell).
pub fn visible_text_count(html: &[u8]) -> usize {
    let b = html;
    let mut n = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'<' => {
                // Skip script/style/noscript bodies entirely.
                let close: &[u8] = if starts_ci(&b[i + 1..], b"script") {
                    b"</script"
                } else if starts_ci(&b[i + 1..], b"style") {
                    b"</style"
                } else if starts_ci(&b[i + 1..], b"noscript") {
                    b"</noscript"
                } else {
                    // Not a skipped tag : skip to end of this tag.
                    while i < b.len() && b[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                };
                i = find_ci(b, close, i + 8)
                    .map(|p| p + close.len() + 1)
                    .unwrap_or(b.len());
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

fn classify_wall(
    text: &str,
    headers: &[(String, String)],
    is_cf: bool,
    status: u16,
    allow_body_markers: bool,
) -> Verdict {
    // Header-based detection is always active: a
    // cf-mitigated / x-datadome header never lies,
    // regardless of page size.
    if is_cf && (status == 403 || status == 503) {
        // CF 403/503: could be a challenge page OR a
        // WAF block / origin error. Check body for
        // challenge markers before classifying.
        if allow_body_markers
            && (text.contains("just a moment")
                || text.contains("cf-chl")
                || text.contains("challenge-platform")
                || text.contains("cf-turnstile")
                || text.contains("challenges.cloudflare.com")
                || text.contains("attention required"))
        {
            return Verdict::Challenge(Vendor::Cloudflare);
        }
        // No challenge markers : WAF block (403) or
        // origin error (503). Ghost solve won't help.
        return Verdict::Blocked;
    }
    // DataDome: the x-datadome header is present on ALL responses
    // from DataDome-protected sites (200s with real content AND 403
    // challenge pages). The header alone is NOT a wall signal :
    // DataDome runs in monitoring mode on many sites (Forbes,
    // Reddit), tagging every response but only blocking on
    // actual bot detection. The wall is:
    //   - 403/429 + x-datadome = challenge (always, regardless of body)
    //   - 200 + x-datadome + small body + datadome/captcha markers = challenge
    //   - 200 + x-datadome + large body = ContentOk (monitoring mode)
    if header(headers, "x-datadome").is_some() {
        // On error statuses, x-datadome always means challenge.
        if status == 403 || status == 429 || status == 503 {
            return Verdict::Challenge(Vendor::DataDome);
        }
        // On 200: only challenge if the body is small AND contains
        // DataDome CHALLENGE markers (not the monitoring script).
        // "datadome" alone matches js.datadome.co/tags.js (monitoring);
        // "captcha-delivery.com" or "datadome"+"captcha" = challenge.
        if (200..300).contains(&status)
            && allow_body_markers
            && (text.contains("captcha-delivery.com")
                || (text.contains("datadome") && text.contains("captcha")))
        {
            return Verdict::Challenge(Vendor::DataDome);
        }
        // 200 with x-datadome but no body markers = real content.
        // Fall through to other checks / ContentOk.
    }

    // Body markers below. On 2xx these only run for SMALL
    // pages: interstitials are tiny; large real pages
    // (Bing SERPs embed inactive turnstile scripts,
    // articles mention vendors) false-positive otherwise :
    // the lesson the ghost oracle learned first.
    if !allow_body_markers {
        return Verdict::ContentOk;
    }

    // Google: sorry/consent interstitials. "unusual traffic"
    // + recaptcha is the sorry page; /sorry/ + recaptcha is
    // its form target. Both are challenge pages, not content :
    // without this, a CAPTCHA page passes as ContentOk.
    if (text.contains("unusual traffic") && text.contains("recaptcha"))
        || (text.contains("/sorry/") && text.contains("recaptcha"))
    {
        return Verdict::Challenge(Vendor::Generic);
    }

    // Cloudflare
    if is_cf || text.contains("cf-chl") || text.contains("cloudflare") {
        if text.contains("attention required") {
            return Verdict::Blocked; // CF hard block page
        }
        if text.contains("just a moment")
            || text.contains("challenge-platform")
            || text.contains("cf-chl")
            || text.contains("cf-turnstile")
            || text.contains("challenges.cloudflare.com")
            || text.contains("performing security verification")
            || status == 403
            || status == 503
        {
            return Verdict::Challenge(Vendor::Cloudflare);
        }
    }
    // DataDome body markers: "captcha-delivery.com" is the
    // challenge-specific script URL. "datadome" alone matches
    // the monitoring script (js.datadome.co/tags.js) present on
    // ALL DataDome-protected pages, even real content.
    // Only trigger on the challenge marker, or "datadome" +
    // "captcha" together.
    if text.contains("captcha-delivery.com")
        || (text.contains("datadome") && text.contains("captcha"))
    {
        return Verdict::Challenge(Vendor::DataDome);
    }
    // Akamai: block pages carry "Reference #…" +
    // edgesuite. A bare "akamai" match false-positives on
    // articles about Akamai Technologies.
    if text.contains("reference #") && text.contains("errors.edgesuite.net")
        || text.contains("_abck")
        || header(headers, "x-akamai-transformed").is_some() && (status == 403 || status == 503)
    {
        return Verdict::Challenge(Vendor::Akamai);
    }
    // PerimeterX / HUMAN. The script/URL markers are
    // challenge-specific; a bare "perimeterx" mention also matches
    // articles ABOUT PerimeterX, so it only fires with a real block
    // signal (error status or captcha co-marker), like the Akamai
    // prose rule above.
    if text.contains("px-captcha")
        || text.contains("human-challenge")
        || text.contains("captcha.px-cloud.net")
        || (text.contains("perimeterx")
            && (status == 403 || status == 503 || text.contains("captcha")))
    {
        return Verdict::Challenge(Vendor::PerimeterX);
    }
    // Imperva / Incapsula. "_incapsula_resource" (script URL) and
    // "incapsula incident id" (every block page carries it) are
    // challenge-specific; bare "incapsula"/"imperva" prose matches
    // articles about the vendor and needs a co-signal.
    if text.contains("_incapsula_resource")
        || text.contains("incapsula incident id")
        || ((text.contains("incapsula") || text.contains("imperva"))
            && (status == 403 || status == 503 || text.contains("captcha")))
    {
        return Verdict::Challenge(Vendor::Imperva);
    }
    // Sucuri. "cloudproxy" (their script id) and the firewall page
    // title are challenge-specific; a bare "sucuri" mention matches
    // every WordPress-security article on the internet.
    if text.contains("cloudproxy")
        || text.contains("sucuri website firewall")
        || (text.contains("sucuri") && (status == 403 || status == 503))
    {
        return Verdict::Challenge(Vendor::Sucuri);
    }
    // Wordfence. Its block pages ride an error status (and the
    // footer credit "generated by wordfence" sits on real pages),
    // so bare prose alone must not score a challenge.
    if text.contains("wordfence")
        && (status == 403
            || status == 503
            || text.contains("your computer's time")
            || text.contains("blocked because"))
    {
        return Verdict::Challenge(Vendor::Wordfence);
    }
    // Generic challenge signals on error statuses.
    if status == 403 || status == 503 || status == 429 {
        if header(headers, "set-cookie").is_some() {
            return Verdict::Challenge(Vendor::Generic); // cookie-warm retry candidate
        }
        if text.contains("captcha") || text.contains("are you a robot") || text.contains("bot") {
            return Verdict::Challenge(Vendor::Generic);
        }
        return Verdict::Blocked;
    }
    // Reddit's current JS-challenge (2026): a nonce form that
    // auto-submits itself on a page with essentially no prose.
    // Nothing but a browser passes it, so HTTP must not score it
    // as content (live case: the whole r/technology shell).
    if text.len() < 16_384 && text.contains("document.forms[0].submit()") && text.contains("nonce")
    {
        return Verdict::Challenge(Vendor::Generic);
    }
    // Amazon's shopping gate: "click the button below to
    // continue shopping" plus nothing else is a bot wall, not
    // a product page.
    if text.len() < 16_384 && text.contains("click the button below to continue shopping") {
        return Verdict::Challenge(Vendor::Generic);
    }
    // Instagram's logged-out shell: a sign-up gate. The profile
    // content is behind login BY DESIGN, so the verdict is Login,
    // never Challenge (there is nothing to solve). Escalating to
    // the ghost just renders the same wall and fails with "no
    // real content was extractable" (live case: /cristiano/).
    if text.len() < 12_000
        && (text.contains("log in to see photos and videos from friends")
            || text.contains("sign up to see photos and videos"))
    {
        return Verdict::AuthWall;
    }
    // Spinner shells: a page whose entire visible text is a
    // "please wait"-style spinner is a challenge shell, never
    // real content. Judge the VISIBLE text, not raw length:
    // SPA hosts keep megabytes of JS around a two-line shell
    // (live case: tiktok's 12-token "Please wait..." fetch).
    if text.len() < 32_768
        && visible_text_count(text.as_bytes()) < 120
        && (text.contains("please wait") || text.contains("one moment"))
    {
        return Verdict::Challenge(Vendor::Generic);
    }
    // Reddit-style interstitials (often served as 200).
    if (text.contains("prove your humanity")
        || text.contains("not for bots")
        || text.contains("please wait for verification"))
        && (interstitial_heading(text) || visible_text_count(text.as_bytes()) < 120)
    {
        return Verdict::Challenge(Vendor::Generic);
    }
    // Small 200-page captchas (Mojeek et al.): a real page
    // is never this tiny with a challenge form on it.
    if text.len() < 16_384
        && text.contains("captcha")
        && (text.contains("verification") || text.contains("challenge") || text.contains("robot"))
    {
        return Verdict::Challenge(Vendor::Generic);
    }
    // Small 200-page bot-check interstitials without a captcha
    // form. IMDB, Amazon, and other server-side bot detection:
    // "verify that you're not a robot" + "JavaScript is disabled".
    // A real page is never this small with these phrases.
    if text.len() < 16_384 && text.contains("verify") && text.contains("robot") {
        return Verdict::Challenge(Vendor::Generic);
    }
    // "JavaScript is disabled" + "not a robot" on a tiny page.
    if text.len() < 16_384 && text.contains("javascript is disabled") && text.contains("robot") {
        return Verdict::Challenge(Vendor::Generic);
    }
    // Cloudflare's bare JS-shell 200: "Enable JavaScript and
    // cookies to continue". The 50-case report's exact example
    // of a response that must never count as successful. The
    // "cookies to continue" co-marker keeps normal <noscript>
    // advice ("enable JavaScript for the best experience") out.
    if text.len() < 16_384
        && text.contains("enable javascript")
        && text.contains("cookies to continue")
    {
        return Verdict::Challenge(if is_cf {
            Vendor::Cloudflare
        } else {
            Vendor::Generic
        });
    }
    Verdict::ContentOk
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wave450_short_quoted_challenge_article_is_content_at_both_layers() {
        let text = "# Browser troubleshooting\n\nThe message prove your humanity asks a reader to authenticate. Here is how to diagnose that prompt without mistaking a troubleshooting article for the prompt itself.";
        assert!(!challenge_text(text));
        assert_eq!(text_wall(text), None);
        let html = format!(
            "<html><title>Browser troubleshooting</title><article><h1>Browser troubleshooting</h1><p>{text}</p></article></html>"
        );
        assert_eq!(detect(200, &[], html.as_bytes()), Verdict::ContentOk);
        assert_eq!(detect_dom_smart(html.as_bytes()), Verdict::ContentOk);
        assert!(challenge_text(
            "# Prove your humanity\nComplete the challenge below."
        ));
    }

    #[test]
    fn wave450_wall_heading_survives_generic_title_and_nested_tags() {
        let html = b"<html><title>Reddit</title><body><h1><span>Prove your humanity</span></h1><form><p>We are committed to safety and security. Complete the challenge below before continuing to the community discussion.</p><div class='g-recaptcha'></div></form></body></html>";
        assert!(matches!(detect_dom_smart(html), Verdict::Challenge(_)));
        let article = b"<html><title>Contact us</title><h1>Contact</h1><form><p>Please use this form to contact our team about product documentation and installation instructions.</p><div class='g-recaptcha'></div></form></html>";
        assert_eq!(detect_dom_smart(article), Verdict::ContentOk);
    }

    #[test]
    fn wave450_short_authentication_article_is_not_a_login_wall() {
        assert_eq!(
            text_wall(
                "# Authentication guide\n\nThe message log in to continue prompts readers to authenticate. Here is how to configure login for your application."
            ),
            None
        );
        assert_eq!(
            text_wall("# Log in to continue\n\nEnter your account credentials."),
            Some(Verdict::AuthWall)
        );
    }

    // #282: the two content-quality failures keep their own codes so
    // an agent can tell "the page is a shell/login wall" from "the
    // challenge never cleared".
    #[test]
    fn interactive_captcha_tells_widgets_from_bare_challenges() {
        assert!(interactive_captcha(
            b"<html><script src=\"https://www.google.com/recaptcha/api.js\"></script></html>"
        ));
        assert!(interactive_captcha(b"<div class=\"cf-turnstile\"></div>"));
        assert!(!interactive_captcha(
            b"<html><head><title>Checking your browser before accessing</title></head><body><p>This process is automatic.</p></body></html>"
        ));
    }

    // #282: the extracted-TEXT test. A challenge interstitial whose
    // prose is 100+ chars sailed past the DOM-level detectors (it
    // has a real form; its title is not a vendor phrase) and the
    // extraction step served it as ContentOk.
    #[test]
    fn challenge_text_catches_extracted_interstitials() {
        assert!(challenge_text(
            "# Verifying your browser…\n\nThis may take a few seconds. Please do not close this tab."
        ));
        assert!(challenge_text(
            "# Prove your humanity\n\nWe're committed to safety and security. But not for bots. Complete the challenge below."
        ));
        assert!(challenge_text("Just a moment..."));
        // A long article that merely mentions a phrase stays content.
        let long = format!(
            "Verifying your browser is a phrase this essay quotes repeatedly. {}",
            "More words follow here to push the text past the size bound. ".repeat(30)
        );
        assert!(!challenge_text(&long));
        // And a page with no marker is untouched.
        assert!(!challenge_text(
            "# Example Domain\n\nThis domain is for use in illustrative examples."
        ));
    }

    // A prose mention of a security vendor on a healthy 200 page is
    // an article ABOUT the vendor, not a wall. The bare-name rules
    // used to fire here and burn a warm retry + route-memory failure
    // + a paid bypass on the next fetch.
    #[test]
    fn vendor_prose_on_200_is_content() {
        for name in [
            "perimeterx",
            "imperva",
            "incapsula",
            "sucuri",
            "wordfence",
            "generated by wordfence",
        ] {
            let body = format!(
                "<html><body><h1>How {name} works</h1><p>Web application firewalls \
                 explained in depth, with diagrams and config examples for admins. \
                 This paragraph pads the article past any tiny-page heuristics.</p></body></html>"
            );
            let v = detect(200, &[], body.as_bytes());
            assert!(
                matches!(v, Verdict::ContentOk),
                "prose mention of {name} on a 200 must stay ContentOk, got {v:?}"
            );
        }
    }

    // The challenge-specific markers still fire on the exact pages
    // they exist to catch.
    #[test]
    fn real_vendor_block_markers_still_fire() {
        let cases: [(&str, &str, u16, Vendor); 5] = [
            (
                "px-captcha",
                "<html><script src=\"https://captcha.px-cloud.net/px.js\"></script><div id=\"px-captcha\"></div></html>",
                200,
                Vendor::PerimeterX,
            ),
            (
                "_incapsula_resource",
                "<html><script src=\"/_Incapsula_Resource?SWRGLO\"></script></html>",
                200,
                Vendor::Imperva,
            ),
            (
                "incapsula incident id",
                "<html><body>Request unsuccessful. Incapsula incident ID: 12345678012</body></html>",
                200,
                Vendor::Imperva,
            ),
            (
                "cloudproxy",
                "<html><script>var _SUCURI = {\"cloudproxy\":\"abc\"}</script></html>",
                200,
                Vendor::Sucuri,
            ),
            (
                "wordfence 403",
                "<html><body><h1>Wordfence Firewall</h1><p>Blocked because your access was limited.</p></body></html>",
                403,
                Vendor::Wordfence,
            ),
        ];
        for (name, body, status, vendor) in cases {
            let v = detect(status, &[], body.as_bytes());
            assert!(
                matches!(v, Verdict::Challenge(ref got) if std::mem::discriminant(got) == std::mem::discriminant(&vendor)),
                "{name} block page must score Challenge({vendor:?}), got {v:?}"
            );
        }
    }

    #[test]
    fn large_serp_with_vendor_mentions_is_content() {
        let body = include_bytes!("../../tests/fixtures/bing-serp.html").to_vec();
        let v = detect(200, &[], &body);
        assert!(matches!(v, Verdict::ContentOk), "got {v:?}");
    }

    #[test]
    fn small_captcha_page_is_challenge() {
        let body = include_bytes!("../../tests/fixtures/mojeek-captcha.html").to_vec();
        let v = detect(200, &[], &body);
        assert!(matches!(v, Verdict::Challenge(_)), "got {v:?}");
    }

    #[test]
    fn imdb_bot_check_page_is_challenge() {
        // IMDB serves this tiny page when it detects a bot:
        // "JavaScript is disabled / verify that you're not a robot"
        let body = b"<html><noscript>JavaScript is disabled In order to continue, we need to verify that you're not a robot. This requires JavaScript. Enable JavaScript and then reload the page.</noscript></html>";
        let v = detect_dom(body);
        assert!(matches!(v, Verdict::Challenge(_)), "got {v:?}");
    }

    #[test]
    fn forbes_200_with_datadome_header_is_content() {
        // Forbes returns x-datadome: protected on ALL responses
        // (200s with full 1.3MB articles AND 403 challenge pages).
        // The header alone is NOT a wall : DataDome runs in
        // monitoring mode. A 200 with a large body is ContentOk.
        let body = vec![b'<'; 1_300_000]; // 1.3MB of content
        let headers = vec![
            ("x-datadome".into(), "protected".into()),
            ("content-type".into(), "text/html".into()),
        ];
        let v = detect(200, &headers, &body);
        assert!(
            matches!(v, Verdict::ContentOk),
            "got {v:?} : Forbes 200 with x-datadome + large body must be ContentOk"
        );
    }

    #[test]
    fn forbes_403_with_datadome_header_is_challenge() {
        // When Forbes DOES block (403), x-datadome means challenge.
        let body = b"<html>DataDome challenge</html>";
        let headers = vec![("x-datadome".into(), "protected".into())];
        let v = detect(403, &headers, body);
        assert!(
            matches!(v, Verdict::Challenge(Vendor::DataDome)),
            "got {v:?}"
        );
    }

    #[test]
    fn datadome_200_small_body_with_markers_is_challenge() {
        // A small 200 page with datadome challenge markers IS a challenge
        // interstitial (captcha-delivery.com is the challenge script).
        let body = b"<html><body>datadome captcha-delivery.com challenge</body></html>";
        let headers = vec![("x-datadome".into(), "protected".into())];
        let v = detect(200, &headers, body);
        assert!(
            matches!(v, Verdict::Challenge(Vendor::DataDome)),
            "got {v:?}"
        );
    }

    #[test]
    fn datadome_200_small_body_monitoring_script_is_content() {
        // A small 200 page with x-datadome header and the DataDome
        // monitoring script (js.datadome.co/tags.js) but NO challenge
        // markers = real content (DataDome in monitoring mode).
        let body = b"<html><head><script src=\"https://js.datadome.co/tags.js\"></script></head><body><p>Real article content about technology news today.</p></body></html>";
        let headers = vec![("x-datadome".into(), "protected".into())];
        let v = detect(200, &headers, body);
        assert!(
            matches!(v, Verdict::ContentOk),
            "got {v:?} : monitoring script must not trigger challenge"
        );
    }

    #[test]
    fn datadome_200_small_body_no_markers_is_content() {
        // A small 200 page with x-datadome header but NO datadome/captcha
        // body markers = real content (DataDome in monitoring mode).
        let body =
            b"<html><body><p>Real article content about technology news today.</p></body></html>";
        let headers = vec![("x-datadome".into(), "protected".into())];
        let v = detect(200, &headers, body);
        assert!(matches!(v, Verdict::ContentOk), "got {v:?}");
    }

    #[test]
    fn detect_dom_smart_real_page_with_turnstile_is_content() {
        // A real page with an embedded Cloudflare Turnstile widget
        // (contact form, login page) has challenge markers but also
        // substantial visible text. detect_dom_smart must return ContentOk.
        let body = b"<html><head><script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\"></script></head><body><h1>Contact Us</h1><p>Fill out the form below and we will get back to you within 24 hours. Our team is dedicated to providing the best possible support for all your inquiries.</p><div class=\"cf-turnstile\"></div><form><input name=\"email\"><textarea name=\"message\"></textarea><button>Send</button></form></body></html>";
        let v = detect_dom_smart(body);
        assert!(
            matches!(v, Verdict::ContentOk),
            "got {v:?} : page with Turnstile widget + real content must be ContentOk"
        );
    }

    #[test]
    fn detect_dom_smart_challenge_interstitial_is_challenge() {
        // A challenge interstitial has < 80 visible chars : detect_dom_smart
        // falls back to detect_dom and correctly identifies the challenge.
        let body = b"<html><head><script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\"></script></head><body><div class=\"cf-turnstile\"></div></body></html>";
        let v = detect_dom_smart(body);
        assert!(
            matches!(v, Verdict::Challenge(_)),
            "got {v:?} : challenge interstitial must be Challenge"
        );
    }

    #[test]
    fn detect_500_is_blocked_not_content() {
        // A 500 status code should NOT be ContentOk : it's a server error.
        let body = b"<html><body>500 Internal Server Error</body></html>";
        let v = detect(500, &[], body);
        assert!(
            matches!(v, Verdict::Blocked),
            "got {v:?} : 500 must be Blocked, not ContentOk"
        );
    }

    #[test]
    fn detect_400_is_blocked_not_content() {
        // A 400 status code should NOT be ContentOk.
        let body = b"<html><body>400 Bad Request</body></html>";
        let v = detect(400, &[], body);
        assert!(
            matches!(v, Verdict::Blocked),
            "got {v:?} : 400 must be Blocked"
        );
    }

    #[test]
    fn detect_502_is_blocked_not_content() {
        // A 502 Bad Gateway should NOT be ContentOk.
        let body = b"<html><body>502 Bad Gateway</body></html>";
        let v = detect(502, &[], body);
        assert!(
            matches!(v, Verdict::Blocked),
            "got {v:?} : 502 must be Blocked"
        );
    }

    #[test]
    fn turnstile_generic_word_does_not_trigger_challenge() {
        // The word "turnstile" alone (without cf-turnstile or
        // challenges.cloudflare.com) should NOT trigger a challenge.
        let body = b"<html><body><h1>Turnstile Documentation</h1><p>This page discusses the turnstile feature in detail and how it works with various configurations.</p></body></html>";
        let v = detect(200, &[], body);
        assert!(
            matches!(v, Verdict::ContentOk),
            "got {v:?} : bare 'turnstile' word must not trigger challenge"
        );
    }

    #[test]
    fn pdf_body_with_wall_markers_is_content() {
        // The live arXiv bug: an HTTP 200 PDF behind Cloudflare
        // (cf-ray header present, so is_cf=true) whose paper text
        // contains "attention required" : the ONLY path to
        // Blocked on a 200 : must parse as content. Body is
        // deliberately < 32KB so the marker scan WOULD run were
        // the binary gate absent.
        let mut body = Vec::new();
        body.extend_from_slice(b"%PDF-1.5\n%\xe2\xe3\xcf\xd3\n");
        body.extend_from_slice(
            b"1 0 obj << /Type /Catalog >> endobj\nstream\nattention required | cloudflare ray id ",
        );
        body.extend_from_slice(b"captcha verification robot challenge just a moment\nendstream");
        let headers = vec![
            ("server".to_string(), "cloudflare".to_string()),
            ("cf-ray".to_string(), "8fa1deadbeef".to_string()),
        ];
        let v = detect(200, &headers, &body);
        assert!(
            matches!(v, Verdict::ContentOk),
            "got {v:?} : a real PDF must never be wall-classified"
        );
    }

    #[test]
    fn binary_image_body_with_captcha_metadata_is_content() {
        // Same class of false positive: a PNG whose EXIF/text
        // chunk mentions "captcha" + "verification" on a 200
        // must not be Challenge. (Null-byte heuristic also
        // covers arbitrary binaries.)
        let mut body = Vec::new();
        body.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        body.extend_from_slice(&[0u8; 64]);
        body.extend_from_slice(b"captcha verification are you a robot");
        let v = detect(200, &[], &body);
        assert!(
            matches!(v, Verdict::ContentOk),
            "got {v:?} : binary bodies are not wall pages"
        );
    }

    #[test]
    fn pdf_status_403_still_classifies_as_wall() {
        // The gate only applies to 2xx: a PDF-flavored body on a
        // 403 is still a wall/block decision (e.g. a CDN serving
        // an error PDF).
        let body = b"%PDF-1.5 denied";
        let v = detect(403, &[], body);
        assert!(
            !matches!(v, Verdict::ContentOk),
            "got {v:?} : non-2xx must not become content via the binary gate"
        );
    }

    #[test]
    fn cf_enable_javascript_cookies_shell_is_challenge() {
        // The 50-case report: "Do not call a response successful
        // when it only contains 'Enable JavaScript and cookies
        // to continue'". Cloudflare 200 shell.
        let body = b"<html><body><h1>Please Enable JavaScript and Cookies to continue</h1><p>This site requires JavaScript and cookies to run.</p></body></html>";
        let headers = vec![("server".to_string(), "cloudflare".to_string())];
        let v = detect(200, &headers, body);
        assert!(
            matches!(v, Verdict::Challenge(Vendor::Cloudflare)),
            "got {v:?} : JS-only shell must never be ContentOk"
        );
    }

    #[test]
    fn noscript_advice_is_still_content() {
        // Ordinary <noscript> "enable JavaScript for the best
        // experience" advice on a real page stays content : the
        // "cookies to continue" co-marker is required.
        let body = b"<html><head><noscript>For the best experience enable JavaScript in your browser settings.</noscript></head><body><h1>Real Article</h1><p>Substantial real body text that is definitely present on this actual page and makes it a real page with content on it.</p></body></html>";
        let v = detect(200, &[], body);
        assert!(matches!(v, Verdict::ContentOk), "got {v:?}");
    }

    // ── interstitial detection (v2.2: the fake-solve fix) ──

    #[test]
    fn cf_performing_security_verification_is_interstitial() {
        // The live allthedifferences.com case: modern Cloudflare
        // interstitial with ~344 visible chars of vendor boilerplate
        // that used to pass the visible-text oracle as "content".
        let body = b"<html><head><title>Just a moment...</title></head><body><div><h1>Just a moment...</h1><noscript>Enable JavaScript and cookies to continue</noscript><p>Performing security verification</p><p>This website uses a security service to protect against malicious bots. This page is displayed while the website verifies you are not a bot.</p><script src=\"https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1\"></script></div></body></html>";
        assert!(detect_interstitial(body).is_some());
        let v = detect_dom_smart(body);
        assert!(
            matches!(v, Verdict::Challenge(Vendor::Cloudflare)),
            "got {v:?} : interstitial with visible text must be Challenge"
        );
        let v2 = detect(200, &[("server".into(), "cloudflare".into())], body);
        assert!(matches!(v2, Verdict::Challenge(_)), "got {v2:?}");
    }

    #[test]
    fn security_check_title_without_markers_is_interstitial() {
        // Interstitial signature via title alone (no scripts needed).
        let body = b"<html><head><title>Please Wait... | Access Denied</title></head><body><p>Checking your browser before accessing the site.</p></body></html>";
        assert!(detect_interstitial(body).is_some());
    }

    #[test]
    fn real_page_titled_about_security_is_content() {
        // A security article has its own title and real text.
        let body = b"<html><head><title>Web Security Guide 2026</title></head><body><h1>Web Security Guide</h1><p>This article explains how security checks work on the modern web, what a security service does, and how verification flows are designed. It covers many topics in substantial depth for readers.</p></body></html>";
        let v = detect_dom_smart(body);
        assert!(matches!(v, Verdict::ContentOk), "got {v:?}");
    }

    #[test]
    fn turnstile_contact_form_still_content_with_interstitial_layer() {
        // The contact form with a Turnstile widget: has a form +
        // inputs + its own title : must stay content.
        let body = b"<html><head><title>Contact Us</title><script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\"></script></head><body><h1>Contact Us</h1><p>Fill out the form below and we will get back to you within 24 hours. Our team is dedicated to providing the best possible support.</p><div class=\"cf-turnstile\"></div><form><input name=\"email\"><textarea name=\"message\"></textarea><button>Send</button></form></body></html>";
        let v = detect_dom_smart(body);
        assert!(matches!(v, Verdict::ContentOk), "got {v:?}");
    }

    #[test]
    fn turnstile_shell_without_form_is_interstitial() {
        // Bare Turnstile shell (no form, no text): interstitial.
        let body = b"<html><head><script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\"></script></head><body><div class=\"cf-turnstile\"></div></body></html>";
        assert!(detect_interstitial(body).is_some());
        let v = detect_dom_smart(body);
        assert!(matches!(v, Verdict::Challenge(_)), "got {v:?}");
    }

    /// scrapingcourse.com/cloudflare-challenge 2026-09 golden fixture:
    /// the ghost DOM after the challenge was passed. A short real page
    /// (no form) that keeps loading Turnstile's public `api.js`. It
    /// used to read as an interstitial on every poll, so the ghost
    /// never settled and the fetch ended walled.
    #[test]
    fn cf_solved_page_loading_turnstile_script_is_content() {
        let body = include_bytes!("../../tests/fixtures/cf-challenge-solved.html");
        assert!(detect_interstitial(body).is_none());
        let v = detect_dom_smart(body);
        assert!(matches!(v, Verdict::ContentOk), "got {v:?}");
    }

    /// Same URL before the challenge is passed: the live Cloudflare
    /// interstitial as the ghost rendered it (Linux, Xvfb).
    #[test]
    fn cf_live_interstitial_is_challenge() {
        let body = include_bytes!("../../tests/fixtures/cf-challenge-interstitial.html");
        let v = detect_dom_smart(body);
        assert!(
            matches!(v, Verdict::Challenge(Vendor::Cloudflare)),
            "got {v:?}"
        );
    }

    /// glassdoor 2026-09 golden fixture: CF block pages put EVERY
    /// challenge marker past the 64KB narrow window (nearest at
    /// byte ~126k inside a 241KB bilingual shell). The 403 must be
    /// a Challenge, never a Blocked (Blocked skips escalation and
    /// the fetch dies on tier 1 alone).
    #[test]
    fn glassdoor_cf_block_markers_beyond_64k() {
        let mut body = vec![b' '; 150_000]; // bilingual shell padding
        body.extend_from_slice(
            b"<div>Ray ID: 8b1234567890abcd</div><script src='https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/b/orchestrate/chl_api/v1'></script><p>Your IP has been blocked. captcha</p>pre>",
        );
        let headers = vec![
            ("server".into(), "cloudflare".into()),
            ("cf-mitigated".into(), "challenge".into()),
        ];
        let v = detect(403, &headers, &body);
        assert!(matches!(v, Verdict::Challenge(_)), "got {v:?}");
    }

    /// Same shape withOUT the cf-mitigated header must still classify
    /// via the wide window (the header is not always present).
    #[test]
    fn glassdoor_cf_block_no_header_wide_window() {
        let mut body = vec![b' '; 150_000];
        body.extend_from_slice(b"<div>Ray ID: 8b1234567890abcd</div> challenge-platform captcha");
        let v = detect(403, &[("server".into(), "cloudflare".into())], &body);
        assert!(matches!(v, Verdict::Challenge(_)), "got {v:?}");
    }
}

// Reddit's 2026 JS-challenge: a nonce form auto-submitting
// itself with essentially no prose (live case: the whole
// r/technology shell scored ContentOk at tier 1).
#[test]
fn reddit_nonce_form_shell_is_challenge() {
    let body = "<!doctype html><html><head><title>Reddit</title></head><body><form><input type=hidden name=nonce value=a4e5c47fd8fd981e></form><script>document.forms[0].submit()</script></body></html>";
    assert!(matches!(
        detect(200, &[], body.as_bytes()),
        Verdict::Challenge(Vendor::Generic)
    ));
}

// Amazon's shopping-gate interstitial is a challenge so the
// ladder can solve it (or fail honestly); it must not ship as
// a successful "product page".
#[test]
fn amazon_shopping_gate_is_challenge() {
    let body = "<html><body><h4>Click the button below to continue shopping</h4><button>Continue shopping</button></body></html>";
    assert!(matches!(
        detect(200, &[], body.as_bytes()),
        Verdict::Challenge(Vendor::Generic)
    ));
}

// A spinner shell whose entire visible text is a loading
// prompt is a challenge shell, never content (live case:
// tiktok's 12-token "Please wait..." ok fetch).
#[test]
fn spinner_shell_is_challenge_not_content() {
    let body = "<html><head><style>.s{opacity:0}</style></head><body><div>Please wait...</div></body></html>";
    assert!(matches!(
        detect(200, &[], body.as_bytes()),
        Verdict::Challenge(Vendor::Generic)
    ));
}

// Instagram's logged-out shell is a login wall BY DESIGN:
// AuthWall, not Challenge (nothing to solve), so the fetch
// stops honestly instead of burning a browser render on the
// same gate (live case: /cristiano/ ended "no real content").
#[test]
fn instagram_logged_out_shell_is_auth_wall() {
    let body = "<html><body><h2>Log in to see photos and videos from friends and accounts you follow.</h2><a>Log in</a><a>Sign up</a></body></html>";
    assert!(matches!(
        detect(200, &[], body.as_bytes()),
        Verdict::AuthWall
    ));
}
