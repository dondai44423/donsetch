//! The library seam: one `Session` shared by threads and async callers.

use crate::testkit::{Reply, Server};
use crate::{Class, Once, Request, Session};
use std::sync::{Arc, Mutex};
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
        // The fixtures are loopback servers; the test opts in explicitly.
        allow_private: true,
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

/// A SOCKS5 proxy on 127.0.0.1 that logs the `user:pass` each connection
/// authenticates with (RFC 1929; empty when none is offered), grants the
/// CONNECT and answers the tunnelled request itself.
fn socks5_proxy() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    fn field(stream: &mut TcpStream) -> std::io::Result<String> {
        let mut len = [0u8; 1];
        stream.read_exact(&mut len)?;
        let mut bytes = vec![0u8; usize::from(len[0])];
        stream.read_exact(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    fn serve(mut stream: TcpStream, log: &Mutex<Vec<String>>) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut greeting = [0u8; 2];
        stream.read_exact(&mut greeting)?;
        let mut methods = vec![0u8; usize::from(greeting[1])];
        stream.read_exact(&mut methods)?;
        let credentials = if methods.contains(&0x02) {
            stream.write_all(&[0x05, 0x02])?;
            let mut version = [0u8; 1];
            stream.read_exact(&mut version)?;
            let user = field(&mut stream)?;
            let pass = field(&mut stream)?;
            stream.write_all(&[0x01, 0x00])?;
            format!("{user}:{pass}")
        } else {
            stream.write_all(&[0x05, 0x00])?;
            String::new()
        };
        log.lock().unwrap().push(credentials);
        let mut request = [0u8; 4];
        stream.read_exact(&mut request)?;
        let address = match request[3] {
            0x01 => 4,
            0x04 => 16,
            _ => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len)?;
                usize::from(len[0])
            }
        };
        let mut rest = vec![0u8; address + 2];
        stream.read_exact(&mut rest)?;
        stream.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 80])?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        let body = page("Through the proxy");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || serve(stream, &seen));
        }
    });
    (addr, log)
}

fn once_via(proxy: String, url: String) -> Once {
    Once {
        proxy: Some(proxy),
        timeout: Duration::from_secs(5),
        ..Once::new(url)
    }
}

#[test]
fn a_proxy_url_the_transport_cannot_read_fails_the_request_and_dials_nothing() {
    // A '/', '?' or '#' in the password ends the URL's address early. The
    // origin answers anyone who reaches it.
    let origin = Server::start(|_| Reply::ok(page("Direct from the origin")));
    let (proxy, proxied) = socks5_proxy();
    for scheme in ["socks5h", "http"] {
        for userinfo in ["alice:pa/ss", "alice:pa#ss", "alice:pa?ss", "alice:/x"] {
            let url = format!("{scheme}://{userinfo}@{proxy}");
            let session = Session::new(false);
            let once = session.once(&once_via(url.clone(), origin.url("/page")));
            let fetched = session.fetch(&Request {
                proxy: Some(url.clone()),
                ..request(origin.url("/page"))
            });
            let report = format!(
                "{url}: one-shot {:?}; document {:?} {} {:?}; the origin took {} direct request(s)",
                once.as_ref().map(|reply| reply.status),
                fetched.class,
                fetched.status,
                fetched.failures,
                origin.hit_count()
            );
            let named = format!("proxy: cannot read proxy URL \"{scheme}://***@{proxy}\": ");
            assert!(
                once.as_ref().is_err_and(|error| error.starts_with(&named)),
                "{report}"
            );
            assert!(
                fetched.class == Class::Network
                    && fetched
                        .failures
                        .last()
                        .is_some_and(|error| error.starts_with(&named)),
                "{report}"
            );
            let mut errors = once.as_ref().err().into_iter().chain(&fetched.failures);
            assert!(
                errors.all(|error| !error.contains("alice")),
                "the credentials are echoed: {report}"
            );
        }
    }
    assert_eq!(origin.hit_count(), 0, "{:?}", origin.hits());
    assert!(proxied.lock().unwrap().is_empty());
}

#[test]
fn a_proxy_scheme_in_capitals_names_the_same_proxy() {
    let origin = Server::start(|_| Reply::ok(page("Direct from the origin")));
    let (proxy, proxied) = socks5_proxy();
    let reply = Session::new(false).once(&once_via(
        format!("SOCKS5H://alice:s3cret@{proxy}"),
        origin.url("/page"),
    ));
    let body = reply
        .as_ref()
        .map(|reply| String::from_utf8_lossy(&reply.bytes).into_owned());
    assert!(
        body.as_ref()
            .is_ok_and(|body| body.contains("Through the proxy")),
        "{body:?}; the origin took {} direct request(s)",
        origin.hit_count()
    );
    assert_eq!(*proxied.lock().unwrap(), ["alice:s3cret"]);
    assert_eq!(origin.hit_count(), 0);
}

// Negative: the proxy URLs the transport reads carry the request as before,
// a percent-encoded '/' in the password included.
#[test]
fn a_readable_proxy_url_still_carries_the_request() {
    let origin = Server::start(|_| Reply::ok(page("Direct from the origin")));
    let (socks, proxied) = socks5_proxy();
    let http = Server::start(|request| {
        // "alice:s3cret" in base64.
        if request.header("proxy-authorization") == Some("Basic YWxpY2U6czNjcmV0") {
            Reply::ok(page("Through the proxy"))
        } else {
            Reply::Full {
                status: 407,
                headers: vec![("proxy-authenticate".into(), "Basic".into())],
                body: Vec::new(),
            }
        }
    });
    let target = origin.url("/page");
    for (url, credentials) in [
        (
            format!("socks5h://alice:s3cret@{socks}"),
            Some("alice:s3cret"),
        ),
        (
            format!("socks5h://alice:pa%2Fss@{socks}"),
            Some("alice:pa/ss"),
        ),
        (format!("socks5://{socks}"), Some("")),
        (format!("http://alice:s3cret@{}", http.addr), None),
    ] {
        let session = Session::new(false);
        let once = session.once(&once_via(url.clone(), target.clone()));
        let fetched = session.fetch(&Request {
            proxy: Some(url.clone()),
            ..request(target.clone())
        });
        let seen = std::mem::take(&mut *proxied.lock().unwrap());
        assert!(
            once.as_ref().is_ok_and(|reply| reply.status == 200)
                && fetched.title == "Through the proxy",
            "{url}: one-shot {:?}; document {:?} {:?}",
            once.as_ref().map(|reply| reply.status),
            fetched.class,
            fetched.failures
        );
        if let Some(credentials) = credentials {
            assert!(
                !seen.is_empty() && seen.iter().all(|seen| seen == credentials),
                "{url}: {seen:?}"
            );
        }
    }
    let hops = http.hits();
    assert!(
        !hops.is_empty() && hops.iter().all(|hop| hop.starts_with(&target)),
        "{hops:?}"
    );
    assert_eq!(origin.hit_count(), 0, "{:?}", origin.hits());
}

#[test]
fn a_refused_proxy_url_is_named_with_its_credentials_masked() {
    for (raw, named) in [
        (
            "socks5h://alice:pa/ss@127.0.0.1:1080",
            "socks5h://***@127.0.0.1:1080",
        ),
        // Without a scheme the user name would read as one.
        ("alice:s3cret@127.0.0.1:1080", "***@127.0.0.1:1080"),
        ("ftp://alice:s3cret@127.0.0.1:21", "ftp://***@127.0.0.1:21"),
        (
            "http://alice:s3cret@127.0.0.1:3128/path",
            "http://***@127.0.0.1:3128/path",
        ),
    ] {
        let error = crate::proxy_url(raw).expect_err(raw);
        assert!(
            error.starts_with(&format!("cannot read proxy URL {named:?}: ")),
            "{error}"
        );
        assert!(
            !error.contains("alice") && !error.contains("s3cret") && !error.contains("pa/ss"),
            "{error}"
        );
    }
    assert_eq!(
        crate::proxy_url("SOCKS5H://alice:s3cret@127.0.0.1:1080").as_deref(),
        Ok("socks5h://alice:s3cret@127.0.0.1:1080")
    );
}

/// A request handed no proxy is a direct connection, whatever the
/// environment names: `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` are not
/// read. The variables are set only in a child process of this test binary,
/// which runs this test alone; this process sets none, so the test does not
/// depend on the runner.
#[test]
fn a_request_handed_no_proxy_is_direct_whatever_the_environment_names() {
    const TARGET: &str = "V5CORE_TEST_DIRECT_TARGET";
    if let Some(target) = std::env::var_os(TARGET) {
        // The child: a one-shot and a document read, no proxy handed over.
        let target = target.into_string().expect("target");
        let session = Session::new(false);
        let once = session.once(&Once {
            timeout: Duration::from_secs(5),
            ..Once::new(target.clone())
        });
        let fetched = session.fetch(&request(target));
        let body = once
            .as_ref()
            .map(|reply| String::from_utf8_lossy(&reply.bytes).into_owned());
        assert!(
            body.as_ref()
                .is_ok_and(|body| body.contains("Direct from the origin"))
                && fetched.title == "Direct from the origin",
            "one-shot {body:?}; document {:?} {:?} {:?}",
            fetched.class,
            fetched.title,
            fetched.failures
        );
        return;
    }
    let origin = Server::start(|_| Reply::ok(page("Direct from the origin")));
    // As an HTTP proxy it answers an absolute-form request for any origin.
    let proxy = Server::start(|_| Reply::ok(page("Through the proxy")));
    let name = "session_tests::a_request_handed_no_proxy_is_direct_whatever_the_environment_names";
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
    child
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(TARGET, origin.url("/page"));
    for variable in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        child.env(variable, proxy.url(""));
    }
    for variable in ["NO_PROXY", "no_proxy", "REQUEST_METHOD"] {
        child.env_remove(variable);
    }
    let output = child.output().expect("run the child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(proxy.hit_count(), 0, "{:?}", proxy.hits());
    assert!(origin.hit_count() >= 2, "{:?}", origin.hits());
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
    assert_eq!(
        server.hit_count(),
        2,
        "one identity past the class that saw it"
    );
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

/// An encounter that lets the session choose (no forced identity), cold.
fn chosen(url: String) -> Request {
    Request {
        identity: None,
        learn: false,
        pace: Duration::ZERO,
        deadline: Duration::from_secs(10),
        ..Request::new(url)
    }
}

#[test]
fn a_refused_connection_is_asked_once() {
    // A port nothing listens on: the refusal comes before any client is
    // shown, so no other identity is asked.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let started = std::time::Instant::now();
    let outcome = Session::new(false).fetch(&chosen(format!("http://127.0.0.1:{port}/")));
    assert_eq!(outcome.class, Class::Network);
    assert_eq!(outcome.attempts.len(), 1, "{:?}", outcome.attempts);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_silent_server_is_asked_by_one_other_wire_class_on_a_short_clock() {
    let silent = Server::start(|_| Reply::Silence);
    let mut request = chosen(silent.url("/"));
    request.policy.request_max = Duration::from_millis(1500);
    let started = std::time::Instant::now();
    let outcome = Session::new(false).fetch(&request);
    let elapsed = started.elapsed();
    assert_eq!(outcome.class, Class::Network);
    // The first client and one of another wire class; never a third.
    assert_eq!(outcome.attempts.len(), 2, "{:?}", outcome.attempts);
    let wires: Vec<&str> = outcome
        .attempts
        .iter()
        .map(|attempt| {
            crate::identity::lookup(&attempt.identity)
                .unwrap()
                .wire_class()
        })
        .collect();
    assert_ne!(wires[0], wires[1], "{wires:?}");
    // Two short clocks, not the encounter's whole deadline.
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn a_refusal_served_as_200_to_one_client_moves_to_the_next() {
    // Akamai's shape: "Access Denied" with a 200 and a reference link, to the
    // transparent client; the page to a browser.
    let server = Server::start(|request| {
        if request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("donsetch/"))
        {
            Reply::ok(
                "<HTML><HEAD><TITLE>Access Denied</TITLE></HEAD><BODY><H1>Access Denied</H1>\
                 You don't have permission to access \"http://www.example.test/\" on this server.<P>\
                 Reference #18.a5f43717.1794020675.1e1ea2d1<P>\
                 https://errors.edgesuite.net/18.a5f43717.1794020675.1e1ea2d1</BODY></HTML>",
            )
        } else {
            Reply::ok(page("Fixture"))
        }
    });
    let outcome = Session::new(false).fetch(&chosen(server.url("/")));
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert_eq!(outcome.attempts[0].class, Class::WallIdentity);
    assert_ne!(outcome.identity.as_deref(), Some("plain"));
}

#[test]
fn a_page_that_navigates_on_load_is_followed_once_as_the_same_client() {
    // Reddit's slugless thread after its gate: a shell whose only job is
    // `location.replace` to the canonical address (2026-11-07).
    let server = Server::start(|request| match request.path.as_str() {
        "/r/x/comments/abc/" => Reply::ok(
            "<html><head><title>Reddit - The heart of the internet</title>\
             <script src=\"https://www.google.com/recaptcha/api.js\"></script></head><body>\
             <div id=\"shreddit-redirect\"><ac-call nonce=\"n\" delay=\"0\" \
             method=\"location.replace(&quot;/r/x/comments/abc/a_title/&quot;)\" target=\"window\" \
             trigger=\"init\"></ac-call></div></body></html>",
        ),
        "/r/x/comments/abc/a_title/" => Reply::ok(page("A thread")),
        _ => Reply::Full {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        },
    });
    let outcome = Session::new(false).fetch(&request(server.url("/r/x/comments/abc/")));
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert!(
        outcome.final_url.ends_with("/r/x/comments/abc/a_title/"),
        "{}",
        outcome.final_url
    );
    assert_eq!(server.hit_count(), 2, "{:?}", server.hits());
}

#[test]
fn a_short_link_that_lands_on_another_host_follows_that_pages_navigation() {
    // redd.it/<id> lands on www.reddit.com, whose shell navigates on its own
    // host: the navigation belongs to the page that asked for it, not to the
    // host the caller named.
    let server = Server::start(|request| {
        let host = request.header("host").unwrap_or_default().to_string();
        let on_short_host = host.starts_with("127.0.0.1");
        match request.path.as_str() {
            "/abc" if on_short_host => Reply::Full {
                status: 301,
                headers: vec![(
                    "Location".into(),
                    format!("http://{}/r/x/comments/abc/", host.replace("127.0.0.1", "localhost")),
                )],
                body: Vec::new(),
            },
            "/r/x/comments/abc/" if !on_short_host => Reply::ok(
                "<html><head><title>Reddit - The heart of the internet</title></head><body>\
                 <div id=\"shreddit-redirect\"><ac-call nonce=\"n\" delay=\"0\" \
                 method=\"location.replace(&quot;/r/x/comments/abc/a_title/&quot;)\" target=\"window\" \
                 trigger=\"init\"></ac-call></div></body></html>",
            ),
            "/r/x/comments/abc/a_title/" if !on_short_host => Reply::ok(page("A thread")),
            _ => Reply::Full {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            },
        }
    });
    let outcome = Session::new(false).fetch(&request(server.url("/abc")));
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert!(
        outcome.final_url.contains("localhost") && outcome.final_url.ends_with("/a_title/"),
        "{}",
        outcome.final_url
    );
    assert_eq!(server.hit_count(), 3, "{:?}", server.hits());
}

#[test]
fn a_redirect_hop_carries_the_cookie_the_redirect_set() {
    // A gate that validates on the first hop and opens its session there.
    let server = Server::start(|request| match request.path.as_str() {
        "/gate" => Reply::Full {
            status: 302,
            headers: vec![
                ("Location".into(), "/page".into()),
                ("Set-Cookie".into(), "session=open; Path=/".into()),
            ],
            body: Vec::new(),
        },
        _ if request
            .header("cookie")
            .is_some_and(|cookie| cookie.contains("session=open")) =>
        {
            Reply::ok(page("Behind the gate"))
        }
        _ => Reply::Full {
            status: 403,
            headers: Vec::new(),
            body: b"Forbidden".to_vec(),
        },
    });
    let outcome = Session::new(false).fetch(&request(server.url("/gate")));
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
}
