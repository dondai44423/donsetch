//! The caller's rules on the wire: redirects hop by hop under a guard, cookies
//! scoped to the host, scheme and path they belong to, resources that are not
//! documents, and the one-request path (`Session::once`).

use crate::testkit::{Reply, Server};
use crate::{Class, Hop, Kind, Once, Policy, Request, Session};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn page(title: &str) -> String {
    format!(
        "<html><head><title>{title}</title></head><body><main>{}</main></body></html>",
        "Plain words a reader came for. ".repeat(20)
    )
}

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

/// Every request's (path, header) for one header name.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

#[test]
fn a_hop_guard_is_asked_before_every_hop_and_can_stop_or_refuse_it() {
    let server = Server::start(|request| match request.path.as_str() {
        "/a" => Reply::Redirect("/b".into()),
        "/b" => Reply::Redirect("/c".into()),
        _ => Reply::ok(page("End")),
    });
    let asked: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let follow_all = {
        let asked = Arc::clone(&asked);
        Arc::new(move |from: &str, to: &str| {
            asked
                .lock()
                .unwrap()
                .push((from.to_string(), to.to_string()));
            Hop::Follow
        })
    };
    let session = Session::new(false);
    let followed = session.fetch(&Request {
        policy: Policy {
            hop: Some(follow_all),
            ..Policy::default()
        },
        ..request(server.url("/a"))
    });
    assert_eq!(followed.class, Class::Content, "{:?}", followed.failures);
    assert!(followed.final_url.ends_with("/c"), "{}", followed.final_url);
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(asked[0].0.ends_with("/a") && asked[0].1.ends_with("/b"));
    assert!(asked[1].0.ends_with("/b") && asked[1].1.ends_with("/c"));

    // Stop: the redirect itself is the answer, and /c is never requested.
    let before = server.hit_count();
    let stopped = session.fetch(&Request {
        policy: Policy {
            hop: Some(Arc::new(|_: &str, to: &str| {
                if to.ends_with("/c") {
                    Hop::Stop
                } else {
                    Hop::Follow
                }
            })),
            ..Policy::default()
        },
        cache: false,
        ..request(server.url("/a"))
    });
    assert_eq!(stopped.status, 302);
    assert!(stopped.final_url.ends_with("/b"), "{}", stopped.final_url);
    assert_eq!(stopped.attempts.len(), 1, "no other identity is asked");
    assert_eq!(server.hits()[before..].to_vec(), vec!["/a", "/b"]);

    // Deny: the fetch fails with the guard's reason, once, for every identity.
    let before = server.hit_count();
    let denied = session.fetch(&Request {
        identity: None,
        policy: Policy {
            hop: Some(Arc::new(|_: &str, _: &str| Hop::Deny("no".into()))),
            ..Policy::default()
        },
        cache: false,
        ..request(server.url("/a"))
    });
    assert_eq!(denied.class, Class::Network);
    assert_eq!(denied.attempts.len(), 1, "{:?}", denied.attempts);
    assert!(
        denied
            .failures
            .iter()
            .any(|failure| failure == "denied: no"),
        "{:?}",
        denied.failures
    );
    assert_eq!(server.hits()[before..].to_vec(), vec!["/a"]);
}

#[test]
fn a_finished_request_leaves_none_of_its_policy_with_the_session() {
    let server = Server::start(|request| match request.path.as_str() {
        "/a" => Reply::Redirect("/b".into()),
        _ => Reply::ok(page("End")),
    });
    // A guard that owns `held` and counts the hops it is asked about: what a
    // caller's guard captures stands for the caller's per-request state.
    let guard = |held: &Arc<()>, asked: &Arc<AtomicUsize>, verdict: Hop| -> crate::HopGuard {
        let held = Arc::clone(held);
        let asked = Arc::clone(asked);
        Arc::new(move |_: &str, _: &str| {
            let _ = &held;
            asked.fetch_add(1, Ordering::SeqCst);
            verdict.clone()
        })
    };
    let session = Session::new(false);

    let held = Arc::new(());
    let asked = Arc::new(AtomicUsize::new(0));
    let fetched = {
        let request = Request {
            policy: Policy {
                hop: Some(guard(&held, &asked, Hop::Follow)),
                ..Policy::default()
            },
            cache: false,
            ..request(server.url("/a"))
        };
        session.fetch(&request)
    };
    assert_eq!(fetched.class, Class::Content, "{:?}", fetched.failures);
    assert_eq!(asked.load(Ordering::SeqCst), 1, "the guard ruled its fetch");
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "the fetch is over, yet the session still holds its hop guard"
    );

    let held = Arc::new(());
    let asked = Arc::new(AtomicUsize::new(0));
    let reply = session
        .once(&Once {
            policy: Policy {
                hop: Some(guard(&held, &asked, Hop::Follow)),
                ..Policy::default()
            },
            ..Once::new(server.url("/a"))
        })
        .expect("once");
    assert_eq!(reply.status, 200);
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the guard ruled its request"
    );
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "the request is over, yet the session still holds its hop guard"
    );

    // The worker and wire those requests used still follow the next
    // request's own rules: this guard stops at /b, and /b is never dialed.
    let held = Arc::new(());
    let asked = Arc::new(AtomicUsize::new(0));
    let before = server.hit_count();
    let stopped = session.fetch(&Request {
        policy: Policy {
            hop: Some(guard(&held, &asked, Hop::Stop)),
            ..Policy::default()
        },
        cache: false,
        ..request(server.url("/a"))
    });
    assert_eq!(stopped.status, 302);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(server.hits()[before..].to_vec(), vec!["/a"]);
}

#[test]
fn without_follow_the_redirect_comes_back_and_no_hop_is_dialed() {
    let server = Server::start(|request| match request.path.as_str() {
        "/a" => full(302, &[("Location", "/b"), ("Set-Cookie", "hop=1")], ""),
        _ => Reply::ok(page("End")),
    });
    let session = Session::new(false);
    let outcome = session.fetch(&Request {
        identity: None,
        policy: Policy {
            follow: false,
            ..Policy::default()
        },
        ..request(server.url("/a"))
    });
    assert_eq!(outcome.status, 302);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(server.hits(), vec!["/a"]);
    assert!(outcome
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("location") && value == "/b"));
    // The redirect's own Set-Cookie is kept for the host that set it.
    let identity = outcome.identity.expect("identity");
    assert_eq!(session.cookie_names("127.0.0.1", &identity), vec!["hop"]);
}

#[test]
fn a_non_web_redirect_ends_the_chain_on_the_3xx() {
    let server = Server::start(|_| Reply::Redirect("mailto:someone@example.com".into()));
    let session = Session::new(false);
    let outcome = session.fetch(&request(server.url("/")));
    assert_eq!(outcome.status, 302);
    assert_eq!(server.hit_count(), 1);
}

#[test]
fn a_redirect_loop_fails_named_after_the_limit() {
    let server = Server::start(|_| Reply::Redirect("/again".into()));
    let session = Session::new(false);
    let outcome = session.fetch(&Request {
        identity: None,
        ..request(server.url("/"))
    });
    assert_eq!(outcome.class, Class::Network);
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.starts_with("redirect:")),
        "{:?}",
        outcome.failures
    );
    assert_eq!(server.hit_count(), crate::REDIRECTS_MAX + 1);
}

#[test]
fn a_cached_pages_validators_never_ride_a_redirect_hop() {
    // /a is cached with an ETag; its revalidation is answered with a redirect
    // to /b, whose own ETag collides. A validator on hop two would draw a 304
    // that confirms /a's old body as /b's.
    let seen: Seen = Arc::default();
    let server = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            let conditional = request.header("if-none-match").map(str::to_string);
            seen.lock()
                .unwrap()
                .push((request.path.clone(), conditional.clone()));
            match (request.path.as_str(), conditional.is_some()) {
                ("/b", true) => full(304, &[("ETag", "\"collide\"")], ""),
                ("/b", false) => full(200, &[("Content-Type", "text/plain")], "b-body"),
                ("/a", true) => Reply::Redirect("/b".into()),
                _ => full(
                    200,
                    &[
                        ("Content-Type", "text/plain"),
                        ("Cache-Control", "max-age=0"),
                        ("ETag", "\"collide\""),
                    ],
                    "a-body",
                ),
            }
        })
    };
    let directory = std::env::temp_dir().join(format!("v5-redirect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let session = Session::at(directory.join("state.json"));
    let first = session.fetch(&request(server.url("/a")));
    assert_eq!(first.body.as_deref(), Some("a-body"));
    let second = session.fetch(&request(server.url("/a")));
    assert_eq!(second.status, 200);
    assert_eq!(second.body.as_deref(), Some("b-body"), "{second:?}");
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|(path, sent)| path == "/a" && sent.is_some()),
        "the revalidation was not sent: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|(path, sent)| path == "/b" && sent.is_some()),
        "a validator rode the hop: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_cookie_goes_only_where_its_host_scheme_and_path_allow() {
    let seen: Seen = Arc::default();
    let other = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            seen.lock().unwrap().push((
                format!("other{}", request.path),
                request.header("cookie").map(str::to_string),
            ));
            Reply::ok(page("Other"))
        })
    };
    // Another host name for the same loopback: `localhost` is not `127.0.0.1`
    // to a cookie (127.0.0.2 would be, but macOS does not route it).
    let target = other.url("/landing").replace("127.0.0.1", "localhost");
    let server = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            seen.lock().unwrap().push((
                request.path.clone(),
                request.header("cookie").map(str::to_string),
            ));
            match request.path.as_str() {
                "/set" => full(
                    200,
                    &[
                        ("Content-Type", "text/plain"),
                        ("Set-Cookie", "plain=1; Path=/"),
                        ("Set-Cookie", "secret=1; Secure; Path=/"),
                        ("Set-Cookie", "scoped=1; Path=/docs"),
                    ],
                    "set",
                ),
                "/away" => Reply::Redirect(target.clone()),
                _ => Reply::ok(page("Page")),
            }
        })
    };
    let session = Session::new(false);
    let cookie_on = |path: &str| -> Option<String> {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(seen_path, _)| seen_path == path)
            .and_then(|(_, cookie)| cookie.clone())
    };
    session.fetch(&request(server.url("/set")));
    session.fetch(&request(server.url("/docs/page")));
    session.fetch(&request(server.url("/docsearch")));
    session.fetch(&request(server.url("/away")));
    // Plain HTTP: never the Secure cookie.
    let docs = cookie_on("/docs/page").expect("cookie on /docs/page");
    assert!(
        docs.contains("plain=1") && docs.contains("scoped=1"),
        "{docs}"
    );
    assert!(!docs.contains("secret"), "{docs}");
    // `/docs` does not cover `/docsearch`.
    let search = cookie_on("/docsearch").expect("cookie on /docsearch");
    assert!(!search.contains("scoped"), "{search}");
    // A redirect to another host carries none of this host's cookies.
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path == "other/landing"),
        "the redirect never reached the other host"
    );
    assert_eq!(cookie_on("other/landing"), None);
}

#[test]
fn a_resource_that_is_not_a_document_is_content_on_the_first_answer() {
    let server = Server::start(|request| match request.path.as_str() {
        "/api" => full(
            200,
            &[("Content-Type", "application/json")],
            "{\"ok\":true}",
        ),
        "/raw" => full(200, &[], "set"),
        "/empty" => full(200, &[("Content-Type", "application/json")], ""),
        _ => full(200, &[("Content-Type", "text/html")], "<p>tiny</p>"),
    });
    let session = Session::new(false);
    for path in ["/api", "/raw"] {
        let outcome = session.fetch(&Request {
            identity: None,
            ..request(server.url(path))
        });
        assert_eq!(
            outcome.class,
            Class::Content,
            "{path}: {:?}",
            outcome.failures
        );
        assert_eq!(outcome.attempts.len(), 1, "{path}");
    }
    // An empty answer is not content, and a thin document still has to meet
    // the document contract.
    let empty = session.fetch(&request(server.url("/empty")));
    assert_ne!(empty.class, Class::Content);
    let thin = session.fetch(&request(server.url("/thin")));
    assert_ne!(thin.class, Class::Content);
    // A caller's own contract is applied to whatever came back.
    let strict = session.fetch(&Request {
        contract: Some(crate::Contract::generic()),
        ..request(server.url("/api"))
    });
    assert_ne!(strict.class, Class::Content);
}

#[test]
fn a_navigation_carries_the_callers_referer_and_a_subresource_does_not() {
    let seen: Seen = Arc::default();
    let server = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            seen.lock().unwrap().push((
                request.path.clone(),
                request.header("referer").map(str::to_string),
            ));
            Reply::ok(page("Page"))
        })
    };
    let session = Session::new(false);
    let policy = Policy {
        navigation: vec![
            ("referer".into(), "https://from.example/".into()),
            ("sec-fetch-site".into(), "cross-site".into()),
        ],
        ..Policy::default()
    };
    session.fetch(&Request {
        policy: policy.clone(),
        ..request(server.url("/nav"))
    });
    session
        .once(&Once {
            kind: Kind::Subresource,
            policy,
            ..Once::new(server.url("/sub"))
        })
        .expect("once");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter()
            .find(|(path, _)| path == "/nav")
            .and_then(|(_, r)| r.clone()),
        Some("https://from.example/".into())
    );
    assert_eq!(
        seen.iter()
            .find(|(path, _)| path == "/sub")
            .and_then(|(_, r)| r.clone()),
        None
    );
}

#[test]
fn once_is_one_request_with_the_callers_headers_and_no_search() {
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let server = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            seen.lock().unwrap().push((
                request.path.clone(),
                request.header("user-agent").unwrap_or_default().to_string(),
            ));
            match request.path.as_str() {
                "/set" => full(403, &[("Set-Cookie", "kept=1")], "no"),
                "/bin" => full(200, &[], "\u{0}\u{1}binary\u{ff}"),
                _ => full(
                    200,
                    &[],
                    &format!("cookie={}", request.header("cookie").unwrap_or_default()),
                ),
            }
        })
    };
    let session = Session::new(false);
    // A wall-looking status is still the reply: no other identity is tried.
    let walled = session
        .once(&Once {
            cookies: true,
            ..Once::new(server.url("/set"))
        })
        .expect("once");
    assert_eq!(walled.status, 403);
    assert_eq!(walled.identity, "chrome-local");
    assert_eq!(server.hit_count(), 1);
    // Its cookie was kept, and is spent only when asked to.
    let spent = session
        .once(&Once {
            cookies: true,
            ..Once::new(server.url("/echo"))
        })
        .expect("once");
    assert_eq!(String::from_utf8_lossy(&spent.bytes), "cookie=kept=1");
    let unspent = session.once(&Once::new(server.url("/echo"))).expect("once");
    assert_eq!(String::from_utf8_lossy(&unspent.bytes), "cookie=");
    // On `plain`, the caller's user-agent is the client.
    session
        .once(&Once {
            identity: Some("plain".into()),
            headers: vec![("user-agent".into(), "Legacy/1.0".into())],
            ..Once::new(server.url("/ua"))
        })
        .expect("once");
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|(path, agent)| path == "/ua" && agent == "Legacy/1.0"));
    // Bytes come back as served.
    let binary = session.once(&Once::new(server.url("/bin"))).expect("once");
    assert_eq!(binary.bytes, "\u{0}\u{1}binary\u{ff}".as_bytes());
    // An unknown identity is refused before any request.
    let before = server.hit_count();
    let error = session
        .once(&Once {
            identity: Some("nobody".into()),
            ..Once::new(server.url("/echo"))
        })
        .expect_err("unknown identity");
    assert!(error.starts_with("identity:"), "{error}");
    assert_eq!(server.hit_count(), before);
}

#[test]
fn a_transport_failure_names_its_class() {
    // Nothing listens on the port: a refused connection.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let session = Session::new(false);
    let error = session
        .once(&Once {
            timeout: Duration::from_secs(3),
            ..Once::new(format!("http://127.0.0.1:{port}/"))
        })
        .expect_err("refused");
    assert!(error.starts_with("connect:"), "{error}");
    let silent = Server::start(|_| Reply::Silence);
    let error = session
        .once(&Once {
            timeout: Duration::from_millis(400),
            ..Once::new(silent.url("/"))
        })
        .expect_err("silent");
    assert!(error.starts_with("timeout:"), "{error}");
}

#[test]
fn a_rate_limit_that_names_its_wait_is_not_asked_again_as_someone_else() {
    let server = Server::start(|_| full(429, &[("Retry-After", "30")], "Too Many Requests"));
    let session = Session::new(false);
    let outcome = session.fetch(&Request {
        identity: None,
        ..request(server.url("/"))
    });
    assert_eq!(outcome.status, 429);
    assert_eq!(outcome.attempts.len(), 1, "{:?}", outcome.attempts);
    assert_eq!(server.hit_count(), 1);
}

#[test]
fn a_later_network_failure_does_not_erase_an_earlier_answer() {
    // The first identity is refused; the second never reaches the server (the
    // listener is gone). The outcome is the refusal, with its status and body.
    let hits = Arc::new(AtomicUsize::new(0));
    let server = {
        let hits = Arc::clone(&hits);
        Server::start(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            Reply::Full {
                status: 403,
                headers: Vec::new(),
                body: b"Forbidden".to_vec(),
            }
        })
    };
    let url = server.url("/");
    let session = Session::new(false);
    let outcome = {
        // Stop the server once it has answered: later attempts fail to connect.
        let stopper = std::thread::spawn({
            let hits = Arc::clone(&hits);
            move || {
                while hits.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                drop(server);
            }
        });
        let outcome = session.fetch(&Request {
            identity: None,
            ..request(url)
        });
        stopper.join().expect("stopper");
        outcome
    };
    assert_eq!(outcome.status, 403, "{:?}", outcome.attempts);
    assert_ne!(outcome.class, Class::Network);
    assert_eq!(outcome.body.as_deref(), Some("Forbidden"));
}

#[test]
fn a_cookie_the_site_expires_is_never_sent_again() {
    let seen: Seen = Arc::default();
    let server = {
        let seen = Arc::clone(&seen);
        Server::start(move |request| {
            seen.lock().unwrap().push((
                request.path.clone(),
                request.header("cookie").map(str::to_string),
            ));
            let cookie = match request.path.as_str() {
                "/login" => "session=alice; Path=/",
                "/logout" => "session=; Max-Age=0; Path=/",
                // Max-Age wins over an Expires still in the future.
                "/drop" => "kept=1; Max-Age=0; Expires=Wed, 01 Jan 2200 00:00:00 GMT; Path=/",
                "/keep" => "kept=1; Max-Age=3600; Expires=Thu, 01 Jan 1970 00:00:01 GMT; Path=/",
                _ => "",
            };
            let mut headers = vec![("Content-Type", "text/plain")];
            if !cookie.is_empty() {
                headers.push(("Set-Cookie", cookie));
            }
            full(200, &headers, "ok")
        })
    };
    let session = Session::new(false);
    let fetch = |path: &str| session.fetch(&request(server.url(path)));
    let host = "127.0.0.1";
    fetch("/login");
    assert_eq!(
        session.cookie_names(host, "chrome"),
        vec!["session".to_string()]
    );
    fetch("/logout");
    fetch("/keep");
    assert_eq!(
        session.cookie_names(host, "chrome"),
        vec!["kept".to_string()]
    );
    fetch("/drop");
    fetch("/after");
    assert!(session.cookie_names(host, "chrome").is_empty());
    let seen = seen.lock().unwrap();
    let after = seen
        .iter()
        .find(|(path, _)| path == "/after")
        .expect("dialed");
    assert_eq!(after.1, None, "{seen:?}");
}

#[test]
fn a_proxy_that_refuses_credentials_names_its_407() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let proxy = format!("http://user:wrong@{}", listener.local_addr().expect("addr"));
    let accepted = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && socket.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        let _ = socket.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        String::from_utf8_lossy(&head).into_owned()
    });
    let session = Session::new(false);
    let error = session
        .once(&crate::Once {
            proxy: Some(proxy),
            timeout: Duration::from_secs(5),
            ..crate::Once::new("https://example.test/")
        })
        .expect_err("a refused tunnel is no reply");
    let head = accepted.join().expect("proxy");
    assert!(head.starts_with("CONNECT example.test:443 "), "{head}");
    assert!(
        error.starts_with("proxy:") && error.contains("407"),
        "{error}"
    );
}

#[test]
fn a_cancelled_fetch_drops_its_connection_and_asks_no_one_else() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/slow", listener.local_addr().expect("addr"));
    let (closed_tx, closed) = std::sync::mpsc::channel();
    let accepted = Arc::new(AtomicUsize::new(0));
    std::thread::spawn({
        let accepted = Arc::clone(&accepted);
        move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { return };
                accepted.fetch_add(1, Ordering::SeqCst);
                let closed_tx = closed_tx.clone();
                std::thread::spawn(move || {
                    // Never answer; report when the client hangs up.
                    let mut buffer = [0u8; 4096];
                    while socket
                        .read(&mut buffer)
                        .map(|read| read > 0)
                        .unwrap_or(false)
                    {}
                    let _ = closed_tx.send(());
                });
            }
        }
    });
    let cancel = crate::Cancel::new();
    let session = Arc::new(Session::new(false));
    let fetch = std::thread::spawn({
        let session = Arc::clone(&session);
        let cancel = cancel.clone();
        move || {
            let mut request = request(url);
            request.identity = None;
            request.deadline = Duration::from_secs(60);
            request.policy.request_max = Duration::from_secs(60);
            request.policy.cancel = Some(cancel);
            session.fetch(&request)
        }
    });
    while accepted.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let asked = Instant::now();
    cancel.cancel();
    closed
        .recv_timeout(Duration::from_secs(2))
        .expect("the connection closes when the caller stops waiting");
    let outcome = fetch.join().expect("fetch");
    assert!(asked.elapsed() < Duration::from_secs(2));
    assert_eq!(outcome.attempts.len(), 1, "{:?}", outcome.attempts);
    assert!(
        outcome.attempts[0]
            .note
            .as_deref()
            .is_some_and(|note| note.starts_with("cancelled:")),
        "{:?}",
        outcome.attempts
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}
