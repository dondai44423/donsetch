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
/// A lane is never rested longer than this, however badly it failed.
const REST_CAP: u64 = 600;
/// A throttled lane rests at least this long, even without `Retry-After`.
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

/// One lane's standing. Times are wall-clock, so a rest survives a restart.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Lane {
    /// Seconds left before the next request may go out.
    #[serde(default)]
    rest_until: u64,
    /// The window the current rest was set from (doubles per failure).
    #[serde(default)]
    rest_secs: u64,
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
}

impl Lane {
    fn for_key(key: &str) -> Self {
        let (_, burst) = policy(source_of(key));
        Self {
            rest_until: 0,
            rest_secs: 0,
            fails: 0,
            tokens: burst,
            refilled: now(),
            last_ok: 0,
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
        "rested" | "not-asked" | "pacing-timeout" | "skipped" | "late" => None,
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
        let lanes = match disk {
            Some(disk) if disk.version == LEDGER_VERSION => disk.lanes,
            // A file from another version is not trusted to mean the same
            // thing: drop it rather than rest healthy lanes on a guess.
            _ => BTreeMap::new(),
        };
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

    /// Record what the lane's request amounted to.
    pub fn record(&mut self, key: &str, outcome: Outcome, retry: Option<u64>) {
        let t = now();
        let (rate, burst) = policy(source_of(key));
        let lane = self
            .lanes
            .entry(key.to_string())
            .or_insert_with(|| Lane::for_key(key));
        // A request was spent, whatever it returned.
        let elapsed = t.saturating_sub(lane.refilled) as f64;
        lane.tokens = (lane.tokens + elapsed * rate).min(burst) - 1.0;
        lane.refilled = t;
        match outcome {
            Outcome::Answered => {
                lane.fails = 0;
                lane.rest_secs = 0;
                lane.rest_until = 0;
                lane.last_ok = t;
            }
            Outcome::Throttled => {
                lane.fails = lane.fails.saturating_add(1);
                let wait = retry
                    .unwrap_or(THROTTLE_BASE)
                    .clamp(THROTTLE_BASE, REST_CAP);
                lane.rest_secs = wait;
                lane.rest_until = t + wait;
            }
            Outcome::Failed => {
                // A hiccup, not a verdict: a net blip gets one more chance
                // before the lane rests.
                lane.fails = lane.fails.saturating_add(1);
                if lane.fails >= 2 {
                    let shift = lane.fails.saturating_sub(2).min(5);
                    let wait = (REST_BASE << shift).min(REST_CAP);
                    lane.rest_secs = wait;
                    lane.rest_until = t + wait;
                }
            }
            Outcome::Blocked => {
                // The source refused: rest before asking again, doubling.
                lane.fails = lane.fails.saturating_add(1);
                let shift = lane.fails.saturating_sub(1).min(5);
                let wait = (REST_BASE << shift).min(REST_CAP);
                lane.rest_secs = wait;
                lane.rest_until = t + wait;
            }
        }
        self.dirty = true;
    }

    /// Write the ledger back, merging what other processes learned meanwhile:
    /// the stricter rest, the later success and the more depleted bucket win, so
    /// two searches in parallel cannot undo each other's lesson.
    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(path) = path() else { return };
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
                    if mine.rest_until > theirs.rest_until {
                        theirs.rest_until = mine.rest_until;
                        theirs.rest_secs = mine.rest_secs;
                    }
                    theirs.fails = theirs.fails.max(mine.fails);
                    theirs.last_ok = theirs.last_ok.max(mine.last_ok);
                    if mine.tokens < theirs.tokens {
                        theirs.tokens = mine.tokens;
                        theirs.refilled = mine.refilled;
                    }
                }
            }
        }
        let disk = Disk {
            version: LEDGER_VERSION,
            lanes: merged,
        };
        let Ok(json) = serde_json::to_string(&disk) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if crate::config::write_private(&tmp, json.as_bytes()).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }

    /// Test and diagnostic view: the lane's standing.
    #[cfg(test)]
    pub fn lane(&self, key: &str) -> Option<&Lane> {
        self.lanes.get(key)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Disk {
    version: u32,
    #[serde(default)]
    lanes: BTreeMap<String, Lane>,
}

fn path() -> Option<std::path::PathBuf> {
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("search-ledger.json"))
}

/// Cross-process serialization for the ledger file: a blocking flock the kernel
/// releases on crash. Best effort, like every other state write.
struct WriteLock {
    #[allow(dead_code)] // RAII: held for the lock, never read
    file: std::fs::File,
}

impl WriteLock {
    fn acquire() -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let path = crate::paths::cache_dir().join("search-ledger.lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .ok()?;
            loop {
                let got = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if got == 0 {
                    return Some(Self { file });
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return None;
                }
            }
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        l.record(&k, Outcome::Blocked, None);
        l.record(&k, Outcome::Blocked, None);
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
        l.record(&k, Outcome::Throttled, Some(120));
        match l.ready(&k) {
            State::Rested(secs) => assert!((110..=120).contains(&secs), "{secs}"),
            State::Paced(wait) => panic!("a throttle must rest the lane, not pace {wait}s"),
            State::Ready => panic!("a 429 must rest the lane"),
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

        let mut b = Ledger::load();
        // The second process saw a longer rest from the first; it must not
        // shorten it when it writes its own answer for another lane.
        b.record(&key("bing", "serp"), Outcome::Answered, None);
        b.save_if_dirty();

        let c = Ledger::load();
        assert!(matches!(c.lane(&key("brave", "serp")), Some(l) if l.rest_until > 0));
        assert!(c.lane(&key("bing", "serp")).is_some());
        unsafe { std::env::remove_var("DONSETCH_CACHE_DIR") };
    }
}
