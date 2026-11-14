//! The content lane against local pages and a local server, fully offline.
//!
//! Every test gives the engine a page the engine did not write and asserts what
//! came back: the document the page's own program assembled, the wire tap of
//! the requests it made, and the honest stop reason when it assembles nothing.

use crate::engine::{content_within_via, Content, MintError};
use crate::envelope::Limits;
use crate::identity::{self, Identity};
use crate::testkit::{Reply, Server};
use std::time::Duration;

fn chrome() -> &'static Identity {
    identity::lookup("chrome").expect("chrome identity")
}

fn lane_limits(wall_ms: u64) -> Limits {
    Limits {
        wall: Duration::from_millis(wall_ms),
        ..Limits::content()
    }
}

/// One content run against a local server, serialised with the engine tests so
/// a timing assertion is the run's own.
fn render_with(
    limits: &Limits,
    server: &Server,
    html: &str,
    target: usize,
) -> Result<Content, MintError> {
    let _turn = crate::engine_tests::serial();
    let url = format!("http://{}/", server.addr);
    content_within_via(limits, &url, html, chrome(), None, None, target)
}

fn render(server: &Server, html: &str, target: usize) -> Result<Content, MintError> {
    render_with(&lane_limits(5_000), server, html, target)
}

/// A page whose visible text is long enough to pass a 200-character contract.
fn filler() -> String {
    "The page's own program assembled this text after the document arrived. ".repeat(4)
}

#[test]
fn a_shell_app_renders_its_own_data() {
    let server = Server::start(|request| match request.path.as_str() {
        "/app.js" => Reply::ok(
            r#"(function () {
  var root = document.getElementById("root");
  var list = document.createElement("ul");
  root.appendChild(list);
  fetch("/api/items").then(function (response) { return response.json(); }).then(function (items) {
    items.forEach(function (item) {
      var li = document.createElement("li");
      li.textContent = item.name;
      li.className = "item";
      list.appendChild(li);
    });
  });
})();"#,
        ),
        "/api/items" => {
            Reply::ok(r#"[{"name":"Alpha Widget"},{"name":"Beta Widget"},{"name":"Gamma Widget"}]"#)
        }
        _ => Reply::ok("shell"),
    });
    let html = r#"<html><head><script src="/app.js"></script></head><body><div id="root"></div></body></html>"#;
    let content = render(&server, html, 30).expect("the content run should finish");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(
        content.document.contains("Alpha Widget"),
        "the document should carry the app's own items: {}",
        content.document
    );
    assert!(content.document.contains("class=\"item\""));
    let api = content
        .transcript
        .iter()
        .find(|entry| entry.url.ends_with("/api/items"))
        .expect("the app's own API call should be on the tap");
    assert_eq!(api.status, 200);
    assert!(
        api.body
            .as_deref()
            .unwrap_or_default()
            .contains("Beta Widget"),
        "the JSON body should be kept for mining"
    );
    assert!(content.text_chars >= 30);
}

#[test]
fn scripts_run_in_document_order() {
    let server = Server::start(|request| {
        match request.path.as_str() {
        "/one.js" => Reply::ok("window.__order = (window.__order || []).concat('one');"),
        "/two.js" => Reply::ok(
            "window.__order = (window.__order || []).concat('two'); document.body.textContent = window.__order.join(' ');",
        ),
        _ => Reply::ok("ok"),
    }
    });
    let html = r#"<html><head><script src="/one.js"></script><script>window.__order = (window.__order || []).concat('inline');</script></head><body><script src="/two.js"></script></body></html>"#;
    let content = render(&server, html, 3).expect("content run");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(
        content.document.contains("one inline two"),
        "scripts must execute in document order: {}",
        content.document
    );
}

#[test]
fn inner_html_parses_and_selectors_match() {
    let server = Server::start(|request| match request.path.as_str() {
        "/dom.js" => Reply::ok(
            r#"(function () {
  var root = document.getElementById("root");
  root.innerHTML = '<div class="panel"><span data-kind="x">Hello</span><span class="skip">Bye</span></div>';
  var span = document.querySelector('.panel span[data-kind="x"]');
  span.textContent = span.textContent + " " + span.dataset.kind;
  var all = document.querySelectorAll("div.panel > span");
  document.body.setAttribute("data-count", String(all.length));
  document.body.appendChild(document.createTextNode(" " + document.body.getAttribute("data-count")));
})();"#,
        ),
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/dom.js"></script></head><body><div id="root"></div></body></html>"#;
    let content = render(&server, html, 3).expect("content run");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(
        content.document.contains("Hello x"),
        "innerHTML content must be live and selectable: {}",
        content.document
    );
    assert!(
        content.document.contains("data-count=\"2\""),
        "the child combinator must match both spans: {}",
        content.document
    );
}

#[test]
fn a_quiet_page_stops_as_quiescent() {
    let server = Server::start(|_| Reply::ok("ok"));
    let html = r#"<html><head><script>window.__loaded = true;</script></head><body><div id="root"></div></body></html>"#;
    let content = render(&server, html, 200).expect("content run");
    assert_eq!(content.stop, "quiescent", "logs: {:?}", content.logs);
    assert!(content.text_chars < 200);
}

#[test]
fn the_wall_clock_bounds_a_spinning_program() {
    let server = Server::start(|_| Reply::ok("ok"));
    let html = r#"<html><head><script>while (true) {}</script></head><body></body></html>"#;
    let limits = lane_limits(800);
    let outcome = render_with(&limits, &server, html, 200);
    assert!(
        matches!(outcome, Err(MintError::WallClock { .. })),
        "a spinning program must be terminated: {outcome:?}"
    );
}

#[test]
fn lazy_loaders_load_when_observed() {
    let server = Server::start(|request| match request.path.as_str() {
        "/lazy.js" => Reply::ok(
            r#"(function () {
  var sentinel = document.getElementById("sentinel");
  var observer = new IntersectionObserver(function (entries) {
    if (!entries[0].isIntersecting) return;
    var article = document.createElement("article");
    article.textContent = "Lazy loaded article body appeared.";
    document.getElementById("root").appendChild(article);
    observer.disconnect();
  });
  observer.observe(sentinel);
})();"#,
        ),
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/lazy.js"></script></head><body><div id="root"><div id="sentinel"></div></div></body></html>"#;
    let content = render(&server, html, 20).expect("content run");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(content
        .document
        .contains("Lazy loaded article body appeared."));
}

#[test]
fn media_queries_answer_from_the_identity_viewport() {
    let server = Server::start(|request| match request.path.as_str() {
        "/media.js" => Reply::ok(
            r#"document.body.textContent = matchMedia("(min-width: 768px)").matches ? "desktop layout" : "mobile layout";"#,
        ),
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/media.js"></script></head><body></body></html>"#;
    let content = render(&server, html, 3).expect("content run");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(content.document.contains("desktop layout"));
}

#[test]
fn serialization_keeps_attributes_void_elements_and_escaping() {
    let server = Server::start(|request| match request.path.as_str() {
        "/build.js" => Reply::ok(
            r#"(function () {
  var root = document.getElementById("root");
  root.setAttribute("data-x", 'a"b&c');
  var img = document.createElement("img");
  img.setAttribute("src", "/p.png");
  root.appendChild(img);
  var p = document.createElement("p");
  p.appendChild(document.createTextNode("a < b & c"));
  root.appendChild(p);
})();"#,
        ),
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/build.js"></script></head><body><div id="root"></div></body></html>"#;
    let content = render(&server, html, 3).expect("content run");
    assert!(
        content.document.contains("<img src=\"/p.png\">"),
        "{}",
        content.document
    );
    assert!(
        content.document.contains("a &lt; b &amp; c"),
        "{}",
        content.document
    );
    assert!(
        content.document.contains("data-x=\"a&quot;b&amp;c\""),
        "{}",
        content.document
    );
}

#[test]
fn timers_and_document_events_drive_rendering() {
    let server = Server::start(|request| match request.path.as_str() {
        "/timer.js" => Reply::ok(
            r#"document.addEventListener("DOMContentLoaded", function () {
  setTimeout(function () {
    var node = document.createElement("p");
    node.textContent = "rendered after a timer";
    document.getElementById("root").appendChild(node);
  }, 25);
});"#,
        ),
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/timer.js"></script></head><body><div id="root"></div></body></html>"#;
    let content = render(&server, html, 5).expect("content run");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(content.document.contains("rendered after a timer"));
}

#[test]
fn a_router_push_state_is_not_a_navigation() {
    let server = Server::start(|request| match request.path.as_str() {
        "/router.js" => Reply::ok(
            r#"history.pushState({ page: 1 }, "", "/somewhere-else");
document.body.textContent = "routed without a document load";"#,
        ),
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/router.js"></script></head><body></body></html>"#;
    let content = render(&server, html, 3).expect("content run");
    assert_eq!(content.stop, "content", "logs: {:?}", content.logs);
    assert!(
        content.navigate.is_none(),
        "a router's pushState must not read as a document navigation: {:?}",
        content.navigate
    );
    assert!(content.visited.ends_with("/somewhere-else"));
}

#[test]
fn the_body_fill_of_a_real_page_crosses_the_contract() {
    // The stop rule is the contract, not the clock: a page that assembles more
    // than the target ends the run as content before its budget.
    let server = Server::start(|request| match request.path.as_str() {
        "/fill.js" => {
            let text = filler();
            Reply::ok(format!("document.body.textContent = {text:?};"))
        }
        _ => Reply::ok("ok"),
    });
    let html = r#"<html><head><script src="/fill.js"></script></head><body></body></html>"#;
    let content = render(&server, html, 200).expect("content run");
    assert_eq!(content.stop, "content");
    assert!(content.text_chars >= 200);
    assert!(content.ms < 4_000.0, "text arrived in {:.0} ms", content.ms);
}
