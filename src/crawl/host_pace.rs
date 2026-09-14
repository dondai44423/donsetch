//! Cross-process host politeness (v4 F2).
//!
//! The in-process governor paces one crawl. Two donsetch processes
//! (daemon + CLI, or parallel test jobs) each hold their own Instant
//! clocks and can stampede the same host. This store is a tiny
//! per-host last-request file so processes share a floor gap.
//!
//! Rules:
//! - One immutable-ish JSON file per host under `<cache>/host-pace/`.
//! - Atomic write (tmp + rename). Corrupt/unreadable = ignore.
//! - Cap 2k files, TTL 7d (prune on write).
//! - Kill: `DONSETCH_NO_HOST_PACE_FILE` / `fetch.host_pace_file=false`.
//! - Never blocks longer than the remaining gap; never invents delay
//!   beyond the configured floor (law 6: stealth through truth).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PACE_VERSION: u32 = 1;
const PACE_HOST_CAP: usize = 2_000;
const PACE_TTL: Duration = Duration::from_secs(7 * 86_400);
/// Floor gap between two processes hitting the same host. The
/// in-process governor already spaces requests; this only stops a
/// second process from landing inside that window.
const DEFAULT_GAP: Duration = Duration::from_millis(250);

#[derive(serde::Serialize, serde::Deserialize)]
struct PaceFile {
    #[serde(default = "pace_version")]
    version: u32,
    host: String,
    /// Unix millis of the last request we stamped.
    last_ms: u64,
}

fn pace_version() -> u32 {
    PACE_VERSION
}

fn enabled() -> bool {
    crate::config::cfg().fetch.host_pace_file && !crate::config::cfg().state.no_disk_state
}

fn pace_dir() -> PathBuf {
    crate::paths::cache_dir().join("host-pace")
}

fn host_key(host: &str) -> String {
    // Filename-safe: hosts can contain dots, ports, IDN punycode.
    // A short blake-style hash keeps the name stable and short.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in host.as_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_pace(path: &Path) -> Option<PaceFile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_pace(path: &Path, host: &str, last_ms: u64) {
    let tmp = path.with_extension("tmp");
    let row = PaceFile {
        version: PACE_VERSION,
        host: host.to_string(),
        last_ms,
    };
    let Ok(json) = serde_json::to_vec(&row) else {
        return;
    };
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Drop stale files past TTL and keep the store under the cap.
/// Best-effort: a prune failure never blocks the crawl.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = now_ms();
    let ttl_ms = PACE_TTL.as_millis() as u64;
    let mut alive: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|x| x.to_str());
        // Failed atomic writes leave *.tmp behind; never treat them
        // as live rows, always drop them.
        if ext == Some("tmp") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if ext != Some("json") {
            continue;
        }
        let last = read_pace(&path).map(|p| p.last_ms).unwrap_or(0);
        if now.saturating_sub(last) < ttl_ms {
            alive.push((last, path));
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    if alive.len() <= PACE_HOST_CAP {
        return;
    }
    alive.sort_unstable_by_key(|(last, _)| *last);
    for (_, path) in alive.iter().take(alive.len() - PACE_HOST_CAP) {
        let _ = std::fs::remove_file(path);
    }
}

/// Wait until this host's cross-process floor gap has elapsed, then
/// stamp the request. Returns the time actually waited (for receipts).
/// Kill switch or missing dir = zero wait.
pub async fn wait_and_stamp(host: &str) -> Duration {
    if host.is_empty() || !enabled() {
        return Duration::ZERO;
    }
    let dir = pace_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return Duration::ZERO;
    }
    let path = dir.join(host_key(host));
    let gap = DEFAULT_GAP;
    let mut waited = Duration::ZERO;
    // One read, at most one sleep, then stamp. A concurrent writer
    // may land inside the window; the floor is best-effort, not a
    // distributed lock.
    if let Some(row) = read_pace(&path)
        && row.host == host
    {
        let last = row.last_ms;
        let now = now_ms();
        let due = last.saturating_add(gap.as_millis() as u64);
        if due > now {
            let remain = Duration::from_millis(due - now);
            // Hard cap: never sleep more than the gap itself (hostile
            // clock or corrupt last_ms cannot park the crawler).
            let remain = remain.min(gap);
            let t0 = Instant::now();
            tokio::time::sleep(remain).await;
            waited = t0.elapsed();
        }
    }
    write_pace(&path, host, now_ms());
    // Prune is directory-wide (read_dir + a stat per file); running
    // it on every stamped request — i.e. every fetched URL — is
    // wasteful on the crawl hot path. Throttle it to at most once per
    // PRUNE_INTERVAL per process; the TTL/cap only need lazy upkeep,
    // and the CAS makes exactly one concurrent caller do the work.
    if prune_due() {
        prune(&dir);
    }
    waited
}

/// At most one prune per process per PRUNE_INTERVAL. Returns true for
/// the single caller that wins the interval.
fn prune_due() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    const PRUNE_INTERVAL_MS: u64 = 60_000;
    static LAST_PRUNE_MS: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    let last = LAST_PRUNE_MS.load(Ordering::Relaxed);
    now.saturating_sub(last) >= PRUNE_INTERVAL_MS
        && LAST_PRUNE_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolate_cache(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("donsetch-pace-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // nextest process-per-test: env set before first cfg().
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
            std::env::set_var("DONSETCH_NO_CONFIG_FILE", "1");
        }
        dir
    }

    #[tokio::test]
    async fn stamp_then_second_call_waits_floor_gap() {
        let _dir = isolate_cache("stamp");
        let host = "pace.example";
        let w1 = wait_and_stamp(host).await;
        assert_eq!(w1, Duration::ZERO, "first stamp never waits");
        let t0 = Instant::now();
        let w2 = wait_and_stamp(host).await;
        let elapsed = t0.elapsed();
        // Second call must observe the floor (allowing a little
        // scheduling slop under the 250ms gap).
        assert!(
            elapsed >= Duration::from_millis(100),
            "second stamp waited only {elapsed:?} (w2={w2:?})"
        );
    }

    #[tokio::test]
    async fn kill_switch_skips_file_and_wait() {
        let dir = isolate_cache("kill");
        unsafe {
            std::env::set_var("DONSETCH_NO_HOST_PACE_FILE", "1");
        }
        let w = wait_and_stamp("kill.example").await;
        assert_eq!(w, Duration::ZERO);
        assert!(
            !dir.join("host-pace")
                .join(host_key("kill.example"))
                .exists(),
            "kill switch must not write"
        );
        unsafe {
            std::env::remove_var("DONSETCH_NO_HOST_PACE_FILE");
        }
    }

    #[tokio::test]
    async fn corrupt_file_is_ignored() {
        let _dir = isolate_cache("corrupt");
        let dir = pace_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(host_key("corrupt.example"));
        std::fs::write(&path, b"{not json").unwrap();
        let w = wait_and_stamp("corrupt.example").await;
        assert_eq!(w, Duration::ZERO, "corrupt file = ignore, never block");
        // Stamp still lands so the next call has a real clock.
        assert!(path.exists());
    }

    #[test]
    fn host_keys_are_stable_and_distinct() {
        assert_eq!(host_key("a.example"), host_key("a.example"));
        assert_ne!(host_key("a.example"), host_key("b.example"));
    }

    #[tokio::test]
    async fn rows_survive_and_prune_drops_stale() {
        let _dir = isolate_cache("prune");
        let dir = pace_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let old = PaceFile {
            version: 1,
            host: "old.example".into(),
            last_ms: now_ms().saturating_sub(PACE_TTL.as_millis() as u64 + 1000),
        };
        let path = dir.join(host_key("old.example"));
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        wait_and_stamp("fresh.example").await;
        assert!(
            !path.exists(),
            "stale host-pace file must be pruned on write"
        );
    }

    // prune is directory-wide and used to run on every stamped
    // request (the crawl hot path); prune_due throttles it to at most
    // once per interval per process. The first call wins, an
    // immediate second is skipped. (nextest process-per-test gives a
    // fresh LAST_PRUNE static, so the first call always wins here.)
    #[test]
    fn prune_is_throttled_after_the_first_call() {
        assert!(prune_due(), "first prune in a process must run");
        assert!(
            !prune_due(),
            "an immediate second prune must be skipped by the throttle"
        );
    }

    // cfg() freezes at first use: keep each test's env isolated via
    // nextest process-per-test.
}
