//! The mined request contract, through the encounter: a shell's first run
//! mines the request that assembled its content, the next fetch replays it as
//! plain HTTP, and an answer that breaks the contract falls back to the lane
//! and mines a fresh one.

use crate::envelope::Limits;
use crate::fetch::{Options, Worker};
use crate::testkit::{Reply, Server};
use crate::Class;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SHELL: &str = r#"<!doctype html><html><head><title>Fixture</title></head>
<body><div id="app"></div><script src="/app.js"></script></body></html>"#;

const SCRIPT: &str = r#"
fetch('/api/items').then(function (response) { return response.json(); }).then(function (data) {
  var root = document.getElementById('app');
  data.items.forEach(function (item) {
    var article = document.createElement('article');
    var link = document.createElement('a');
    link.setAttribute('href', item.url);
    link.textContent = item.title;
    article.appendChild(link);
    var body = document.createElement('p');
    body.textContent = item.body;
    article.appendChild(body);
    root.appendChild(article);
  });
});
"#;

fn items() -> String {
    r#"{"items":[
      {"title":"The first story a reader came for","url":"https://example.com/one","body":"A paragraph of real text that the page shows to a reader, long enough to count."},
      {"title":"The second story a reader came for","url":"https://example.com/two","body":"Another paragraph of real text in the list, long enough to count."},
      {"title":"The third story a reader came for","url":"https://example.com/three","body":"A third paragraph of real text in the list, long enough to count."}
    ]}"#
    .to_string()
}

fn options() -> Options {
    Options {
        capture_body: true,
        arrival: false,
        max_tries: 3,
        deadline: Duration::from_secs(8),
        contract: Some(crate::Contract::generic()),
        persist: true,
        identity: None,
        pace_ms: 0,
        engine: Limits::from_env(),
        content: Limits::content(),
        proxy: None,
        cache: false,
        revalidate: false,
        policy: Default::default(),
        // The fixture origin is loopback; the test explicitly opts in.
        allow_private: true,
    }
}

#[test]
fn a_shell_run_mines_a_contract_and_the_next_fetch_replays_it() {
    // The state path is process-global; nextest runs each test in its own
    // process, which this test depends on.
    let dir = std::env::temp_dir().join(format!("donsetch-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("V5_STATE", dir.join("state.json"));

    let api_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&api_calls);
    let server = Server::start(move |request| match request.path.as_str() {
        "/" => Reply::ok(SHELL),
        "/app.js" => Reply::Full {
            status: 200,
            headers: vec![("Content-Type".into(), "text/javascript".into())],
            body: SCRIPT.as_bytes().to_vec(),
        },
        "/api/items" => {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 2 {
                // The contract's second call is a 403: the replay must drop the
                // contract, and the lane behind it must mine a fresh one.
                Reply::Full {
                    status: 403,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body: b"refused".to_vec(),
                }
            } else {
                Reply::Full {
                    status: 200,
                    headers: vec![("Content-Type".into(), "application/json".into())],
                    body: items().into_bytes(),
                }
            }
        }
        _ => Reply::Full {
            status: 404,
            headers: Vec::new(),
            body: b"not found".to_vec(),
        },
    });

    let mut worker = Worker::new(true);
    let url = server.url("/");
    let first = worker.fetch(&url, &options());
    assert_eq!(first.class, Class::Content, "{:?}", first.failures);
    assert_eq!(first.transport, "program", "{:?}", first.attempts);
    assert!(
        first
            .evidence
            .iter()
            .any(|line| line.contains("mined replay contract")),
        "{:?}",
        first.evidence
    );

    // The replay is 403: the lane takes over, the contract is replaced.
    let second = worker.fetch(&url, &options());
    assert_eq!(second.class, Class::Content, "{:?}", second.failures);
    assert_eq!(second.transport, "program", "{:?}", second.attempts);
    assert!(
        second
            .attempts
            .iter()
            .any(|attempt| attempt.transport == "replay" && attempt.status == 403),
        "{:?}",
        second.attempts
    );
    assert!(
        second
            .evidence
            .iter()
            .any(|line| line.contains("mined replay contract")),
        "{:?}",
        second.evidence
    );

    // The fresh contract answers: plain HTTP, the app's own request, no lane.
    let third = worker.fetch(&url, &options());
    assert_eq!(third.transport, "replay", "{:?}", third.attempts);
    assert!(third.items >= 3, "{}", third.body_text);
    assert!(third.failures.is_empty());
    assert!(
        third.sample[0].contains("first story"),
        "{:?}",
        third.sample
    );

    let _ = std::fs::remove_dir_all(&dir);
}
