//! The library seam: one `Session` shared by threads and async callers.

use crate::testkit::{Reply, Server};
use crate::{Class, Request, Session};
use std::sync::Arc;
use std::time::Duration;

fn page(title: &str) -> String {
    format!(
        "<html><head><title>{title}</title></head><body><main>{}</main></body></html>",
        "Plain words a reader came for. ".repeat(20)
    )
}

fn content_server() -> Server {
    Server::start(|_| Reply::ok(page("Fixture")))
}

fn request(url: String) -> Request {
    Request {
        identity: Some("chrome".into()),
        pace: Duration::ZERO,
        deadline: Duration::from_secs(10),
        ..Request::new(url)
    }
}

#[test]
fn a_session_is_shared_by_threads() {
    fn shared<T: Send + Sync + 'static>() {}
    shared::<Session>();
}

#[test]
fn a_learning_call_folds_back_and_a_cold_probe_does_not() {
    let server = content_server();
    let session = Session::new(false);
    let cold = session.fetch(&Request {
        learn: false,
        ..request(server.url("/"))
    });
    assert_eq!(cold.class, Class::Content, "{:?}", cold.failures);
    assert!(session.state().origins.is_empty(), "a cold probe learned");
    let warm = session.fetch(&request(server.url("/")));
    assert_eq!(warm.class, Class::Content);
    assert!(
        session.state().origins.contains_key("127.0.0.1"),
        "{:?}",
        session.state().origins.keys()
    );
}

#[test]
fn the_body_headers_and_bytes_come_back_when_asked() {
    let server = Server::start(|request| match request.path.as_str() {
        "/doc.bin" => Reply::Full {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/octet-stream".into()),
                ("X-Fixture".into(), "1".into()),
            ],
            body: vec![0xff, 0xfe, 0x00, 0x80],
        },
        _ => Reply::ok(page("Fixture")),
    });
    let session = Session::new(false);
    let outcome = session.fetch(&request(server.url("/")));
    assert!(outcome
        .body
        .as_deref()
        .is_some_and(|body| body.contains("<title>Fixture</title>")));
    assert!(outcome.bytes.is_none(), "text needs no second copy");
    let binary = session.fetch(&Request {
        contract: Some(crate::Contract {
            min_text_chars: Some(0),
            ..crate::Contract::generic()
        }),
        ..request(server.url("/doc.bin"))
    });
    assert_eq!(
        binary.bytes.as_deref(),
        Some(&[0xff, 0xfe, 0x00, 0x80][..]),
        "{:?}",
        binary.failures
    );
    assert!(binary
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("x-fixture") && value == "1"));
}

#[test]
fn async_callers_share_one_session_through_spawn_blocking() {
    let server = Arc::new(content_server());
    let session = Arc::new(Session::new(false));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let classes = runtime.block_on(async {
        let mut tasks = Vec::new();
        for index in 0..6 {
            let session = Arc::clone(&session);
            let url = server.url(&format!("/page/{index}"));
            tasks.push(tokio::task::spawn_blocking(move || {
                session.fetch(&request(url)).class
            }));
        }
        let mut classes = Vec::new();
        for task in tasks {
            classes.push(task.await.expect("join"));
        }
        classes
    });
    assert!(
        classes.iter().all(|class| *class == Class::Content),
        "{classes:?}"
    );
    assert!(session.state().origins.contains_key("127.0.0.1"));
}

#[test]
fn a_proxy_carries_the_whole_request() {
    // A name that resolves nowhere: only a proxy can answer for it. An HTTP
    // proxy receives the absolute URL as the request target.
    let proxy = Server::start(|request| {
        if request.path.starts_with("http://www.unresolvable.test/") {
            Reply::ok(page("Through the proxy"))
        } else {
            Reply::Full {
                status: 502,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }
    });
    let session = Session::new(false);
    let direct = session.fetch(&request("http://www.unresolvable.test/".into()));
    assert_ne!(direct.class, Class::Content);
    let via = session.fetch(&Request {
        proxy: Some(proxy.url("")),
        ..request("http://www.unresolvable.test/".into())
    });
    assert_eq!(
        via.class,
        Class::Content,
        "{:?} {:?}",
        via.failures,
        proxy.hits()
    );
    assert_eq!(via.title, "Through the proxy");
}

#[test]
fn an_apps_own_call_carries_no_navigation_header() {
    // A CORS call with `sec-fetch-user` or `upgrade-insecure-requests` is a
    // combination no browser sends; data paths go out as subresources.
    let server = Server::start(|request| {
        let navigation = ["upgrade-insecure-requests", "sec-fetch-user"]
            .iter()
            .any(|name| request.header(name).is_some());
        Reply::ok(if navigation {
            "navigation"
        } else {
            "subresource"
        })
    });
    let identity = crate::identity::lookup("chrome").expect("chrome");
    let wire = crate::http::Wire::via(identity, None).expect("wire");
    let url = server.url("/api");
    let call = |kind| crate::http::Call {
        method: "GET",
        url: &url,
        cookie: None,
        timeout: Duration::from_secs(5),
        extra: &[
            ("accept", "application/json"),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-origin"),
        ],
        body: None,
        cap: None,
        kind,
    };
    assert_eq!(
        wire.subresource(identity, call(crate::http::Kind::Subresource))
            .expect("call")
            .body,
        "subresource"
    );
    assert_eq!(
        wire.call(identity, call(crate::http::Kind::Navigate))
            .expect("call")
            .body,
        "navigation"
    );
}

#[test]
fn a_session_can_be_dropped_inside_an_async_context() {
    // Each client owns a runtime; tokio panics when one is dropped where
    // blocking is not allowed, and callers drop sessions wherever they like.
    let server = content_server();
    let session = Session::new(false);
    assert_eq!(
        session.fetch(&request(server.url("/"))).class,
        Class::Content
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move { drop(session) });
}

/// A wall that answers every client but one. `pass` reads the user agent.
fn walled(wall: fn() -> Reply, pass: fn(&str) -> bool) -> Server {
    Server::start(move |request| {
        if pass(request.header("user-agent").unwrap_or_default()) {
            Reply::ok(page("Fixture"))
        } else {
            wall()
        }
    })
}

fn cold(url: String) -> Request {
    Request {
        learn: false,
        pace: Duration::ZERO,
        deadline: Duration::from_secs(10),
        ..Request::new(url)
    }
}

fn tried(outcome: &crate::Outcome) -> Vec<&str> {
    outcome
        .attempts
        .iter()
        .filter(|attempt| attempt.transport == "document")
        .map(|attempt| attempt.identity.as_str())
        .collect()
}

#[test]
fn a_named_vendor_sends_the_next_attempt_to_what_passed_it() {
    // Cloudflare's managed challenge, served to every client but the app client.
    let server = walled(
        || Reply::Full {
            status: 403,
            headers: vec![("cf-mitigated".into(), "challenge".into())],
            body: b"<html><title>Just a moment...</title></html>".to_vec(),
        },
        // The app client's agent, as it goes on the wire.
        |agent| agent.starts_with("okhttp/"),
    );
    let outcome = Session::new(false).fetch(&cold(server.url("/")));
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert_eq!(tried(&outcome), ["plain", "okhttp"]);
}

#[test]
fn a_human_check_costs_one_identity_beyond_the_class_that_saw_it() {
    // A human widget is a class decision before it is a human step: the class
    // that claims no browser is the one a site asks to run a program instead
    // (Reddit serves the desktop Chrome class its captcha and every other class
    // its JS gate). One more identity is spent finding that out, and the second
    // human wall ends the encounter.
    let server = walled(
        || Reply::Full {
            status: 403,
            headers: Vec::new(),
            body: b"<html>Are you a robot?</html>".to_vec(),
        },
        |_| false,
    );
    let outcome = Session::new(false).fetch(&cold(server.url("/")));
    assert_eq!(outcome.class, Class::WallInteraction);
    assert_eq!(tried(&outcome), ["plain", "chrome"]);
    assert_eq!(server.hit_count(), 2, "one identity past the class that saw it");
}

#[test]
fn a_wall_that_outlasts_every_attempt_is_still_named_with_its_body() {
    // DataDome: two identities are known to pass it; here neither does.
    let server = walled(
        || Reply::Full {
            status: 403,
            headers: vec![("x-datadome".into(), "protected".into())],
            body: b"<html>blocked</html>".to_vec(),
        },
        |_| false,
    );
    let outcome = Session::new(false).fetch(&cold(server.url("/")));
    assert_eq!(outcome.class, Class::WallInteraction);
    assert_eq!(tried(&outcome), ["plain", "chrome-android", "chrome-win"]);
    assert_eq!(
        outcome.wall.as_ref().map(|wall| wall.vendor.as_str()),
        Some("datadome")
    );
    assert_eq!(outcome.body.as_deref(), Some("<html>blocked</html>"));
}

#[test]
fn a_forced_identity_is_never_reordered() {
    let server = walled(
        || Reply::Full {
            status: 403,
            headers: vec![("cf-mitigated".into(), "challenge".into())],
            body: Vec::new(),
        },
        |_| false,
    );
    let outcome = Session::new(false).fetch(&Request {
        identity: Some("chrome".into()),
        ..cold(server.url("/"))
    });
    assert_eq!(tried(&outcome), ["chrome"]);
}

#[test]
fn a_page_every_client_is_served_identically_costs_two_requests_not_the_portfolio() {
    // A short, honest page: the generic contract finds it thin whoever asks.
    let short = Server::start(|_| {
        Reply::ok("<html><head><title>Short</title></head><body><p>Brief.</p></body></html>")
    });
    let outcome = Session::new(false).fetch(&cold(short.url("/")));
    assert_eq!(outcome.class, Class::Incomplete);
    assert_eq!(short.hit_count(), 2, "{:?}", tried(&outcome));
    // A per-request nonce outside the text does not make the answers differ.
    let nonce = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&nonce);
    let noncing = Server::start(move |_| {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Reply::ok(format!(
            "<html><body><p data-n=\"{n}\">Brief.</p><script>var n={n}</script></body></html>"
        ))
    });
    Session::new(false).fetch(&cold(noncing.url("/")));
    assert_eq!(noncing.hit_count(), 2);
    // A page whose text differs per client keeps searching the portfolio.
    let varying = Server::start(|request| {
        Reply::ok(format!(
            "<html><body><p>{}</p></body></html>",
            request.header("user-agent").unwrap_or_default()
        ))
    });
    Session::new(false).fetch(&cold(varying.url("/")));
    assert!(varying.hit_count() > 2);
}

/// `www.feed.test` through a proxy: its document is a shell, its feed answers
/// with `feed` (status, body) — the test changes it between calls.
fn feed_site(feed: Arc<std::sync::Mutex<(u16, String)>>) -> Server {
    Server::start(move |request| {
        if request.path.ends_with("/feed.rss") {
            let (status, body) = feed.lock().unwrap().clone();
            Reply::Full {
                status,
                headers: vec![("Content-Type".into(), "application/rss+xml".into())],
                body: body.into_bytes(),
            }
        } else {
            Reply::ok(
                "<html><head><title>App</title></head><body><div id=root></div></body></html>",
            )
        }
    })
}

fn items(n: usize) -> String {
    (0..n).map(|i| format!("<item><title>Story number {i} about the river</title><link>http://www.feed.test/{i}</link></item>")).collect()
}

fn via(proxy: &Server) -> Request {
    Request {
        proxy: Some(proxy.url("")),
        pace: Duration::ZERO,
        deadline: Duration::from_secs(10),
        ..Request::new("http://www.feed.test/")
    }
}

fn documents(outcome: &crate::Outcome) -> usize {
    tried(outcome).len()
}

#[test]
fn an_origin_whose_interface_answered_is_asked_there_first_and_kept_through_a_rate_limit() {
    let feed = Arc::new(std::sync::Mutex::new((200, items(5))));
    let proxy = feed_site(Arc::clone(&feed));
    let session = Session::new(false);
    let first = session.fetch(&via(&proxy));
    assert_eq!(first.transport, "data", "{:?}", first.attempts);
    assert_eq!(
        documents(&first),
        1,
        "first contact reads the document first"
    );
    let second = session.fetch(&via(&proxy));
    assert_eq!(
        (second.transport.as_str(), documents(&second)),
        ("data", 0),
        "{:?}",
        second.attempts
    );
    // A 429 asks for time, not for another route.
    feed.lock().unwrap().0 = 429;
    session.fetch(&via(&proxy));
    assert_eq!(
        session.state().origins["www.feed.test"].route.as_deref(),
        Some("data")
    );
}

#[test]
fn an_interface_that_stops_answering_is_forgotten() {
    let feed = Arc::new(std::sync::Mutex::new((200, items(5))));
    let proxy = feed_site(Arc::clone(&feed));
    let session = Session::new(false);
    session.fetch(&via(&proxy));
    assert_eq!(
        session.state().origins["www.feed.test"].route.as_deref(),
        Some("data")
    );
    feed.lock().unwrap().0 = 404;
    let lost = session.fetch(&via(&proxy));
    assert_ne!(lost.class, Class::Content);
    assert!(
        documents(&lost) >= 1,
        "the encounter ran whole: {:?}",
        lost.attempts
    );
    assert_eq!(session.state().origins["www.feed.test"].route, None);
}

#[test]
fn a_session_keeps_its_state_and_pages_where_its_owner_says() {
    let dir = std::env::temp_dir().join(format!(
        "v5session-{}-{}",
        std::process::id(),
        crate::state::now_ms()
    ));
    let path = dir.join("v5").join("state.json");
    let server = Server::start(|request| Reply::Full {
        status: if request.header("if-none-match") == Some("\"s\"") {
            304
        } else {
            200
        },
        headers: vec![
            ("Content-Type".into(), "text/html".into()),
            ("ETag".into(), "\"s\"".into()),
        ],
        body: page("Kept").into_bytes(),
    });
    Session::at(&path).fetch(&request(server.url("/")));
    assert!(path.exists(), "the state file is where the owner put it");
    assert_eq!(
        std::fs::read_dir(dir.join("v5").join("pages"))
            .map(|pages| pages.count())
            .unwrap_or(0),
        1,
        "and the page beside it"
    );
    // A new session on the same path starts from what the first learned.
    let again = Session::at(&path);
    assert!(again.state().origins.contains_key("127.0.0.1"));
    assert_eq!(
        again.fetch(&request(server.url("/"))).cache.as_deref(),
        Some("revalidated")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_stored_winner_that_starts_failing_is_replaced_inside_one_encounter() {
    let refuse_plain = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch = Arc::clone(&refuse_plain);
    let server = Server::start(move |request| {
        let plain = request
            .header("user-agent")
            .unwrap_or_default()
            .starts_with("donsetch/");
        if plain && switch.load(std::sync::atomic::Ordering::Relaxed) {
            Reply::ok("<html><head><title>Fixture</title></head><body><p>Thin.</p></body></html>")
        } else {
            Reply::ok(page("Fixture"))
        }
    });
    let session = Session::new(false);
    let learn = || {
        session.fetch(&Request {
            pace: Duration::ZERO,
            deadline: Duration::from_secs(10),
            ..Request::new(server.url("/"))
        })
    };
    assert_eq!(tried(&learn()), ["plain"], "plain is the stored winner");
    refuse_plain.store(true, std::sync::atomic::Ordering::Relaxed);
    let replaced = learn();
    assert_eq!(replaced.class, Class::Content, "{:?}", replaced.attempts);
    assert_eq!(
        tried(&replaced).len(),
        2,
        "the old winner, then its replacement: {:?}",
        tried(&replaced)
    );
    let winner = replaced.identity.clone().expect("an identity");
    assert_ne!(winner, "plain");
    assert_eq!(
        tried(&learn()),
        [winner.as_str()],
        "the replacement goes first next time"
    );
}
