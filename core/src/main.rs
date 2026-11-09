//! v5 fetch core — carried identity and state.
//!
//! Commands
//!   fetch <url>          one encounter, portfolio selection, state spent/earned
//!   probe --targets <f>  identity matrix: every portfolio identity against every target
//!   bench --targets <f>  cold per-process measurements, paired with curl
//!   identities           print the portfolio and the UA each identity presents
//!   state [show|reset]   inspect or clear the state store (reset also clears the page cache)
//!   cache clear          forget every cached page, keep what was learned
//!
//! The challenge engine runs the site's own program in-process, in V8, when a
//! wall demands it. No browser is linked and none is launched.

use donsetch_core::contract::{self, Contract};
use donsetch_core::fetch::{self, class_name, Options, Worker};
use donsetch_core::{classify, envelope, identity, state};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Manifest {
    targets: Vec<Target>,
}

#[derive(Clone, Deserialize)]
struct Target {
    id: String,
    url: String,
    #[serde(default)]
    contract: Option<Contract>,
}

#[derive(Serialize, Clone)]
struct Equivalence {
    matched: usize,
    checked: usize,
}

#[derive(Serialize)]
struct TargetReport {
    id: String,
    url: String,
    class: String,
    transport: String,
    items: usize,
    sample: Vec<String>,
    sample_urls: Vec<String>,
    equivalence: Option<Equivalence>,
    identity: Option<String>,
    ok: bool,
    ms: f64,
    ms_first: f64,
    /// "fresh" (no request) or "revalidated" (304) when the page cache answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache: Option<String>,
    status: u16,
    title: String,
    text_chars: usize,
    attempts: Vec<fetch::Attempt>,
    failures: Vec<String>,
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = arguments.first().map(String::as_str).unwrap_or("help");
    let flags = Flags::parse(&arguments[arguments.len().min(1)..]);
    let status = match command {
        "fetch" => command_fetch(&arguments, &flags),
        "probe" => command_probe(&flags),
        "bench" => command_bench(&flags),
        "identities" => command_identities(),
        "scorecard" => command_scorecard(&flags),
        "state" => command_state(&arguments, &flags),
        "cache" => {
            donsetch_core::cache::PageCache::for_state().clear();
            println!("page cache cleared");
            0
        }
        _ => {
            eprintln!(
                "usage: donsetch-core <fetch <url>|probe --targets <file>|bench --targets <file>|identities|scorecard|state>"
            );
            2
        }
    };
    std::process::exit(status);
}

struct Flags {
    values: std::collections::HashMap<String, String>,
    switches: std::collections::HashSet<String>,
}

impl Flags {
    fn parse(arguments: &[String]) -> Self {
        let mut values = std::collections::HashMap::new();
        let mut switches = std::collections::HashSet::new();
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            if let Some(name) = argument.strip_prefix("--") {
                if let Some(equal) = name.find('=') {
                    values.insert(name[..equal].to_string(), name[equal + 1..].to_string());
                } else if index + 1 < arguments.len() && !arguments[index + 1].starts_with("--") {
                    values.insert(name.to_string(), arguments[index + 1].clone());
                    index += 1;
                } else {
                    switches.insert(name.to_string());
                }
            }
            index += 1;
        }
        Self { values, switches }
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn number(&self, name: &str) -> Option<u64> {
        self.value(name).and_then(|value| value.parse().ok())
    }
}

fn command_identities() -> i32 {
    let headers = std::env::args().any(|argument| argument == "--headers");
    for identity in identity::PORTFOLIO {
        if headers {
            println!("== {}", identity.name);
            if let Some(emulation) = identity.emulation() {
                for (name, value) in emulation.headers.iter() {
                    println!(
                        "   {}: {}",
                        name.as_str(),
                        value.to_str().unwrap_or("<binary>")
                    );
                }
            } else {
                for (name, value) in identity.headers() {
                    println!("   {name}: {value}");
                }
            }
            continue;
        }
        // What the wire sends: the identity's own headers over its profile's.
        let user_agent = identity.script_view()["userAgent"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        println!(
            "{:<8} {:<10} {}",
            identity.name,
            format!("{:?}", identity.family),
            user_agent
        );
        println!("         {}", identity.note);
    }
    0
}

/// What one identity actually put on the wire, as a public fingerprint echo
/// reads it.
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct WirePrint {
    ja4: String,
    peetprint: String,
    akamai: String,
    user_agent: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ScoreBaseline {
    endpoint: String,
    identities: std::collections::BTreeMap<String, WirePrint>,
}

/// The live wire every browser identity presents, diffed against the recorded
/// baseline.
///
/// An identity is a claim until it is measured, and the claim rots quietly: a
/// dependency bump, a profile edit or a browser release changes the TLS/HTTP-2
/// shape while the user agent still says the old version. One request per
/// identity to a public fingerprint echo, every field compared; `--record`
/// rewrites the baseline deliberately, never automatically.
fn command_scorecard(flags: &Flags) -> i32 {
    const ENDPOINT: &str = "https://tls.peet.ws/api/all";
    const BASELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scorecard-baseline.json");
    let field = |json: &serde_json::Value, path: [&str; 2]| -> String {
        json.get(path[0])
            .and_then(|inner| inner.get(path[1]))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut measured = std::collections::BTreeMap::new();
    let mut unreachable = Vec::new();
    for identity in identity::PORTFOLIO
        .iter()
        .filter(|identity| identity.family == identity::Family::Browser)
    {
        let options = Options {
            capture_body: true,
            proxy: None,
            cache: false,
            revalidate: false,
            arrival: false,
            max_tries: 1,
            deadline: Duration::from_secs(10),
            contract: Some(Contract::generic()),
            persist: false,
            identity: Some(identity.name.to_string()),
            pace_ms: 900,
            engine: envelope::Limits::from_env(),
            policy: Default::default(),
        };
        let mut session = Worker::new(false);
        let outcome = session.fetch(ENDPOINT, &options);
        let Some(body) = outcome.body.as_deref() else {
            unreachable.push(format!("{}: no body ({:?})", identity.name, outcome.class));
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
            unreachable.push(format!("{}: the echo did not answer JSON", identity.name));
            continue;
        };
        measured.insert(
            identity.name.to_string(),
            WirePrint {
                ja4: field(&json, ["tls", "ja4"]),
                peetprint: field(&json, ["tls", "peetprint_hash"]),
                akamai: field(&json, ["http2", "akamai_fingerprint_hash"]),
                user_agent: json
                    .get("user_agent")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
        );
    }
    if flags.switches.contains("record") {
        let baseline = ScoreBaseline {
            endpoint: ENDPOINT.to_string(),
            identities: measured,
        };
        let Ok(json) = serde_json::to_string_pretty(&baseline) else {
            eprintln!("scorecard: cannot serialise the baseline");
            return 1;
        };
        if let Err(error) = std::fs::write(BASELINE, format!("{json}\n")) {
            eprintln!("scorecard: cannot write {BASELINE}: {error}");
            return 1;
        }
        println!(
            "recorded {} identities in {BASELINE}",
            baseline.identities.len()
        );
        return 0;
    }
    if flags.switches.contains("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&measured).unwrap_or_default()
        );
    }
    let Some(raw) = std::fs::read_to_string(BASELINE).ok() else {
        eprintln!("scorecard: no baseline at {BASELINE}; record one with --record");
        return 1;
    };
    let Ok(baseline) = serde_json::from_str::<ScoreBaseline>(&raw) else {
        eprintln!("scorecard: {BASELINE} does not parse");
        return 1;
    };
    let mut failures = unreachable;
    let mut checked = 0usize;
    for (name, want) in &baseline.identities {
        let Some(got) = measured.get(name) else {
            failures.push(format!("{name}: no measurement this run"));
            continue;
        };
        checked += 1;
        let mut drift = Vec::new();
        for (field, want, got) in [
            ("ja4", &want.ja4, &got.ja4),
            ("peetprint", &want.peetprint, &got.peetprint),
            ("akamai", &want.akamai, &got.akamai),
            ("user-agent", &want.user_agent, &got.user_agent),
        ] {
            if want != got {
                drift.push(format!("{field}: {want} -> {got}"));
            }
        }
        if drift.is_empty() {
            if !flags.switches.contains("quiet") {
                println!("{name:<14} ok  {}", got.ja4);
            }
        } else {
            failures.push(format!("{name}: {}", drift.join("; ")));
        }
    }
    for failure in &failures {
        println!("DRIFT {failure}");
    }
    if failures.is_empty() {
        println!("scorecard: {checked} identities match the baseline");
        0
    } else {
        println!(
            "scorecard: {} of {} identities drifted or were unmeasurable; re-capture deliberately \
and record with `donsetch-core scorecard --record`",
            failures.len(),
            baseline.identities.len()
        );
        1
    }
}

fn command_state(arguments: &[String], flags: &Flags) -> i32 {
    let action = arguments.get(2).map(String::as_str).unwrap_or("show");
    match action {
        "reset" => {
            donsetch_core::cache::PageCache::for_state().clear();
            if let Some(path) = state::state_path() {
                let _ = std::fs::remove_file(&path);
                println!("cleared {}", path.display());
            }
            0
        }
        _ => {
            let state = state::State::load();
            let _ = flags;
            match serde_json::to_string_pretty(&state) {
                Ok(raw) => {
                    println!("{raw}");
                    0
                }
                Err(error) => {
                    eprintln!("state: {error}");
                    1
                }
            }
        }
    }
}

fn command_fetch(arguments: &[String], flags: &Flags) -> i32 {
    let Some(url) = arguments.get(1) else {
        eprintln!("fetch: a URL is required");
        return 2;
    };
    let contract = flags
        .value("contract")
        .and_then(|raw| serde_json::from_str::<Contract>(raw).ok())
        .or_else(|| flags.value("contract-file").and_then(read_contract))
        .unwrap_or_else(Contract::generic);
    let options = Options {
        capture_body: flags.value("body").is_some(),
        proxy: flags.value("proxy").map(str::to_string),
        cache: !flags.switches.contains("no-cache"),
        revalidate: flags.switches.contains("revalidate"),
        arrival: flags.switches.contains("arrival"),
        max_tries: flags
            .number("max-tries")
            .map(|value| value as usize)
            // Three attempts is the product default: enough to find the identity
            // an origin accepts, inside the 3 s budget. --single keeps it to one.
            .unwrap_or(if flags.switches.contains("single") {
                1
            } else {
                3
            }),
        deadline: Duration::from_millis(flags.number("deadline").unwrap_or(3000)),
        contract: Some(contract),
        persist: !flags.switches.contains("no-state"),
        identity: flags.value("identity").map(str::to_string),
        pace_ms: flags
            .number("pace")
            .map(|value| value as i64)
            .unwrap_or(900),
        engine: envelope::Limits::from_env(),
        policy: Default::default(),
    };
    let mut session = Worker::new(options.persist);
    let outcome = session.fetch(url, &options);
    if flags.switches.contains("json") {
        println!("{}", serde_json::to_string(&outcome).unwrap_or_default());
    } else {
        print_result(&outcome);
    }
    if let Some(path) = flags.value("body") {
        // Inspection only: the response body exactly as it arrived.
        if let Some(body) = &outcome.body {
            let _ = std::fs::write(path, body);
        }
    }
    if outcome.class == classify::Class::Content {
        0
    } else {
        1
    }
}

fn read_contract(path: &str) -> Option<Contract> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn print_result(outcome: &fetch::Outcome) {
    println!(
        "{} {} in {:.0} ms via {} (first attempt {:.0} ms)",
        class_name(outcome.class),
        outcome.status,
        outcome.ms_total,
        outcome.identity.as_deref().unwrap_or("-"),
        outcome.ms_first
    );
    if let Some(cache) = &outcome.cache {
        println!("  page cache: {cache}");
    }
    for attempt in &outcome.attempts {
        println!(
            "  {:<13} {:<15} {:>3} {:>7.0} ms {:>8} B {:<6} {}",
            attempt.identity,
            class_name(attempt.class),
            attempt.status,
            attempt.ms,
            attempt.bytes,
            attempt.version,
            attempt.note.clone().unwrap_or_default()
        );
    }
    if !outcome.title.is_empty() {
        println!(
            "  title: {:?} ({} text chars)",
            outcome.title, outcome.text_chars
        );
    }
    if !outcome.evidence.is_empty() {
        println!("  evidence: {}", outcome.evidence.join(", "));
    }
    if outcome.spent_state {
        println!("  spent carried state");
    }
}

fn load_manifest(flags: &Flags) -> Option<Manifest> {
    let path = flags.value("targets")?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Identity matrix: every identity against every target, no selection, no state.
fn command_probe(flags: &Flags) -> i32 {
    let Some(manifest) = load_manifest(flags) else {
        eprintln!("probe: --targets <manifest.json> is required");
        return 2;
    };
    let chosen: Vec<&'static str> = match flags.value("identities") {
        Some(list) => list
            .split(',')
            .filter_map(|name| identity::lookup(name.trim()).map(|identity| identity.name))
            .collect(),
        None => identity::names(),
    };
    let timeout = Duration::from_millis(flags.number("deadline").unwrap_or(8000));
    let mut session = Worker::new(false);
    let mut rows: Vec<serde_json::Value> = Vec::new();
    println!("{:<13} {}", "target", chosen.join("  "));
    for target in &manifest.targets {
        let contract = target.contract.clone().unwrap_or_else(Contract::generic);
        let mut cells = Vec::new();
        for name in &chosen {
            let attempt = session.probe_one(name, &target.url, &contract, timeout);
            // Space identities out: the probe measures acceptance, not burst rate.
            std::thread::sleep(Duration::from_millis(250));
            let cell = format!("{}:{:.0}", short(class_name(attempt.class)), attempt.ms);
            cells.push(cell.clone());
            rows.push(serde_json::json!({
                "target": target.id,
                "identity": name,
                "class": class_name(attempt.class),
                "status": attempt.status,
                "ms": attempt.ms,
                "bytes": attempt.bytes,
                "note": attempt.note,
            }));
            print!("{cell:<width$}", width = name.len().max(6) + 8);
        }
        println!();
    }
    if let Some(path) = flags.value("out") {
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(&rows).unwrap_or_default(),
        );
        println!("wrote {path}");
    }
    0
}

fn short(class: &str) -> &'static str {
    match class {
        "content" => "OK",
        "wall_identity" => "ident",
        "wall_state" => "state",
        "wall_contract" => "contr",
        "wall_interaction" => "human",
        "wall_program" => "prog",
        "shell" => "shell",
        "http_error" => "http",
        "network" => "net",
        _ => "inc",
    }
}

/// Cold benches: each fetch is a fresh process, exactly like a one-shot CLI use.
/// Curl runs the same URL in the same window and the same contract is applied to
/// its body, so the comparison is like for like.
fn command_bench(flags: &Flags) -> i32 {
    let Some(manifest) = load_manifest(flags) else {
        eprintln!("bench: --targets <manifest.json> is required");
        return 2;
    };
    let runs = flags.number("runs").unwrap_or(1).max(1);
    let with_curl = flags.switches.contains("curl");
    // Cold mode is the default: no carried state, one identity, one request.
    // --persist measures the steady state where discovery was already paid.
    let persist = flags.switches.contains("persist");
    let discover = flags.switches.contains("discover");
    let deadline = flags.number("deadline").unwrap_or(3000);
    // One lane for the whole bench: the egress class is the variable under test.
    let proxy = flags.value("proxy").map(str::to_string);
    let only = flags.value("only").map(|list| {
        list.split(',')
            .map(|id| id.trim().to_string())
            .collect::<Vec<_>>()
    });
    let shell: PathBuf = std::env::current_exe().expect("current executable");
    let mut reports: Vec<TargetReport> = Vec::new();
    let mut curl_rows: Vec<serde_json::Value> = Vec::new();

    for run in 0..runs {
        for target in &manifest.targets {
            if let Some(only) = &only {
                if !only.contains(&target.id) {
                    continue;
                }
            }
            let contract = target.contract.clone().unwrap_or_else(Contract::generic);
            // Fresh process, no carried state: the honest first-contact number.
            let started = Instant::now();
            let mut args: Vec<String> = vec![
                "fetch".into(),
                target.url.clone(),
                "--json".into(),
                "--deadline".into(),
                deadline.to_string(),
                "--contract".into(),
                serde_json::to_string(&contract).unwrap_or_default(),
            ];
            if !persist {
                args.push("--no-state".into());
            }
            if let Some(proxy) = &proxy {
                args.push("--proxy".into());
                args.push(proxy.clone());
            }
            if flags.switches.contains("no-cache") {
                args.push("--no-cache".into());
            }
            if discover {
                args.push("--discover".into());
            } else {
                args.push("--single".into());
            }
            let output = std::process::Command::new(&shell).args(&args).output();
            let process_ms = started.elapsed().as_secs_f64() * 1000.0;
            let outcome: serde_json::Value = output
                .ok()
                .and_then(|output| serde_json::from_slice(&output.stdout).ok())
                .unwrap_or(serde_json::Value::Null);
            let report = TargetReport {
                id: target.id.clone(),
                url: target.url.clone(),
                class: outcome["class"].as_str().unwrap_or("network").to_string(),
                transport: outcome["transport"]
                    .as_str()
                    .unwrap_or("document")
                    .to_string(),
                items: outcome["items"].as_u64().unwrap_or(0) as usize,
                sample: outcome["sample"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                sample_urls: outcome["sample_urls"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                equivalence: None,
                identity: outcome["identity"].as_str().map(str::to_string),
                ok: outcome["class"].as_str() == Some("content"),
                ms: process_ms,
                ms_first: outcome["ms_first"].as_f64().unwrap_or(0.0),
                cache: outcome["cache"].as_str().map(str::to_string),
                status: outcome["status"].as_u64().unwrap_or(0) as u16,
                title: outcome["title"].as_str().unwrap_or_default().to_string(),
                text_chars: outcome["text_chars"].as_u64().unwrap_or(0) as usize,
                attempts: serde_json::from_value(outcome["attempts"].clone()).unwrap_or_default(),
                failures: outcome["failures"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let mut report = report;
            if let Some(directory) = flags.value("browser-dir") {
                if report.transport == "data" && !report.sample.is_empty() {
                    if let Ok(page) =
                        std::fs::read_to_string(format!("{directory}/{}.txt", report.id))
                    {
                        let page = normalize_text(&page);
                        let checked = report.sample.len();
                        let matched = report
                            .sample
                            .iter()
                            .filter(|title| {
                                let needle = normalize_text(title);
                                needle.len() >= 12 && page.contains(&needle)
                            })
                            .count();
                        report.equivalence = Some(Equivalence { matched, checked });
                        if matched < 3 {
                            report.ok = false;
                            report.class = format!("{}+weak-equivalence", report.class);
                        }
                    }
                }
            }
            let report = report;
            eprintln!(
                "run {} {} {:<16} {:>8.0} ms  via {}",
                run + 1,
                target.id,
                report.class,
                report.ms,
                report.identity.as_deref().unwrap_or("-")
            );
            if with_curl {
                let curl = run_curl(&target.url, &contract, proxy.as_deref());
                curl_rows.push(serde_json::json!({
                    "target": target.id,
                    "run": run + 1,
                    "ok": curl.0,
                    "ms": curl.1,
                    "status": curl.2,
                }));
            }
            reports.push(report);
            // Keep the pacing honest: no machine-gun bursts at one origin.
            std::thread::sleep(Duration::from_millis(900));
        }
    }

    let summary = summarize(&reports, &curl_rows);
    let document = serde_json::json!({
        "schema": "donsetch-core/bench/1",
        "runs": runs,
        "targets": reports,
        "curl": curl_rows,
        "summary": summary,
    });
    if let Some(path) = flags.value("out") {
        if let Ok(raw) = serde_json::to_string_pretty(&document) {
            let _ = std::fs::write(path, raw);
        }
    }
    print_summary(&summary);
    0
}

/// Lowercase words only: the same text survives markdown, HTML and JSON framing.
fn normalize_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_curl(url: &str, contract: &Contract, proxy: Option<&str>) -> (bool, f64, u16) {
    let started = Instant::now();
    let mut args: Vec<&str> = vec!["-sS", "-L", "--compressed", "-m", "10"];
    if let Some(proxy) = proxy {
        args.push("--proxy");
        args.push(proxy);
    }
    args.extend(["-o", "-", "-w", "\n%{http_code}", url]);
    let output = std::process::Command::new("curl").args(&args).output();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    match output {
        Ok(output) => {
            let raw = String::from_utf8_lossy(&output.stdout).to_string();
            let (body, status) = match raw.rfind('\n') {
                Some(index) => (
                    raw[..index].to_string(),
                    raw[index + 1..].trim().parse::<u16>().unwrap_or(0),
                ),
                None => (raw, 0),
            };
            let passed =
                (200..400).contains(&status) && contract::verify(&body, url, contract).passed;
            (passed, ms, status)
        }
        Err(_) => (false, ms, 0),
    }
}

fn summarize(reports: &[TargetReport], curl_rows: &[serde_json::Value]) -> serde_json::Value {
    let mut ok = 0;
    let mut by_class = std::collections::BTreeMap::new();
    let mut latencies: Vec<f64> = Vec::new();
    for report in reports {
        if report.ok {
            ok += 1;
        }
        *by_class.entry(report.class.clone()).or_insert(0) += 1;
        latencies.push(report.ms);
    }
    latencies.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let percentile = |p: f64| -> f64 {
        if latencies.is_empty() {
            return 0.0;
        }
        let index = ((latencies.len() as f64 - 1.0) * p).round() as usize;
        latencies[index]
    };
    let curl_ok = curl_rows
        .iter()
        .filter(|row| row["ok"].as_bool() == Some(true))
        .count();
    let mut ratios: Vec<f64> = Vec::new();
    for row in curl_rows {
        let curl_ms = row["ms"].as_f64().unwrap_or(0.0);
        let target = row["target"].as_str().unwrap_or_default();
        if let Some(report) = reports
            .iter()
            .find(|report| report.id == target && report.ok && row["ok"].as_bool() == Some(true))
        {
            if curl_ms > 0.0 {
                ratios.push(report.ms / curl_ms);
            }
        }
    }
    ratios.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    serde_json::json!({
        "fetches": reports.len(),
        "content": ok,
        "classes": by_class,
        "content_rate": if reports.is_empty() { 0.0 } else { ok as f64 / reports.len() as f64 },
        "ms_p50": percentile(0.5),
        "ms_p95": percentile(0.95),
        "ms_max": latencies.last().copied().unwrap_or(0.0),
        "curl_content": curl_ok,
        "curl_fetches": curl_rows.len(),
        "median_ratio_vs_curl": if ratios.is_empty() { 0.0 } else { ratios[ratios.len() / 2] },
        "ratios": ratios,
    })
}

fn print_summary(summary: &serde_json::Value) {
    println!(
        "content {}/{} ({:.0}%)  p50 {:.0} ms  p95 {:.0} ms  max {:.0} ms  classes {}",
        summary["content"].as_u64().unwrap_or(0),
        summary["fetches"].as_u64().unwrap_or(0),
        summary["content_rate"].as_f64().unwrap_or(0.0) * 100.0,
        summary["ms_p50"].as_f64().unwrap_or(0.0),
        summary["ms_p95"].as_f64().unwrap_or(0.0),
        summary["ms_max"].as_f64().unwrap_or(0.0),
        summary["classes"],
    );
    if summary["curl_fetches"].as_u64().unwrap_or(0) > 0 {
        println!(
            "curl content {}/{}  median ratio {:.2}",
            summary["curl_content"].as_u64().unwrap_or(0),
            summary["curl_fetches"].as_u64().unwrap_or(0),
            summary["median_ratio_vs_curl"].as_f64().unwrap_or(0.0),
        );
    }
    let _ = std::io::stdout().flush();
}
