//! The executor: spend a plan through the v5 fetch engine.
//!
//! One page at a time per host, one identity, one host budget. A plan is a
//! durable job: every completed page is written beside the job before the
//! next request, so a restart resumes without re-fetching a page it already
//! returned. A classified wall ends the host for this job and is declared
//! with its class/vendor; there is no per-page retry storm.

use super::model::ModelSummary;
use super::model::plan::{Plan, PlanRow, PlanStop};
use super::{
    CrawlOptions, CrawlPage, CrawlResult, FetchedPage, FocusStats, InventoryStatus, PageFetcher,
    StopReason,
};
use crate::crawl::governor::Governor;
use crate::detect::walls::{Vendor, Verdict};
use crate::extract::{self, ContentKind, ExtractOptions, PartialContent};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const JOB_VERSION: u32 = 1;
const JOB_CAP: usize = 50;
const JOB_TTL_SECS: u64 = 7 * 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStop {
    Covered,
    Budget,
    Deadline,
    Walled,
    Cancelled,
    Empty,
    Throttled,
}

impl RunStop {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Covered => "covered",
            Self::Budget => "budget",
            Self::Deadline => "deadline",
            Self::Walled => "walled",
            Self::Cancelled => "cancelled",
            Self::Empty => "empty",
            Self::Throttled => "throttled",
        }
    }

    fn as_stop(self) -> StopReason {
        match self {
            Self::Covered | Self::Empty => StopReason::FrontierEmpty,
            Self::Budget => StopReason::MaxPages,
            Self::Deadline => StopReason::Deadline,
            Self::Walled => StopReason::Walled,
            Self::Cancelled => StopReason::Cancelled,
            Self::Throttled => StopReason::ThrottledOut,
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Covered | Self::Walled | Self::Empty)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallReport {
    pub url: String,
    /// `challenge`, `blocked`, `auth`, or `paywall`.
    pub class: String,
    pub vendor: Option<String>,
    pub status: u16,
    pub ask: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub job: String,
    pub planned: usize,
    pub fetched: usize,
    pub requests: usize,
    pub attempts: usize,
    pub throttled: usize,
    pub complete: bool,
    pub stop: RunStop,
    pub wall: Option<WallReport>,
    pub elapsed_ms: u64,
    pub page_ms_p50: u64,
    /// Per-page wall times, for the paired gate artifact.
    pub page_ms: Vec<u64>,
    pub plan_requests: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFocus {
    omitted: bool,
    blocks_total: usize,
    blocks_shown: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPartial {
    reason: String,
    items_found: usize,
    items_total: Option<usize>,
}

/// A page as the job stores it. `PartialContent` carries a `&'static str`
/// reason, so it cannot be derived into a deserialize type directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPage {
    focus: Option<StoredFocus>,
    url: String,
    title: String,
    kind: ContentKind,
    markdown: String,
    chars: usize,
    next_offset: Option<usize>,
    partial: Option<StoredPartial>,
    quality: f32,
    duplicate: bool,
    parent: Option<String>,
    score: f64,
    lastmod: Option<String>,
    fetched_at: u64,
    #[serde(default)]
    fingerprint: Option<String>,
}

impl StoredPage {
    fn from_page(p: &CrawlPage) -> Self {
        Self {
            focus: p.focus.as_ref().map(|f| StoredFocus {
                omitted: f.omitted,
                blocks_total: f.blocks_total,
                blocks_shown: f.blocks_shown,
            }),
            url: p.url.clone(),
            title: p.title.clone(),
            kind: p.kind,
            markdown: p.markdown.clone(),
            chars: p.chars,
            next_offset: p.next_offset,
            partial: p.partial.as_ref().map(|p| StoredPartial {
                reason: p.reason.to_string(),
                items_found: p.items_found,
                items_total: p.items_total,
            }),
            quality: p.quality,
            duplicate: p.duplicate,
            parent: p.parent.clone(),
            score: p.score,
            lastmod: p.lastmod.clone(),
            fetched_at: p.fetched_at,
            fingerprint: p.fingerprint.clone(),
        }
    }

    fn into_page(self) -> CrawlPage {
        CrawlPage {
            focus: self.focus.map(|f| FocusStats {
                omitted: f.omitted,
                blocks_total: f.blocks_total,
                blocks_shown: f.blocks_shown,
            }),
            url: self.url,
            title: self.title,
            kind: self.kind,
            markdown: self.markdown,
            chars: self.chars,
            next_offset: self.next_offset,
            partial: self.partial.map(|p| PartialContent {
                // Bounded to one small string per stored page; the job file
                // itself is bounded to 200 pages and 50 jobs.
                reason: Box::leak(p.reason.into_boxed_str()),
                items_found: p.items_found,
                items_total: p.items_total,
            }),
            quality: self.quality,
            duplicate: self.duplicate,
            parent: self.parent,
            score: self.score,
            lastmod: self.lastmod,
            fetched_at: self.fetched_at,
            fingerprint: self.fingerprint,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    version: u32,
    pub id: String,
    pub seed: String,
    pub plan: Plan,
    #[serde(default)]
    pub model: Option<ModelSummary>,
    #[serde(default)]
    pub pages: Vec<StoredPage>,
    #[serde(default)]
    pub skipped: Vec<(String, String)>,
    #[serde(default)]
    pub attempted: Vec<String>,
    #[serde(default)]
    pub requests: usize,
    #[serde(default)]
    pub attempts: usize,
    #[serde(default)]
    pub throttled: usize,
    #[serde(default)]
    pub page_ms: Vec<u64>,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub stop: Option<RunStop>,
    #[serde(default)]
    pub wall: Option<WallReport>,
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

impl Job {
    /// A `pN` handle is the page's stable position in this job. Duplicate
    /// pages still occupy their position; the digest simply hides their card.
    pub fn page_by_handle(&self, handle: &str) -> Option<CrawlPage> {
        let n = handle.strip_prefix('p')?.parse::<usize>().ok()?;
        if n == 0 {
            return None;
        }
        self.pages.get(n - 1).cloned().map(StoredPage::into_page)
    }

    pub fn handle_count(&self) -> usize {
        self.pages.len()
    }
}

pub struct JobStore {
    root: PathBuf,
}

impl Default for JobStore {
    fn default() -> Self {
        Self {
            root: crate::paths::cache_dir().join("crawl").join("jobs"),
        }
    }
}

impl JobStore {
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub fn create(
        &self,
        seed: &str,
        plan: Plan,
        model: Option<ModelSummary>,
    ) -> Result<Job, String> {
        let now = unix_now();
        let id = new_job_id(seed, now);
        let job = Job {
            version: JOB_VERSION,
            id,
            seed: seed.to_string(),
            plan,
            model,
            pages: Vec::new(),
            skipped: Vec::new(),
            attempted: Vec::new(),
            requests: 0,
            attempts: 0,
            throttled: 0,
            page_ms: Vec::new(),
            complete: false,
            stop: None,
            wall: None,
            started_at: now,
            updated_at: now,
        };
        self.save(&job)?;
        Ok(job)
    }

    /// The newest job id, for a bare `crawl get pN` after a run.
    pub fn latest_id(&self) -> Result<String, String> {
        let entries = std::fs::read_dir(&self.root)
            .map_err(|_| "no run jobs yet; run a crawl first".to_string())?;
        let mut best: Option<(u64, String)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| is_valid_job_id(s))
            else {
                continue;
            };
            let stamp = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(t, _)| stamp > *t) {
                best = Some((stamp, id.to_string()));
            }
        }
        best.map(|(_, id)| id)
            .ok_or_else(|| "no run jobs yet; run a crawl first".to_string())
    }

    pub fn load(&self, id: &str) -> Result<Job, String> {
        if !is_valid_job_id(id) {
            return Err(format!("run job unknown or expired: {id}"));
        }
        let path = self.path(id);
        let raw = std::fs::read(&path).map_err(|_| format!("run job unknown or expired: {id}"))?;
        let job: Job =
            serde_json::from_slice(&raw).map_err(|_| format!("run job is corrupt: {id}"))?;
        if job.version != JOB_VERSION {
            return Err(format!(
                "run job version {} is not supported (expected {JOB_VERSION})",
                job.version
            ));
        }
        Ok(job)
    }

    pub fn save(&self, job: &Job) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(|e| format!("run job dir: {e}"))?;
        let path = self.path(&job.id);
        let tmp = self
            .root
            .join(format!("{}.{}.tmp", job.id, std::process::id()));
        let bytes = serde_json::to_vec(job).map_err(|e| format!("run job encode: {e}"))?;
        crate::config::write_private(&tmp, &bytes).map_err(|e| format!("run job write: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("run job rename: {e}"))?;
        self.sweep();
        Ok(())
    }

    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let now = unix_now();
        let mut live: Vec<(u64, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now.saturating_sub(modified) > JOB_TTL_SECS {
                let _ = std::fs::remove_file(&path);
            } else {
                live.push((modified, path));
            }
        }
        if live.len() <= JOB_CAP {
            return;
        }
        live.sort_unstable_by_key(|(t, _)| *t);
        for (_, path) in live.iter().take(live.len() - JOB_CAP) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn is_valid_job_id(id: &str) -> bool {
    id.len() == 17 && id.starts_with('j') && id.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

fn new_job_id(seed: &str, now: u64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in seed
        .as_bytes()
        .iter()
        .chain(now.to_le_bytes().iter())
        .chain(std::process::id().to_le_bytes().iter())
        .chain(seq.to_le_bytes().iter())
    {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("j{h:016x}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn median_ms(mut values: Vec<u64>) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn page_sig(title: &str, markdown: &str) -> u64 {
    let mut s = String::with_capacity(240);
    s.push_str(&title.trim().to_lowercase());
    s.push('|');
    s.extend(markdown.chars().filter(|c| !c.is_whitespace()).take(200));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn wall_of(page: &FetchedPage) -> Option<WallReport> {
    let (class, vendor) = match page.verdict {
        Verdict::Challenge(v) => ("challenge", Some(vendor_name(v).to_string())),
        Verdict::Blocked => ("blocked", None),
        Verdict::AuthWall => ("auth", None),
        Verdict::Paywall => ("paywall", None),
        Verdict::ContentOk | Verdict::SoftNotFound => return None,
    };
    Some(WallReport {
        url: page.url.clone(),
        class: class.to_string(),
        vendor,
        status: page.status,
        ask: "another egress or a real browser driver (bladebro)".to_string(),
    })
}

fn vendor_name(v: Vendor) -> &'static str {
    match v {
        Vendor::Cloudflare => "cloudflare",
        Vendor::DataDome => "datadome",
        Vendor::Akamai => "akamai",
        Vendor::Kasada => "kasada",
        Vendor::PerimeterX => "perimeterx",
        Vendor::Imperva => "imperva",
        Vendor::Sucuri => "sucuri",
        Vendor::Wordfence => "wordfence",
        Vendor::Generic => "generic",
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default()
}

async fn wait_lane(governor: &Governor, host: &str, seq: u64, deadline: Instant) -> Option<String> {
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if let Some(lane) = governor.best_lane(host) {
            let id = lane.id.clone();
            let wait = governor.wait_for(host, &id, seq);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if wait > Duration::ZERO {
                tokio::time::sleep(wait.min(remaining)).await;
            }
            return Some(id);
        }
        // The shared host penalty box is still closed. Wait for it, not
        // for a fresh lane; a rate limit is waited out, never rotated.
        let wait = governor.wait_for(host, "*", seq);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Some("direct".to_string());
        }
        tokio::time::sleep(wait.min(remaining)).await;
    }
}

fn cancelled(opts: &CrawlOptions) -> bool {
    opts.cancel.as_ref().is_some_and(|rx| *rx.borrow())
}

/// A deadline, cancel or throttle stop happened before a page outcome.
/// The row must stay resumable: remove it from the persisted attempted set.
fn forget_attempt(job: &mut Job, attempted: &mut HashSet<String>, url: &str) {
    job.attempted.retain(|u| u != url);
    attempted.remove(url);
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

pub async fn execute(
    fetch: PageFetcher,
    governor: Arc<Governor>,
    opts: &CrawlOptions,
    store: &JobStore,
    mut job: Job,
) -> Result<CrawlResult, String> {
    let started = Instant::now();
    let deadline_at = started + opts.deadline;

    // A completed or walled job is idempotent: resume returns exactly what
    // the earlier run produced, with no network request.
    if job.complete || job.wall.is_some() {
        return Ok(finish(job, started));
    }
    if job.stop.is_some_and(|s| s.terminal()) {
        return Ok(finish(job, started));
    }
    job.stop = None;

    let target = job.plan.pages.len();
    if target == 0 {
        job.stop = Some(RunStop::Empty);
        job.complete = false;
        store.save(&job)?;
        return Ok(finish(job, started));
    }

    let mut attempted: HashSet<String> = job.attempted.iter().cloned().collect();
    let mut rows: VecDeque<PlanRow> = job
        .plan
        .pages
        .iter()
        .chain(job.plan.reserves.iter())
        .filter(|row| !attempted.contains(&row.url))
        .cloned()
        .collect();
    let host_budget = target + job.plan.reserves.len().min(24) + 4;
    let mut done = job.pages.len();
    let mut seq = job.attempts as u64;
    let mut sigs: HashSet<u64> = job
        .pages
        .iter()
        .map(|p| page_sig(&p.title, &p.markdown))
        .collect();
    let mut used_chars: usize = job
        .pages
        .iter()
        .filter(|p| !p.duplicate)
        .map(|p| p.chars)
        .sum();
    let mut stop: Option<RunStop> = None;

    'work: while done < target && !rows.is_empty() {
        if cancelled(opts) {
            stop = Some(RunStop::Cancelled);
            break;
        }
        if Instant::now() >= deadline_at {
            stop = Some(RunStop::Deadline);
            break;
        }
        if job.attempts >= host_budget {
            stop = Some(RunStop::Budget);
            break;
        }
        let row = rows.pop_front().expect("checked non-empty");
        if !attempted.insert(row.url.clone()) {
            continue;
        }
        job.attempted.push(row.url.clone());
        let host = host_of(&row.url);
        let mut retried_throttle = false;
        let mut row_done = false;

        'fetch: loop {
            if cancelled(opts) {
                forget_attempt(&mut job, &mut attempted, &row.url);
                stop = Some(RunStop::Cancelled);
                break 'work;
            }
            if Instant::now() >= deadline_at {
                forget_attempt(&mut job, &mut attempted, &row.url);
                stop = Some(RunStop::Deadline);
                break 'work;
            }
            let Some(lane) = wait_lane(&governor, &host, seq, deadline_at).await else {
                forget_attempt(&mut job, &mut attempted, &row.url);
                stop = Some(RunStop::Deadline);
                break 'work;
            };
            seq = seq.wrapping_add(1);
            let t0 = Instant::now();
            let fetched = tokio::time::timeout_at(
                deadline_at.into(),
                fetch(row.url.clone(), lane.clone(), None, None),
            )
            .await;
            let Some(page) = fetched.ok() else {
                job.attempts += 1;
                job.requests += 1;
                job.page_ms.push(t0.elapsed().as_millis() as u64);
                forget_attempt(&mut job, &mut attempted, &row.url);
                stop = Some(RunStop::Deadline);
                break 'work;
            };
            job.attempts += 1;
            job.requests += 1;
            job.page_ms.push(t0.elapsed().as_millis() as u64);

            if matches!(page.status, 429 | 503) {
                job.throttled += 1;
                governor.on_throttled(&host, &lane);
                let reason = format!("http {} (host throttled)", page.status);
                if !retried_throttle && Instant::now() < deadline_at {
                    retried_throttle = true;
                    let wait = governor
                        .wait_for(&host, &lane, seq)
                        .min(deadline_at.saturating_duration_since(Instant::now()));
                    if wait > Duration::ZERO {
                        tokio::time::sleep(wait).await;
                    }
                    continue 'fetch;
                }
                job.skipped.push((row.url.clone(), reason));
                forget_attempt(&mut job, &mut attempted, &row.url);
                stop = Some(RunStop::Throttled);
                break 'work;
            }

            if let Some(denial) = &page.denied {
                job.skipped
                    .push((row.url.clone(), super::policy_skip_reason(denial)));
                break 'fetch;
            }
            if let Some(wall) = wall_of(&page) {
                job.wall = Some(wall);
                stop = Some(RunStop::Walled);
                break 'work;
            }
            if !matches!(page.verdict, Verdict::ContentOk) || !(200..300).contains(&page.status) {
                let reason = page
                    .error_hint
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", page.verdict));
                job.skipped.push((row.url.clone(), reason));
                break 'fetch;
            }

            governor.on_success(&host, &lane, page.latency);
            let ctype = page
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("text/html");
            if crate::fetch::guards::is_binary(&page.body, ctype) {
                let kind = ctype.split(';').next().unwrap_or("unknown").trim();
                job.skipped
                    .push((row.url.clone(), format!("binary ({kind})")));
                break 'fetch;
            }
            // The job stores the full extracted page: `crawl get pN` must be
            // lossless. The digest applies the focus at card time instead.
            let eo = ExtractOptions {
                focus: None,
                max_chars: Some(opts.per_page_max),
                verified: true,
                ..Default::default()
            };
            let extracted = tokio::time::timeout_at(
                deadline_at.into(),
                extract::extract_off_worker(&page.body, ctype, &page.url, &eo),
            )
            .await;
            let Ok(Ok(r)) = extracted else {
                if extracted.is_err() {
                    forget_attempt(&mut job, &mut attempted, &row.url);
                    stop = Some(RunStop::Deadline);
                    break 'work;
                }
                let reason = match extracted {
                    Ok(Err(e)) => format!("extract failed: {e}"),
                    _ => "extract failed".into(),
                };
                job.skipped.push((row.url.clone(), reason));
                break 'fetch;
            };

            let mut md = r.markdown;
            let mut chars = md.chars().count();
            let mut next_offset = r.next_offset;
            let mut budget_after = false;
            let sig = page_sig(r.title.as_deref().unwrap_or(""), &md);
            let duplicate = sigs.contains(&sig);
            if !duplicate && r.quality < opts.min_quality {
                job.skipped
                    .push((page.url.clone(), format!("low quality ({:.2})", r.quality)));
                break 'fetch;
            }
            if !duplicate {
                sigs.insert(sig);
                let remaining = opts.max_total_chars.saturating_sub(used_chars);
                if remaining == 0 {
                    job.skipped
                        .push((row.url.clone(), "char budget spent".into()));
                    stop = Some(RunStop::Budget);
                    break 'work;
                }
                if chars > remaining {
                    let cut = md
                        .char_indices()
                        .nth(remaining)
                        .map(|(i, _)| i)
                        .unwrap_or(md.len());
                    md.truncate(cut);
                    chars = remaining;
                    next_offset = Some(cut.min(r.next_offset.unwrap_or(usize::MAX)));
                    budget_after = true;
                }
                used_chars += chars;
            }

            let page = CrawlPage {
                focus: opts.focus.as_ref().map(|_| FocusStats {
                    omitted: r.focus_omitted,
                    blocks_total: r.blocks_total,
                    blocks_shown: r.blocks_shown,
                }),
                url: page.url.clone(),
                title: r.title.unwrap_or_default(),
                kind: r.content_kind,
                markdown: md,
                chars,
                next_offset,
                partial: r.partial,
                quality: r.quality,
                duplicate,
                parent: None,
                score: row.score,
                lastmod: row.freshness.clone(),
                fetched_at: unix_now(),
                fingerprint: r.fingerprint,
            };
            job.pages.push(StoredPage::from_page(&page));
            done += 1;
            job.stop = None;
            store.save(&job)?;
            if let Some(cb) = &opts.progress {
                cb(done, rows.len());
            }
            row_done = true;
            if budget_after {
                stop = Some(RunStop::Budget);
                break 'work;
            }
            break 'fetch;
        }

        if !row_done {
            job.stop = None;
            store.save(&job)?;
        }
        if done >= target {
            break;
        }
    }

    if stop.is_none() {
        if done >= target {
            stop = Some(if job.plan.stop == PlanStop::Budget {
                RunStop::Budget
            } else {
                RunStop::Covered
            });
        } else if rows.is_empty() {
            stop = Some(if done == 0 {
                RunStop::Empty
            } else {
                RunStop::Budget
            });
        } else {
            stop = Some(RunStop::Budget);
        }
    }
    job.stop = stop;
    job.complete =
        done >= target && matches!(job.stop, Some(RunStop::Covered) | Some(RunStop::Budget));
    store.save(&job)?;
    Ok(finish(job, started))
}

fn seed_host(seed: &str) -> String {
    let trimmed = seed.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let parsed = if trimmed.contains("://") {
        url::Url::parse(trimmed).ok()
    } else {
        url::Url::parse(&format!("https://{trimmed}")).ok()
    };
    parsed
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

fn finish(job: Job, started: Instant) -> CrawlResult {
    // The dossier is the delta memory: record every completed page's
    // fingerprint/lastmod here, after the bytes are safely in the job.
    let host = job
        .model
        .as_ref()
        .map(|m| m.host.clone())
        .unwrap_or_else(|| seed_host(&job.plan.seed));
    let updates: Vec<super::model::dossier::PageUpdate> = job
        .pages
        .iter()
        .map(|p| super::model::dossier::PageUpdate {
            url: p.url.clone(),
            fingerprint: p.fingerprint.clone(),
            lastmod: p.lastmod.clone(),
            chars: p.chars,
            fetched_at: p.fetched_at,
        })
        .collect();
    super::model::dossier::remember_pages(&host, &updates);
    let page_ms = job.page_ms.clone();
    let stop = job.stop.unwrap_or(if job.complete {
        RunStop::Covered
    } else {
        RunStop::Budget
    });
    let summary = RunSummary {
        job: job.id.clone(),
        planned: job.plan.pages.len(),
        fetched: job.pages.len(),
        requests: job.requests,
        attempts: job.attempts,
        throttled: job.throttled,
        complete: job.complete,
        stop,
        wall: job.wall.clone(),
        elapsed_ms: elapsed_ms(started),
        page_ms_p50: median_ms(page_ms.clone()),
        page_ms,
        plan_requests: job.model.as_ref().map(|m| m.requests).unwrap_or(0),
    };
    let attempted: HashSet<&str> = job.attempted.iter().map(String::as_str).collect();
    let queued = job
        .plan
        .pages
        .iter()
        .chain(job.plan.reserves.iter())
        .filter(|r| !attempted.contains(r.url.as_str()))
        .map(|r| r.url.clone())
        .collect();
    let model = job.model.clone();
    let inventory = InventoryStatus {
        complete: model.as_ref().is_some_and(|m| m.complete),
        source: "run",
        limit: 0,
    };
    CrawlResult {
        inventory,
        seed: job.seed.clone(),
        pages: job
            .pages
            .iter()
            .cloned()
            .map(StoredPage::into_page)
            .collect(),
        queued,
        depth_cut: Vec::new(),
        filtered_out: 0,
        skipped: job.skipped.clone(),
        stop: stop.as_stop(),
        elapsed: Duration::from_millis(summary.elapsed_ms),
        map: Vec::new(),
        crawl_delay: None,
        resume: Some(job.id.clone()),
        denied: Default::default(),
        seed_failure: None,
        model,
        plan: Some(job.plan.clone()),
        run: Some(summary),
        inventory_urls: Vec::new(),
        delta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::governor::{Lane, LaneKind};
    use crate::crawl::model::infer::{Family, SiteKind, sections_from_families};
    use crate::crawl::model::plan::{self, PlanRequest};
    use crate::detect::walls::Vendor;
    use crate::egress::request_route::RequestRoute;
    use futures_util::FutureExt;
    use std::sync::Mutex;

    fn fam(i: usize) -> Family {
        Family {
            section: format!("docs/s{i}"),
            template: "docs/:v".to_string(),
            size: 1,
            lastmod: None,
            sample: vec![format!("https://ex.com/docs/s{i}/page")],
        }
    }

    fn fixture_plan(n: usize) -> Plan {
        let families: Vec<Family> = (0..n).map(fam).collect();
        let sections = sections_from_families(&families);
        plan::build(PlanRequest {
            seed: "https://ex.com/",
            host: "ex.com",
            site_kind: SiteKind::Docs,
            focus: None,
            max_pages: n,
            families: &families,
            sections: &sections,
            urls: &[],
            explain: false,
        })
    }

    fn options() -> CrawlOptions {
        CrawlOptions {
            mode: super::super::CrawlMode::Run,
            respect_robots: false,
            deadline: Duration::from_secs(10),
            max_total_chars: 100_000,
            per_page_max: 2_000,
            ..CrawlOptions::default()
        }
    }

    fn governor() -> Arc<Governor> {
        Arc::new(Governor::new(vec![Lane {
            id: "direct".into(),
            kind: LaneKind::Direct,
        }]))
    }

    fn page(url: &str, status: u16, verdict: Verdict, body: &str) -> FetchedPage {
        FetchedPage {
            lane: "direct".into(),
            route: Some(RequestRoute::direct()),
            url: url.into(),
            status,
            headers: vec![("content-type".into(), "text/html".into())],
            body: body.as_bytes().to_vec(),
            verdict,
            latency: Duration::from_millis(5),
            cached: false,
            error_hint: None,
            denied: None,
        }
    }

    fn mock(
        hits: Arc<Mutex<Vec<String>>>,
        respond: impl Fn(&str) -> FetchedPage + Send + Sync + Clone + 'static,
    ) -> PageFetcher {
        Arc::new(
            move |url: String, _lane: String, _ref: Option<String>, _gate| {
                let hits = Arc::clone(&hits);
                let respond = respond.clone();
                async move {
                    hits.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(url.clone());
                    respond(&url)
                }
                .boxed()
            },
        )
    }

    fn temp_store(tag: &str) -> JobStore {
        let root = std::env::temp_dir().join(format!(
            "donsetch-run-{tag}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        JobStore::at(root)
    }

    #[tokio::test]
    async fn spends_a_plan_and_resume_is_idempotent() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let fetch = mock(hits.clone(), |url| {
            page(
                url,
                200,
                Verdict::ContentOk,
                "<html><head><title>Page</title></head><body><main><h1>Page</h1><p>Useful content for the run test.</p></main></body></html>",
            )
        });
        let store = temp_store("idempotent");
        let job = store
            .create("https://ex.com/", fixture_plan(2), None)
            .unwrap();
        let result = execute(fetch.clone(), governor(), &options(), &store, job)
            .await
            .unwrap();
        assert_eq!(result.pages.len(), 2);
        assert_eq!(result.run.as_ref().unwrap().requests, 2);
        assert_eq!(result.stop, StopReason::MaxPages);
        let id = result.resume.clone().unwrap();
        let first_urls: Vec<String> = result.pages.iter().map(|p| p.url.clone()).collect();
        let first_requests = result.run.as_ref().unwrap().requests;

        let job = store.load(&id).unwrap();
        let resumed = execute(fetch, governor(), &options(), &store, job)
            .await
            .unwrap();
        assert_eq!(resumed.pages.len(), 2);
        assert_eq!(
            resumed
                .pages
                .iter()
                .map(|p| p.url.clone())
                .collect::<Vec<_>>(),
            first_urls
        );
        assert_eq!(resumed.run.as_ref().unwrap().requests, first_requests);
        assert!(resumed.run.as_ref().unwrap().complete);
        assert_eq!(
            hits.lock().unwrap().len(),
            2,
            "resume must not re-fetch a stored page"
        );
    }

    #[tokio::test]
    async fn a_classified_wall_stops_the_host_in_one_request_and_resume_declares_it() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let fetch = mock(hits.clone(), |url| {
            page(
                url,
                200,
                Verdict::Challenge(Vendor::Cloudflare),
                "<html><title>Just a moment</title></html>",
            )
        });
        let store = temp_store("wall");
        let job = store
            .create("https://ex.com/", fixture_plan(3), None)
            .unwrap();
        let result = execute(fetch, governor(), &options(), &store, job)
            .await
            .unwrap();
        assert_eq!(result.stop, StopReason::Walled);
        assert_eq!(result.run.as_ref().unwrap().requests, 1);
        assert_eq!(hits.lock().unwrap().len(), 1);
        let wall = result.run.as_ref().unwrap().wall.as_ref().unwrap();
        assert_eq!(wall.class, "challenge");
        assert_eq!(wall.vendor.as_deref(), Some("cloudflare"));

        let id = result.resume.clone().unwrap();
        let job = store.load(&id).unwrap();
        let resumed = execute(
            mock(hits.clone(), |url| {
                page(url, 200, Verdict::ContentOk, "unused")
            }),
            governor(),
            &options(),
            &store,
            job,
        )
        .await
        .unwrap();
        assert_eq!(resumed.stop, StopReason::Walled);
        assert_eq!(resumed.run.as_ref().unwrap().requests, 1);
        assert_eq!(hits.lock().unwrap().len(), 1, "a wall is not retried");
    }

    #[tokio::test]
    async fn a_failed_page_is_replaced_from_the_reserves() {
        let first = "https://ex.com/docs/s0/page";
        let hits = Arc::new(Mutex::new(Vec::new()));
        let fetch = mock(hits.clone(), move |url| {
            if url == first {
                page(url, 404, Verdict::SoftNotFound, "<html>not found</html>")
            } else {
                page(
                    url,
                    200,
                    Verdict::ContentOk,
                    "<html><head><title>Reserve</title></head><body><main><h1>Reserve</h1><p>The reserve page carries the content after the primary failed.</p></main></body></html>",
                )
            }
        });
        let store = temp_store("reserve");
        // One section with three samples: one page plus reserves.
        let families = vec![Family {
            section: "docs/s0".into(),
            template: "docs/:v".into(),
            size: 3,
            lastmod: None,
            sample: vec![
                first.into(),
                "https://ex.com/docs/s0/reserve-a".into(),
                "https://ex.com/docs/s0/reserve-b".into(),
            ],
        }];
        let sections = sections_from_families(&families);
        let plan = plan::build(PlanRequest {
            seed: "https://ex.com/",
            host: "ex.com",
            site_kind: SiteKind::Docs,
            focus: None,
            max_pages: 1,
            families: &families,
            sections: &sections,
            urls: &[],
            explain: false,
        });
        assert!(!plan.reserves.is_empty(), "fixture needs a reserve");
        let job = store.create("https://ex.com/", plan, None).unwrap();
        let result = execute(fetch, governor(), &options(), &store, job)
            .await
            .unwrap();
        assert_eq!(result.pages.len(), 1);
        assert_eq!(result.pages[0].url, "https://ex.com/docs/s0/reserve-a");
        assert!(result.skipped.iter().any(|(u, _)| u == first));
        assert_eq!(hits.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn skipped_low_quality_pages_do_not_spend_the_char_budget() {
        let low_url = "https://ex.com/docs/s0/page";
        let good_url = "https://ex.com/docs/s1/page";
        let hits = Arc::new(Mutex::new(Vec::new()));
        let nav_noise: String = (0..60)
            .map(|i| format!("<a href=\"/nav/{i}\">Link {i}</a> "))
            .collect();
        let low_html = format!(
            "<html><head><title>Nav</title></head><body><nav>{nav_noise}</nav></body></html>"
        );
        let good_html = "<html><head><title>Architecture Guide</title></head><body><main><article><h1>Architecture Guide</h1><p>This guide explains the request pipeline, connection pooling, retry semantics, and structured extraction guarantees in complete detail for operators.</p><p>Each stage enforces deterministic memory, wall-clock, and byte ceilings so long-running crawls stay predictable across thousands of pages.</p></article></main></body></html>";
        let fetch = mock(hits, move |url| {
            if url == low_url {
                page(url, 200, Verdict::ContentOk, &low_html)
            } else {
                page(url, 200, Verdict::ContentOk, good_html)
            }
        });
        let store = temp_store("low-quality-budget");
        let job = store
            .create("https://ex.com/", fixture_plan(2), None)
            .unwrap();
        let mut opts = options();
        opts.min_quality = 0.35;
        opts.max_total_chars = 350;
        let result = execute(fetch, governor(), &opts, &store, job)
            .await
            .unwrap();
        assert!(
            result
                .skipped
                .iter()
                .any(|(u, why)| u == low_url && why.starts_with("low quality")),
            "expected low-quality skip on first page, got {:?}",
            result.skipped
        );
        assert_eq!(
            result.pages.len(),
            1,
            "second high-quality page must still fit in max_total_chars; skipped={:?}",
            result.skipped
        );
        assert_eq!(result.pages[0].url, good_url);
    }

    #[test]
    fn seed_host_parses_authority_without_port_or_query() {
        assert_eq!(seed_host("https://Example.COM:8443?foo=1"), "example.com");
        assert_eq!(
            seed_host("https://docs.example.org/guide"),
            "docs.example.org"
        );
        assert_eq!(seed_host("ex.com:8080/path"), "ex.com");
        assert_eq!(seed_host(""), "");
    }
}
