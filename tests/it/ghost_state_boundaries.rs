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
