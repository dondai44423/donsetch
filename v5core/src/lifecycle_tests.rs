//! The credential lifecycle, end to end, against a fake origin that behaves like an
//! AWS WAF front: a challenge page, a vendor `verify` endpoint, and a token with
//! a server-side lifetime the client cannot see.
//!
//! The origin keeps its own clock so a test can age a credential without sleeping:
//! the test moves the origin's clock and the fetcher's recorded mint time by the
//! same amount, which is exactly what elapsed time does.

use super::*;
use crate::state::{BACKOFF_BASE_SECS, BACKOFF_MAX_SECS, MINT_CAP, MINT_WINDOW_SECS};
use crate::testkit::{Reply, Request, Server};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MODE_NORMAL: u8 = 0;
/// The origin accepts no token at all (a mint it will never honour).
const MODE_REFUSE_ALL: u8 = 1;
/// A token it no longer accepts is answered with a bare 403, no challenge page.
const MODE_BARE_403: u8 = 2;

struct Origin {
    server: Server,
    verifies: Arc<AtomicU32>,
    clock: Arc<AtomicI64>,
    mode: Arc<AtomicU8>,
    /// Every `aws-waf-token` value the origin saw on a request to `/`.
    tokens: Arc<Mutex<Vec<String>>>,
}

// The origin is plain HTTP on loopback, so its token is not `Secure`: a Secure
// cookie is never sent over plain HTTP (`State::cookie_header_for`).
fn challenge_page() -> String {
    "<html><head><title>Human Verification</title><script>\
     var x = new XMLHttpRequest(); x.open('POST', '/verify'); x.send('proof'); \
     document.cookie = 'aws-waf-token=' + x.responseText + '; Max-Age=345600; Path=/';\
     </script></head><body>aws-waf-token challenge</body></html>"
        .to_string()
}

fn content_page() -> String {
    format!(
        "<html><head><title>The Real Page</title></head><body><p>{}</p></body></html>",
        "Real content that a reader came for. ".repeat(20)
    )
}

fn token_of(request: &Request) -> Option<String> {
    request
        .header("cookie")?
        .split(';')
        .filter_map(|pair| pair.trim().strip_prefix("aws-waf-token="))
        .next()
        .map(str::to_string)
}

impl Origin {
    fn start(ttl: i64) -> Self {
        let verifies = Arc::new(AtomicU32::new(0));
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let ttl = Arc::new(AtomicI64::new(ttl));
        let mode = Arc::new(AtomicU8::new(MODE_NORMAL));
        let tokens = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let verifies = Arc::clone(&verifies);
            let clock = Arc::clone(&clock);
            let ttl = Arc::clone(&ttl);
            let mode = Arc::clone(&mode);
            let tokens = Arc::clone(&tokens);
            Server::start(move |request| {
                if request.path == "/verify" {
                    verifies.fetch_add(1, Ordering::SeqCst);
                    return Reply::ok(format!("t{}", clock.load(Ordering::SeqCst)));
                }
                let token = token_of(request);
                if let Some(token) = &token {
                    tokens.lock().unwrap().push(token.clone());
                }
                let now = clock.load(Ordering::SeqCst);
                let valid = token
                    .as_deref()
                    .and_then(|token| token.strip_prefix('t'))
                    .and_then(|issued| issued.parse::<i64>().ok())
                    .is_some_and(|issued| now - issued <= ttl.load(Ordering::SeqCst));
                let mode = mode.load(Ordering::SeqCst);
                if valid && mode != MODE_REFUSE_ALL {
                    return Reply::ok(content_page());
                }
                if token.is_some() && mode == MODE_BARE_403 {
                    return Reply::Full {
                        status: 403,
                        headers: Vec::new(),
                        body: Vec::new(),
                    };
                }
                Reply::Full {
                    status: 202,
                    headers: vec![
                        ("x-amzn-waf-action".to_string(), "challenge".to_string()),
                        ("content-type".to_string(), "text/html".to_string()),
                    ],
                    body: challenge_page().into_bytes(),
                }
            })
        };
        Self {
            server,
            verifies,
            clock,
            mode,
            tokens,
        }
    }

    fn verifies(&self) -> u32 {
        self.verifies.load(Ordering::SeqCst)
    }

    fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn requests_to_root(&self) -> usize {
        self.server
            .hits()
            .iter()
            .filter(|path| path.as_str() == "/")
            .count()
    }

    /// Let `seconds` pass: for the origin, and for the fetcher's own records.
    fn elapse(&self, session: &mut Worker, seconds: i64) {
        self.clock.fetch_add(seconds, Ordering::SeqCst);
        for per_identity in session.state.credentials.values_mut() {
            for credential in per_identity.values_mut() {
                credential.minted_unix -= seconds;
            }
        }
        for ledger in session.state.mints.values_mut() {
            for at in ledger.recent.iter_mut() {
                *at -= seconds;
            }
            if ledger.blocked_until != 0 {
                ledger.blocked_until -= seconds;
            }
        }
    }
}

fn options() -> Options {
    Options {
        identity: Some("chrome".to_string()),
        pace_ms: 0,
        persist: false,
        deadline: Duration::from_secs(20),
        ..Options::default()
    }
}

fn fetch(session: &mut Worker, origin: &Origin) -> Outcome {
    session.fetch(&origin.server.url("/"), &options())
}

fn engine_notes(outcome: &Outcome) -> usize {
    outcome
        .attempts
        .iter()
        .filter(|attempt| {
            attempt
                .note
                .as_deref()
                .is_some_and(|note| note.starts_with("engine:"))
        })
        .count()
}

fn host() -> &'static str {
    "127.0.0.1"
}

#[test]
fn a_first_contact_mints_and_is_content() {
    let origin = Origin::start(300);
    let mut session = Worker::new(false);
    let outcome = fetch(&mut session, &origin);
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert_eq!(origin.verifies(), 1);
    let credential = &session.state.credentials[host()]["chrome"];
    assert_eq!(credential.vendor, "aws-waf");
    assert_eq!(credential.names, vec!["aws-waf-token".to_string()]);
    assert_eq!(
        credential.declared_ttl,
        Some(345_600),
        "the vendor's declared lifetime is kept"
    );
    assert_eq!(credential.uses, 1, "the origin accepted it once");
}

#[test]
fn two_fetches_inside_the_lifetime_cost_one_mint() {
    let origin = Origin::start(300);
    let mut session = Worker::new(false);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    origin.elapse(&mut session, 120);
    let second = fetch(&mut session, &origin);
    assert_eq!(second.class, Class::Content);
    assert_eq!(
        origin.verifies(),
        1,
        "the second fetch must spend, not mint"
    );
    assert_eq!(
        second.attempts.len(),
        1,
        "one request, no engine: {:?}",
        second.attempts
    );
    assert_eq!(engine_notes(&second), 0);
    assert!(second.spent_state);
    let ledger = &session.state.mints[host()];
    assert!(
        ledger.accepted_age >= 120,
        "the window is at least {} s",
        ledger.accepted_age
    );
    assert_eq!(ledger.refused_age, None);
}

#[test]
fn an_expired_credential_costs_exactly_one_re_mint() {
    let origin = Origin::start(300);
    let mut session = Worker::new(false);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    origin.elapse(&mut session, 120);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    assert_eq!(origin.verifies(), 1);

    // Past the origin's immunity time. The fetcher cannot know: the cookie says 4 days.
    origin.elapse(&mut session, 400);
    let third = fetch(&mut session, &origin);
    assert_eq!(third.class, Class::Content, "{:?}", third.attempts);
    assert_eq!(
        origin.verifies(),
        2,
        "exactly one re-mint, not zero and not two"
    );
    assert_eq!(engine_notes(&third), 1);
    // The refused spend, the engine's own request: the refusal costs one request.
    assert_eq!(third.attempts.len(), 2, "{:?}", third.attempts);

    // What the refusal taught: the window ended by this age, and it had been
    // accepted at least as old as the second fetch.
    let ledger = &session.state.mints[host()];
    // 120 + 400 s after the mint; the clock is whole seconds and still runs
    // while the test does, so a second boundary may fall in between.
    assert!(
        matches!(ledger.refused_age, Some(520..=521)),
        "{:?}",
        ledger.refused_age
    );
    assert!(ledger.accepted_age >= 120);
}

#[test]
fn a_learned_lifetime_renews_before_the_refusal() {
    let origin = Origin::start(300);
    let mut session = Worker::new(false);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    origin.elapse(&mut session, 100);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    origin.elapse(&mut session, 250); // 350 s old: refused, and learned
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    assert_eq!(origin.verifies(), 2);
    let learned = session.state.mints[host()]
        .refused_age
        .expect("a lifetime was learned");
    // Whole seconds, and the clock still runs while the test does: a second
    // boundary may fall inside the elapse. The invariant is the refusal's
    // own age, not a stopwatch reading.
    assert!(
        matches!(learned, 349..=351),
        "the refusal must teach the age it happened at (~350 s): {learned}"
    );

    // The new credential reaches 0.9 x the learned age while the origin would
    // still accept it (300 s): the fetcher renews instead of being refused.
    origin.elapse(&mut session, 320);
    let stale_token = {
        let tokens = origin.tokens.lock().unwrap();
        tokens.last().cloned().expect("a token was seen")
    };
    let seen_before = origin.tokens.lock().unwrap().len();
    let renewed = fetch(&mut session, &origin);
    assert_eq!(renewed.class, Class::Content, "{:?}", renewed.attempts);
    assert_eq!(origin.verifies(), 3);
    let tokens = origin.tokens.lock().unwrap();
    assert!(
        tokens[seen_before..]
            .iter()
            .all(|token| token != &stale_token),
        "the aging token must not be presented again: {:?}",
        &tokens[seen_before..]
    );
}

#[test]
fn a_refusal_of_a_credential_that_never_worked_teaches_no_lifetime() {
    // A mint the origin never honours says nothing about how long tokens live:
    // learning "refused at age 0" would renew every fetch forever.
    let mut state = State::default();
    state.mint_recorded("a.test", "chrome", "aws-waf", &[], 1_000);
    state.credential_verdict("a.test", "chrome", false, 1_002);
    assert_eq!(state.mints["a.test"].refused_age, None);
    assert!(
        !state.has_credential("a.test", "chrome"),
        "a refused credential is dropped"
    );
}

#[test]
fn a_mint_the_origin_refuses_backs_off_instead_of_repeating() {
    let origin = Origin::start(300);
    origin.set_mode(MODE_REFUSE_ALL);
    let mut session = Worker::new(false);

    let first = fetch(&mut session, &origin);
    assert_eq!(first.class, Class::WallProgram, "a wall is reported as one");
    assert_eq!(origin.verifies(), 1);
    assert!(!session.state.has_credential(host(), "chrome"));
    assert_eq!(session.state.mints[host()].failures, 1);
    assert!(
        session.state.cookie_header(host(), "chrome").is_none(),
        "no refused token is kept"
    );

    // The next fetch does not pay for another mint.
    let second = fetch(&mut session, &origin);
    assert_eq!(second.class, Class::WallProgram);
    assert_eq!(origin.verifies(), 1, "no second mint while backing off");
    assert!(
        second.attempts.iter().any(|attempt| attempt
            .note
            .as_deref()
            .is_some_and(|note| note.contains("backing off"))),
        "the outcome says why the engine did not run: {:?}",
        second.attempts
    );
    assert!(
        second.ms_total < first.ms_total,
        "backing off is cheaper than minting"
    );

    // Once the backoff is served the engine runs again, and waits twice as long.
    origin.elapse(&mut session, BACKOFF_BASE_SECS + 1);
    let third = fetch(&mut session, &origin);
    assert_eq!(third.class, Class::WallProgram);
    assert_eq!(origin.verifies(), 2);
    let ledger = &session.state.mints[host()];
    assert_eq!(ledger.failures, 2);
    let wait = ledger.blocked_until - crate::state::now_unix();
    assert!(
        wait > BACKOFF_BASE_SECS && wait <= 2 * BACKOFF_BASE_SECS,
        "the second wait doubles: {wait} s"
    );

    // And a mint the origin finally honours ends the streak.
    origin.set_mode(MODE_NORMAL);
    origin.elapse(&mut session, 2 * BACKOFF_BASE_SECS + 1);
    let fourth = fetch(&mut session, &origin);
    assert_eq!(fourth.class, Class::Content);
    let ledger = &session.state.mints[host()];
    assert_eq!((ledger.failures, ledger.blocked_until), (0, 0));
}

#[test]
fn the_mint_cap_bounds_what_one_host_can_cost() {
    // A token that lives 1 s forces a re-mint on every fetch.
    let origin = Origin::start(1);
    let mut session = Worker::new(false);
    for round in 0..MINT_CAP {
        let outcome = fetch(&mut session, &origin);
        assert_eq!(
            outcome.class,
            Class::Content,
            "round {round}: {:?}",
            outcome.attempts
        );
        origin.elapse(&mut session, 5);
    }
    assert_eq!(origin.verifies() as usize, MINT_CAP);

    let capped = fetch(&mut session, &origin);
    assert_eq!(capped.class, Class::WallProgram);
    assert_eq!(
        origin.verifies() as usize,
        MINT_CAP,
        "the cap is a hard bound"
    );
    assert!(
        capped.attempts.iter().any(|attempt| attempt
            .note
            .as_deref()
            .is_some_and(|note| note.contains("mint cap"))),
        "{:?}",
        capped.attempts
    );

    // The window slides: after it passes, minting is allowed again.
    origin.elapse(&mut session, MINT_WINDOW_SECS + 1);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    assert_eq!(origin.verifies() as usize, MINT_CAP + 1);
}

#[test]
fn a_bare_403_for_a_stale_credential_gets_one_clean_ask_and_one_mint() {
    let origin = Origin::start(300);
    origin.set_mode(MODE_BARE_403);
    let mut session = Worker::new(false);
    assert_eq!(fetch(&mut session, &origin).class, Class::Content);
    let before = origin.requests_to_root();

    origin.elapse(&mut session, 400);
    let outcome = fetch(&mut session, &origin);
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert_eq!(origin.verifies(), 2, "one re-mint");
    // The stale spend (403), the clean ask that returns the challenge (202), the
    // engine's retry with the new token (200): three requests, no loop.
    assert_eq!(
        origin.requests_to_root() - before,
        3,
        "{:?}",
        origin.server.hits()
    );
}

// ---- the ledger on its own -----------------------------------------------------

#[test]
fn the_gate_names_the_budget_and_the_backoff() {
    let mut state = State::default();
    assert!(state.engine_gate("a.test", 1_000).is_ok());
    for offset in 0..MINT_CAP as i64 {
        state.mint_recorded("a.test", "chrome", "aws-waf", &[], 1_000 + offset);
    }
    let capped = state.engine_gate("a.test", 1_100).unwrap_err();
    assert!(capped.contains("mint cap"), "{capped}");
    assert!(state
        .engine_gate("a.test", 1_000 + MINT_WINDOW_SECS + MINT_CAP as i64)
        .is_ok());

    state.mint_failed("b.test", "chrome", "origin answered 202", 5_000);
    let backing = state.engine_gate("b.test", 5_001).unwrap_err();
    assert!(
        backing.contains("backing off") && backing.contains("origin answered 202"),
        "{backing}"
    );
    assert!(state
        .engine_gate("b.test", 5_000 + BACKOFF_BASE_SECS)
        .is_ok());
}

#[test]
fn the_backoff_doubles_and_is_capped() {
    let mut state = State::default();
    let mut waits = Vec::new();
    for _ in 0..12 {
        state.mint_failed("a.test", "chrome", "refused", 0);
        waits.push(state.mints["a.test"].blocked_until);
    }
    assert_eq!(waits[0], BACKOFF_BASE_SECS);
    assert_eq!(waits[1], 2 * BACKOFF_BASE_SECS);
    assert_eq!(waits[2], 4 * BACKOFF_BASE_SECS);
    assert_eq!(*waits.last().unwrap(), BACKOFF_MAX_SECS);
}

#[test]
fn a_declared_lifetime_alone_expires_a_credential() {
    let mut state = State::default();
    let cookie = StoredCookie {
        name: "aws-waf-token".into(),
        value: "t1".into(),
        domain: None,
        path: None,
        expires_unix: Some(1_000 + 300),
        secure: true,
        http_only: false,
    };
    state.store_cookies("a.test", "chrome", vec![cookie.clone()]);
    state.mint_recorded("a.test", "chrome", "aws-waf", &[cookie], 1_000);
    assert!(!state.expire_stale_credential("a.test", "chrome", 1_299));
    assert!(state.expire_stale_credential("a.test", "chrome", 1_300));
    assert!(state.cookie_header("a.test", "chrome").is_none());
}

#[test]
fn a_dropped_credential_takes_only_its_own_cookies() {
    let mut state = State::default();
    let token = StoredCookie {
        name: "aws-waf-token".into(),
        value: "t1".into(),
        domain: None,
        path: None,
        expires_unix: None,
        secure: true,
        http_only: false,
    };
    let session = StoredCookie {
        name: "session-id".into(),
        value: "keep".into(),
        ..token.clone()
    };
    state.store_cookies("a.test", "chrome", vec![token.clone(), session]);
    state.mint_recorded("a.test", "chrome", "aws-waf", &[token], 1_000);
    state.drop_credential("a.test", "chrome");
    assert_eq!(
        state.cookie_header("a.test", "chrome").as_deref(),
        Some("session-id=keep")
    );
}

// ---- a hostile origin, through the whole encounter -------------------------------

fn hostile_origin(script: &'static str) -> Server {
    Server::start(move |_| Reply::Full {
        status: 202,
        headers: vec![
            ("x-amzn-waf-action".to_string(), "challenge".to_string()),
            ("content-type".to_string(), "text/html".to_string()),
        ],
        body: format!(
            "<html><head><script>{script}</script></head><body>aws-waf-token</body></html>"
        )
        .into_bytes(),
    })
}

fn hostile_fetch(server: &Server, session: &mut Worker, wall_ms: u64) -> (Outcome, Duration) {
    let mut bounded = options();
    bounded.engine.wall = Duration::from_millis(wall_ms);
    let started = Instant::now();
    let outcome = session.fetch(&server.url("/"), &bounded);
    (outcome, started.elapsed())
}

#[test]
fn a_challenge_that_never_ends_costs_one_wall_clock_and_is_then_left_alone() {
    let server = hostile_origin("for (;;) {}");
    let mut session = Worker::new(false);

    let (first, took) = hostile_fetch(&server, &mut session, 800);
    assert_eq!(
        first.class,
        Class::WallProgram,
        "a hostile page is a wall, never content"
    );
    assert!(
        took < Duration::from_millis(800 + 3_000),
        "the fetch took {took:?}"
    );
    let note = first
        .attempts
        .iter()
        .filter_map(|attempt| attempt.note.as_deref())
        .find(|note| note.starts_with("engine:"))
        .expect("the outcome names the engine");
    assert!(note.contains("wall-clock cap"), "{note}");
    assert_eq!(session.state.mints[host()].failures, 1);

    let (second, took) = hostile_fetch(&server, &mut session, 800);
    assert_eq!(second.class, Class::WallProgram);
    assert!(
        took < Duration::from_millis(800),
        "the second fetch must not run the program again: {took:?}"
    );
    assert!(second.attempts.iter().any(|attempt| attempt
        .note
        .as_deref()
        .is_some_and(|note| note.contains("backing off"))));
}

#[test]
fn a_challenge_that_exhausts_memory_is_a_wall_not_a_crash() {
    let server = hostile_origin("var keep = []; for (;;) keep.push(new Array(100000).fill(1.5));");
    let mut session = Worker::new(false);
    let mut bounded = options();
    bounded.engine.heap_max = 64 * 1024 * 1024;
    let outcome = session.fetch(&server.url("/"), &bounded);
    assert_eq!(outcome.class, Class::WallProgram);
    assert!(outcome.attempts.iter().any(|attempt| attempt
        .note
        .as_deref()
        .is_some_and(|note| note.contains("heap cap of 64 MB"))));
    assert_eq!(session.state.mints[host()].failures, 1);
}

#[test]
fn the_engine_page_check_follows_the_vendor() {
    // A real Cloudflare managed challenge (Glassdoor, 2026-11-06): the inline
    // `_cf_chl_opt` bootstrap and the challenge-platform loader, and none of the
    // AWS WAF markers. The header names the wall; only the body carries a program.
    let cloudflare = "<html><head><title>Just a moment...</title>\
        <script>window._cf_chl_opt={cvId:'3'};</script>\
        <script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1\"></script>\
        </head><body></body></html>";
    assert!(crate::engine::is_cloudflare_page(cloudflare));
    assert!(!crate::engine::is_aws_waf_page(cloudflare));
    assert!(is_program_page("cloudflare", cloudflare));
    assert!(!is_program_page("aws-waf", cloudflare));

    let aws_waf = "<html><script>window.gokuProps={};</script></html>";
    assert!(is_program_page("aws-waf", aws_waf));
    assert!(!is_program_page("cloudflare", aws_waf));

    // Reddit's gate: a form whose field its own inline program computes.
    let reddit = "<html><form method=\"GET\" action=\"/r/rust/\">\
        <input type=\"hidden\" name=\"jsc_token\" value=\"ab\"/></form></html>";
    assert!(is_program_page("reddit", reddit));
    assert!(!is_program_page("aws-waf", reddit));
}

/// Reddit's gate end to end: the origin serves its challenge page to a client
/// with no answer, and content to the request the page's own program builds --
/// the seed it wrote, doubled, with the token the form declared.
struct Gate {
    server: Server,
    answered: Arc<AtomicU32>,
}

const GATE_SEED: &str = "979da15b3033efb7";

impl Gate {
    fn start() -> Self {
        let answered = Arc::new(AtomicU32::new(0));
        let server = {
            let answered = Arc::clone(&answered);
            Server::start(move |request| {
                let path = request.path.as_str();
                if path.contains(&format!("solution={GATE_SEED}{GATE_SEED}"))
                    && path.contains("js_challenge=1")
                    && path.contains("jsc_token=tok-gate")
                {
                    answered.fetch_add(1, Ordering::SeqCst);
                    return Reply::ok(content_page());
                }
                Reply::ok(gate_page())
            })
        };
        Self { server, answered }
    }

    fn answered(&self) -> u32 {
        self.answered.load(Ordering::SeqCst)
    }
}

/// The page Reddit serves a client it means to gate, with this test's own seed.
fn gate_page() -> String {
    format!(
        "<!DOCTYPE html><html><head><title>Reddit</title>\
         <script>document.addEventListener(\"DOMContentLoaded\",async function(){{\
         var e=document.forms[0],n=(e.onsubmit=function(t){{return new URLSearchParams(document.location.search)\
         .forEach((e,n)=>t.target.appendChild(Object.assign(document.createElement(\"input\"),\
         {{name:e,type:\"hidden\",value:n}}))),!0}},await(async e=>e+e)(\"{GATE_SEED}\"));\
         e.elements.namedItem(\"solution\").value=n,e.requestSubmit()}},{{once:!0}});</script>\
         </head><body><form hidden method=\"GET\" action=\"/\">\
         <input type=\"hidden\" name=\"solution\" />\
         <input type=\"hidden\" name=\"js_challenge\" value=\"1\"/>\
         <input type=\"hidden\" name=\"jsc_token\" value=\"tok-gate\"/>\
         <input type=\"hidden\" name=\"jsc_orig_r\" value=\"\"/>\
         </form></body></html>"
    )
}

#[test]
fn a_gated_document_is_content_once_its_own_program_has_run() {
    let origin = Gate::start();
    let mut session = Worker::new(false);
    let outcome = session.fetch(&origin.server.url("/"), &options());
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert_eq!(
        origin.answered(),
        1,
        "one solved request, and it is the content request"
    );
    assert!(engine_notes(&outcome) >= 1, "{:?}", outcome.attempts);
}

/// The same origin, with Reddit's other door: the class the state learned to
/// trust is served a human check, and every other class the JS gate. The
/// encounter must cross to the class that is asked for a program instead of
/// declaring the human step on the first answer.
#[test]
fn a_human_check_on_the_learned_client_sends_the_encounter_to_the_class_that_gets_a_program() {
    let answered = Arc::new(AtomicU32::new(0));
    let server = {
        let answered = Arc::clone(&answered);
        Server::start(move |request| {
            if request
                .path
                .contains(&format!("solution={GATE_SEED}{GATE_SEED}"))
            {
                answered.fetch_add(1, Ordering::SeqCst);
                return Reply::ok(content_page());
            }
            let agent = request.header("user-agent").unwrap_or_default();
            if agent.contains("Chrome/") {
                // The human check: a widget, no program to run.
                return Reply::ok(
                    "<html><head><title>Prove your humanity</title>\
                     <script src=\"https://www.google.com/recaptcha/api.js\"></script>\
                     </head><body>Prove your humanity</body></html>",
                );
            }
            Reply::ok(gate_page())
        })
    };
    let mut session = Worker::new(false);
    session
        .state
        .origins
        .entry(host().to_string())
        .or_default()
        .chosen = Some("chrome".to_string());
    let portfolio = Options {
        identity: None,
        pace_ms: 0,
        persist: false,
        deadline: Duration::from_secs(20),
        ..Options::default()
    };
    let outcome = session.fetch(&server.url("/"), &portfolio);
    assert_eq!(outcome.class, Class::Content, "{:?}", outcome.attempts);
    assert!(
        answered.load(Ordering::SeqCst) >= 1,
        "{:?}",
        outcome.attempts
    );
    assert!(
        outcome
            .attempts
            .iter()
            .any(|attempt| attempt.class == Class::WallInteraction),
        "the human check was seen, and crossed: {:?}",
        outcome.attempts
    );
}
