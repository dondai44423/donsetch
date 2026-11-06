//! The envelope against hostile pages and hostile servers, fully offline.
//!
//! Every test runs a page program the engine did not write against a local
//! server and asserts a bound: the mint ends, the process survives, and the
//! thing the page reached for was never reached.

use crate::engine::{mint_within, Mint, MintError};
use crate::envelope::Limits;
use crate::identity::{self, Identity};
use crate::testkit::{Reply, Server};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The engine slot already serialises mints, but a test that is waiting for it
/// would have its stopwatch running. Every test takes this first, so a measured
/// time is the mint's own.
static SERIAL: Mutex<()> = Mutex::new(());

pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn chrome() -> &'static Identity {
    identity::lookup("chrome").expect("chrome identity")
}

fn limits() -> Limits {
    Limits {
        wall: Duration::from_millis(6_000),
        ..Limits::default()
    }
}

fn page(script: &str) -> String {
    format!("<html><head><script>{script}</script></head><body></body></html>")
}

/// One mint and how long it took, measured once this test has its turn.
fn run_timed(page_url: &str, script: &str, limits: &Limits) -> (Result<Mint, MintError>, Duration) {
    let _turn = serial();
    let started = Instant::now();
    let outcome = mint_within(limits, page_url, &page(script), chrome(), None);
    (outcome, started.elapsed())
}

fn run(page_url: &str, script: &str, limits: &Limits) -> Result<Mint, MintError> {
    run_timed(page_url, script, limits).0
}

fn cookie(mint: &Mint, name: &str) -> Option<String> {
    mint.cookies
        .iter()
        .find(|cookie| cookie.name == name)
        .map(|cookie| cookie.value.clone())
}

/// The page's URL under a name pinned to `server`, so every other loopback
/// address (127.0.0.1 included) is someone else's local service. Only 127.0.0.1
/// is configured on every OS (macOS has no 127.0.0.2 without an alias).
fn page_on_a_name(server: &Server) -> String {
    crate::envelope::test_hosts::pin("www.page.test", server.addr.ip());
    format!("http://www.page.test:{}/", server.addr.port())
}

/// A server that answers `ok` to everything.
fn quiet() -> Server {
    Server::start(|_| Reply::ok("ok"))
}

// ---- the program itself --------------------------------------------------------

#[test]
fn a_benign_challenge_mints_inside_the_envelope() {
    let server = Server::start(|request| match request.path.as_str() {
        "/challenge.js" => Reply::ok("document.cookie = 'half=1; Path=/';"),
        "/verify" => Reply::ok("token-from-the-server"),
        _ => Reply::ok("ok"),
    });
    let script = "var x = new XMLHttpRequest(); x.open('POST','/verify'); x.send('proof'); \
                  document.cookie = 'tok=' + x.responseText + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("a benign mint succeeds");
    assert_eq!(
        cookie(&mint, "tok").as_deref(),
        Some("token-from-the-server")
    );
    assert_eq!(mint.cost.ops, 1);
    assert!(
        mint.cost.heap_used < 64 * 1024 * 1024,
        "a challenge is not a heap story"
    );
    assert!(mint.ms < 3_000.0);
}

/// Reddit's gate as the origin serves it: a form whose one field the page's own
/// inline program computes (the seed doubled) and submits. `SEED` stands in for
/// the per-request value the server writes into the script.
const REDDIT_GATE: &str = r#"<!DOCTYPE html><html><head><title>Reddit</title>
<script nonce="n1">
document.addEventListener("DOMContentLoaded",async function(){var e=document.forms[0],n=(e.onsubmit=function(t){return new URLSearchParams(document.location.search).forEach((e,n)=>t.target.appendChild(Object.assign(document.createElement("input"),{name:e,type:"hidden",value:n}))),!0},await(async e=>e+e)("SEED"));e.elements.namedItem("solution").value=n,e.requestSubmit()},{once:!0});
</script></head><body><main><div class="logo">Reddit</div></main>
<form hidden method="GET" action="/r/rust/">
<input type="hidden" name="solution" />
<input type="hidden" name="js_challenge" value="1"/>
<input type="hidden" name="jsc_token" value="tok-abc123"/>
<input type="hidden" name="jsc_orig_r" value=""/>
</form></body></html>"#;

#[test]
fn a_form_challenge_navigates_where_its_own_program_says() {
    let _turn = serial();
    let html = REDDIT_GATE.replace("SEED", "979da15b3033efb7");
    let mint = mint_within(
        &limits(),
        "https://www.reddit.com/r/rust/",
        &html,
        chrome(),
        None,
    )
    .expect("the gate mints");
    assert_eq!(
        mint.redirect.as_deref(),
        Some("https://www.reddit.com/r/rust/?solution=979da15b3033efb7979da15b3033efb7&js_challenge=1&jsc_token=tok-abc123&jsc_orig_r="),
        "the form's own fields in document order, with the value its program set"
    );
    assert!(
        mint.surfaces.iter().any(|surface| surface == "form submit"),
        "the host names the surface it needed: {:?}",
        mint.surfaces
    );
}

#[test]
fn the_page_cannot_see_the_engines_ops() {
    let server = quiet();
    let script = "document.cookie = 'deno=' + typeof Deno + '; Path=/'; \
                  document.cookie = 'has=' + ('Deno' in globalThis) + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "deno").as_deref(), Some("undefined"));
    assert_eq!(cookie(&mint, "has").as_deref(), Some("false"));
}

#[test]
fn webcrypto_is_real_and_refuses_what_it_lacks() {
    // NIST GCM case 3: a real ciphertext and tag, the round trip, an HMAC that
    // verifies, a PBKDF2-derived key that encrypts, and the browser's error names.
    let server = quiet();
    let script = r#"
      const s = crypto.subtle, hex = (b) => Array.from(new Uint8Array(b), (x) => x.toString(16).padStart(2, "0")).join("");
      const un = (h) => new Uint8Array(h.match(/../g).map((x) => parseInt(x, 16)));
      const set = (k, v) => { document.cookie = k + "=" + v + "; Path=/"; };
      (async () => {
        const key = await s.importKey("raw", un("feffe9928665731c6d6a8f9467308308"), "AES-GCM", true, ["encrypt", "decrypt"]);
        const iv = un("cafebabefacedbaddecaf888");
        const plain = un("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255");
        const sealed = await s.encrypt({ name: "AES-GCM", iv }, key, plain);
        set("tag", hex(sealed).slice(-32));
        set("round", hex(await s.decrypt({ name: "AES-GCM", iv }, key, sealed)) === hex(plain));
        set("kind", Object.prototype.toString.call(key) + "/" + key.algorithm.length + "/" + key.type);
        set("jwk", (await s.exportKey("jwk", key)).alg);
        const mac = await s.importKey("raw", new TextEncoder().encode("Jefe"), { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
        const msg = new TextEncoder().encode("what do ya want for nothing?");
        const sig = await s.sign("HMAC", mac, msg);
        set("hmac", hex(sig).slice(0, 16));
        set("verify", (await s.verify("HMAC", mac, sig, msg)) + "," + (await s.verify("HMAC", mac, new Uint8Array(32), msg)));
        const base = await s.importKey("raw", new TextEncoder().encode("password"), "PBKDF2", false, ["deriveKey"]);
        const derived = await s.deriveKey({ name: "PBKDF2", hash: "SHA-1", salt: new TextEncoder().encode("salt"), iterations: 2 }, base,
          { name: "AES-GCM", length: 128 }, false, ["encrypt"]);
        set("derived", (await s.encrypt({ name: "AES-GCM", iv }, derived, plain)).byteLength);
        try { await s.encrypt({ name: "RSA-OAEP" }, key, plain); set("rsa", "resolved"); } catch (e) { set("rsa", e.name); }
        try { await s.decrypt({ name: "AES-GCM", iv }, key, new Uint8Array(sealed).fill(1, 0, 1)); set("auth", "resolved"); } catch (e) { set("auth", e.name); }
        try { new CryptoKey(); set("ctor", "made"); } catch (e) { set("ctor", e.name); }
      })();
    "#;
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    let want = [
        ("tag", "4d5c2af327cd64a62cf35abd2ba6fab4"),
        ("round", "true"),
        ("kind", "[object CryptoKey]/128/secret"),
        ("jwk", "A128GCM"),
        ("hmac", "5bdcc146bf60754e"),
        ("verify", "true,false"),
        ("derived", "80"),
        ("rsa", "NotSupportedError"),
        ("auth", "OperationError"),
        ("ctor", "TypeError"),
    ];
    for (name, value) in want {
        assert_eq!(cookie(&mint, name).as_deref(), Some(value), "{name}");
    }
}

// ---- wall clock ------------------------------------------------------------------

fn assert_stopped_at_the_wall(script: &str, wall_ms: u64) {
    let server = quiet();
    let mut bounded = limits();
    bounded.wall = Duration::from_millis(wall_ms);
    let (outcome, elapsed) = run_timed(&server.url("/"), script, &bounded);
    assert_eq!(
        outcome.err(),
        Some(MintError::WallClock { limit_ms: wall_ms }),
        "`{script}` must end in a wall-clock stop"
    );
    assert!(
        elapsed < Duration::from_millis(wall_ms + 2_500),
        "the stop must be prompt, took {elapsed:?}"
    );
}

#[test]
fn an_endless_loop_is_terminated_at_the_wall_clock() {
    assert_stopped_at_the_wall("for (;;) {}", 700);
}

#[test]
fn a_loop_that_swallows_its_own_exceptions_is_still_terminated() {
    // V8's termination is uncatchable: try/catch/finally cannot absorb it.
    assert_stopped_at_the_wall(
        "for (;;) { try { for (;;) {} } catch (e) {} finally {} }",
        700,
    );
}

#[test]
fn a_microtask_chain_cannot_starve_the_watchdog() {
    assert_stopped_at_the_wall(
        "var f = function () { Promise.resolve().then(f); }; f();",
        700,
    );
}

#[test]
fn catastrophic_regex_backtracking_is_terminated() {
    assert_stopped_at_the_wall("/(a+)+$/.test('a'.repeat(60) + 'b');", 700);
}

#[test]
fn a_thread_parked_forever_is_terminated() {
    assert_stopped_at_the_wall(
        "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);",
        700,
    );
}

#[test]
fn a_timer_storm_cannot_outlive_the_wall_clock() {
    let server = quiet();
    let mut bounded = limits();
    bounded.wall = Duration::from_millis(900);
    // Whether it ends in a stop or runs out its own timers, it ends on time.
    let (_, elapsed) = run_timed(
        &server.url("/"),
        "setInterval(function () { for (var i = 0; i < 1e6; i++); }, 1);",
        &bounded,
    );
    assert!(
        elapsed < Duration::from_millis(900 + 2_500),
        "took {elapsed:?}"
    );
}

#[test]
fn deep_recursion_ends_without_taking_the_process_down() {
    let server = quiet();
    let script = "function f() { return f() + 1; } try { f(); } catch (e) { \
                  document.cookie = 'err=' + e.name + '; Path=/'; }";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "err").as_deref(), Some("RangeError"));
}

// ---- memory ----------------------------------------------------------------------

#[test]
fn heap_exhaustion_terminates_instead_of_aborting_the_process() {
    let server = quiet();
    let mut small = limits();
    small.heap_max = 64 * 1024 * 1024;
    let (outcome, elapsed) = run_timed(
        &server.url("/"),
        "var keep = []; for (;;) keep.push(new Array(100000).fill(0.5));",
        &small,
    );
    assert_eq!(outcome.err(), Some(MintError::Heap { limit_mb: 64 }));
    assert!(
        elapsed < Duration::from_secs(4),
        "heap stop took {elapsed:?}"
    );
}

#[test]
fn a_doubling_string_ends_without_aborting_the_process() {
    let server = quiet();
    let mut small = limits();
    small.heap_max = 64 * 1024 * 1024;
    // V8 may refuse the length itself (a catchable RangeError) or hit the heap
    // ceiling first (a stop); either way the process is alive to assert it.
    let outcome = run(
        &server.url("/"),
        "var s = 'x'; for (;;) { s += s; }",
        &small,
    );
    assert!(
        matches!(outcome, Err(MintError::Heap { .. }) | Ok(_)),
        "unexpected: {outcome:?}"
    );
}

#[test]
fn a_gigabyte_typed_array_is_refused() {
    let server = quiet();
    let mut small = limits();
    small.buffer_max = 16 * 1024 * 1024;
    // V8 reports the refused allocation as memory pressure and the program is
    // terminated: a page that wants a gigabyte is not a challenge.
    let outcome = run(
        &server.url("/"),
        "new Uint8Array(1 << 30); document.cookie = 'big=allocated; Path=/';",
        &small,
    );
    assert_eq!(outcome.err(), Some(MintError::Buffers { limit_mb: 16 }));
}

#[test]
fn many_small_buffers_cannot_add_up_past_the_cap() {
    let server = quiet();
    let mut small = limits();
    small.buffer_max = 16 * 1024 * 1024;
    let outcome = run(
        &server.url("/"),
        "var keep = []; for (var i = 0; i < 200; i++) keep.push(new Uint8Array(1 << 20));",
        &small,
    );
    assert_eq!(outcome.err(), Some(MintError::Buffers { limit_mb: 16 }));
}

#[test]
fn memory_is_released_between_mints() {
    let server = quiet();
    let mut small = limits();
    small.heap_max = 64 * 1024 * 1024;
    let _ = run(
        &server.url("/"),
        "var keep = []; for (;;) keep.push(new Array(100000).fill(1));",
        &small,
    );
    // The process is healthy afterwards: the next mint is an ordinary mint.
    let mint = run(
        &server.url("/"),
        "document.cookie = 'after=1; Path=/';",
        &limits(),
    )
    .expect("a mint after a heap stop");
    assert_eq!(cookie(&mint, "after").as_deref(), Some("1"));
}

// ---- the network -----------------------------------------------------------------

#[test]
fn file_urls_are_never_read() {
    let server = quiet();
    let script = "var r = 'none'; try { var a = fetch('file:///etc/passwd'); r = 'fetched'; } \
                  catch (e) { r = 'refused'; } \
                  var x = new XMLHttpRequest(); x.open('GET', 'file:///etc/hostname'); \
                  x.onload = function () { r += '+xhr-read'; }; x.onerror = function () { r += '+xhr-refused'; }; \
                  x.send(); \
                  document.cookie = 'r=' + r + '; Path=/'; \
                  document.cookie = 'len=' + x.responseText.length + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "r").as_deref(), Some("refused+xhr-refused"));
    assert_eq!(cookie(&mint, "len").as_deref(), Some("0"));
    assert_eq!(server.hit_count(), 0);
}

#[test]
fn other_schemes_are_never_reached() {
    let server = quiet();
    let script = "var out = []; ['ftp://127.0.0.1/x', 'gopher://127.0.0.1/x', 'ws://127.0.0.1/x', \
                  'wss://127.0.0.1/x', 'file://localhost/etc/passwd'].forEach(function (u) { \
                  try { fetch(u); out.push('reached'); } catch (e) { out.push('refused'); } }); \
                  document.cookie = 'out=' + out.join('.') + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(
        cookie(&mint, "out").as_deref(),
        Some("refused.refused.refused.refused.refused")
    );
    assert_eq!(server.hit_count(), 0);
}

#[test]
fn a_redirect_chain_is_cut_at_the_limit() {
    let server = Server::start(|request| {
        let step: u32 = request.path.trim_start_matches("/r/").parse().unwrap_or(0);
        Reply::Redirect(format!("/r/{}", step + 1))
    });
    let script = "var r = 'followed'; try { fetch('/r/0'); } catch (e) { r = 'cut'; } \
                  document.cookie = 'r=' + r + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "r").as_deref(), Some("cut"));
    let hits = server.hit_count();
    assert!(
        hits <= limits().redirects_max + 2,
        "a 100-hop chain cost {hits} requests"
    );
}

#[test]
fn a_redirect_into_a_private_address_is_never_followed() {
    // The page lives on a name; the target is a loopback literal: private, and
    // not the page's host.
    let secret = Server::start(|_| Reply::ok("secret"));
    let hop = secret.url("/secret");
    let server = Server::start(move |_| Reply::Redirect(hop.clone()));
    let script = "var r = 'followed'; try { var res = fetch('/hop'); r = res.status; } \
                  catch (e) { r = 'blocked'; } document.cookie = 'r=' + r + '; Path=/';";
    let mint = run(&page_on_a_name(&server), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "r").as_deref(), Some("blocked"));
    assert_eq!(
        secret.hit_count(),
        0,
        "the private address was connected to"
    );
}

#[test]
fn a_private_address_is_never_asked_directly() {
    let secret = Server::start(|_| Reply::ok("secret"));
    let server = quiet();
    let script = format!(
        "var out = []; ['{0}', 'http://localhost:{1}/', 'http://169.254.169.254/latest/meta-data/', \
         'http://[::1]:{1}/', 'http://2130706433:{1}/', 'http://0x7f.0.0.2:{1}/'].forEach(function (u) {{ \
         try {{ fetch(u); out.push('reached'); }} catch (e) {{ out.push('blocked'); }} }}); \
         document.cookie = 'out=' + out.join('.') + '; Path=/';",
        secret.url("/secret"),
        secret.addr.port()
    );
    let mint = run(&page_on_a_name(&server), &script, &limits()).expect("mint");
    assert_eq!(
        cookie(&mint, "out").as_deref(),
        Some("blocked.blocked.blocked.blocked.blocked.blocked")
    );
    assert_eq!(
        secret.hit_count(),
        0,
        "the private address was connected to"
    );
}

#[test]
fn a_name_that_resolves_to_a_private_address_is_refused_at_dns() {
    // `localhost` is a name, not a literal: only the resolver can catch it.
    let secret = Server::start(|_| Reply::ok("secret"));
    // The page lives on a name, so 127.0.0.1 is someone else's local service.
    let page_server = Server::start(|_| Reply::ok("ok"));
    let script = format!(
        "var r = 'reached'; try {{ fetch('http://localhost:{}/'); }} catch (e) {{ r = 'blocked'; }} \
         document.cookie = 'r=' + r + '; Path=/';",
        secret.addr.port()
    );
    let mint = run(&page_on_a_name(&page_server), &script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "r").as_deref(), Some("blocked"));
    assert_eq!(secret.hit_count(), 0);
}

#[test]
fn unsafe_ports_are_never_connected_to() {
    let server = quiet();
    let script = "var out = []; [22, 25, 6667].forEach(function (p) { \
                  try { fetch('http://example.com:' + p + '/'); out.push('reached'); } \
                  catch (e) { out.push('blocked'); } }); \
                  document.cookie = 'out=' + out.join('.') + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(
        cookie(&mint, "out").as_deref(),
        Some("blocked.blocked.blocked")
    );
}

#[test]
fn an_oversized_response_is_refused_not_buffered() {
    let server = Server::start(|request| match request.path.as_str() {
        "/declared" => Reply::ok(vec![b'x'; 4 * 1024 * 1024]),
        _ => Reply::Endless { chunk: 64 * 1024 },
    });
    let mut small = limits();
    small.response_body_max = 512 * 1024;
    let script = "var out = []; ['/declared', '/endless'].forEach(function (p) { \
                  try { fetch(p).then(function (r) { out.push('read' + r.textSync.length); }); } \
                  catch (e) { out.push('refused'); } }); \
                  document.cookie = 'out=' + out.join('.') + '; Path=/';";
    let (mint, elapsed) = run_timed(&server.url("/"), script, &small);
    let mint = mint.expect("mint");
    assert_eq!(cookie(&mint, "out").as_deref(), Some("refused.refused"));
    assert_eq!(mint.cost.bytes_read, 0, "no refused byte counts as read");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

#[test]
fn total_bytes_are_budgeted_across_the_mint() {
    let server = Server::start(|_| Reply::ok(vec![b'x'; 300 * 1024]));
    let mut small = limits();
    small.response_body_max = 512 * 1024;
    small.total_response_max = 1024 * 1024;
    let script = "var read = 0; for (var i = 0; i < 10; i++) { \
                  try { fetch('/b' + i).then(function (r) { read += r.textSync.length; }); } catch (e) {} } \
                  document.cookie = 'read=' + read + '; Path=/';";
    let mint = run(&server.url("/"), script, &small).expect("mint");
    let read: usize = cookie(&mint, "read").unwrap().parse().unwrap();
    assert!(
        read <= small.total_response_max,
        "read {read} against a 1 MiB total"
    );
    assert!(read >= 3 * 300 * 1024, "the budget is spent, not skipped");
}

#[test]
fn a_trickling_server_cannot_outlast_the_wall_clock() {
    let server = Server::start(|_| Reply::Trickle {
        every: Duration::from_millis(100),
    });
    let mut bounded = limits();
    bounded.wall = Duration::from_millis(1_500);
    let (outcome, elapsed) = run_timed(
        &server.url("/"),
        "try { fetch('/slow'); } catch (e) {}",
        &bounded,
    );
    assert!(
        elapsed < Duration::from_millis(1_500 + 2_500),
        "a 1 byte/100 ms server held the engine for {elapsed:?}"
    );
    // The op's timeout is what is left of the wall clock, so the mint either
    // returns with the request failed or is stopped: never a hang.
    assert!(matches!(outcome, Ok(_) | Err(MintError::WallClock { .. })));
}

#[test]
fn a_silent_server_cannot_outlast_the_wall_clock() {
    let server = Server::start(|_| Reply::Silence);
    let mut bounded = limits();
    bounded.wall = Duration::from_millis(1_200);
    let (_, elapsed) = run_timed(
        &server.url("/"),
        "try { fetch('/never'); } catch (e) {}",
        &bounded,
    );
    assert!(
        elapsed < Duration::from_millis(1_200 + 2_500),
        "took {elapsed:?}"
    );
}

#[test]
fn a_request_flood_stops_at_the_op_budget() {
    let server = quiet();
    let mut small = limits();
    small.ops_max = 5;
    let script = "var ok = 0, refused = 0; for (var i = 0; i < 500; i++) { \
                  try { fetch('/f' + i); ok++; } catch (e) { refused++; } } \
                  document.cookie = 'ok=' + ok + '; Path=/'; \
                  document.cookie = 'refused=' + refused + '; Path=/';";
    let mint = run(&server.url("/"), script, &small).expect("mint");
    assert_eq!(cookie(&mint, "ok").as_deref(), Some("5"));
    assert_eq!(cookie(&mint, "refused").as_deref(), Some("495"));
    assert_eq!(server.hit_count(), 5);
}

#[test]
fn an_oversized_request_body_is_never_sent() {
    let server = quiet();
    let mut small = limits();
    small.request_body_max = 1024;
    let script =
        "var r = 'sent'; try { fetch('/up', { method: 'POST', body: 'x'.repeat(4096) }); } \
                  catch (e) { r = 'refused'; } document.cookie = 'r=' + r + '; Path=/';";
    let mint = run(&server.url("/"), script, &small).expect("mint");
    assert_eq!(cookie(&mint, "r").as_deref(), Some("refused"));
    assert_eq!(server.hit_count(), 0);
}

#[test]
fn a_page_cannot_set_the_connections_own_headers() {
    let server = Server::start(|request| {
        Reply::ok(format!(
            "host={};te={}",
            request.header("host").unwrap_or(""),
            request.header("te").unwrap_or("")
        ))
    });
    let script =
        "fetch('/h', { headers: { Host: 'evil.test', TE: 'trailers' } }).then(function (r) { \
                  document.cookie = 'seen=' + r.textSync.replace(/[;=]/g, '_') + '; Path=/'; });";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    let seen = cookie(&mint, "seen").unwrap();
    assert!(
        !seen.contains("evil.test"),
        "the Host header was overridden: {seen}"
    );
    assert!(!seen.contains("trailers"), "TE was passed through: {seen}");
}

// ---- one engine at a time --------------------------------------------------------

#[test]
fn two_mints_in_one_process_never_overlap() {
    let wall = 600u64;
    let _turn = serial();
    let started = Instant::now();
    let workers: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(move || {
                let server = quiet();
                let mut bounded = limits();
                bounded.wall = Duration::from_millis(wall);
                mint_within(
                    &bounded,
                    &server.url("/"),
                    &page("for (;;) {}"),
                    chrome(),
                    None,
                )
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(
            worker.join().unwrap().err(),
            Some(MintError::WallClock { limit_ms: wall })
        );
    }
    // Two 600 ms stops in series take at least 1.2 s; overlapped they would take 0.6 s.
    assert!(
        started.elapsed() >= Duration::from_millis(2 * wall),
        "the engines overlapped: {:?}",
        started.elapsed()
    );
}

/// A module whose own memory section asks for 2048 pages (128 MB), exporting
/// nothing: its memory is invisible to anything that inspects exports.
const WASM_128MB: &str = "new Uint8Array([0,97,115,109,1,0,0,0, 5,4,1,0,128,16])";

#[test]
fn wasm_memory_is_budgeted_however_it_is_asked_for() {
    let server = quiet();
    let mut small = limits();
    small.buffer_max = 32 * 1024 * 1024;
    let script = format!(
        "var out = []; \
         try {{ new WebAssembly.Memory({{ initial: 16384 }}); out.push('big-ok'); }} catch (e) {{ out.push('big-' + e.name); }} \
         try {{ var m = new WebAssembly.Memory({{ initial: 256 }}); out.push('16mb-ok'); m.grow(1000); out.push('grow-ok'); }} catch (e) {{ out.push('grow-' + e.name); }} \
         try {{ new WebAssembly.Instance(new WebAssembly.Module({WASM_128MB})); out.push('inst-ok'); }} catch (e) {{ out.push('inst-' + e.name); }} \
         document.cookie = 'out=' + out.join(',') + '; Path=/'; \
         WebAssembly.instantiate(new WebAssembly.Module({WASM_128MB})).then(function () {{ document.cookie = 'viamodule=ok; Path=/'; }}, \
           function (e) {{ document.cookie = 'viamodule=' + e.name + '; Path=/'; }}); \
         WebAssembly.instantiate({WASM_128MB}).then(function () {{ document.cookie = 'viabytes=ok; Path=/'; }}, function () {{}}); \
         WebAssembly.compile({WASM_128MB}).then(function () {{ document.cookie = 'compiled=ok; Path=/'; }}, function () {{}});"
    );
    let mint = run(&server.url("/"), &script, &small).expect("mint");
    let out = cookie(&mint, "out").unwrap();
    // 16 MB fits the 32 MB budget; everything else asks for more than is left.
    assert_eq!(
        out, "big-RangeError,16mb-ok,grow-RangeError,inst-RangeError",
        "got {out}"
    );
    // Instantiating a module object is charged before V8 is asked, and rejects. Bytes
    // handed to the async entry points are compiled by V8 on its own threads and
    // never settle here (the host drains microtasks only); they must not deliver.
    assert_eq!(cookie(&mint, "viamodule").as_deref(), Some("RangeError"));
    assert_eq!(cookie(&mint, "viabytes"), None);
    assert_eq!(cookie(&mint, "compiled"), None);
}

#[test]
fn a_benign_module_and_the_browser_surface_still_work() {
    let server = quiet();
    // The minimal module: no memory at all. It must compile, instantiate and keep
    // instanceof/constructor identities a challenge may check.
    let script = "var bytes = new Uint8Array([0,97,115,109,1,0,0,0]); \
                  var mod = new WebAssembly.Module(bytes); var inst = new WebAssembly.Instance(mod); \
                  var mem = new WebAssembly.Memory({ initial: 1 }); mem.grow(1); \
                  var r = [mod instanceof WebAssembly.Module, inst instanceof WebAssembly.Instance, \
                           mem instanceof WebAssembly.Memory, mem.buffer.byteLength, \
                           mem.constructor === WebAssembly.Memory, \
                           WebAssembly.validate(bytes), typeof WebAssembly.compile, typeof WebAssembly.instantiate]; \
                  document.cookie = 'r=' + r.join(',') + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(
        cookie(&mint, "r").as_deref(),
        Some("true,true,true,131072,true,true,function,function")
    );
}

#[test]
fn a_wasm_memory_loop_is_stopped_before_it_can_exhaust_the_machine() {
    let server = quiet();
    let mut small = limits();
    small.buffer_max = 64 * 1024 * 1024;
    let (outcome, elapsed) = run_timed(
        &server.url("/"),
        "var keep = []; for (;;) { var m = new WebAssembly.Memory({ initial: 512 }); \
         new Uint8Array(m.buffer).fill(1); keep.push(m); }",
        &small,
    );
    // The host budget throws a RangeError the page does not catch: the program
    // ends with an exception, which the host reports as a finished mint with
    // nothing to show, long before the wall clock.
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    match outcome {
        Ok(mint) => assert!(mint.cookies.is_empty()),
        Err(error) => assert!(
            matches!(error, MintError::Buffers { .. } | MintError::Heap { .. }),
            "{error:?}"
        ),
    }
}

// ---- the browser the page sees ---------------------------------------------------

#[test]
fn a_domain_cookie_for_the_page_host_is_handed_back() {
    // `Domain=<host>` is keyed `.<host>`; the hand-back must still match the host
    // (measured: AWS WAF sets its token this way when no domain list is given).
    let server = quiet();
    let script = "document.cookie = 'dom=1; Domain=' + location.hostname + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "dom").as_deref(), Some("1"));
}

#[test]
fn a_cookie_for_a_foreign_or_suffix_domain_is_dropped() {
    // RFC 6265 5.3: a Domain that does not cover the host, or is a public
    // suffix, is ignored; SDKs probe exactly this to find the registrable domain.
    // The page makes no request, so a name that resolves nowhere is fine.
    let script = "for (const d of ['www.shop.co.uk', 'shop.co.uk', 'co.uk', 'uk', 'other.co.uk']) \
                    document.cookie = 'c_' + d.replace(/\\./g, '_') + '=1; Domain=' + d + '; Path=/'; \
                  document.cookie = 'jar=' + document.cookie.split('; ').map((p) => p.split('=')[0]).sort().join('+') + '; Path=/';";
    let _turn = serial();
    let mint = mint_within(
        &limits(),
        "https://www.shop.co.uk/",
        &page(script),
        chrome(),
        None,
    )
    .expect("mint");
    assert_eq!(
        cookie(&mint, "jar").as_deref(),
        Some("c_shop_co_uk+c_www_shop_co_uk")
    );
    assert!(cookie(&mint, "c_co_uk").is_none() && cookie(&mint, "c_other_co_uk").is_none());
    assert_eq!(cookie(&mint, "c_www_shop_co_uk").as_deref(), Some("1"));
}

#[test]
fn the_host_invents_no_page_configuration() {
    // A vendor global the page did not set changes what the vendor reports
    // (measured: an invented cookie-domain list made AWS WAF refuse the token).
    let server = quiet();
    let script = "document.cookie = 'list=' + typeof window.awsWafCookieDomainList + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "list").as_deref(), Some("undefined"));
}

#[test]
fn stylesheets_are_requested_before_the_scripts_after_them() {
    let server = Server::start(|request| match request.path.as_str() {
        "/app.js" => Reply::ok("document.cookie = 'ran=1; Path=/';"),
        _ => Reply::ok("ok"),
    });
    let html = "<html><head><link rel=\"stylesheet\" href=\"/site.css\" /><link rel=\"icon\" href=\"/i.ico\">\
                <script src=\"/app.js\"></script></head><body></body></html>";
    let _turn = serial();
    let mint = mint_within(&limits(), &server.url("/"), html, chrome(), None).expect("mint");
    assert_eq!(cookie(&mint, "ran").as_deref(), Some("1"));
    let hits = server.hits();
    let css = hits
        .iter()
        .position(|path| path == "/site.css")
        .expect("stylesheet requested");
    let js = hits
        .iter()
        .position(|path| path == "/app.js")
        .expect("script requested");
    assert!(css < js, "{hits:?}");
    assert!(
        !hits.iter().any(|path| path == "/i.ico"),
        "only stylesheets block: {hits:?}"
    );
}

#[test]
fn random_values_fill_every_byte_and_uuids_are_version_4() {
    let server = quiet();
    let script = r#"
      const set = (k, v) => { document.cookie = k + "=" + v + "; Path=/"; };
      const wide = crypto.getRandomValues(new Uint32Array(64));
      set("wide", wide.some((x) => x > 255));
      const same = crypto.getRandomValues(new Uint8Array(16));
      set("same", same instanceof Uint8Array);
      try { crypto.getRandomValues(new Float32Array(4)); set("float", "filled"); } catch (e) { set("float", e.name); }
      try { crypto.getRandomValues(new Uint8Array(65537)); set("quota", "filled"); } catch (e) { set("quota", e.name); }
      set("uuid", /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()));
    "#;
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    for (name, value) in [
        ("wide", "true"),
        ("same", "true"),
        ("float", "TypeMismatchError"),
        ("quota", "QuotaExceededError"),
        ("uuid", "true"),
    ] {
        assert_eq!(cookie(&mint, name).as_deref(), Some(value), "{name}");
    }
}

#[test]
fn url_search_params_are_live_both_ways() {
    // Measured: the AWS WAF SDK adds `region` through `url.searchParams.set`
    // and posts to `url.href`; a detached searchParams drops it.
    let server = quiet();
    let script = r#"
      const set = (k, v) => { document.cookie = k + "=" + encodeURIComponent(v) + "; Path=/"; };
      const u = new URL("https://h.example/mp_verify?a=1#f");
      u.searchParams.set("region", "ap south");
      set("href", u.href);
      u.search = "?b=2";
      set("after", u.searchParams.get("b") + "," + u.searchParams.has("region"));
      set("rel", new URL("../x?y#z", "https://h.example/a/b/c").href);
      set("bad", URL.canParse("http://[::1") + "," + (() => { try { new URL("nope"); return "made"; } catch (e) { return e.name; } })());
      set("form", new URLSearchParams({ q: "a b&c", e: "é" }).toString());
    "#;
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    let decoded = |name: &str| {
        cookie(&mint, name).map(|value| {
            url::form_urlencoded::parse(format!("v={value}").as_bytes())
                .next()
                .unwrap()
                .1
                .into_owned()
        })
    };
    assert_eq!(
        decoded("href").as_deref(),
        Some("https://h.example/mp_verify?a=1&region=ap+south#f")
    );
    assert_eq!(decoded("after").as_deref(), Some("2,false"));
    assert_eq!(decoded("rel").as_deref(), Some("https://h.example/a/x?y#z"));
    assert_eq!(decoded("bad").as_deref(), Some("false,TypeError"));
    assert_eq!(decoded("form").as_deref(), Some("q=a+b%26c&e=%C3%A9"));
}

#[test]
fn the_clock_has_chromes_resolution() {
    let server = quiet();
    let script = "let fine = true; for (let i = 0; i < 200; i++) { const t = performance.now() * 10; if (Math.abs(t - Math.round(t)) > 1e-6) fine = false; } \
                  document.cookie = 'fine=' + fine + '; Path=/'; \
                  document.cookie = 'origin=' + (Math.abs(performance.timeOrigin + performance.now() - Date.now()) < 50) + '; Path=/';";
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    assert_eq!(cookie(&mint, "fine").as_deref(), Some("true"));
    assert_eq!(cookie(&mint, "origin").as_deref(), Some("true"));
}

#[test]
fn media_and_plugins_answer_as_desktop_chrome() {
    let server = quiet();
    let script = r#"
      const set = (k, v) => { document.cookie = k + "=" + encodeURIComponent(v) + "; Path=/"; };
      const v = document.createElement("video");
      set("mp4", v.canPlayType('video/mp4; codecs="avc1.42E01E"'));
      set("bogus", v.canPlayType("video/x-bogus") === "");
      set("mse", MediaSource.isTypeSupported('video/webm; codecs="vp9"'));
      set("plugins", navigator.plugins.length + "," + navigator.mimeTypes.length + "," + navigator.pdfViewerEnabled);
      set("plugin", navigator.plugins[0].name + "|" + navigator.plugins[0][0].type);
    "#;
    let mint = run(&server.url("/"), script, &limits()).expect("mint");
    let get = |name: &str| {
        cookie(&mint, name).map(|value| {
            value
                .replace("%20", " ")
                .replace("%7C", "|")
                .replace("%2C", ",")
                .replace("%2F", "/")
        })
    };
    assert_eq!(get("mp4").as_deref(), Some("probably"));
    assert_eq!(get("bogus").as_deref(), Some("true"));
    assert_eq!(get("mse").as_deref(), Some("true"));
    assert_eq!(get("plugins").as_deref(), Some("5,2,true"));
    assert_eq!(get("plugin").as_deref(), Some("PDF Viewer|application/pdf"));
}
