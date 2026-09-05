//! CLI adapter: thin layer over the shared tool core.
//!
//! Parses argv into the same JSON args the MCP dispatcher receives,
//! calls the exact same `call_tool`, and renders the result.
//! Zero logic duplication : all behavior lives in the core.

use crate::DISPLAY_NAME;
use crate::mcp::server::{self, Daemon};
use crate::spec;
use clap::error::ErrorKind as ClapErrorKind;
use serde_json::{Value, json};
use std::sync::Arc;

// ── Exit codes ───────────────────────────────────────────────

pub const EXIT_OK: u8 = 0;
pub const EXIT_PERMANENT: u8 = 1;
pub const EXIT_TRANSIENT: u8 = 2;
pub const EXIT_WALLED: u8 = 3;

// ── Entry point ──────────────────────────────────────────────

/// Run a tool command (fetch/search/crawl). Returns exit code.
pub async fn run(cmd: &str, args: &[String]) -> u8 {
    let Some(tool) = spec::by_cli_cmd(cmd) else {
        eprintln!("donsetch: unknown command '{cmd}'");
        return EXIT_PERMANENT;
    };

    // Build clap command from the spec table and parse.
    // `try_get_matches_from` expects the first element to be
    // the binary name (like std::env::args).
    let cli = spec::cli_command(tool);
    let mut full_args = vec![cmd.to_string()];
    full_args.extend_from_slice(args);
    let matches = match cli.try_get_matches_from(&full_args) {
        Ok(m) => m,
        Err(e) => match e.kind() {
            ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => {
                print!("{e}");
                return EXIT_OK;
            }
            _ => {
                eprintln!("{e}");
                return EXIT_PERMANENT;
            }
        },
    };
    let json_mode = matches.get_flag("json");
    let quiet = matches.get_flag("quiet");

    // Construct the shared Daemon (same as MCP).
    let daemon = match Daemon::new().await {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("[{cmd}] error: failed to initialize: {e}");
            return EXIT_PERMANENT;
        }
    };

    // Build the JSON args from parsed matches.
    let base_args = spec::matches_to_json(tool, &matches);

    if tool.cli_cmd == "fetch" {
        let urls: Vec<String> = matches
            .get_many::<String>("url")
            .map(|u| u.cloned().collect())
            .unwrap_or_default();
        if urls.len() > 1 {
            let code = run_bulk_fetch(&daemon, tool, &base_args, &urls, json_mode, quiet).await;
            daemon.shutdown().await;
            return code;
        }
    }

    // Single call.
    let params = json!({"name": tool.name, "arguments": base_args});
    let result = match server::call_tool(&daemon, &params).await {
        Ok(v) => v,
        Err((code, msg)) => {
            eprintln!("[{cmd}] error (code {code}): {msg}");
            daemon.shutdown().await;
            return EXIT_PERMANENT;
        }
    };
    let code = render_result(&result, json_mode, quiet, cmd);
    daemon.shutdown().await;
    code
}

/// Parallel bulk fetch: one call_tool per URL, results in order.
async fn run_bulk_fetch(
    daemon: &Arc<Daemon>,
    tool: &spec::ToolSpec,
    base_args: &Value,
    urls: &[String],
    json_mode: bool,
    quiet: bool,
) -> u8 {
    let futures: Vec<_> = urls
        .iter()
        .map(|url| {
            let mut args = base_args.clone();
            if let Some(obj) = args.as_object_mut() {
                obj.insert("url".into(), json!(url));
            }
            let params = json!({"name": tool.name, "arguments": args});
            let daemon = daemon.clone();
            async move { server::call_tool(&daemon, &params).await }
        })
        .collect();

    let results = futures_util::future::join_all(futures).await;
    if json_mode {
        let mut out = Vec::new();
        let mut worst = EXIT_OK;
        let rank = |code: u8, worst: &mut u8| {
            // transient(2) + walled(3) must not collapse to
            // permanent(1): an agent gating on the exit code loses
            // the retry distinction. Severity order: OK < walled <
            // transient < permanent.
            let sev = match code {
                EXIT_OK => 0,
                EXIT_WALLED => 1,
                EXIT_TRANSIENT => 2,
                _ => 3,
            };
            let cur = match *worst {
                EXIT_OK => 0,
                EXIT_WALLED => 1,
                EXIT_TRANSIENT => 2,
                _ => 3,
            };
            if sev > cur {
                *worst = code;
            }
        };
        for (i, result) in results.iter().enumerate() {
            match result {
                Ok(v) => {
                    let exit = exit_code_of(v);
                    rank(exit, &mut worst);
                    out.push(render_json_envelope(v, &urls[i]));
                }
                Err((code, msg)) => {
                    rank(*code as u8, &mut worst);
                    out.push(json!({
                        "ok": false,
                        "url": urls[i],
                        "error": {"code": code, "message": msg},
                    }));
                }
            }
        }
        let envelope = json!({"results": out});
        println!("{envelope}");
        worst
    } else {
        let mut worst_exit = EXIT_OK;
        for (i, result) in results.iter().enumerate() {
            if i > 0 {
                println!();
            }
            // Multi-fetch markers: a 12-URL batch of similar pages is
            // unparseable without an explicit per-result boundary.
            if urls.len() > 1 {
                println!("### [{}] {}", i + 1, urls[i]);
            }
            match result {
                Ok(v) => {
                    let code = render_result(v, false, quiet, "fetch");
                    if code != EXIT_OK {
                        worst_exit = code;
                    }
                }
                Err((code, msg)) => {
                    eprintln!("[fetch] error (code {code}): {msg}");
                    worst_exit = EXIT_PERMANENT;
                }
            }
        }
        worst_exit
    }
}

// ── Rendering ───────────────────────────────────────────────

/// Render a single call_tool result. Returns exit code.
fn render_result(result: &Value, json_mode: bool, quiet: bool, cmd: &str) -> u8 {
    let exit = exit_code_of(result);

    if json_mode {
        let envelope = render_json_envelope(result, "");
        println!("{envelope}");
        return exit;
    }

    // Extract content text. Skip [meta] blocks (metadata
    // embedded for MCP clients that drop text when
    // structuredContent is present; the CLI uses
    // structuredContent directly for its stats line).
    let content = result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.starts_with("[meta]"))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_error {
        // Strip redundant tool-name prefix from core error messages
        // (e.g. "crawl: url required" → "url required" since we already
        // prefix with [cmd] error:).
        let sep = format!("{cmd}: ");
        let msg = if content.starts_with(sep.as_str()) {
            &content[sep.len()..]
        } else {
            content.as_str()
        };
        eprintln!("[{cmd}] error: {msg}");
    } else {
        // Success: content goes to stdout.
        print!("{content}");
    }

    if !quiet && !is_error {
        eprintln!("{}", stats_line(cmd, result, content.len()));
    }

    exit
}

/// Read a field from model state first, then from the client-only
/// debug payload. Compact MCP contracts move transport telemetry to
/// `_meta`; the CLI still surfaces it.
fn pick<'a>(sc: &'a Value, dbg: &'a Value, name: &str) -> Option<&'a Value> {
    sc.get(name).or_else(|| dbg.get(name))
}

/// Build the compact one-line stats string for stderr.
fn stats_line(cmd: &str, result: &Value, content_len: usize) -> String {
    let sc = result.get("structuredContent").cloned().unwrap_or_default();
    let dbg = result
        .pointer("/_meta")
        .cloned()
        .map(|meta| {
            let ns = match cmd {
                "fetch" => "com.donsetch/fetch-debug",
                "search" => "com.donsetch/search-debug",
                "crawl" => "com.donsetch/crawl-debug",
                _ => "",
            };
            meta.get(ns).cloned().unwrap_or_default()
        })
        .unwrap_or_default();
    match cmd {
        "fetch" => {
            let tokens_est = pick(&sc, &dbg, "tokens_est")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let tier = pick(&sc, &dbg, "tier")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let verdict = pick(&sc, &dbg, "verdict")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let mut parts = vec![
                format!("{content_len} chars"),
                format!("~{tokens_est} tokens"),
                format!("tier {tier}"),
                verdict.to_string(),
            ];
            if let Some(off) = sc.get("next_offset").and_then(|v| v.as_u64())
                && off > 0
            {
                parts.push(format!("next_offset {off}"));
            }
            if sc.get("thin").and_then(|v| v.as_bool()).unwrap_or(false) {
                parts.push("thin".into());
            }
            format!("[{cmd}] ok · {}", parts.join(" · "))
        }
        "search" => {
            let n = sc
                .get("results")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let ms = pick(&sc, &dbg, "elapsed_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let provider = pick(&sc, &dbg, "provider")
                .and_then(|v| v.as_str())
                .unwrap_or("local");
            let mut parts = vec![
                format!("{n} results"),
                format!("{:.1}s", ms as f64 / 1000.0),
                format!("provider {provider}"),
            ];
            if sc.get("weak").and_then(|v| v.as_bool()).unwrap_or(false) {
                parts.push("weak consensus".into());
            }
            // Surface degraded engine health in plain mode so agents
            // can tell coverage dropped without parsing --json.
            if let Some(engines) = pick(&sc, &dbg, "engines").and_then(|e| e.as_array()) {
                let degraded: Vec<String> = engines
                    .iter()
                    .filter_map(|e| {
                        let status = e.get("status").and_then(|s| s.as_str())?;
                        let name = e.get("engine").and_then(|n| n.as_str())?;
                        if status == "ok" {
                            return None;
                        }
                        // Shorten status for compactness.
                        let short = if status.starts_with("blocked:") {
                            format!("{name} blocked")
                        } else {
                            format!("{name} {status}")
                        };
                        Some(short)
                    })
                    .collect();
                if !degraded.is_empty() {
                    parts.push(format!("degraded: {}", degraded.join(", ")));
                }
            }
            format!("[{cmd}] ok · {}", parts.join(" · "))
        }
        "crawl" => {
            let n = sc
                .get("pages")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let total: u64 = dbg
                .get("pages")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.get("chars").and_then(|c| c.as_u64()))
                        .sum()
                })
                .unwrap_or(0);
            let elapsed = pick(&sc, &dbg, "elapsed_s")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let stop = pick(&sc, &dbg, "stop")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let mut parts = vec![
                format!("{n} pages"),
                format!("{total} chars"),
                format!("{elapsed:.1}s"),
                format!("stop {stop}"),
            ];
            if let Some(r) = sc.get("resume").and_then(|v| v.as_str())
                && !r.is_empty()
            {
                parts.push(format!("resume {r}"));
            }
            format!("[{cmd}] ok · {}", parts.join(" · "))
        }
        _ => format!("[{cmd}] ok"),
    }
}

/// Extract exit code from a call_tool result Value.
fn exit_code_of(result: &Value) -> u8 {
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !is_error {
        return EXIT_OK;
    }
    match result.get("errorKind").and_then(|v| v.as_str()) {
        Some("walled") => EXIT_WALLED,
        Some("transient") => EXIT_TRANSIENT,
        _ => EXIT_PERMANENT,
    }
}

/// Build the --json envelope for a single result.
fn render_json_envelope(result: &Value, url: &str) -> Value {
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let content = result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.starts_with("[meta]"))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let sc = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    if is_error {
        let kind = result
            .get("errorKind")
            .and_then(|v| v.as_str())
            .unwrap_or("permanent");
        let mut envelope = json!({
            "ok": false,
            "error": {"kind": kind, "message": content},
        });
        // Errors carry structure too (next_action, escalation
        // trace, url, status) : surface it, don't drop it.
        if !sc.is_null() {
            envelope["meta"] = sc;
        }
        if !url.is_empty() {
            envelope["url"] = json!(url);
        }
        envelope
    } else {
        let mut envelope = json!({
            "ok": true,
            "content": content,
            "meta": machine_meta(result, &sc),
        });
        if !url.is_empty() {
            envelope["url"] = json!(url);
        }
        envelope
    }
}

/// Machine meta for the --json channel: the compact model surface
/// (structuredContent) plus the full machine view re-materialized
/// from the client-only debug namespace when the tool carries one
/// (search). The MCP contract keeps _meta out of the model's
/// context by design; the CLI channel is FOR machines, so it loses
/// nothing (the compact-contract PR had silently broken every
/// script that reads meta.results[].snippet; the live bench
/// caught it).
fn machine_meta(result: &Value, sc: &Value) -> Value {
    const SEARCH_MACHINE_FIELDS: &[&str] = &[
        "intent",
        "weak",
        "cached",
        "elapsed_ms",
        "provider",
        "results",
        "engines",
    ];
    let debug = result
        .get("_meta")
        .and_then(|m| m.get("com.donsetch/search-debug"))
        .filter(|p| p.is_object());
    let mut meta = sc.clone();
    if let Some(dbg) = debug.and_then(|p| p.as_object())
        && let Some(meta_obj) = meta.as_object_mut()
    {
        for field in SEARCH_MACHINE_FIELDS {
            if let Some(v) = dbg.get(*field) {
                meta_obj.insert((*field).to_string(), v.clone());
            }
        }
    }
    meta
}

// ── Top-level help ───────────────────────────────────────────

/// Print top-level help (when no subcommand given or --help).
pub fn print_top_help() {
    println!("{DISPLAY_NAME} : web research for AI agents: fetch, search, crawl");
    println!();
    println!("USAGE: donsetch <command> [args]");
    println!();
    println!("AGENT TOOLS:");
    for tool in spec::TOOLS {
        println!("  {:8} {}", tool.cli_cmd, tool.summary);
    }
    println!();
    println!("DISCOVERY:");
    println!(
        "  {:8} Print tool schemas as JSON (same as MCP tools/list)",
        "tools"
    );
    println!();
    println!("MANAGEMENT:");
    println!("  {:8} Start MCP server (JSON-RPC on stdio)", "mcp");
    println!("  {:8} Manage BYOK search provider keys", "keys");
    println!(
        "  {:8} Sign into a site: later fetches replay your session",
        "login"
    );
    println!("  {:8} Manage proxy configuration", "proxy");
    println!("  {:8} Quick status overview", "status");
    println!(
        "  {:8} Kill orphaned Chrome instances + clean stale locks",
        "stop"
    );
    println!("  {:8} Health check with auto-fix", "doctor");
    println!("  {:8} Self-update from GitHub Releases", "update");
    println!("  {:8} Revert to previous version", "rollback");
    println!("  {:8} Show version and build info", "version");
    println!();
    println!("Run `donsetch <command> --help` for parameters.");
    println!("Run `donsetch help <command>` for detailed help on a command.");
    println!();
    println!(
        "Exit codes: 0 success · 1 permanent error · 2 transient (retry) · 3 walled (try different source)"
    );
}

/// Print the tools/list JSON (for `donsetch tools`).
pub fn print_tools_json() {
    let list = crate::mcp::tools::list();
    println!("{list}");
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_success() {
        let v = json!({"content": [{"type": "text", "text": "ok"}], "isError": false});
        assert_eq!(exit_code_of(&v), EXIT_OK);
    }

    #[test]
    fn exit_code_permanent() {
        let v = json!({
            "content": [{"type": "text", "text": "bad url"}],
            "isError": true,
            "errorKind": "permanent"
        });
        assert_eq!(exit_code_of(&v), EXIT_PERMANENT);
    }

    #[test]
    fn exit_code_transient() {
        let v = json!({
            "content": [{"type": "text", "text": "timeout"}],
            "isError": true,
            "errorKind": "transient"
        });
        assert_eq!(exit_code_of(&v), EXIT_TRANSIENT);
    }

    #[test]
    fn exit_code_walled() {
        let v = json!({
            "content": [{"type": "text", "text": "captcha wall"}],
            "isError": true,
            "errorKind": "walled"
        });
        assert_eq!(exit_code_of(&v), EXIT_WALLED);
    }

    #[test]
    fn exit_code_no_kind_defaults_permanent() {
        let v = json!({
            "content": [{"type": "text", "text": "error"}],
            "isError": true
        });
        assert_eq!(exit_code_of(&v), EXIT_PERMANENT);
    }

    #[test]
    fn stats_line_fetch() {
        let v = json!({
            "structuredContent": {
                "total_chars": 15853,
                "next_offset": 15923,
                "thin": false
            },
            "_meta": {
                "com.donsetch/fetch-debug": {
                    "tokens_est": 4000,
                    "tier": "1",
                    "verdict": "ContentOk"
                }
            }
        });
        let s = stats_line("fetch", &v, 15853);
        assert!(s.contains("15853 chars"));
        assert!(s.contains("~4000 tokens"));
        assert!(s.contains("tier 1"));
        assert!(s.contains("ContentOk"));
        assert!(s.contains("next_offset 15923"));
    }

    #[test]
    fn stats_line_fetch_prefers_model_state() {
        // Field present in both surfaces: model state wins over debug.
        let v = json!({
            "structuredContent": { "tier": "2(ghost)", "verdict": "ContentOk" },
            "_meta": { "com.donsetch/fetch-debug": { "tier": "1", "verdict": "Blocked" } }
        });
        let s = stats_line("fetch", &v, 0);
        assert!(s.contains("tier 2(ghost)"));
        assert!(s.contains("ContentOk"));
    }

    #[test]
    fn stats_line_search() {
        let v = json!({
            "structuredContent": {"results": [{"rank": 1}, {"rank": 2}], "weak": true},
            "_meta": {
                "com.donsetch/search-debug": {"elapsed_ms": 2100, "provider": null}
            }
        });
        let s = stats_line("search", &v, 0);
        assert!(s.contains("2 results"));
        assert!(s.contains("weak consensus"));
    }

    #[test]
    fn stats_line_search_degraded_engines() {
        let v = json!({
            "structuredContent": {"results": [{"rank": 1}], "weak": false},
            "_meta": {
                "com.donsetch/search-debug": {
                    "elapsed_ms": 5000,
                    "provider": null,
                    "engines": [
                        {"engine": "bing", "status": "ok", "hits": 5, "ms": 800},
                        {"engine": "mojeek", "status": "blocked:429", "hits": 0, "ms": 100},
                        {"engine": "brave", "status": "timeout", "hits": 0, "ms": 5000}
                    ]
                }
            }
        });
        let s = stats_line("search", &v, 0);
        assert!(s.contains("degraded"));
        assert!(s.contains("mojeek blocked"));
        assert!(s.contains("brave timeout"));
        assert!(!s.contains("bing")); // healthy engine not mentioned
    }

    #[test]
    fn stats_line_search_all_engines_ok() {
        let sc = json!({
            "results": [{"title": "a"}],
            "elapsed_ms": 2000,
            "provider": null,
            "weak": false,
            "engines": [
                {"engine": "bing", "status": "ok", "hits": 5, "ms": 800},
                {"engine": "ddg", "status": "ok", "hits": 3, "ms": 600}
            ]
        });
        let s = stats_line("search", &sc, 0);
        assert!(!s.contains("degraded"));
    }

    #[test]
    fn stats_line_crawl() {
        let v = json!({
            "structuredContent": {
                "pages": [{"url": "https://e.com/1"}, {"url": "https://e.com/2"}],
                "stop": "MaxPages",
                "resume": "abc123"
            },
            "_meta": {
                "com.donsetch/crawl-debug": {
                    "pages": [{"chars": 5000}, {"chars": 3000}],
                    "elapsed_s": 12.3
                }
            }
        });
        let s = stats_line("crawl", &v, 0);
        assert!(s.contains("2 pages"));
        assert!(s.contains("8000 chars"));
        assert!(s.contains("stop MaxPages"));
        assert!(s.contains("resume abc123"));
    }

    #[test]
    fn json_envelope_success() {
        let v = json!({
            "content": [{"type": "text", "text": "# Title\nContent"}],
            "structuredContent": {"verdict": "ContentOk"},
            "isError": false
        });
        let env = render_json_envelope(&v, "");
        assert_eq!(env["ok"], json!(true));
        assert_eq!(env["content"], json!("# Title\nContent"));
        assert_eq!(env["meta"]["verdict"], json!("ContentOk"));
    }

    #[test]
    fn json_envelope_error() {
        let v = json!({
            "content": [{"type": "text", "text": "blocked by captcha"}],
            "isError": true,
            "errorKind": "walled"
        });
        let env = render_json_envelope(&v, "https://example.com");
        assert_eq!(env["ok"], json!(false));
        assert_eq!(env["error"]["kind"], json!("walled"));
        assert_eq!(env["url"], json!("https://example.com"));
    }
}
