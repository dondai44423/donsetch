//! The lane ledger: what each (source, lane) may spend, and how long it rests.
//!
//! Pacing used to live in the process. Every CLI call learned the same lesson
//! again — brave 429s, bing stalling, crates.io rate-limiting — and paid for it
//! with a lost lane or a two-second wait. The ledger is one file, keyed per
//! **(source, lane)** rather than per host (`bing` the SERP and `bing-news` the
//! RSS are different products with different limits), shared by every process
//! that shares the cache dir.
//!
//! Each lane carries a token bucket, a `Retry-After` when the source sent one,
//! and an exponential rest behind a circuit breaker. A lane that answers again
//! walks back to normal. The tenth query is as healthy as the first.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const LEDGER_VERSION: u32 = 1;
/// First rest after a misbehaving lane.
const REST_BASE: u64 = 45;
/// The cap on our own failure backoff; a published Retry-After may be longer.
const REST_CAP: u64 = 600;
/// The fallback when a throttled source publishes no Retry-After.
const THROTTLE_BASE: u64 = 60;

/// Whether a lane may be asked right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    /// Seconds until the lane's own pace allows the next request (the token
    /// bucket). Not a wall and not a rest: a caller may wait it out and ask.
    Paced(u64),
    /// Seconds until the lane may be asked again after a refusal or a run of
    /// transport failures.
    Rested(u64),
}

/// What a lane's last request amounted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The source answered, with items or with none.
    Answered,
    /// A transient transport hiccup (net error, timeout). One is not a wall.
    Failed,
    /// An explicit refusal: a status page, a block, a captcha. That is a wall,
    /// and the lane rests before it is asked again.
    Blocked,
    /// The source said it was being asked too often.
    Throttled,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
enum RestCause {
    Backoff,
    // Older ledgers did not distinguish a refusal from a failure backoff.
    // Preserve an active unknown wait rather than accidentally erasing a 429.
    #[default]
    Legacy,
    Refusal,
    RetryAfter,
}

/// One lane's standing. Times are wall-clock, so a rest survives a restart.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Lane {
    /// Seconds left before the next request may go out.
    #[serde(default)]
    rest_until: u64,
    /// The window the current rest was set from (doubles per failure).
    #[serde(default)]
    rest_secs: u64,
    #[serde(default)]
    rest_cause: RestCause,
    /// Consecutive transport failures; an answer resets it.
    #[serde(default)]
    fails: u32,
    /// Token bucket, refilled by the lane's rate.
    #[serde(default)]
    tokens: f64,
    #[serde(default)]
    refilled: u64,
    #[serde(default)]
    last_ok: u64,
    /// Receipt ordering across writers; old ledgers start without an order.
    #[serde(default)]
    updated_ms: u64,
}

impl Lane {
    fn for_key(key: &str) -> Self {
        let (_, burst) = policy(source_of(key));
        Self {
            rest_until: 0,
            rest_secs: 0,
            rest_cause: RestCause::Backoff,
            fails: 0,
            tokens: burst,
            refilled: now(),
            last_ok: 0,
            updated_ms: 0,
        }
    }
}

impl Default for Lane {
    fn default() -> Self {
        Self::for_key("")
    }
}

/// Requests per second and burst depth for a source class.
fn policy(source: &str) -> (f64, f64) {
    match source {
        // A search endpoint answers one query at a time; two in flight is the
        // most a single search ever asks, and the pace keeps repeated CLI calls
        // from looking like a scraper.
        "bing" | "ddg" | "ddg_lite" | "ddg_html" | "yahoo" | "brave" | "mojeek" => (0.5, 2.0),
        "google" => (0.1, 1.0),
        "bing-news" => (0.25, 1.0),
        "feed" => (0.5, 3.0),
        "gdelt" => (0.05, 1.0),
        "registry" => (0.2, 2.0),
        _ => (0.5, 2.0),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn prune_expired_origins(lanes: &mut BTreeMap<String, Lane>) {
    let t = now();
    lanes.retain(|key, lane| !key.starts_with("origin:") || lane.rest_until > t);
}

/// The ledger key: the source and the lane inside it.
pub fn key(source: &str, lane: &str) -> String {
    format!("{source}|{lane}")
}

/// The source half of a key (for policy lookup).
fn source_of(key: &str) -> &str {
    key.split('|').next().unwrap_or(key)
}

/// What a lane's report status means to the ledger.
///
/// A source that answered with nothing is not a failure — it is an empty
/// answer, and resting it would hide a working lane. Only transport trouble
/// and throttling rest a lane.
pub fn classify(status: &str) -> Option<Outcome> {
    let head = status.split(':').next().unwrap_or(status);
    match head {
        "ok" | "cached" | "thin" | "no-results" | "empty" | "empty-parse" | "off-query" => {
            Some(Outcome::Answered)
        }
        "rate-limited" | "throttled" | "429" => Some(Outcome::Throttled),
        "http-status" | "blocked" | "captcha" | "forbidden" | "auth-fail" => Some(Outcome::Blocked),
        // The scheduler cut the lane, or the plan never asked it: neither is
        // evidence about the source.
        "rested" | "not-asked" | "pacing-timeout" | "budget-skipped" | "skipped" | "late" => None,
        _ => Some(Outcome::Failed),
    }
}

/// The seconds a throttling source asked for, when it said.
pub fn retry_after(status: &str) -> Option<u64> {
    let (head, tail) = status.split_once(':')?;
    if !matches!(head, "rate-limited" | "throttled") {
        return None;
    }
    tail.trim()
        .trim_end_matches('s')
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
}

/// The ledger itself. One instance per process, saved after the search.
pub struct Ledger {
    lanes: BTreeMap<String, Lane>,
    dirty: bool,
}

impl Ledger {
    pub fn load() -> Self {
        let Some(path) = path() else {
            return Self {
                lanes: BTreeMap::new(),
                dirty: false,
            };
        };
        let disk = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Disk>(&raw).ok());
        let mut lanes = match disk {
            Some(disk) if disk.version == LEDGER_VERSION => disk.lanes,
            // A file from another version is not trusted to mean the same
            // thing: drop it rather than rest healthy lanes on a guess.
            _ => BTreeMap::new(),
        };
        prune_expired_origins(&mut lanes);
        Self {
            lanes,
            dirty: false,
        }
    }

    /// May this lane be asked? Refills its bucket on the way.
    pub fn ready(&mut self, key: &str) -> State {
        let (rate, burst) = policy(source_of(key));
        let lane = self
            .lanes
            .entry(key.to_string())
            .or_insert_with(|| Lane::for_key(key));
        let t = now();
        let elapsed = t.saturating_sub(lane.refilled) as f64;
        if elapsed > 0.0 {
            lane.tokens = (lane.tokens + elapsed * rate).min(burst);
            lane.refilled = t;
        }
        if lane.rest_until > t {
            return State::Rested(lane.rest_until - t);
        }
        if lane.tokens < 1.0 {
            let wait = ((1.0 - lane.tokens) / rate).ceil().max(1.0) as u64;
            return State::Paced(wait);
        }
        State::Ready
    }

    /// A known origin's rest, without creating a pacing bucket for every URL.
    pub(crate) fn origin_rest(&mut self, origin: &str) -> Option<u64> {
        let key = format!("origin:{origin}|http");
        let until = self.lanes.get(&key)?.rest_until;
        let left = until.saturating_sub(now());
        if left > 0 {
            return Some(left);
        }
        self.lanes.remove(&key);
        self.dirty = true;
        None
    }

    pub(crate) fn record_origin(&mut self, origin: &str, outcome: Outcome, retry: Option<u64>) {
        prune_expired_origins(&mut self.lanes);
        self.record(&format!("origin:{origin}|http"), outcome, retry);
    }

    /// Record what the lane's request amounted to.
    pub fn record(&mut self, key: &str, outcome: Outcome, retry: Option<u64>) {
        let t = now();
        let (rate, burst) = policy(source_of(key));
        let lane = self
            .lanes
            .entry(key.to_string())
            .or_insert_with(|| Lane::for_key(key));
        let receipt_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        lane.updated_ms = receipt_ms.max(lane.updated_ms.saturating_add(1));
        // A request was spent, whatever it returned.
        let elapsed = t.saturating_sub(lane.refilled) as f64;
        lane.tokens = (lane.tokens + elapsed * rate).min(burst) - 1.0;
        lane.refilled = t;
        let published = (lane.rest_cause != RestCause::Backoff && lane.rest_until > t).then_some((
            lane.rest_until,
            lane.rest_secs,
            lane.rest_cause,
        ));
        match outcome {
            Outcome::Answered => {
                lane.fails = 0;
                if lane.rest_cause == RestCause::Backoff || lane.rest_until <= t {
                    lane.rest_secs = 0;
                    lane.rest_until = 0;
                    lane.rest_cause = RestCause::Backoff;
                }
                lane.last_ok = t;
            }
            Outcome::Throttled => {
                lane.fails = lane.fails.saturating_add(1);
                let wait = retry.filter(|secs| *secs > 0).unwrap_or(THROTTLE_BASE);
                lane.rest_secs = wait;
                lane.rest_until = t.saturating_add(wait);
                lane.rest_cause = RestCause::RetryAfter;
            }
            Outcome::Failed => {
                // A hiccup, not a verdict: a net blip gets one more chance
                // before the lane rests.
                lane.fails = lane.fails.saturating_add(1);
                if lane.fails >= 2 {
                    let shift = lane.fails.saturating_sub(2).min(5);
                    let wait = (REST_BASE << shift).min(REST_CAP);
                    lane.rest_secs = wait;
                    lane.rest_until = t.saturating_add(wait);
                    lane.rest_cause = RestCause::Backoff;
                }
            }
            Outcome::Blocked => {
                // The source refused: rest before asking again, doubling.
                lane.fails = lane.fails.saturating_add(1);
                let shift = lane.fails.saturating_sub(1).min(5);
                let wait = (REST_BASE << shift).min(REST_CAP);
                lane.rest_secs = wait;
                lane.rest_until = t.saturating_add(wait);
                lane.rest_cause = RestCause::Refusal;
            }
        }
        if let Some((until, secs, cause)) = published
            && (until > lane.rest_until || (until == lane.rest_until && lane.rest_cause < cause))
        {
            lane.rest_until = until;
            lane.rest_secs = secs;
            lane.rest_cause = cause;
        }
        self.dirty = true;
    }

    /// Merge concurrent receipts: preserve explicit refusals, let newer
    /// successes clear failure backoff, and compare refilled token balances.
    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = path() else {
            self.dirty = false;
            return;
        };
        let Some(_guard) = WriteLock::acquire() else {
            return;
        };
        let mut merged = match std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Disk>(&raw).ok())
        {
            Some(disk) if disk.version == LEDGER_VERSION => disk.lanes,
            _ => BTreeMap::new(),
        };
        for (key, mine) in &self.lanes {
            match merged.get_mut(key) {
                None => {
                    merged.insert(key.clone(), mine.clone());
                }
                Some(theirs) => {
                    *theirs = merge_lane(key, mine, theirs);
                }
            }
        }
        // Merging an older writer must not resurrect expired per-page origins.
        prune_expired_origins(&mut merged);
        let disk = Disk {
            version: LEDGER_VERSION,
            lanes: merged,
        };
        let Ok(json) = serde_json::to_string(&disk) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if crate::config::write_private(&tmp, json.as_bytes()).is_ok()
            && std::fs::rename(tmp, path).is_ok()
        {
            self.dirty = false;
        }
    }

    /// Test and diagnostic view: the lane's standing.
    #[cfg(test)]
    pub fn lane(&self, key: &str) -> Option<&Lane> {
        self.lanes.get(key)
    }
}

fn merge_lane(key: &str, mine: &Lane, theirs: &Lane) -> Lane {
    let t = now();
    let mut merged = if mine.updated_ms > theirs.updated_ms {
        mine.clone()
    } else {
        theirs.clone()
    };
    if mine.updated_ms == theirs.updated_ms {
        // Concurrent or legacy receipts have no reliable ordering.
        merged.fails = mine.fails.max(theirs.fails);
        if mine.rest_until > merged.rest_until {
            merged.rest_until = mine.rest_until;
            merged.rest_secs = mine.rest_secs;
            merged.rest_cause = mine.rest_cause;
        }
    }
    // A late success may clear our own backoff, never an origin's refusal.
    for receipt in [mine, theirs] {
        if receipt.rest_cause != RestCause::Backoff
            && receipt.rest_until > t
            && (receipt.rest_until > merged.rest_until
                || (receipt.rest_until == merged.rest_until
                    && receipt.rest_cause > merged.rest_cause))
        {
            merged.rest_until = receipt.rest_until;
            merged.rest_secs = receipt.rest_secs;
            merged.rest_cause = receipt.rest_cause;
        }
    }
    merged.last_ok = mine.last_ok.max(theirs.last_ok);
    // Token debts from different seconds must first be refilled to the same
    // instant. Comparing raw balances resurrected old debt or old burst room.
    let (rate, burst) = policy(source_of(key));
    let available =
        |lane: &Lane| (lane.tokens + t.saturating_sub(lane.refilled) as f64 * rate).min(burst);
    merged.tokens = available(mine).min(available(theirs));
    merged.refilled = t;
    merged
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Disk {
    version: u32,
    #[serde(default)]
    lanes: BTreeMap<String, Lane>,
}

/// Seed a cold measurement with unexpired refusals, without carrying answers,
/// trust, token consumption or transport-failure backoff into it.
pub(crate) fn carry_refusals(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    fn read(path: &std::path::Path) -> Result<Disk, String> {
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice::<Disk>(&raw)
                .map_err(|error| format!("cannot read refusal ledger {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Disk {
                version: LEDGER_VERSION,
                lanes: BTreeMap::new(),
            }),
            Err(error) => Err(error.to_string()),
        }
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _guard = WriteLock::at_wait(
        &to.with_extension("lock"),
        std::time::Duration::from_millis(500),
    )
    .ok_or_else(|| format!("cannot lock refusal ledger {}", to.display()))?;
    let source = read(from)?;
    let mut target = read(to)?;
    if source.version != LEDGER_VERSION || target.version != LEDGER_VERSION {
        return Err("cannot carry refusals from an unknown ledger version".into());
    }
    let t = now();
    for (key, lane) in source.lanes {
        if lane.rest_until <= t || lane.rest_cause == RestCause::Backoff {
            continue;
        }
        let entry = target
            .lanes
            .entry(key.clone())
            .or_insert_with(|| Lane::for_key(&key));
        if lane.rest_until > entry.rest_until {
            entry.rest_until = lane.rest_until;
            entry.rest_secs = lane.rest_secs;
            entry.rest_cause = lane.rest_cause;
        }
    }
    prune_expired_origins(&mut target.lanes);
    let json = serde_json::to_vec(&target).map_err(|error| error.to_string())?;
    let tmp = to.with_extension(format!("{}.tmp", std::process::id()));
    crate::config::write_private(&tmp, &json).map_err(|error| error.to_string())?;
    std::fs::rename(tmp, to).map_err(|error| error.to_string())
}

/// Preserve a published wait reported by an older measured binary whose disk
/// format did not distinguish refusals from transport backoff.
pub(crate) fn carry_report_refusals(
    path: &std::path::Path,
    reports: &[super::EngineReport],
) -> Result<(), String> {
    let disk = match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice::<Disk>(&raw).map_err(|error| error.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Disk {
            version: LEDGER_VERSION,
            lanes: BTreeMap::new(),
        },
        Err(error) => return Err(error.to_string()),
    };
    if disk.version != LEDGER_VERSION {
        return Err("unknown measured ledger version".into());
    }
    let mut ledger = Ledger {
        lanes: disk.lanes,
        dirty: false,
    };
    for report in reports {
        let Some(outcome @ (Outcome::Blocked | Outcome::Throttled)) = classify(&report.status)
        else {
            continue;
        };
        let key = key(
            super::lane_source(&report.engine),
            super::lane_of(&report.engine),
        );
        ledger.record(&key, outcome, retry_after(&report.status));
    }
    let json = serde_json::to_vec(&Disk {
        version: LEDGER_VERSION,
        lanes: ledger.lanes,
    })
    .map_err(|error| error.to_string())?;
    crate::config::write_private(path, &json).map_err(|error| error.to_string())
}

fn path() -> Option<std::path::PathBuf> {
    if crate::config::cfg().state.no_disk_state {
        return None;
    }
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("search-ledger.json"))
}

/// Cross-process serialization without blocking the search on another writer.
/// The kernel releases this lock on crash; a missed write remains dirty.
struct WriteLock {
    #[allow(dead_code)] // RAII: held for the lock, never read
    file: std::fs::File,
}

impl WriteLock {
    fn at_wait(path: &std::path::Path, budget: std::time::Duration) -> Option<Self> {
        let until = std::time::Instant::now() + budget;
        loop {
            if let Some(guard) = Self::at(path) {
                return Some(guard);
            }
            if std::time::Instant::now() >= until {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn acquire() -> Option<Self> {
        Self::at(&crate::paths::cache_dir().join("search-ledger.lock"))
    }

    fn at(path: &std::path::Path) -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok()?;
        file.try_lock().ok()?;
        Some(Self { file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_measurements_carry_refusals_without_carrying_failure_or_trust_history() {
        let dir =
            std::env::temp_dir().join(format!("donsetch-refusal-carry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        let target = dir.join("target.json");
        let t = now();
        let lanes = [
            ("quota", RestCause::RetryAfter, t + 900),
            ("block", RestCause::Refusal, t + 45),
            ("legacy", RestCause::Legacy, t + 45),
            ("expired", RestCause::RetryAfter, t.saturating_sub(1)),
            ("timeout", RestCause::Backoff, t + 45),
        ]
        .into_iter()
        .map(|(key, cause, until)| {
            let mut lane = Lane::for_key(key);
            lane.rest_cause = cause;
            lane.rest_until = until;
            lane.rest_secs = 900;
            lane.tokens = -10.0;
            lane.fails = 8;
            (key.to_string(), lane)
        })
        .collect();
        std::fs::write(
            &source,
            serde_json::to_vec(&Disk {
                version: LEDGER_VERSION,
                lanes,
            })
            .unwrap(),
        )
        .unwrap();
        carry_refusals(&source, &target).unwrap();
        let copied: Disk = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(copied.lanes.len(), 3);
        assert_eq!(copied.lanes["quota"].rest_until, t + 900);
        assert_eq!(copied.lanes["quota"].tokens, 2.0);
        assert_eq!(copied.lanes["quota"].fails, 0);
        assert!(!copied.lanes.contains_key("expired"));
        assert!(!copied.lanes.contains_key("timeout"));
        let workers: Vec<_> = (0..4)
            .map(|i| {
                let file = dir.join(format!("source-{i}.json"));
                let mut lane = Lane::for_key("new");
                lane.rest_cause = RestCause::RetryAfter;
                lane.rest_until = t + 900;
                lane.rest_secs = 900;
                std::fs::write(
                    &file,
                    serde_json::to_vec(&Disk {
                        version: LEDGER_VERSION,
                        lanes: BTreeMap::from([(format!("new-{i}"), lane)]),
                    })
                    .unwrap(),
                )
                .unwrap();
                let target = target.clone();
                std::thread::spawn(move || carry_refusals(&file, &target).unwrap())
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let merged: Disk = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(
            merged.lanes.len(),
            7,
            "concurrent cold groups retain every source's refusal"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_lane_rests_and_the_next_failure_rests_it_longer() {
        let mut l = Ledger {
            lanes: BTreeMap::new(),
            dirty: false,
        };
        let k = key("brave", "serp");
        assert_eq!(l.ready(&k), State::Ready);
        l.record(&k, Outcome::Blocked, None);
        match l.ready(&k) {
            State::Rested(secs) => assert!(secs > 30, "{secs}"),
            State::Paced(wait) => panic!("a blocked lane must rest, not pace {wait}s"),
            State::Ready => panic!("a blocked lane must rest"),
        }
        let first = l.lane(&k).unwrap().rest_secs;
        l.record(&k, Outcome::Blocked, None);
        assert!(l.lane(&k).unwrap().rest_secs > first);
        // A transient failure does not rest on the first occurrence.
        let soft = key("ddg", "serp");
        l.record(&soft, Outcome::Failed, None);
        assert_eq!(l.ready(&soft), State::Ready, "one hiccup is not a wall");
    }

    #[test]
    fn an_answer_walks_the_lane_back_to_normal() {
        let mut l = Ledger {
            lanes: BTreeMap::new(),
            dirty: false,
        };
        let k = key("bing", "serp");
        // A successful request clears transport-failure backoff. Explicit
        // refusal windows are preserved even if an in-flight request succeeds.
        l.record(&k, Outcome::Failed, None);
        l.record(&k, Outcome::Failed, None);
        assert!(l.lane(&k).unwrap().fails >= 2);
        l.record(&k, Outcome::Answered, None);
        let lane = l.lane(&k).unwrap();
        assert_eq!(lane.fails, 0);
        assert_eq!(lane.rest_until, 0);
    }

    #[test]
    fn a_throttle_honors_retry_after() {
        let mut l = Ledger {
            lanes: BTreeMap::new(),
            dirty: false,
        };
        let k = key("crates", "registry");
        for wait in [1, 120, 900, u64::MAX] {
            let before = now();
            l.record(&k, Outcome::Throttled, Some(wait));
            let after = now();
            let lane = l.lane(&k).unwrap();
            assert_eq!(lane.rest_secs, wait);
            assert!(
                lane.rest_until >= before.saturating_add(wait)
                    && lane.rest_until <= after.saturating_add(wait)
            );
            if wait >= 120 {
                for later in [Outcome::Answered, Outcome::Failed, Outcome::Blocked] {
                    l.record(&k, later, None);
                }
                assert!(
                    matches!(l.ready(&k), State::Rested(_)),
                    "a 429 must rest the lane"
                );
            }
        }
    }

    #[test]
    fn the_source_and_the_lane_are_one_key() {
        let bing = key("bing", "serp");
        let news = key("bing-news", "wire");
        assert_ne!(bing, news);
        let mut l = Ledger {
            lanes: BTreeMap::new(),
            dirty: false,
        };
        l.record(&bing, Outcome::Blocked, None);
        assert!(matches!(l.ready(&bing), State::Rested(_)));
        assert_eq!(
            l.ready(&news),
            State::Ready,
            "bing-news has its own standing"
        );
    }

    #[test]
    fn transport_trouble_is_not_an_empty_answer() {
        assert_eq!(classify("ok"), Some(Outcome::Answered));
        assert_eq!(classify("no-results"), Some(Outcome::Answered));
        assert_eq!(classify("off-query"), Some(Outcome::Answered));
        assert_eq!(classify("http-status"), Some(Outcome::Blocked));
        assert_eq!(classify("blocked:cloudflare"), Some(Outcome::Blocked));
        assert_eq!(classify("net"), Some(Outcome::Failed));
        assert_eq!(classify("late"), None, "a slow lane is not a walled one");
        assert_eq!(classify("rate-limited:3600"), Some(Outcome::Throttled));
        assert_eq!(classify("rested:120"), None);
        assert_eq!(classify("not-asked"), None);
        assert_eq!(retry_after("rate-limited:3600"), Some(3600));
        assert_eq!(retry_after("rate-limited:3600s"), Some(3600));
        assert_eq!(retry_after("429"), None);
    }

    #[test]
    fn what_two_processes_learned_is_merged_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("donsetch-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &dir) };

        let mut a = Ledger::load();
        a.record(&key("brave", "serp"), Outcome::Blocked, None);
        a.save_if_dirty();
        assert!(!a.dirty);
        let expired = "origin:https://expired.example|http";
        let mut old_disk: Disk =
            serde_json::from_slice(&std::fs::read(path().unwrap()).unwrap()).unwrap();
        let old_origin = Lane {
            rest_until: now().saturating_sub(1),
            rest_cause: RestCause::RetryAfter,
            ..Lane::default()
        };
        old_disk.lanes.insert(expired.into(), old_origin);
        std::fs::write(path().unwrap(), serde_json::to_vec(&old_disk).unwrap()).unwrap();
        assert!(Ledger::load().lane(expired).is_none());
        let held = WriteLock::acquire().unwrap();
        a.record(&key("yahoo", "serp"), Outcome::Throttled, Some(900));
        let start = std::time::Instant::now();
        a.save_if_dirty();
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert!(
            a.dirty,
            "a busy writer must not lose a refusal or stall search"
        );
        drop(held);
        a.save_if_dirty();
        assert!(!a.dirty);
        let saved: Disk = serde_json::from_slice(&std::fs::read(path().unwrap()).unwrap()).unwrap();
        assert!(
            !saved.lanes.contains_key(expired),
            "a disk merge must prune expired origins"
        );
        // A failed write keeps the outstanding refusal dirty so a later call
        // retries persistence instead of silently forgetting its published wait.
        a.record(&key("google-cse", "serp"), Outcome::Throttled, Some(900));
        let tmp = path().unwrap().with_extension("tmp");
        std::fs::create_dir(&tmp).unwrap();
        a.save_if_dirty();
        assert!(a.dirty);
        std::fs::remove_dir(&tmp).unwrap();
        a.save_if_dirty();
        assert!(!a.dirty);
        assert!(matches!(
            Ledger::load().ready(&key("google-cse", "serp")),
            State::Rested(890..=900)
        ));

        let mut b = Ledger::load();
        // The second process saw a longer rest from the first; it must not
        // shorten it when it writes its own answer for another lane.
        b.record(&key("bing", "serp"), Outcome::Answered, None);
        b.record(&key("brave", "serp"), Outcome::Answered, None);
        assert!(
            matches!(b.ready(&key("brave", "serp")), State::Rested(_)),
            "an in-flight success cannot cancel a refusal"
        );
        b.save_if_dirty();

        let c = Ledger::load();
        assert!(matches!(c.lane(&key("brave", "serp")), Some(l) if l.rest_until > 0));
        assert!(c.lane(&key("bing", "serp")).is_some());

        let recovering = key("recovering", "serp");
        let mut failed = Ledger::load();
        failed.record(&recovering, Outcome::Failed, None);
        failed.record(&recovering, Outcome::Failed, None);
        failed.save_if_dirty();
        let mut stale = Ledger::load();
        let mut healthy = Ledger::load();
        healthy.record(&recovering, Outcome::Answered, None);
        healthy.save_if_dirty();
        // An unrelated write by an old process cannot restore the cleared
        // failure streak or backoff after a newer successful request.
        stale.record(&key("other", "serp"), Outcome::Answered, None);
        stale.save_if_dirty();
        let healed = Ledger::load();
        assert_eq!(healed.lane(&recovering).unwrap().fails, 0);
        assert_eq!(healed.lane(&recovering).unwrap().rest_until, 0);

        let old = Lane {
            tokens: -10.0,
            refilled: now().saturating_sub(100),
            ..Lane::default()
        };
        let current = Lane {
            tokens: 1.0,
            updated_ms: 1,
            ..Lane::default()
        };
        assert!(
            (1.0..=2.0).contains(&merge_lane("bing|serp", &current, &old).tokens),
            "a stale depleted balance must be refilled before merging"
        );
        unsafe { std::env::remove_var("DONSETCH_CACHE_DIR") };
    }
}
