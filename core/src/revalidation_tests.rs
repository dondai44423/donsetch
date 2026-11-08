//! What a 304 may and may not confirm (RFC 9111 §4.3.4), and what a late or
//! unsolicited one must never do to the page cache.

use crate::cache::{self, Entry};
use crate::testkit::{Reply, Server};
use crate::{Class, Request, Session};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn request(url: String) -> Request {
    Request {
        identity: Some("chrome".into()),
        pace: Duration::ZERO,
        deadline: Duration::from_secs(10),
        ..Request::new(url)
    }
}

fn full(status: u16, headers: &[(&str, &str)], body: &str) -> Reply {
    Reply::Full {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

/// A session with a page cache of its own, in a directory removed on drop.
struct Cached {
    session: Session,
    directory: std::path::PathBuf,
}

impl Cached {
    fn new(name: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("v5-revalidation-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        Self {
            session: Session::at(directory.join("state.json")),
            directory,
        }
    }
}

impl Drop for Cached {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn stored(etag: Option<&str>, last_modified: Option<&str>) -> Entry {
    let mut headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
    if let Some(tag) = etag {
        headers.push(("ETag".to_string(), tag.to_string()));
    }
    if let Some(date) = last_modified {
        headers.push(("Last-Modified".to_string(), date.to_string()));
    }
    // A Date well after the Last-Modified, so the date is a validator.
    headers.push((
        "Date".to_string(),
        "Mon, 07 Nov 1994 08:49:37 GMT".to_string(),
    ));
    cache::entry(
        "k",
        "http://a.test/",
        "http://a.test/",
        "chrome",
        200,
        &headers,
        b"body",
        None,
        0,
    )
    .expect("storable")
}

fn header(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

#[test]
fn a_304_selects_only_the_representation_its_validator_names() {
    let strong = stored(Some("\"v1\""), None);
    let weak = stored(Some("W/\"v1\""), None);
    let dated = stored(None, Some("Sun, 06 Nov 1994 08:49:37 GMT"));
    assert!(cache::selects(&strong, &[header("ETag", "\"v1\"")]));
    assert!(!cache::selects(&strong, &[header("ETag", "\"v2\"")]));
    // A weak validator selects by weak comparison; a strong one only a strong copy.
    assert!(cache::selects(&strong, &[header("ETag", "W/\"v1\"")]));
    assert!(!cache::selects(&weak, &[header("ETag", "\"v1\"")]));
    assert!(cache::selects(&weak, &[header("ETag", "W/\"v1\"")]));
    // Two tags are no answer.
    assert!(!cache::selects(
        &strong,
        &[header("ETag", "\"v1\""), header("ETag", "\"v1\"")]
    ));
    assert!(cache::selects(
        &dated,
        &[header("Last-Modified", "Sun, 06 Nov 1994 08:49:37 GMT")]
    ));
    assert!(!cache::selects(
        &dated,
        &[header("Last-Modified", "Sun, 06 Nov 1994 08:49:38 GMT")]
    ));
    // No validator in the 304: the one stored copy.
    assert!(cache::selects(&strong, &[]));
    // A tag for a copy stored without one selects nothing.
    assert!(!cache::selects(&dated, &[header("ETag", "\"v1\"")]));
}

#[test]
fn a_304_naming_another_representation_is_asked_again_without_validators() {
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let server = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            let conditional = request.header("if-none-match").is_some();
            seen.lock().unwrap().push(conditional);
            if conditional {
                full(
                    304,
                    &[("ETag", "\"v2\""), ("Cache-Control", "max-age=600")],
                    "",
                )
            } else if seen.lock().unwrap().len() == 1 {
                full(
                    200,
                    &[("Content-Type", "text/plain"), ("ETag", "\"v1\"")],
                    "old body",
                )
            } else {
                full(
                    200,
                    &[("Content-Type", "text/plain"), ("ETag", "\"v2\"")],
                    "new body",
                )
            }
        })
    };
    let cached = Cached::new("mismatch");
    let first = cached.session.fetch(&request(server.url("/p")));
    assert_eq!(first.body.as_deref(), Some("old body"));
    let second = cached.session.fetch(&request(server.url("/p")));
    assert_eq!(second.body.as_deref(), Some("new body"), "{second:?}");
    assert_eq!(second.cache.as_deref(), Some("changed"));
    assert_eq!(*seen.lock().unwrap(), vec![false, true, false]);
}

#[test]
fn an_unsolicited_304_ends_the_encounter_and_confirms_nothing() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = {
        let hits = Arc::clone(&hits);
        Server::start(move |request| {
            let count = hits.fetch_add(1, Ordering::SeqCst);
            match (count, request.path.as_str()) {
                (0, _) => full(
                    200,
                    &[("Content-Type", "text/plain"), ("ETag", "\"v1\"")],
                    "original",
                ),
                (_, "/a") => Reply::Redirect("/b".into()),
                _ => full(304, &[], ""),
            }
        })
    };
    let cached = Cached::new("unsolicited");
    assert_eq!(
        cached
            .session
            .fetch(&request(server.url("/a")))
            .body
            .as_deref(),
        Some("original")
    );
    // The revalidation of /a is redirected; /b answers 304 to a request that
    // carried no validator. No body may stand for it, and no identity is tried
    // after it.
    let outcome = cached.session.fetch(&Request {
        identity: None,
        ..request(server.url("/a"))
    });
    assert_eq!(outcome.status, 304);
    assert_eq!(outcome.class, Class::HttpError);
    assert_eq!(outcome.attempts.len(), 1, "{:?}", outcome.attempts);
    assert_ne!(outcome.body.as_deref(), Some("original"));
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[test]
fn a_304_renews_the_stored_metadata_it_carries() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = {
        let hits = Arc::clone(&hits);
        Server::start(move |_| match hits.fetch_add(1, Ordering::SeqCst) {
            0 => full(
                200,
                &[
                    ("Content-Type", "text/plain"),
                    ("ETag", "\"v1\""),
                    ("Cache-Control", "max-age=0"),
                    ("X-Revision", "old"),
                    ("Set-Cookie", "never=stored"),
                ],
                "owned body",
            ),
            _ => full(
                304,
                &[
                    ("ETag", "\"v1\""),
                    ("Cache-Control", "max-age=600"),
                    ("X-Revision", "validated"),
                    ("Content-Encoding", "gzip"),
                ],
                "",
            ),
        })
    };
    let cached = Cached::new("metadata");
    cached.session.fetch(&request(server.url("/p")));
    let validated = cached.session.fetch(&request(server.url("/p")));
    assert_eq!(validated.cache.as_deref(), Some("revalidated"));
    assert_eq!(validated.body.as_deref(), Some("owned body"));
    let value = |name: &str| {
        validated
            .headers
            .iter()
            .filter(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(value("x-revision"), vec!["validated"]);
    assert_eq!(value("cache-control"), vec!["max-age=600"]);
    assert_eq!(value("content-type"), vec!["text/plain"]);
    assert!(
        value("set-cookie").is_empty(),
        "a stored page never sets a cookie"
    );
    assert!(
        value("content-encoding").is_empty(),
        "the body is stored decoded"
    );
}

#[test]
fn a_page_found_gone_is_evicted_and_a_late_confirmation_cannot_restore_it() {
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = {
        let seen = Arc::clone(&seen);
        let gone = Arc::clone(&gone);
        Server::start(move |request| {
            seen.lock()
                .unwrap()
                .push(request.header("if-none-match").is_some());
            if gone.load(Ordering::SeqCst) {
                full(404, &[], "")
            } else {
                full(
                    200,
                    &[("Content-Type", "text/plain"), ("ETag", "\"v1\"")],
                    "owned body",
                )
            }
        })
    };
    let cached = Cached::new("gone");
    let first = cached.session.fetch(&request(server.url("/p")));
    let snapshot = cached
        .session
        .cache_entry("chrome", None, &server.url("/p"))
        .expect("stored");
    gone.store(true, Ordering::SeqCst);
    let missing = cached.session.fetch(&request(server.url("/p")));
    assert_eq!(missing.status, 404);
    assert!(
        cached
            .session
            .cache_entry("chrome", None, &server.url("/p"))
            .is_none(),
        "a 404 leaves no copy to serve"
    );
    // A confirmation of the old copy that lands now stores nothing.
    let late = cache::confirmed(snapshot.clone(), &[], None, 1);
    assert!(!cached.session.cache().put_confirmed(&late, &snapshot));
    let after = cached.session.fetch(&request(server.url("/p")));
    assert_eq!(after.status, 404);
    assert_eq!(*seen.lock().unwrap(), vec![false, true, false]);
    assert_eq!(first.body.as_deref(), Some("owned body"));
}

#[test]
fn a_late_confirmation_never_overwrites_a_newer_copy() {
    let cache = cache::PageCache::in_memory();
    let old = stored(Some("\"v1\""), None);
    let mut newer = old.clone();
    newer.body = b"newer".to_vec();
    newer.body_hash = cache::hash(&newer.body);
    newer.full_unix = old.full_unix + 5;
    cache.put(&old);
    let confirmed = cache::confirmed(old.clone(), &[], None, 10);
    assert!(
        cache.put_confirmed(&confirmed, &old),
        "the copy is still the one confirmed"
    );
    cache.put(&newer);
    let late = cache::confirmed(old.clone(), &[], None, 20);
    assert!(!cache.put_confirmed(&late, &old));
    assert_eq!(cache.get("k").expect("stored").body_hash, newer.body_hash);
}

#[test]
fn a_page_that_varies_on_the_referer_is_served_only_to_that_referer() {
    let server = Server::start(|request| {
        let body = if request.header("referer").is_some() {
            "linked representation"
        } else {
            "typed representation"
        };
        full(
            200,
            &[
                ("Content-Type", "text/plain"),
                ("Cache-Control", "max-age=600"),
                ("Vary", "Accept-Encoding, Referer"),
            ],
            body,
        )
    });
    let cached = Cached::new("vary-referer");
    let typed = || request(server.url("/p"));
    let linked = || {
        let mut linked = typed();
        linked.policy.navigation = vec![
            ("referer".into(), "https://elsewhere.test/".into()),
            ("sec-fetch-site".into(), "cross-site".into()),
        ];
        linked
    };
    assert_eq!(
        cached.session.fetch(&typed()).body.as_deref(),
        Some("typed representation")
    );
    // A checked read inside the lifetime earns the next one.
    cached.session.fetch(&typed());
    let served = cached.session.fetch(&typed());
    assert_eq!(served.cache.as_deref(), Some("fresh"));
    let other = cached.session.fetch(&linked());
    assert_eq!(
        other.body.as_deref(),
        Some("linked representation"),
        "{other:?}"
    );
    assert_ne!(other.cache.as_deref(), Some("fresh"));
    assert_eq!(server.hit_count(), 3);
}

#[test]
fn a_page_that_does_not_vary_on_the_referer_is_one_copy() {
    assert!(cache::varies_on_navigation(&[header(
        "Vary",
        "Cookie, referer"
    )]));
    assert!(cache::varies_on_navigation(&[header("vary", "Origin")]));
    assert!(!cache::varies_on_navigation(&[header(
        "Vary",
        "Accept-Encoding, Accept-Language"
    )]));
    assert!(!cache::varies_on_navigation(&[header("X-Vary", "Referer")]));
}
