//! Background probe-down recovery (v4 phase 0.2).
//!
//! Route memory learns "this host needs tier 2" from challenge
//! verdicts. Without a healer, that decision is one-way: a site
//! that relaxes its wall would keep paying the slow tier forever
//! (the RecheckCold path heals, but lazily, at the cost of one
//! user-visible fetch). The prober closes the loop: on a budget,
//! in the background, it re-probes stale walled hosts with a
//! cold tier-1 request and lets the wall DOWNGRADE when the
//! evidence says so. Probes never touch the shared cookie jar
//! (a probe answers "does a COLD client still get walled?") and
//! never read or write the revalidation cache (a cached page is
//! not evidence about the wall right now).
//!
//! Failure-class discipline: a probe that errors at the transport
//! level is INCONCLUSIVE. It records into the failure histogram
//! (visibility) and re-arms the probe cadence (no hammering), but
//! it never clears a wall and never sets one. A flaky network can
//! not poison route memory in either direction.
//!
//! Budgets: one scan every SCAN_INTERVAL_SECS, at most
//! PROBES_PER_PASS hosts per pass (stalest first), PROBE_GAP
//! between individual probes, PROBE_DEADLINE per probe. Kill
//! switches: DONSETCH_NO_ROUTE_PROBES, and route memory's own
//! NO_ROUTE_MEMORY / ROUTE_MEMORY_READONLY (probing without being
//! allowed to record would be pure waste).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::detect::walls::Verdict;
use crate::fetch::client::Fetcher;

use super::cache::{FailClass, GhostState};

/// How often the prober scans for stale walled hosts.
const SCAN_INTERVAL_SECS: u64 = 120;
/// Max probes per scan pass.
const PROBES_PER_PASS: usize = 3;
/// Minimum pause between individual probes (politeness + budget).
const PROBE_GAP: Duration = Duration::from_secs(15);
/// Hard deadline per probe: a slow host must never stall the loop.
const PROBE_DEADLINE: Duration = Duration::from_secs(8);
/// A walled host is a probe candidate once its last cold evidence
/// is this old (matches the RecheckCold cadence: probing just
/// moves that re-check off the user's request path).
const PROBE_STALE_SECS: u64 = 6 * 3600;

/// Test hooks for the live battery (seconds; the production
/// defaults above apply when unset). Never documented for users.
fn scan_interval() -> Duration {
    std::env::var("DONSETCH_PROBE_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_secs)
        .unwrap_or(SCAN_INTERVAL_SECS_DUR)
}

fn probe_stale_secs() -> u64 {
    std::env::var("DONSETCH_PROBE_STALE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(PROBE_STALE_SECS)
}

/// Probing on/off, composed of the dedicated switch and the route
/// memory switches (a read-only or disabled memory makes probing
/// pointless).
fn probes_enabled() -> bool {
    !crate::config::env_flag("DONSETCH_NO_ROUTE_PROBES")
        && !crate::config::env_flag("DONSETCH_NO_ROUTE_MEMORY")
        && !crate::config::env_flag("DONSETCH_ROUTE_MEMORY_READONLY")
}

/// Spawn the background prober. Daemon modes only: one-shot CLI
/// invocations exit before the first scan and never spawn this.
pub fn spawn(fetcher: Arc<Fetcher>, state: Arc<Mutex<GhostState>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // First scan after a settling delay: a just-booted daemon
        // has better things to do than probe.
        let every = scan_interval();
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if !probes_enabled() {
                continue;
            }
            let candidates: Vec<String> = {
                let st = state.lock().await;
                st.probe_candidates(probe_stale_secs(), PROBES_PER_PASS)
            };
            for host in candidates {
                if !probes_enabled() {
                    break;
                }
                probe_one(&fetcher, &state, &host).await;
                tokio::time::sleep(PROBE_GAP).await;
            }
        }
    })
}

const SCAN_INTERVAL_SECS_DUR: Duration = Duration::from_secs(SCAN_INTERVAL_SECS);

/// One cold probe + the route-memory update it implies.
async fn probe_one(fetcher: &Arc<Fetcher>, state: &Arc<Mutex<GhostState>>, host: &str) {
    // Probe the REAL origin: the scheme and port the host was
    // actually fetched on, not a guessed https upgrade.
    let (scheme, port) = {
        let st = state.lock().await;
        st.profile_origin(host)
    };
    let is_default_port = (scheme == "https" && port == 443) || (scheme == "http" && port == 80);
    let url = if is_default_port {
        format!("{scheme}://{host}/")
    } else {
        format!("{scheme}://{host}:{port}/")
    };
    let outcome = tokio::time::timeout(PROBE_DEADLINE, fetcher.fetch_cold_probe(&url)).await;
    let mut st = state.lock().await;
    st.note_probe();
    match outcome {
        Ok(Ok(out)) => match out.verdict {
            Verdict::Challenge(v) => {
                // Wall still stands: re-arm confidence at zero user
                // cost (the next fetch skips the doomed tier-1
                // attempt thanks to fresh last_cold_check).
                let vendor = format!("{v:?}").to_lowercase();
                st.record_cold_walled(host, Some(&vendor));
            }
            Verdict::ContentOk => {
                // PROBE-DOWN RECOVERY: the wall is gone for a cold
                // client. Route downgrades back to tier 1; future
                // fetches get fast again without anyone noticing.
                st.record_cold_ok(host);
            }
            // 404/paywall/auth on the front page says nothing about
            // the wall on real paths: inconclusive, re-arm cadence.
            _ => st.record_probe_inconclusive(host),
        },
        Ok(Err(e)) => {
            let class = match &e {
                crate::error::FetchError::Timeout | crate::error::FetchError::Io(_) => {
                    FailClass::Network
                }
                crate::error::FetchError::Tls(_) => FailClass::Tls,
                _ => FailClass::Other,
            };
            st.record_failure(host, class);
            st.record_probe_inconclusive(host);
        }
        Err(_elapsed) => {
            st.record_failure(host, FailClass::Network);
            st.record_probe_inconclusive(host);
        }
    }
}
