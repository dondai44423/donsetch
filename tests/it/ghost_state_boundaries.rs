//! Persisted learning boundaries use the actual library's disk writes, not
//! the unit build's no-op save. Depends on nextest's per-test process sandbox.

use donsetch::ghost::cache::{GhostState, RouteDecision, cookies_fresh_at, now};
use serde_json::{Value, json};
use std::path::PathBuf;

const HOST: &str = "boundary.example";
const URL: &str = "https://boundary.example/page";

fn copied_state(seed: &Value) -> (GhostState, PathBuf, Vec<u8>) {
    let root = crate::sandbox();
    assert!(!donsetch::config::cfg().state.no_disk_state);
    let bytes = serde_json::to_vec(seed).unwrap();
    let original = root.join("boundary-original.json");
    std::fs::write(&original, &bytes).unwrap();
    std::fs::copy(&original, root.join("ghost-state.json")).unwrap();
    (GhostState::load(), original, bytes)
}

#[test]
fn persisted_observation_counters_saturate_without_losing_the_observation() {
    type Operation = (&'static str, fn(&mut GhostState), &'static [&'static str]);
    let operations: &[Operation] = &[
        ("fetch", |s| s.record_fetch(HOST), &["fetch_count"]),
        (
            "cold-ok",
            |s| s.record_cold_ok(HOST),
            &["fetch_count", "t1_samples"],
        ),
        (
            "cold-wall",
            |s| s.record_cold_walled(HOST, Some("owned")),
            &["fetch_count", "walled_count", "t1_samples"],
        ),
        (
            "warm-ok",
            |s| s.record_warm_ok(HOST, &[]),
            &["fetch_count", "warm_ok_count", "warm_samples"],
        ),
        (
            "warm-stale",
            |s| s.record_warm_stale(HOST),
            &[
                "fetch_count",
                "warm_fail_count",
                "warm_fail_streak",
                "warm_samples",
            ],
        ),
        (
            "solved",
            |s| s.record_solved(HOST, &[], Some("owned"), false),
            &["solve_count", "ghost_samples"],
        ),
        (
            "wall-failed",
            |s| s.record_wall_failed(HOST),
            &["wall_fail_streak", "ghost_samples"],
        ),
    ];
    let mut failures = Vec::new();
    let mut reached = 0;
    for (name, operation, counters) in operations {
        for field in *counters {
            let mut seed = json!({"version": 3, "profiles": {HOST: {}}});
            seed["profiles"][HOST][*field] = json!(u32::MAX - 1);
            let (mut state, original, bytes) = copied_state(&seed);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                operation(&mut state);
                let first: Value = serde_json::to_value(GhostState::load()).unwrap();
                assert_eq!(first["profiles"][HOST][*field], json!(u32::MAX));
                operation(&mut state);
                let saved: Value = serde_json::to_value(GhostState::load()).unwrap();
                assert_eq!(saved["profiles"][HOST][*field], json!(u32::MAX));
                // A saturated count must not suppress the observation or save.
                let p = &saved["profiles"][HOST];
                match *name {
                    "fetch" | "cold-ok" | "cold-wall" => {
                        assert!(p["last_cold_check"].as_u64().unwrap() > 0)
                    }
                    "warm-ok" => assert!(p["last_refreshed"].as_u64().unwrap() > 0),
                    "warm-stale" => assert!(p["observed_lifetime"].as_u64().unwrap() >= 120),
                    "solved" => assert_eq!(p["wall_vendor"], "owned"),
                    "wall-failed" => assert!(p["last_wall_fail"].as_u64().unwrap() > 0),
                    _ => unreachable!(),
                }
            }));
            reached += 1;
            assert_eq!(std::fs::read(original).unwrap(), bytes, "seed changed");
            if result.is_err() {
                failures.push(format!("{name}/{field}"));
            }
        }
    }
    assert_eq!(reached, 17);
    assert!(
        failures.is_empty(),
        "persisted counter failures: {failures:?}"
    );
}

#[test]
fn persisted_future_solve_is_not_fresh_or_warm() {
    for future in [now() + 3600, u64::MAX] {
        let seed = json!({"version": 3, "profiles": {HOST: {
            "needs_tier2": true, "last_solved": future,
            "last_cold_check": now(), "replay_ok": true,
            "cookies": [{"name": "cf_clearance", "value": "owned", "domain": HOST}]
        }}});
        let (mut state, original, bytes) = copied_state(&seed);
        assert!(!cookies_fresh_at(&state.profiles[HOST], now()));
        assert!(matches!(
            state.route_for_class(HOST, "direct"),
            RouteDecision::SkipToSolve
        ));
        assert_eq!(state.route_stats().2, 0);
        state.profiles.get_mut(HOST).unwrap().last_solved = now();
        assert!(cookies_fresh_at(&state.profiles[HOST], now()));
        assert!(matches!(
            state.route_for_class(HOST, "direct"),
            RouteDecision::Warm(_)
        ));
        assert_eq!(std::fs::read(original).unwrap(), bytes);
    }
}

#[test]
fn persisted_future_render_is_not_served() {
    for future in [now() + 3600, u64::MAX] {
        let seed = json!({"version": 3, "renders": {URL: {"html": "owned", "at": future}}});
        let (mut state, original, bytes) = copied_state(&seed);
        assert!(state.render_for(URL).is_none());
        state.record_render(URL, "fresh-owned");
        assert_eq!(
            GhostState::load().render_for(URL).unwrap().html,
            "fresh-owned"
        );
        assert_eq!(std::fs::read(original).unwrap(), bytes);
    }
}

#[test]
fn persisted_render_context_rejects_other_routes_and_legacy_entries() {
    let seed = json!({"version": 3, "renders": {URL: {"html": "legacy", "at": now()}}});
    let (mut state, original, bytes) = copied_state(&seed);
    assert_eq!(state.render_for(URL).unwrap().html, "legacy");
    assert!(state.render_for_context(URL, "owned-route-a").is_none());
    state.record_render_context(URL, "scoped-owned", "owned-route-a");
    let state = GhostState::load();
    assert_eq!(
        state.render_for_context(URL, "owned-route-a").unwrap().html,
        "scoped-owned"
    );
    assert!(state.render_for_context(URL, "owned-route-b").is_none());
    assert!(state.render_for(URL).is_none());
    assert!(donsetch::ghost::cache::clear_session_cookies_for(HOST));
    assert!(
        GhostState::load()
            .render_for_context(URL, "owned-route-a")
            .is_none()
    );
    assert_eq!(std::fs::read(original).unwrap(), bytes);
}

#[test]
fn persisted_scoped_render_rejects_expired_and_future_timestamps() {
    for at in [now() - 301, now() + 3600, u64::MAX] {
        let seed = json!({"version": 3, "renders": {URL: {
            "html": "stale", "at": at, "context": "owned-route-a"
        }}});
        let (mut state, original, bytes) = copied_state(&seed);
        assert!(state.render_for_context(URL, "owned-route-a").is_none());
        state.record_render_context(URL, "fresh-owned", "owned-route-a");
        assert_eq!(
            GhostState::load()
                .render_for_context(URL, "owned-route-a")
                .unwrap()
                .html,
            "fresh-owned"
        );
        assert_eq!(std::fs::read(original).unwrap(), bytes);
    }
}

#[test]
fn persisted_browser_render_retains_its_own_document_receipt() {
    let (mut state, original, bytes) = copied_state(&json!({"version":3}));
    for status in [Some(201), None] {
        let document = donsetch::ghost::document::Document {
            generation: 7,
            frame: "owned-main".into(),
            loader: "owned-loader".into(),
            url: URL.into(),
            status,
        };
        state.record_render_document(&document, "owned browser content", "owned-route-a");
        let saved = GhostState::load();
        let render = saved.render_for_context(URL, "owned-route-a").unwrap();
        assert_eq!(render.document.as_ref(), Some(&document));
        assert_eq!(render.html, "owned browser content");
        assert!(saved.render_for(URL).is_none());
    }
    assert_eq!(std::fs::read(original).unwrap(), bytes);
}

#[test]
fn persisted_render_replacement_at_capacity_keeps_other_entries() {
    let mut seed = json!({"version": 3, "renders": {}});
    for i in 0..20 {
        seed["renders"][format!("https://boundary.example/{i}")] =
            json!({"html": "owned", "at": now() - 30 + i, "context": "owned-route-a"});
    }
    let (mut state, original, bytes) = copied_state(&seed);
    state.record_render_context(
        "https://boundary.example/19",
        "replacement",
        "owned-route-b",
    );
    let saved = GhostState::load();
    assert_eq!(
        saved.renders.len(),
        20,
        "updating an existing key must not evict another page"
    );
    assert!(saved.renders.contains_key("https://boundary.example/0"));
    assert_eq!(
        saved
            .render_for_context("https://boundary.example/19", "owned-route-b")
            .unwrap()
            .html,
        "replacement"
    );
    state.record_render_context("https://boundary.example/new", "new", "owned-route-b");
    let saved = GhostState::load();
    assert_eq!(saved.renders.len(), 20);
    assert!(!saved.renders.contains_key("https://boundary.example/0"));
    assert!(saved.renders.contains_key("https://boundary.example/1"));
    assert!(
        saved
            .render_for_context("https://boundary.example/new", "owned-route-b")
            .is_some()
    );
    assert_eq!(std::fs::read(original).unwrap(), bytes);
}

#[test]
fn persisted_future_cold_check_rearms_instead_of_skipping_http_forever() {
    for future in [now() + 3600, u64::MAX] {
        let seed = json!({"version": 3, "profiles": {HOST: {
            "needs_tier2": true, "last_cold_check": future
        }}});
        let (mut state, original, bytes) = copied_state(&seed);
        assert!(matches!(
            state.route_for_class(HOST, "direct"),
            RouteDecision::RecheckCold
        ));
        state.record_probe_inconclusive(HOST);
        assert!(matches!(
            GhostState::load().route_for_class(HOST, "direct"),
            RouteDecision::SkipToSolve
        ));
        assert_eq!(std::fs::read(original).unwrap(), bytes);
    }
}

#[test]
fn persisted_future_wall_failure_does_not_create_an_immortal_cooldown() {
    for future in [now() + 3600, u64::MAX] {
        let seed = json!({"version": 3, "profiles": {HOST: {
            "wall_fail_streak": 2, "last_wall_fail": future
        }}});
        let (mut state, original, bytes) = copied_state(&seed);
        assert!(matches!(
            state.route_for_class(HOST, "direct"),
            RouteDecision::Cold
        ));
        assert_eq!(state.route_stats().3, 0);
        state.record_wall_failed(HOST);
        assert!(matches!(
            GhostState::load().route_for_class(HOST, "direct"),
            RouteDecision::SolveCooldown(_)
        ));
        assert_eq!(std::fs::read(original).unwrap(), bytes);
    }
}

#[test]
fn persisted_future_network_failure_is_not_recent_evidence() {
    let seed = json!({"version": 3, "profiles": {HOST: {"failures": [
        [now() + 3600, "network"], [u64::MAX, "network"],
        [now(), "network"], [now() - 601, "network"], [now(), "block"]
    ]}}});
    let (state, original, bytes) = copied_state(&seed);
    assert_eq!(state.recent_network_failures(HOST), 1);
    assert!(matches!(
        state.route_for_class(HOST, "direct"),
        RouteDecision::Cold
    ));
    assert_eq!(std::fs::read(original).unwrap(), bytes);
}

#[test]
fn persisted_future_cold_check_is_due_for_probe_with_stale_hosts() {
    let n = now();
    let seed = json!({"version": 3, "profiles": {
        HOST: {"needs_tier2": true, "last_cold_check": u64::MAX},
        "stale.example": {"needs_tier2": true, "last_cold_check": n - 30_000},
        "fresh.example": {"needs_tier2": true, "last_cold_check": n},
        "easy.example": {"last_cold_check": u64::MAX}
    }});
    let (state, original, bytes) = copied_state(&seed);
    assert_eq!(state.probe_candidates(20_000, 2), [HOST, "stale.example"]);
    assert_eq!(state.probe_candidates(20_000, 1), [HOST]);
    assert_eq!(std::fs::read(original).unwrap(), bytes);
}

#[test]
fn persisted_save_reports_io_failure_and_recovers() {
    const CHILD: &str = "DONSETCH_TEST_STATE_IO_CHILD";
    let root = crate::sandbox();
    let path = root.join("ghost-state.json");
    let tmp = root.join("ghost-state.json.tmp");
    let displaced = root.with_extension("displaced");
    if let Ok(fault) = std::env::var(CHILD) {
        let mut state = GhostState::load();
        assert_eq!(state.profiles[HOST].fetch_count, 7);
        match fault.as_str() {
            "mkdir" => {
                std::fs::rename(&root, &displaced).unwrap();
                std::fs::write(&root, "owned directory blocker").unwrap();
            }
            "open" => std::fs::create_dir(&tmp).unwrap(),
            "rename" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            #[cfg(unix)]
            "write" => {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // Limit only this owned child. Its stderr/stdout are pipes;
                // the real temporary state write fails with EFBIG.
                unsafe {
                    assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit), 0);
                    limit.rlim_cur = 0;
                    assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
                    libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                }
            }
            "healthy" => {}
            other => panic!("unknown owned I/O fault: {other}"),
        }
        state.record_fetch(HOST);
        assert_eq!(state.profiles[HOST].fetch_count, 8);
        println!("ACTUAL OBSERVATION AND SAVE REACHED");
        return;
    }

    let run_child = |fault| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ghost_state_boundaries::persisted_save_reports_io_failure_and_recovers",
                "--nocapture",
            ])
            .env(CHILD, fault)
            .output()
            .unwrap();
        assert!(output.status.success(), "owned child: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("ACTUAL OBSERVATION AND SAVE REACHED")
        );
        String::from_utf8(output.stderr).unwrap()
    };
    let faults = [
        "mkdir",
        "open",
        "rename",
        #[cfg(unix)]
        "write",
    ];
    let mut unreported = Vec::new();
    for fault in faults {
        let seed = json!({"version": 3, "profiles": {HOST: {
            "fetch_count": 7, "session_cookies": [{
                "name": "session", "value": "OWNED-SECRET-NEVER-LOG", "domain": HOST
            }]
        }}});
        let (_, original, bytes) = copied_state(&seed);
        let stderr = run_child(fault);
        assert!(!stderr.contains("OWNED-SECRET-NEVER-LOG"));
        if !stderr.contains("[ghost] cookie vault persist failed:") {
            unreported.push(fault);
        }
        match fault {
            "mkdir" => {
                assert_eq!(
                    std::fs::read(displaced.join("ghost-state.json")).unwrap(),
                    bytes
                );
                std::fs::remove_file(&root).unwrap();
                std::fs::rename(&displaced, &root).unwrap();
            }
            "rename" => {
                std::fs::remove_dir(&path).unwrap();
                std::fs::copy(&original, &path).unwrap();
            }
            _ => assert_eq!(std::fs::read(&path).unwrap(), bytes),
        }
        if tmp.is_dir() {
            std::fs::remove_dir(&tmp).unwrap();
        } else if tmp.exists() {
            std::fs::remove_file(&tmp).unwrap();
        }
        assert_eq!(std::fs::read(original).unwrap(), bytes);
        let stderr = run_child("healthy");
        assert!(!stderr.contains("cookie vault persist failed"), "{stderr}");
        assert_eq!(GhostState::load().profiles[HOST].fetch_count, 8);
    }
    assert!(
        unreported.is_empty(),
        "silent persistence failures: {unreported:?}"
    );
}
