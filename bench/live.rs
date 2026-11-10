//! The live protocol runner: how many runs, from what state, against which build.
//!
//! `src/search/bench.rs` judges one answer; this module decides what counts as
//! one measurement and how a run becomes an artifact.
//!
//! **Three things make a search number mean something.**
//!
//! 1. **A cold cache per run.** Egress state, the lane ledger, the learned
//!    identity scores and the page cache all change what a call returns. A bench
//!    that inherits the user's state measures the user's history, so `--cold`
//!    (the default) gives every run an empty `DONSETCH_CACHE_DIR`. Because the
//!    BYOK keys live under that same cache root, a cold run is also keyless by
//!    construction — "BYOK is off when local is measured" is enforced by the
//!    filesystem, not by a flag someone can forget.
//! 2. **Runs that are not one hour.** Sources are challenged in bursts; a corpus
//!    measured inside one minute is a measurement of that minute. `--runs N
//!    --gap-min M` repeats the corpus N times with the gap, and every run keeps
//!    its own start time in the artifact, so a report can say which hours the
//!    number came from.
//! 3. **The same rows against another build.** `--bin` drives any released
//!    binary's `search --json`, so 5.2.1 (and 5.1.1) can be measured with the
//!    same corpus, the same metrics and the same judge. That is what makes a
//!    "before" artifact comparable instead of anecdotal.
//!
//! The mode is recorded in the artifact: `in-process` (this build, recall and
//! request counts available) or `bin:<path>` (another build, recall absent
//! because pre-merge candidates never leave the process).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use super::{
    AnswerRow, BenchQuery, Options, RowRecord, Run, domains, judge, now_unix, report,
    write_artifact,
};

/// The protocol entry point: one measurement, or N of them, then one report.
pub async fn protocol(opts: &Options, queries: &[BenchQuery]) -> i32 {
    if let Some(bin) = opts.bin.clone() {
        return external(opts, queries, &bin).await;
    }
    if !opts.cold {
        // One run, in this process, on whatever state the user already has.
        let child = std::env::var_os(CHILD_ENV).is_some();
        return match super::one_run(opts, queries, 0).await {
            Ok(run) => finish(opts, "in-process", None, &[run], None, !child),
            Err(e) => {
                crate::diag!("search-bench: {e}");
                1
            }
        };
    }
    // Cold: one child process per run, each with its own cache directory, so no
    // learned state, ledger entry or warm page survives from one run to the next.
    let root = temp_root();
    let mut runs = Vec::new();
    for i in 0..opts.runs {
        if i > 0 && !opts.gap.is_zero() {
            crate::diag!(
                "[search-bench] waiting {} min before run {}",
                opts.gap.as_secs() / 60,
                i + 1
            );
            tokio::time::sleep(opts.gap).await;
        }
        let out = root.join(format!("run-{i}.json"));
        match child_run(opts, i, &out) {
            Ok(run) => runs.push(run),
            Err(e) => {
                crate::diag!("search-bench: {e}");
                let _ = std::fs::remove_dir_all(&root);
                return 1;
            }
        }
    }
    finish(opts, "in-process", None, &runs, Some(&root), true)
}

/// Set on the child process of a cold run: it writes its artifact and prints
/// only the per-row lines, because the parent prints the merged report. Named
/// outside the `DONSETCH_` namespace on purpose: the config loader rejects any
/// `DONSETCH_*` that is not `DONSETCH_<SECTION>__<KEY>`, and an internal marker
/// is not configuration.
const CHILD_ENV: &str = "DSB_BENCH_CHILD";

fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("donsetch-search-bench-{}", std::process::id()))
}

/// Everything a run needs, as arguments: built explicitly rather than by editing
/// the parent's argv, so a flag added later cannot make a child re-orchestrate.
fn child_args(opts: &Options, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "search-bench".into(),
        "--queries".into(),
        opts.queries.display().to_string(),
        "--set".into(),
        opts.set.clone(),
        "--pace-ms".into(),
        opts.pace.as_millis().to_string(),
        "--max-results".into(),
        opts.max_results.to_string(),
        "--keep-state".into(),
        "--out".into(),
        out.display().to_string(),
    ];
    if let Some(shape) = &opts.shape {
        a.push("--shape".into());
        a.push(shape.clone());
    }
    if !opts.only.is_empty() {
        a.push("--only".into());
        a.push(opts.only.join(","));
    }
    if opts.smoke {
        a.push("--smoke".into());
    }
    if let Some(n) = opts.limit {
        a.push("--limit".into());
        a.push(n.to_string());
    }
    if opts.verbose {
        a.push("-v".into());
    }
    a
}

/// One child run: same binary, empty cache, artifact on disk.
fn child_run(opts: &Options, index: usize, out: &Path) -> Result<Run, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let cache = out.with_file_name(format!("cache-{index}"));
    std::fs::create_dir_all(&cache)
        .map_err(|e| format!("cannot create {}: {e}", cache.display()))?;
    let status = Command::new(&exe)
        .args(child_args(opts, out))
        .env("DONSETCH_CACHE_DIR", &cache)
        .env(CHILD_ENV, "1")
        .status()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    // Exit 3 is the run's own verdict: rows broke a contract (`must_not`,
    // redirect, stale, undated, foreign). That is data the parent merges and
    // reports, not a failure of the run — the same rule `exit_code` applies to
    // a single-process run.
    if !status.success() && status.code() != Some(3) {
        return Err(format!("child run {index} exited with {status}"));
    }
    read_run(out)
}

#[derive(serde::Deserialize)]
struct ChildArtifact {
    #[serde(default)]
    run_started_unix: Vec<u64>,
    #[serde(default)]
    queries: Vec<RowRecord>,
}

fn read_run(path: &Path) -> Result<Run, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let doc: ChildArtifact =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Run {
        started_unix: doc.run_started_unix.first().copied().unwrap_or(0),
        finished_unix: now_unix(),
        rows: doc.queries,
    })
}

/// Print the merged report, write the artifact, and say whether the run is clean.
fn finish(
    opts: &Options,
    mode: &str,
    bin_version: Option<&str>,
    runs: &[Run],
    cleanup: Option<&Path>,
    announce: bool,
) -> i32 {
    if announce {
        report(
            &format!("search-bench · {mode} · {} run(s)", runs.len()),
            runs,
        );
    }
    if let Some(path) = &opts.out {
        let doc = super::artifact(opts, mode, bin_version, runs);
        if let Err(e) = write_artifact(path, &doc) {
            crate::diag!("search-bench: {e}");
            if let Some(dir) = cleanup {
                let _ = std::fs::remove_dir_all(dir);
            }
            return 1;
        }
        if announce {
            crate::diag!("artifact: {}", path.display());
        }
    }
    if let Some(dir) = cleanup {
        let _ = std::fs::remove_dir_all(dir);
    }
    exit_code(runs)
}

/// A run is clean when no row broke a contract. `must_not` and redirects are
/// hard: they are what the product promises never to hand the caller.
fn exit_code(runs: &[Run]) -> i32 {
    let rows: Vec<&RowRecord> = runs.iter().flat_map(|r| r.rows.iter()).collect();
    let dirty = rows.iter().any(|r| {
        !r.violations.is_empty() || r.redirects > 0 || r.stale > 0 || r.undated > 0 || r.foreign > 0
    });
    i32::from(dirty) * 3
}

// ── The other build: `--bin <path> search --json` ────────────────────────────

#[derive(serde::Deserialize, Default)]
struct CliOut {
    #[serde(default)]
    meta: CliMeta,
}

#[derive(serde::Deserialize, Default)]
struct CliMeta {
    #[serde(default)]
    results: Vec<CliResult>,
    #[serde(default)]
    sources: Vec<CliSource>,
}

#[derive(serde::Deserialize)]
struct CliResult {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    snippet: String,
}

#[derive(serde::Deserialize)]
struct CliSource {
    #[serde(default)]
    id: String,
    #[serde(default)]
    items: usize,
    #[serde(default)]
    state: String,
}

/// A per-row deadline for the external build. The product has its own budget;
/// this only stops one pathological row from hanging the whole bench.
const EXTERNAL_DEADLINE_MS: u32 = 30_000;

async fn external(opts: &Options, queries: &[BenchQuery], bin: &Path) -> i32 {
    let version = bin_version(bin);
    let root = temp_root();
    let mut runs = Vec::new();
    for i in 0..opts.runs {
        if i > 0 && !opts.gap.is_zero() {
            crate::diag!(
                "[search-bench] waiting {} min before run {}",
                opts.gap.as_secs() / 60,
                i + 1
            );
            tokio::time::sleep(opts.gap).await;
        }
        let cache = root.join(format!("bin-cache-{i}"));
        if let Err(e) = std::fs::create_dir_all(&cache) {
            crate::diag!("search-bench: cannot create {}: {e}", cache.display());
            return 1;
        }
        let started_unix = now_unix();
        let mut rows = Vec::new();
        for (n, q) in queries.iter().enumerate() {
            if n > 0 {
                tokio::time::sleep(opts.pace).await;
            }
            rows.push(external_row(bin, &cache, opts, q));
        }
        runs.push(Run {
            started_unix,
            finished_unix: now_unix(),
            rows,
        });
    }
    let mode = format!("bin:{}", bin.display());
    finish(opts, &mode, version.as_deref(), &runs, Some(&root), true)
}

fn bin_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

fn external_row(bin: &Path, cache: &Path, opts: &Options, q: &BenchQuery) -> RowRecord {
    let t0 = Instant::now();
    let max = opts.max_results.to_string();
    let deadline = EXTERNAL_DEADLINE_MS.to_string();
    let out = Command::new(bin)
        .args([
            "search",
            q.query.as_str(),
            "--json",
            "--max-results",
            max.as_str(),
            "--deadline-ms",
            deadline.as_str(),
        ])
        .env("DONSETCH_CACHE_DIR", cache)
        .output();
    let ms = t0.elapsed().as_millis() as u64;
    let (answers, census) = match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            match serde_json::from_str::<CliOut>(&text) {
                Ok(doc) => {
                    let answers: Vec<AnswerRow> = doc
                        .meta
                        .results
                        .iter()
                        .map(|r| AnswerRow {
                            url: r.url.clone(),
                            title: r.title.clone(),
                            snippet: r.snippet.clone(),
                        })
                        .collect();
                    let census = doc
                        .meta
                        .sources
                        .iter()
                        .map(|s| format!("{} {} {}", s.id, s.items, s.state))
                        .collect::<Vec<_>>()
                        .join(" · ");
                    (answers, census)
                }
                Err(e) => (
                    Vec::new(),
                    format!(
                        "unreadable answer: {e}: {}",
                        text.chars().take(120).collect::<String>()
                    ),
                ),
            }
        }
        Err(e) => (Vec::new(), format!("cannot run {}: {e}", bin.display())),
    };
    let judged = judge(q, &answers);
    let redirects = answers
        .iter()
        .filter(|r| {
            crate::search::contract::url_class(&r.url) != crate::search::contract::UrlClass::Page
        })
        .count();
    // The language contract is checked on what the answer carries; the date
    // contract is not measurable here, because the external envelope returns no
    // publication dates. Both stay honest: `foreign` is counted, `stale` and
    // `undated` are zero because nothing was seen, and in-process runs are the
    // ones that gate them.
    let mut foreign = 0usize;
    if let Some(lang) = q.lang.as_deref() {
        for row in answers.iter().take(3) {
            if !crate::search::locale::native(lang, &row.title, &row.snippet) {
                foreign += 1;
            }
        }
    }
    let record = RowRecord {
        id: q.id.clone(),
        shape: q.shape.clone(),
        set: q.set.clone(),
        query: q.query.clone(),
        expect: q.expect.clone(),
        hit: judged.hit,
        key_at: judged.key_at,
        violations: judged.violations,
        junk5: judged.junk5,
        redirects,
        ms,
        urls: answers.iter().map(|a| a.url.clone()).collect(),
        domains: domains(&answers),
        recall: None,
        requests: None,
        census,
        // No dates in the external envelope: nothing was seen to fail, and
        // in-process runs are the ones that gate the date contract.
        stale: 0,
        undated: 0,
        foreign,
        raw: None,
    };
    if opts.verbose {
        crate::diag!(
            "{:<6} {:<10} {:>5.1}s  {}",
            q.id,
            super::verdict(&record),
            ms as f64 / 1000.0,
            q.query
        );
    }
    record
}
