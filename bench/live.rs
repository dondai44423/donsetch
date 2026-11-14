//! The live protocol runner: how many runs, from what state, against which build.
//!
//! `src/search/bench.rs` judges one answer; this module decides what counts as
//! one measurement and how a run becomes an artifact.
//!
//! **Three things make a search number mean something.**
//!
//! 1. **A cold cache per run and shape.** Egress state, the lane ledger, the
//!    learned identity scores and the page cache all change what a call returns.
//!    A bench that inherits the user's state measures the user's history, so
//!    `--cold` (the default) gives every shape of every run an empty
//!    `DONSETCH_CACHE_DIR`, carrying only active source refusals; `--one-ledger` runs all shapes of a run from one. Because the
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
    // Cold: one child process per run and shape, each with its own cache
    // directory. Published source refusals survive; answers and learned state do not.
    let root = temp_root();
    let driver = match std::env::current_exe()
        .map_err(|e| e.to_string())
        .and_then(|source| snapshot(&source, &root, "driver"))
    {
        Ok(path) => path,
        Err(e) => {
            crate::diag!("search-bench: {e}");
            return 1;
        }
    };
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
        let mut run = Run {
            started_unix: now_unix(),
            finished_unix: 0,
            rows: Vec::new(),
        };
        for (g, ids) in groups(opts, queries).iter().enumerate() {
            let out = root.join(format!("run-{i}-{g}.json"));
            match child_run(&driver, opts, ids, &out) {
                Ok(part) => run.rows.extend(part.rows),
                Err(e) => {
                    crate::diag!("search-bench: {e}");
                    crate::diag!(
                        "search-bench: retained failed run state at {}",
                        root.display()
                    );
                    return 1;
                }
            }
        }
        run.finished_unix = now_unix();
        runs.push(run);
    }
    finish(opts, "in-process", None, &runs, Some(&root), true)
}

/// The rows of one run, split into the groups that each start from an empty
/// state: one per shape, in corpus order, or the whole selection with
/// `--one-ledger`. Each group is named by its row ids, so a child runs exactly
/// the rows the parent selected.
fn groups(opts: &Options, queries: &[BenchQuery]) -> Vec<Vec<String>> {
    if opts.one_ledger {
        return vec![queries.iter().map(|q| q.id.clone()).collect()];
    }
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for q in queries {
        match out.iter_mut().find(|(shape, _)| *shape == q.shape) {
            Some((_, ids)) => ids.push(q.id.clone()),
            None => out.push((q.shape.clone(), vec![q.id.clone()])),
        }
    }
    out.into_iter().map(|(_, ids)| ids).collect()
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
/// The rows are named by id: the parent selected them once.
fn child_args(opts: &Options, ids: &[String], out: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "search-bench".into(),
        "--queries".into(),
        opts.queries.display().to_string(),
        "--set".into(),
        "all".into(),
        "--only".into(),
        ids.join(","),
        "--pace-ms".into(),
        opts.pace.as_millis().to_string(),
        "--max-results".into(),
        opts.max_results.to_string(),
        "--keep-state".into(),
        "--out".into(),
        out.display().to_string(),
    ];
    if opts.verbose {
        a.push("-v".into());
    }
    a
}

/// A build/install changing its original path must not change a run halfway
/// through. Each run tree owns an executable snapshot, including on Windows.
fn snapshot(source: &Path, root: &Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let target = root.join(name);
    std::fs::copy(source, &target)
        .map_err(|e| format!("cannot snapshot {}: {e}", source.display()))?;
    Ok(target)
}

/// Cold measurements ignore the user's file settings. Environment switches
/// remain explicit experiment inputs and are recorded in the artifact.
fn cold_command(exe: &Path, cache: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .env("DONSETCH_CACHE_DIR", cache)
        .env_remove("DONSETCH_CONFIG")
        .env("DONSETCH_NO_UPDATE_CHECK", "1")
        .env("DONSETCH_NO_CONFIG_FILE", "1");
    command
}

/// One child run: same binary, empty cache, artifact on disk.
fn refusal_journal(opts: &Options) -> PathBuf {
    opts.out
        .as_deref()
        .and_then(Path::parent)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("bench/results"))
        .join("search-refusals.json")
}

fn seed_refusals(opts: &Options, cache: &Path) -> Result<(), String> {
    let target = cache.join("search-ledger.json");
    crate::search::ledger::carry_refusals(
        &crate::paths::cache_dir().join("search-ledger.json"),
        &target,
    )?;
    crate::search::ledger::carry_refusals(&refusal_journal(opts), &target)
}

fn save_refusals(opts: &Options, cache: &Path) -> Result<(), String> {
    crate::search::ledger::carry_refusals(&cache.join("search-ledger.json"), &refusal_journal(opts))
}

fn child_run(exe: &Path, opts: &Options, ids: &[String], out: &Path) -> Result<Run, String> {
    let cache = out.with_extension("cache");
    std::fs::create_dir_all(&cache)
        .map_err(|e| format!("cannot create {}: {e}", cache.display()))?;
    seed_refusals(opts, &cache)?;
    let status = cold_command(exe, &cache)
        .args(child_args(opts, ids, out))
        .env(CHILD_ENV, "1")
        .status()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    // Exit 3 is the run's own verdict: rows broke a contract (`must_not`,
    // redirect, stale, undated, foreign). That is data the parent merges and
    // reports, not a failure of the run — the same rule `exit_code` applies to
    // a single-process run.
    if !status.success() && status.code() != Some(3) {
        return Err(format!("child run {} exited with {status}", out.display()));
    }
    save_refusals(opts, &cache)?;
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
    #[serde(default)]
    engines: Vec<crate::search::EngineReport>,
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
    let source = bin.to_path_buf();
    let root = temp_root();
    let frozen = match snapshot(bin, &root, "measured") {
        Ok(path) => path,
        Err(e) => {
            crate::diag!("search-bench: {e}");
            return 1;
        }
    };
    let bin = frozen.as_path();
    let version = bin_version(bin);
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
        let started_unix = now_unix();
        let mut rows = Vec::new();
        for (g, ids) in groups(opts, queries).iter().enumerate() {
            let cache = root.join(format!("bin-cache-{i}-{g}"));
            if let Err(e) = std::fs::create_dir_all(&cache) {
                crate::diag!("search-bench: cannot create {}: {e}", cache.display());
                return 1;
            }
            if let Err(error) = seed_refusals(opts, &cache) {
                crate::diag!("search-bench: {error}");
                return 1;
            }
            for (n, q) in queries.iter().filter(|q| ids.contains(&q.id)).enumerate() {
                if n > 0 || !rows.is_empty() {
                    tokio::time::sleep(opts.pace).await;
                }
                let row = external_row(bin, &cache, opts, q);
                if let Err(error) = crate::search::ledger::carry_report_refusals(
                    &cache.join("search-ledger.json"),
                    &row.reports,
                )
                .and_then(|()| save_refusals(opts, &cache))
                {
                    crate::diag!("search-bench: {error}");
                    return 1;
                }
                rows.push(row);
            }
        }
        runs.push(Run {
            started_unix,
            finished_unix: now_unix(),
            rows,
        });
    }
    let mode = format!("bin:{}", source.display());
    let mut measured = opts.clone();
    measured.bin = Some(frozen.clone());
    finish(
        &measured,
        &mode,
        version.as_deref(),
        &runs,
        Some(&root),
        true,
    )
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
    let out = cold_command(bin, cache)
        .args([
            "search",
            "--json",
            "--max-results",
            max.as_str(),
            "--deadline-ms",
            deadline.as_str(),
            "--",
            q.query.as_str(),
        ])
        .output();
    let ms = t0.elapsed().as_millis() as u64;
    let (answers, census, reports) = match out {
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
                    (answers, census, doc.meta.engines)
                }
                Err(e) => (
                    Vec::new(),
                    format!(
                        "unreadable answer: {e}: {}",
                        text.chars().take(120).collect::<String>()
                    ),
                    Vec::new(),
                ),
            }
        }
        Err(e) => (
            Vec::new(),
            format!("cannot run {}: {e}", bin.display()),
            Vec::new(),
        ),
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
            if !crate::search::native_script(lang, &row.title, &row.snippet) {
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
        evidence: answers,
        ranking: None,
        reports,
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn replacing_the_build_path_cannot_change_the_measured_executable() {
        let root = std::env::temp_dir().join(format!("bench-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("build");
        std::fs::write(&source, b"measured version").unwrap();
        let frozen = snapshot(&source, &root, "driver").unwrap();
        std::fs::write(&source, b"later rebuild").unwrap();
        assert_eq!(std::fs::read(frozen).unwrap(), b"measured version");
        let command = cold_command(&source, &root);
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("DONSETCH_NO_CONFIG_FILE")],
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(env[std::ffi::OsStr::new("DONSETCH_CONFIG")], None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
