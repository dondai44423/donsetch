//! Google's no-JavaScript mobile endpoint. Protocol references (not dependencies):
//! searxng/searxng: searx/engines/google.py; deedy5/ddgs: ddgs/engines/google.py.
//! Uses DonShadow's existing TLS/HTTP stack, not a Nokia TLS emulation or browser.

use scraper::{ElementRef, Html};

use super::{Hit, sel, text};

/// Observed working identities, not an availability guarantee. Profile fallback
/// is bounded by the search retry wave, never an internal request loop.
pub const PROFILES: &[(&str, &str)] = &[
    (
        "6230-03.15",
        "Nokia6230/2.0 (03.15) Profile/MIDP-2.0 Configuration/CLDC-1.1",
    ),
    (
        "6230-05.50",
        "Nokia6230/2.0 (05.50) Profile/MIDP-2.0 Configuration/CLDC-1.1",
    ),
    (
        "6230-04.44",
        "Nokia6230/2.0 (04.44) Profile/MIDP-2.0 Configuration/CLDC-1.1",
    ),
    (
        "6230i-03.80",
        "Nokia6230i/2.0 (03.80) Profile/MIDP-2.0 Configuration/CLDC-1.1",
    ),
    (
        "6280-03.60",
        "Nokia6280/2.0 (03.60) Profile/MIDP-2.0 Configuration/CLDC-1.1",
    ),
    (
        "7610-5.0509.0",
        "Nokia7610/2.0 (5.0509.0) SymbianOS/7.0s Series60/2.1 Profile/MIDP-2.0 Configuration/CLDC-1.0",
    ),
    (
        "7610-7.0642.0",
        "Nokia7610/2.0 (7.0642.0) SymbianOS/7.0s Series60/2.1 Profile/MIDP-2.0 Configuration/CLDC-1.0",
    ),
];

pub const USER_AGENT: &str = PROFILES[0].1;

pub fn profile_user_agent(id: Option<&str>) -> Option<&'static str> {
    match id {
        None => Some(USER_AGENT),
        Some(id) => PROFILES
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, ua)| *ua),
    }
}

pub fn configured_user_agent() -> Option<&'static str> {
    match std::env::var("DONSETCH_GOOGLE_PROFILE") {
        Ok(id) => profile_user_agent(Some(&id)),
        Err(std::env::VarError::NotPresent) => profile_user_agent(None),
        Err(_) => None,
    }
}

/// Adapter-local identity selection, not a health or retry scheduler.
/// Egress eligibility, deadlines and quarantine belong to the search core.
pub(crate) struct ProfileSelector {
    initial: Option<usize>,
    cursors: std::sync::Mutex<std::collections::HashMap<String, Cursor>>,
}

struct Cursor {
    index: usize,
    generation: u64,
}

pub(crate) struct ProfileLease {
    egress: String,
    index: usize,
    generation: u64,
}

impl ProfileLease {
    pub fn user_agent(&self) -> &'static str {
        PROFILES[self.index].1
    }
    pub fn label(&self) -> String {
        format!("google@{}", PROFILES[self.index].0)
    }
}

impl ProfileSelector {
    pub fn from_env() -> Self {
        let initial = configured_user_agent()
            .and_then(|ua| PROFILES.iter().position(|(_, candidate)| *candidate == ua));
        Self {
            initial,
            cursors: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// A retry carries the previous failure as generic attempt evidence. Only
    /// this adapter interprets a Google CAPTCHA as an identity change on an
    /// untouched egress. An evolved cursor takes precedence over old evidence.
    pub fn select(
        &self,
        egress: &str,
        previous: Option<(&str, &str)>,
    ) -> Result<ProfileLease, &'static str> {
        let initial = self.initial.ok_or("invalid-config")?;
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cursor = cursors.entry(egress.into()).or_insert(Cursor {
            index: initial,
            generation: 0,
        });
        let alternative = previous.and_then(|(label, status)| {
            if status != "blocked:captcha" || cursor.generation != 0 {
                return None;
            }
            let id = label.strip_prefix("google@")?;
            PROFILES
                .iter()
                .position(|(name, _)| *name == id)
                .map(|index| (index + 1) % PROFILES.len())
        });
        Ok(ProfileLease {
            egress: egress.into(),
            index: alternative.unwrap_or(cursor.index),
            generation: cursor.generation,
        })
    }

    pub fn finish(&self, lease: &ProfileLease, status: &str) {
        if !matches!(status, "ok" | "blocked:captcha") {
            return;
        }
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cursor) = cursors.get_mut(&lease.egress) else {
            return;
        };
        if cursor.generation != lease.generation {
            return;
        }
        let next = if status == "blocked:captcha" {
            (lease.index + 1) % PROFILES.len()
        } else {
            lease.index
        };
        if status == "blocked:captcha" || cursor.index != next {
            cursor.generation = cursor.generation.wrapping_add(1);
        }
        cursor.index = next;
    }
}

pub fn url(query: &str) -> String {
    let mut url = url::Url::parse("https://www.google.com/wml/search").unwrap();
    url.query_pairs_mut().extend_pairs([
        ("q", query),
        ("sca_esv", "1"),
        ("hl", "en"),
        ("ie", "utf-8"),
        ("oe", "utf-8"),
    ]);
    url.into()
}

/// Do not follow consent/CAPTCHA redirects or mistake an HTTP-200 wall for hits.
pub fn response_error(
    status: u16,
    headers: &[(String, String)],
    html: &str,
) -> Option<&'static str> {
    // Rate limiting takes precedence even if the body contains a CAPTCHA.
    if status == 429 {
        return Some("blocked:http-status");
    }
    let location = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if location.contains("/sorry")
        || location.contains("sorry.google.com")
        || (html.len() < 2000 && html.contains("/sorry/"))
        || html.contains("id=\"captcha-form\"")
        || html.contains("id='captcha-form'")
    {
        return Some("blocked:captcha");
    }
    if location.contains("consent.google.") {
        return Some("blocked:consent");
    }
    if (300..400).contains(&status) {
        return Some("blocked:redirect");
    }
    if status != 200 {
        return Some("blocked:http-status");
    }
    if html.contains("Your browser isn't supported any more") {
        return Some("blocked:unsupported-browser");
    }
    None
}

fn target(href: &str) -> Option<String> {
    if !(href.starts_with("https://") || href.starts_with("http://") || href.starts_with("/url?")) {
        return None;
    }
    let base = url::Url::parse("https://www.google.com").unwrap();
    let wrapped = base.join(href).ok()?;
    let target = if matches!(wrapped.host_str(), Some("google.com" | "www.google.com"))
        && wrapped.path() == "/url"
    {
        let (_, value) = wrapped
            .query_pairs()
            .find(|(key, _)| key == "q" || key == "url")?;
        url::Url::parse(&value).ok()?
    } else {
        wrapped
    };
    if !matches!(target.scheme(), "http" | "https")
        || target.host_str().is_none()
        || !target.username().is_empty()
        || target.password().is_some()
    {
        return None;
    }
    if matches!(target.host_str(), Some("google.com" | "www.google.com"))
        && matches!(
            target.path(),
            "/search" | "/wml/search" | "/url" | "/preferences" | "/sorry" | "/sorry/"
        )
    {
        return None;
    }
    Some(target.into())
}

pub fn parse(doc: &Html) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let link = sel("a.fuLhoc[href]");
    let title = sel("span.CVA68e");
    let snippet = sel("div.taTFJ span.FrIlee");
    for block in doc.select(&sel("div.zMzFAb")) {
        let Some(a) = block.select(&link).next() else {
            continue;
        };
        let Some(t) = a.select(&title).next() else {
            continue;
        };
        let body = block
            .select(&snippet)
            .map(text)
            .collect::<Vec<_>>()
            .join(" ");
        push(&mut hits, &mut seen, a, text(t), body);
    }
    // Structural fallback used by DDGS: title div followed by a table-bearing
    // snippet div. Do not fall back to all anchors (navigation is not a result).
    let span = sel("span");
    let table = sel("table");
    for block in doc.select(&sel("div")) {
        let children: Vec<_> = block.children().filter_map(ElementRef::wrap).collect();
        if children.len() < 2
            || children[0].value().name() != "div"
            || children[1].value().name() != "div"
            || children[1].select(&table).next().is_none()
        {
            continue;
        }
        let Some(a) = children[0]
            .children()
            .filter_map(ElementRef::wrap)
            .find(|el| el.value().name() == "a" && el.value().attr("href").is_some())
        else {
            continue;
        };
        // Keep the title inside the result anchor, not a neighbouring UI label.
        let Some(t) = a.select(&span).next() else {
            continue;
        };
        push(&mut hits, &mut seen, a, text(t), text(children[1]));
    }
    hits
}

fn push(
    hits: &mut Vec<Hit>,
    seen: &mut std::collections::HashSet<String>,
    a: ElementRef<'_>,
    title: String,
    snippet: String,
) {
    let Some(url) = a.value().attr("href").and_then(target) else {
        return;
    };
    if title.is_empty() || !seen.insert(url.clone()) {
        return;
    }
    hits.push(Hit {
        title,
        url,
        snippet,
        rank: hits.len(),
        published: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> ProfileSelector {
        ProfileSelector {
            initial: Some(0),
            cursors: Default::default(),
        }
    }

    #[test]
    fn selector_wraps_without_cooldowns_and_restart_resets() {
        let profiles = selector();
        for _ in 0..2 {
            for index in 0..PROFILES.len() {
                let lease = profiles.select("direct", None).unwrap();
                assert_eq!(lease.index, index);
                profiles.finish(&lease, "blocked:captcha");
            }
        }
        let a = profiles.select("direct", None).unwrap();
        profiles.finish(&a, "blocked:captcha");
        let b = profiles.select("direct", None).unwrap();
        profiles.finish(&b, "ok");
        assert_eq!(profiles.select("direct", None).unwrap().index, 1);
        assert_eq!(selector().select("direct", None).unwrap().index, 0);
    }

    #[test]
    fn retry_can_change_egress_and_identity_without_rotating_other_errors() {
        let profiles = selector();
        let a = profiles.select("proxy-a", None).unwrap();
        profiles.finish(&a, "blocked:captcha");
        let label = a.label();
        let b = profiles
            .select("proxy-b", Some((&label, "blocked:captcha")))
            .unwrap();
        assert_eq!(b.index, 1);
        profiles.finish(&b, "ok");
        assert_eq!(profiles.select("proxy-b", None).unwrap().index, 1);
        assert_eq!(profiles.select("proxy-c", None).unwrap().index, 0);
        for status in [
            "blocked:http-status",
            "blocked:429",
            "blocked:consent",
            "timeout",
            "net",
        ] {
            let lease = profiles.select("proxy-c", Some((&label, status))).unwrap();
            assert_eq!(lease.index, 0);
            profiles.finish(&lease, status);
            assert_eq!(profiles.select("proxy-c", None).unwrap().index, 0);
        }
    }

    #[test]
    fn delayed_retry_respects_evolved_cursors_including_wraparound() {
        let profiles = selector();
        let a = profiles.select("direct", None).unwrap();
        profiles.finish(&a, "blocked:captcha");
        let b = profiles.select("direct", None).unwrap();
        profiles.finish(&b, "blocked:captcha");
        let label = a.label();
        let retry = profiles
            .select("direct", Some((&label, "blocked:captcha")))
            .unwrap();
        assert_eq!(retry.index, 2);
        profiles.finish(&retry, "ok");
        assert_eq!(profiles.select("direct", None).unwrap().index, 2);

        // Returning to A after a full cycle is not an untouched cursor.
        for _ in 2..PROFILES.len() {
            let lease = profiles.select("direct", None).unwrap();
            profiles.finish(&lease, "blocked:captcha");
        }
        let retry = profiles
            .select("direct", Some((&label, "blocked:captcha")))
            .unwrap();
        assert_eq!(retry.index, 0);
    }

    #[test]
    fn cross_egress_retry_preserves_existing_preference() {
        let profiles = selector();
        let a = profiles.select("proxy-a", None).unwrap();
        profiles.finish(&a, "blocked:captcha");
        for _ in 0..2 {
            let lease = profiles.select("proxy-b", None).unwrap();
            profiles.finish(&lease, "blocked:captcha");
        }
        let label = a.label();
        let retry = profiles
            .select("proxy-b", Some((&label, "blocked:captcha")))
            .unwrap();
        assert_eq!(retry.index, 2);
        profiles.finish(&retry, "ok");
        assert_eq!(profiles.select("proxy-b", None).unwrap().index, 2);
    }

    #[test]
    fn unchanged_success_does_not_hide_captcha_but_stale_outcomes_cannot_rewind() {
        let profiles = selector();
        let a = profiles.select("direct", None).unwrap();
        let concurrent_a = profiles.select("direct", None).unwrap();
        profiles.finish(&a, "ok");
        profiles.finish(&concurrent_a, "blocked:captcha");
        let b = profiles.select("direct", None).unwrap();
        assert_eq!(b.index, 1);
        profiles.finish(&b, "ok");
        profiles.finish(&a, "ok");
        profiles.finish(&a, "blocked:captcha");
        assert_eq!(profiles.select("direct", None).unwrap().index, 1);
    }

    #[test]
    fn invalid_configuration_fails_without_selecting_a_profile() {
        let profiles = ProfileSelector {
            initial: None,
            cursors: Default::default(),
        };
        assert_eq!(
            profiles.select("direct", None).err(),
            Some("invalid-config")
        );
    }

    #[test]
    fn rate_limiting_takes_precedence_over_captcha_markup() {
        assert_eq!(
            response_error(429, &[], "<form id='captcha-form'>"),
            Some("blocked:http-status")
        );
    }

    #[test]
    fn profiles_are_unique_valid_and_reject_unknown_ids() {
        assert_eq!(PROFILES.len(), 7);
        assert_eq!(
            PROFILES.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [
                "6230-03.15",
                "6230-05.50",
                "6230-04.44",
                "6230i-03.80",
                "6280-03.60",
                "7610-5.0509.0",
                "7610-7.0642.0"
            ]
        );
        let mut ids = std::collections::HashSet::new();
        let mut user_agents = std::collections::HashSet::new();
        for (id, ua) in PROFILES {
            assert!(ids.insert(id));
            assert!(user_agents.insert(ua));
            assert!(ua.starts_with("Nokia"));
            assert_eq!(profile_user_agent(Some(id)), Some(*ua));
            assert!(ua.is_ascii() && !ua.bytes().any(|b| b.is_ascii_control()));
        }
        assert_eq!(profile_user_agent(None), Some(USER_AGENT));
        assert_eq!(
            profile_user_agent(None),
            profile_user_agent(Some("6230-03.15"))
        );
        assert_eq!(profile_user_agent(Some("unknown")), None);
        assert_eq!(profile_user_agent(Some("")), None);
    }

    #[test]
    fn walls_and_http_errors_are_not_successful_pages() {
        assert_eq!(
            response_error(200, &[], "Your browser isn't supported any more"),
            Some("blocked:unsupported-browser")
        );
        for status in [403, 429, 500, 503] {
            assert_eq!(response_error(status, &[], ""), Some("blocked:http-status"));
        }
    }

    #[test]
    fn query_round_trips_without_parameter_injection() {
        let query = "caffè Rust & C++ #東京";
        let u = url::Url::parse(&url(query)).unwrap();
        assert_eq!(u.path(), "/wml/search");
        assert_eq!(u.query_pairs().find(|(k, _)| k == "q").unwrap().1, query);
        assert_eq!(u.query_pairs().count(), 5);
    }

    #[test]
    fn redirects_preserve_complete_target_query_and_fragment() {
        assert_eq!(
            target("/url?sa=U&q=https%3A%2F%2Fexample.org%2Fp%3Fa%3D1%26b%3D2%23part").unwrap(),
            "https://example.org/p?a=1&b=2#part"
        );
        assert_eq!(
            target("https://example.org/?q=a+b").unwrap(),
            "https://example.org/?q=a+b"
        );
        for bad in [
            "javascript:alert(1)",
            "/url?q=javascript%3Aalert(1)",
            "/url?sa=U",
            "/wml/search?q=x",
            "https://user:pass@example.org/",
        ] {
            assert!(target(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parses_xml_layout_and_deduplicates_without_truncating_urls() {
        let html = r#"<?xml version="1.0" encoding="UTF-8"?>
        <div class="zMzFAb"><a class="fuLhoc" href="/url?q=https%3A%2F%2Fexample.org%2F%3Fa%3D1%26b%3D2&amp;sa=U"><span class="CVA68e">Rust &amp; TLS</span></a><div class="taTFJ"><span class="FrIlee">Native <b>HTTP</b></span></div></div>
        <div class="zMzFAb"><a class="fuLhoc" href="https://example.org/?a=1&amp;b=2"><span class="CVA68e">Duplicate</span></a></div>
        <div class="zMzFAb"><a class="fuLhoc" href="https://example.org/?a=1&amp;b=3"><span class="CVA68e">Distinct</span></a></div>"#;
        let hits = parse(&Html::parse_document(html));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust & TLS");
        assert_eq!(hits[0].snippet, "Native HTTP");
        assert_eq!(hits[0].url, "https://example.org/?a=1&b=2");
        assert_eq!(hits[1].rank, 1);
    }

    #[test]
    fn structural_fallback_and_navigation_exclusion() {
        let html = r#"<div><div><a href="https://example.org/"><span>Title</span></a></div><div><table><tr><td>Snippet</td></tr></table></div></div>
        <a href="https://google.com/preferences">Settings</a><script>enableJavascript()</script>"#;
        let hits = parse(&Html::parse_document(html));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Snippet");
        assert!(
            parse(&Html::parse_document(
                "<a href='https://example.org'>Navigation</a>"
            ))
            .is_empty()
        );
    }

    #[test]
    fn block_redirects_are_not_results() {
        assert_eq!(
            response_error(
                302,
                &[(
                    "Location".into(),
                    "https://www.google.com/sorry/index".into()
                )],
                ""
            ),
            Some("blocked:captcha")
        );
        assert_eq!(
            response_error(
                302,
                &[("location".into(), "https://consent.google.com/m".into())],
                ""
            ),
            Some("blocked:consent")
        );
        assert_eq!(
            response_error(200, &[], "<form id='captcha-form'>"),
            Some("blocked:captcha")
        );
        assert_eq!(response_error(302, &[], ""), Some("blocked:redirect"));
        assert_eq!(response_error(200, &[], "ordinary results"), None);
    }
}
