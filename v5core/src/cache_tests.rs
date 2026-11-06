//! The page cache through a real session against a local site that changes,
//! lies, walls and redirects under it. What must hold: a changed page is never
//! served as current once the server says so, and a fresh serve is only what the
//! server allowed.

use crate::testkit::{Reply, Request as Seen, Server};
use crate::{Class, Request, Session};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The site's current page: its text, its validator and its caching headers.
#[derive(Clone)]
struct Page {
    text: &'static str,
    etag: Option<&'static str>,
    cache_control: &'static str,
    /// Answer a wall instead of the page.
    walled: bool,
}

impl Page {
    fn new(text: &'static str, etag: &'static str) -> Self {
        Self {
            text,
            etag: Some(etag),
            cache_control: "no-cache",
            walled: false,
        }
    }
}

fn html(text: &str) -> Vec<u8> {
    format!(
        "<html><head><title>Cached</title></head><body><main>{text} {}</main></body></html>",
        "Words a reader came for. ".repeat(12)
    )
    .into_bytes()
}

/// A site that serves `page`, honours (or abuses) `If-None-Match`, and records
/// every request it saw.
fn site(page: Arc<Mutex<Page>>, seen: Arc<Mutex<Vec<Seen>>>) -> Server {
    Server::start(move |request| {
        seen.lock().unwrap().push(Seen {
            method: request.method.clone(),
            path: request.path.clone(),
            headers: request.headers.clone(),
            body: Vec::new(),
        });
        let page = page.lock().unwrap().clone();
        if page.walled {
            return Reply::Full {
                status: 403,
                headers: vec![("x-datadome".into(), "1".into())],
                body: b"<html>blocked</html>".to_vec(),
            };
        }
        let mut headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Cache-Control".to_string(), page.cache_control.to_string()),
        ];
        if let Some(tag) = page.etag {
            headers.push(("ETag".into(), tag.into()));
        }
        let asked = request.header("if-none-match");
        let current = asked.is_some() && asked == page.etag;
        // A site answers 304 whenever the validator matches; one whose
        // validator does not track its text is lying, and this site can.
        if current {
            return Reply::Full {
                status: 304,
                headers,
                body: Vec::new(),
            };
        }
        Reply::Full {
            status: 200,
            headers,
            body: html(page.text),
        }
    })
}

fn get(session: &Session, url: String) -> crate::Outcome {
    session.fetch(&Request {
        identity: Some("chrome".into()),
        pace: Duration::ZERO,
        deadline: Duration::from_secs(10),
        ..Request::new(url)
    })
}

fn text(outcome: &crate::Outcome) -> String {
    outcome.body.clone().unwrap_or_default()
}

fn conditional(seen: &Arc<Mutex<Vec<Seen>>>) -> Option<String> {
    seen.lock()
        .unwrap()
        .last()
        .and_then(|request| request.header("if-none-match").map(str::to_string))
}

struct Fixture {
    page: Arc<Mutex<Page>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    server: Server,
    session: Session,
}

fn fixture(page: Page) -> Fixture {
    let page = Arc::new(Mutex::new(page));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = site(Arc::clone(&page), Arc::clone(&seen));
    Fixture {
        page,
        seen,
        server,
        session: Session::new(false),
    }
}

impl Fixture {
    fn fetch(&self) -> crate::Outcome {
        get(&self.session, self.server.url("/"))
    }
    fn set(&self, change: impl FnOnce(&mut Page)) {
        change(&mut self.page.lock().unwrap());
    }
    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[test]
fn an_unchanged_page_is_revalidated_with_a_browsers_conditional_request() {
    let site = fixture(Page::new("first edition", "\"v1\""));
    let first = site.fetch();
    assert_eq!(first.class, Class::Content);
    assert_eq!(first.cache, None);
    let second = site.fetch();
    assert_eq!(second.class, Class::Content, "{:?}", second.attempts);
    assert_eq!(second.cache.as_deref(), Some("revalidated"));
    assert!(text(&second).contains("first edition"));
    assert_eq!(conditional(&site.seen).as_deref(), Some("\"v1\""));
    // Measured from Chrome revisiting a page: the validators follow
    // Accept-Language, and no Cache-Control is added (that is a reload).
    let seen = site.seen.lock().unwrap();
    let names: Vec<String> = seen
        .last()
        .unwrap()
        .headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    let at = |name: &str| names.iter().position(|known| known == name);
    assert!(at("cache-control").is_none(), "{names:?}");
    assert!(
        at("accept-language").unwrap() < at("if-none-match").unwrap(),
        "{names:?}"
    );
}

#[test]
fn a_changed_page_is_served_as_changed_the_first_time_it_is_asked_for() {
    let site = fixture(Page::new("first edition", "\"v1\""));
    site.fetch();
    site.fetch();
    site.set(|page| {
        page.text = "second edition";
        page.etag = Some("\"v2\"");
    });
    let after = site.fetch();
    assert!(text(&after).contains("second edition"), "{}", text(&after));
    assert_eq!(
        after.cache.as_deref(),
        Some("changed"),
        "the caller is told the page moved on"
    );
    let again = site.fetch();
    assert_eq!(again.cache.as_deref(), Some("revalidated"));
    assert_eq!(conditional(&site.seen).as_deref(), Some("\"v2\""));
    assert!(text(&again).contains("second edition"));
    // It changed its tag with its text: its validators are now believed longer.
    let host = site.server.addr.ip().to_string();
    assert!(site.session.state().origins[&host].validators_proven);
}

#[test]
fn a_fresh_page_is_served_without_a_request_only_once_the_site_has_kept_its_promise() {
    let mut page = Page::new("fresh", "\"f\"");
    page.cache_control = "max-age=30";
    let site = fixture(page);
    site.fetch();
    // Inside the declared lifetime, the first read checks the promise.
    let checked = site.fetch();
    assert_eq!(checked.cache.as_deref(), Some("revalidated"));
    let before = site.requests();
    let served = site.fetch();
    assert_eq!(served.cache.as_deref(), Some("fresh"));
    assert_eq!(
        site.requests(),
        before,
        "a kept promise earns one read without a request"
    );
    assert!(text(&served).contains("fresh"));
    // The earned read is spent: the next one checks again.
    assert_eq!(site.fetch().cache.as_deref(), Some("revalidated"));
    assert_eq!(site.requests(), before + 1);
    // Asked to revalidate, the session asks even when a fresh read is earned.
    let asked = site.session.fetch(&Request {
        revalidate: true,
        identity: Some("chrome".into()),
        pace: Duration::ZERO,
        ..Request::new(site.server.url("/"))
    });
    // (Two revalidations on a site not yet proven: this one is the audit.)
    assert_ne!(asked.cache.as_deref(), Some("fresh"));
    assert_eq!(site.requests(), before + 2);
}

#[test]
fn a_site_that_changes_inside_the_lifetime_it_declared_always_asks_after() {
    let mut page = Page::new("edition one", "\"1\"");
    page.cache_control = "max-age=30";
    let site = fixture(page);
    site.fetch();
    site.fetch(); // a check: unchanged, one read earned
    site.set(|page| {
        page.text = "edition two";
        page.etag = Some("\"2\"");
    });
    // The earned read may be the old edition: that is the one read a promise
    // buys. The check after it sees the change and the promise is broken.
    let mut stale = 0;
    for _ in 0..6 {
        let outcome = site.fetch();
        if text(&outcome).contains("edition one") {
            stale += 1;
            continue;
        }
        assert_ne!(
            outcome.cache.as_deref(),
            Some("fresh"),
            "a site that broke its promise is never served without asking"
        );
    }
    assert!(stale <= 1, "{stale} stale reads");
    let host = site.server.addr.ip().to_string();
    assert!(site.session.state().origins[&host].fresh_broken_until_unix > crate::state::now_unix());
}

#[test]
fn a_page_the_server_forbids_storing_is_always_fetched_whole() {
    let mut page = Page::new("private", "\"p\"");
    page.cache_control = "no-store";
    let site = fixture(page);
    site.fetch();
    let second = site.fetch();
    assert_eq!(second.cache, None);
    assert_eq!(
        conditional(&site.seen),
        None,
        "nothing stored, nothing to revalidate"
    );
}

#[test]
fn a_site_whose_validators_lie_is_caught_by_an_audit_and_then_fetched_whole() {
    let site = fixture(Page::new("price 10", "\"same\""));
    site.fetch();
    // The text changes; the validator does not, and the server answers 304.
    site.set(|page| page.text = "price 12");
    let mut fetches = 0;
    loop {
        fetches += 1;
        let outcome = site.fetch();
        if text(&outcome).contains("price 12") {
            break;
        }
        // An unproven site's copy is checked whole after two revalidations.
        assert!(
            fetches <= crate::cache::AUDIT_AFTER_UNPROVEN as usize + 1,
            "a lie outlived its audit: {fetches} fetches"
        );
    }
    // Caught: from now on no conditional request is sent to this origin.
    site.set(|page| page.text = "price 15");
    let after = site.fetch();
    assert_eq!(conditional(&site.seen), None);
    assert!(text(&after).contains("price 15"));
    let host = site.server.addr.ip().to_string();
    assert!(site.session.state().origins[&host].distrust_until_unix > crate::state::now_unix());
}

#[test]
fn a_wall_during_revalidation_never_replaces_the_page_or_is_served_from_it() {
    let site = fixture(Page::new("kept", "\"k\""));
    site.fetch();
    site.set(|page| page.walled = true);
    let walled = site.fetch();
    assert_ne!(
        walled.class,
        Class::Content,
        "a wall is not the cached page"
    );
    site.set(|page| page.walled = false);
    let back = site.fetch();
    assert_eq!(
        back.cache.as_deref(),
        Some("revalidated"),
        "{:?}",
        back.attempts
    );
    assert!(text(&back).contains("kept"));
}

#[test]
fn a_cached_page_is_held_to_the_contract_of_the_request_that_reads_it() {
    let mut page = Page::new("about rivers", "\"r\"");
    page.cache_control = "max-age=30";
    let site = fixture(page);
    site.fetch();
    let before = site.requests();
    let contract = crate::Contract {
        text_contains: Some(vec!["mountains".into()]),
        ..crate::Contract::generic()
    };
    let strict = site.session.fetch(&Request {
        contract: Some(contract),
        identity: Some("chrome".into()),
        pace: Duration::ZERO,
        ..Request::new(site.server.url("/"))
    });
    assert_ne!(
        strict.cache.as_deref(),
        Some("fresh"),
        "a copy that fails the contract is not served"
    );
    assert!(site.requests() > before);
}

#[test]
fn a_cold_probe_neither_reads_nor_writes_the_cache() {
    let mut page = Page::new("cold", "\"c\"");
    page.cache_control = "max-age=30";
    let site = fixture(page);
    let probe = |site: &Fixture| {
        site.session.fetch(&Request {
            learn: false,
            identity: Some("chrome".into()),
            pace: Duration::ZERO,
            ..Request::new(site.server.url("/"))
        })
    };
    probe(&site);
    let second = probe(&site);
    assert_eq!(second.cache, None);
    assert_eq!(conditional(&site.seen), None);
    assert_eq!(site.requests(), 2);
}

#[test]
fn a_304_from_a_page_the_redirect_now_points_elsewhere_confirms_nothing() {
    // `/` redirects to `/a` (tag "a"); later to `/b`, which falsely answers 304 to
    // any If-None-Match. The session must ask `/b` again, unconditionally.
    let target = Arc::new(Mutex::new("/a"));
    let paths = Arc::clone(&target);
    let server = Server::start(move |request| {
        let to = *paths.lock().unwrap();
        match request.path.as_str() {
            "/" => Reply::Redirect(to.to_string()),
            "/b" if request.header("if-none-match").is_some() => Reply::Full {
                status: 304,
                headers: vec![("ETag".into(), "\"a\"".into())],
                body: Vec::new(),
            },
            path => Reply::Full {
                status: 200,
                headers: vec![
                    ("Content-Type".into(), "text/html".into()),
                    ("Cache-Control".into(), "no-cache".into()),
                    ("ETag".into(), "\"a\"".into()),
                ],
                body: html(if path == "/a" { "page a" } else { "page b" }),
            },
        }
    });
    let session = Session::new(false);
    assert!(text(&get(&session, server.url("/"))).contains("page a"));
    *target.lock().unwrap() = "/b";
    let moved = get(&session, server.url("/"));
    assert!(text(&moved).contains("page b"), "{}", text(&moved));
    assert_eq!(moved.cache.as_deref(), Some("changed"));
}
