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

/// Interstitial titles/phrases that vendor challenge pages use in
/// `<title>` / `<h1>`. Real pages virtually never title themselves
/// these : a page ABOUT Cloudflare has its own title.
const INTERSTITIAL_TITLES: &[&str] = &[
    "just a moment",
    "performing security verification",
    "checking your browser",
    "attention required",
    "verify you are human",
    "verify that you are human",
    "verifying you are human",
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
    "challenges.cloudflare.com",
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

    // Title/H1 route: strongest signal, immune to visible-text counts.
    let title = extract_title_or_h1(&text);
    if let Some(t) = title
        && INTERSTITIAL_TITLES.iter().any(|m| t.contains(m))
    {
        return Some(vendor_from_markers(&text).unwrap_or(Vendor::Generic));
    }

    // Near-empty route: tiny visible text + challenge script + no
    // form (a login/contact page with a Turnstile widget has BOTH a
    // form and real visible text : it must not match).
    let visible = visible_text_count(body);
    if visible < 400
        && INTERSTITIAL_MARKERS.iter().any(|m| text.contains(m))
        && !text.contains("<form")
        && !text.contains("<input")
    {
        return Some(vendor_from_markers(&text).unwrap_or(Vendor::Generic));
    }
    None
}

/// Best-effort `<title>` (head) or first `<h1>` text, lowercased.
fn extract_title_or_h1(lower_text: &str) -> Option<String> {
    for (open, close) in [("<title", "</title>"), ("<h1", "</h1>")] {
        let start = lower_text.find(open)?;
        if let Some(content_start) = lower_text[start..].find('>') {
            let from = start + content_start + 1;
            if let Some(end) = lower_text[from..].find(close) {
                let t = &lower_text[from..from + end];
                // Strip nested tags inside the title (h1 can wrap spans).
                let cleaned: String = t
                    .chars()
                    .filter(|c| *c != '<')
                    .collect::<String>()
                    .split('<')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let cleaned = cleaned.trim();
                if !cleaned.is_empty() {
                    return Some(cleaned.to_string());
                }
            }
        }
    }
    None
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
    // Reddit-style interstitials (often served as 200).
    if text.contains("prove your humanity")
        || text.contains("not for bots")
        || text.contains("please wait for verification")
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
