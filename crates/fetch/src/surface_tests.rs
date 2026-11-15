//! The content host's JS-visible surface, measured against the reference browser.
//!
//! `surface_reference.json` is the same probe page (`surface_probe.html`) read by this box's
//! real Chromium 157, headed, and captured with bladebro. The lane is fed the same page through
//! the engine, and every surface it answers — window geometry, screen, media queries, the
//! observer family, the constructors a bot check reads — must agree with the
//! reference, except what the fixture's `declared` block names as the lane's own
//! contract: no layout (positions are zeros, a box is only as big as its declared
//! style), every observed element visible (the lazy-loader contract), and
//! computed style without a cascade.
//!
//! A headless capture is not the reference: it reports `hover: none`,
//! `pointer: none` and `prefers-color-scheme: light`, which no desktop client
//! reports. Regenerate with the recipe in `browser.regenerate`.

use crate::engine::{content_within_via_policy, Content};
use crate::envelope::Limits;
use crate::identity::{self, Identity};
use crate::testkit::{Reply, Server};
use serde_json::{json, Value};
use std::time::Duration;

const PROBE: &str = include_str!("surface_probe.html");
const REFERENCE: &str = include_str!("surface_reference.json");

fn chrome_local() -> &'static Identity {
    identity::lookup("chrome-local").expect("the chrome-local identity")
}

fn reference() -> Value {
    serde_json::from_str(REFERENCE).expect("the reference fixture parses")
}

/// The probe page through the lane, once, with the answers it assembled.
fn lane_answers() -> (Value, Content) {
    let server = Server::start(|request| match request.path.as_str() {
        "/" => Reply::ok(PROBE),
        _ => Reply::ok("not found"),
    });
    let _turn = crate::engine_tests::serial();
    let url = format!("http://{}/", server.addr);
    let content = content_within_via_policy(
        &Limits {
            wall: Duration::from_millis(6_000),
            ..Limits::content()
        },
        &url,
        PROBE,
        chrome_local(),
        None,
        None,
        200,
        true,
    )
    .expect("the content run should finish");
    (answers_of(&content), content)
}

/// The probe's answers, read out of the document the page's program assembled.
fn answers_of(content: &Content) -> Value {
    let html = scraper::Html::parse_document(&content.document);
    let selector = scraper::Selector::parse("script#answers").expect("a selector");
    let text = html
        .select(&selector)
        .next()
        .map(|element| element.text().collect::<String>())
        .unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!("the probe's answers should be JSON ({error}): {text}");
    })
}

/// A dotted path into a JSON document, for readable failures.
fn pick<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut node = value;
    for step in path.split('.') {
        node = node.get(step)?;
    }
    Some(node)
}

/// One surface: the lane's answer against the reference's, named by the
/// reference's own path (the lane names the probe's timer-phase capture
/// `geometryAfterTimers`, so a few rows read across, never renamed).
fn compare(
    wrong: &mut Vec<String>,
    lane: &Value,
    measured: &Value,
    lane_path: &str,
    measured_path: &str,
) {
    let want = pick(measured, measured_path)
        .cloned()
        .unwrap_or(Value::Null);
    let got = pick(lane, lane_path).cloned().unwrap_or(Value::Null);
    if want != got {
        wrong.push(format!("{measured_path}: lane {got} reference {want}"));
    }
}

/// Every measured surface, checked against the reference. One lane run, one list
/// of disagreements: a surface that drifts names itself.
#[test]
fn the_lane_matches_the_reference_browser() {
    let reference = reference();
    let measured = &reference["measured"];
    let (lane, content) = lane_answers();
    assert!(
        lane["done"].as_bool().unwrap_or(false),
        "the probe's timer phases must all run: {}",
        serde_json::to_string(&lane).unwrap_or_default()
    );

    let mut wrong: Vec<String> = Vec::new();

    // Window geometry, screen and the document's own facts.
    for path in [
        "geometry.innerWidth",
        "geometry.innerHeight",
        "geometry.outerWidth",
        "geometry.outerHeight",
        "geometry.devicePixelRatio",
        "geometry.scrollX",
        "geometry.scrollY",
        "geometry.screenX",
        "geometry.screenY",
        "geometry.screen.width",
        "geometry.screen.height",
        "geometry.screen.availWidth",
        "geometry.screen.availHeight",
        "geometry.screen.colorDepth",
        "geometry.screen.pixelDepth",
        "geometry.screen.orientation",
        "geometry.scrollbarWidth",
        "geometry.scrollbarHeight",
        "geometry.compatMode",
        "geometry.characterSet",
        "geometry.contentType",
        "geometry.visibilityState",
        // "loading" while the document's own script runs, "complete" after its
        // timers have drained.
        "geometry.readyState",
    ] {
        compare(&mut wrong, &lane, measured, path, path);
    }
    compare(
        &mut wrong,
        &lane,
        measured,
        "geometryAfterTimers.readyState",
        "afterTimers.readyState",
    );
    compare(
        &mut wrong,
        &lane,
        measured,
        "geometryAfterTimers.innerWidth",
        "afterTimers.innerWidth",
    );
    compare(
        &mut wrong,
        &lane,
        measured,
        "geometryAfterTimers.innerHeight",
        "afterTimers.innerHeight",
    );
    compare(
        &mut wrong,
        &lane,
        measured,
        "geometryAfterTimers.devicePixelRatio",
        "afterTimers.devicePixelRatio",
    );

    // Every media query the reference answered.
    let queries = measured["media"]
        .as_object()
        .expect("the reference carries media queries")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert!(queries.len() >= 80, "the probe covers the media family");
    for query in &queries {
        let want = measured["media"][query].clone();
        let got = lane["media"][query].clone();
        if want != got {
            wrong.push(format!("matchMedia({query}): lane {got} reference {want}"));
        }
    }

    // The constructors and functions a bot check reads for their presence.
    let absent = reference["declaredAbsent"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|name| name.as_str().map(String::from))
        .collect::<Vec<_>>();
    let presence = measured["presence"]
        .as_object()
        .expect("the reference carries presence");
    for (name, want) in presence {
        let got = lane["presence"][name].clone();
        if &got != want && !absent.contains(name) {
            wrong.push(format!("window.{name}: lane {got} reference {want}"));
        }
    }

    // The observer family: delivery order, record shape, entry shape and the
    // declared size each entry reports.
    for path in [
        "observers.mutation.records",
        "observers.mutation.types",
        "observers.mutation.attributeNames",
        "observers.mutation.added",
        "observers.mutation.targets",
        "observers.mutation.characterData",
        "observers.mutation.syncDelivered",
        "observers.intersectionOrder",
        "observers.resize.count",
    ] {
        compare(&mut wrong, &lane, measured, path, path);
    }
    if lane["observers"]["mutationSyncDelivered"] != json!(false) {
        wrong.push(
            "observers.mutationSyncDelivered: the callback must not run inside the script".into(),
        );
    }
    // The resize entries: the target's declared content and border boxes, and the
    // callback carries both targets at once, as Chrome's does.
    for target in ["sized", "paddedio"] {
        for field in [
            "width",
            "height",
            "borderWidth",
            "borderHeight",
            "contentWidth",
            "contentHeight",
        ] {
            compare(
                &mut wrong,
                &lane["observers"]["resize"]["targets"][target],
                &measured["observers"]["resize"]["targets"][target],
                field,
                field,
            );
        }
    }
    // The intersection entries: box, root and coverage are the reference's; the
    // visible/ratio pair is the lane's declared contract.
    for target in ["inview", "paddedio", "far"] {
        for field in [
            "width",
            "height",
            "hasRootBounds",
            "rootWidth",
            "rootHeight",
            "time",
        ] {
            compare(
                &mut wrong,
                &lane["observers"]["intersection"][target],
                &measured["observers"]["intersection"][target],
                field,
                field,
            );
        }
        let entry = &lane["observers"]["intersection"][target];
        if entry["isIntersecting"] != json!(true)
            || entry["intersectionRatio"].as_f64() != Some(1.0)
        {
            wrong.push(format!(
                "observers.intersection.{target}: the lane's contract is visible at ratio 1, got {entry}"
            ));
        }
        if entry["coveredWidth"] != entry["width"] || entry["coveredHeight"] != entry["height"] {
            wrong.push(format!(
                "observers.intersection.{target}: a visible entry covers its own box, got {entry}"
            ));
        }
    }
    for target in ["inview", "paddedio"] {
        for field in ["coveredWidth", "coveredHeight"] {
            compare(
                &mut wrong,
                &lane["observers"]["intersection"][target],
                &measured["observers"]["intersection"][target],
                field,
                field,
            );
        }
    }

    assert!(
        wrong.is_empty(),
        "the lane's surface disagrees with the reference browser (stop={}, {} text chars):\n  {}",
        content.stop,
        content.text_chars,
        wrong.join("\n  ")
    );
}

/// Declared, not drifted: the lane has no layout. A position is always zero, a
/// box is only as big as its own declared style, and the document's client box is
/// the viewport less the scrollbar. A change that starts reporting laid-out
/// rectangles is a change to the lane's contract, not an improvement.
#[test]
fn the_lane_reports_no_layout() {
    let reference = reference();
    let boxes = &reference["measured"]["boxes"];
    let (lane, content) = lane_answers();
    let geometry = &reference["measured"]["geometry"];
    let inner = geometry["innerWidth"].as_i64().expect("innerWidth");
    let inner_height = geometry["innerHeight"].as_i64().expect("innerHeight");
    let scrollbar = geometry["scrollbarWidth"].as_i64().expect("scrollbarWidth");

    let document = &lane["boxes"];
    assert_eq!(
        document["documentElement"]["clientWidth"].as_i64(),
        Some(inner - scrollbar),
        "the document's client width is the viewport less the scrollbar"
    );
    assert_eq!(
        document["documentElement"]["clientHeight"].as_i64(),
        Some(inner_height),
        "the document's client height is the viewport"
    );
    assert_eq!(
        document["body"]["clientWidth"].as_i64(),
        Some(inner - scrollbar),
        "the body's client box is the document's: there are no margins to compute"
    );

    let bordered = boxes
        .as_object()
        .expect("the reference measures declared boxes");
    for (id, element) in document["elements"]
        .as_object()
        .expect("the probe measures elements")
    {
        assert_eq!(
            element["clientRects"].as_i64(),
            Some(1),
            "{id}: one client rect"
        );
        let rect = element["rect"].as_array().expect("a rect");
        assert_eq!(rect[0], json!(0), "{id}: x is never computed");
        assert_eq!(rect[4], json!(0), "{id}: top is never computed");
        assert_eq!(rect[5], json!(0), "{id}: left is never computed");
        match bordered.get(id) {
            // A declared size is what the page wrote: the three box models agree
            // with a real client on exactly these elements.
            Some(want) => {
                let width = want["border"]["width"].as_i64().expect("a border width");
                let height = want["border"]["height"].as_i64().expect("a border height");
                assert_eq!(
                    rect[2],
                    json!(width),
                    "{id}: the border box width is declared"
                );
                assert_eq!(
                    rect[3],
                    json!(height),
                    "{id}: the border box height is declared"
                );
                for (model, prefix) in [
                    ("client", "client"),
                    ("border", "offset"),
                    ("scroll", "scroll"),
                ] {
                    for (axis, suffix) in [("width", "Width"), ("height", "Height")] {
                        let want = want[model][axis].as_i64().expect("a measured box");
                        let got = element[format!("{prefix}{suffix}")].as_i64();
                        assert_eq!(got, Some(want), "{id}: {prefix}{suffix} is declared");
                    }
                }
                assert_eq!(
                    element["clientWidth"].as_i64(),
                    Some(want["client"]["width"].as_i64().unwrap()),
                    "{id}: clientWidth is the content box plus padding"
                );
            }
            // No declared size: no layout, so every box is zero and nothing is
            // invented from the content.
            None => {
                assert_eq!(rect[2], json!(0), "{id}: an undeclared width is 0");
                assert_eq!(rect[3], json!(0), "{id}: an undeclared height is 0");
            }
        }
    }
    assert_eq!(lane["errors"], json!([]), "the probe must not throw");
    assert_eq!(
        content.stop, "quiescent",
        "the probe assembles no content contract"
    );
}

/// The measurement, printed for the run artifact. Not a gate: it answers what the
/// lane says today, for a human comparing it with the reference.
#[test]
#[ignore = "measurement: prints the lane's surface answers"]
fn print_the_lane_surface() {
    let (lane, content) = lane_answers();
    println!("stop={} text_chars={}", content.stop, content.text_chars);
    println!(
        "{}",
        serde_json::to_string_pretty(&lane).unwrap_or_default()
    );
}
