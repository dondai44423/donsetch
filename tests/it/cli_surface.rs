//! The management-command CLI contract, on the real binary.
//!
//! Every case here was a measured defect on the shipped 4.10.0 binary:
//!
//! - `doctor --frobnicate` ran the whole health check (60 lines, exit 0)
//!   because the hand-parsed management commands ignored flags they did
//!   not recognize, while the clap-parsed tools refused them.
//! - `status --json` printed the human view.
//! - `donsetch help proxy` printed a stale copy of the proxy help that
//!   knew nothing of `proxy fetch|crawl on|off`, because the text lived
//!   in two places.
//! - `donsetch fetc` printed "unknown command" and the top help with no
//!   suggestion; `donsetch help nonsense` exited 0.
//! - `donsetch -V` was an unknown command while `-h` worked.
//!
//! Plus the sponsor opt-out contract: no stale Proxy-Seller line is
//! printed, and DONSETCH_NO_SPONSOR stays reserved without warnings.
//!
//! Children get their own root and never inherit a DONSETCH_ variable, so
//! nothing here can read or write the developer's real cache or config.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A private root for one test: `cache/` + an empty `donsetch.toml`.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir()
            .join("donsetch-test")
            .join(format!("cli-surface-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::write(root.join("donsetch.toml"), "").unwrap();
        Self { root }
    }

    /// The binary under test, with every inherited DONSETCH_ variable
    /// removed and this sandbox's own two installed.
    fn cmd(&self, extra_env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
        for (k, _) in std::env::vars_os() {
            let name = k.to_string_lossy();
            if name.starts_with("DONSETCH_")
                || name.starts_with("DONGHOST_")
                || name.starts_with("CLOAKBROWSER_")
            {
                cmd.env_remove(&k);
            }
        }
        cmd.env("DONSETCH_CONFIG", self.root.join("donsetch.toml"))
            .env("DONSETCH_CACHE_DIR", self.root.join("cache"));
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(&[]).args(args).output().expect("donsetch runs")
    }

    fn run_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.cmd(extra_env)
            .args(args)
            .output()
            .expect("donsetch runs")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The agent tools are clap-parsed and already refused unknown flags; the
/// management commands must give the same answer, not a silent run.
#[test]
fn an_unknown_flag_is_refused_not_ignored() {
    let sb = Sandbox::new("unknown-flag");

    let doctor = sb.run(&["doctor", "--frobnicate"]);
    assert_eq!(doctor.status.code(), Some(1), "doctor must refuse");
    assert!(
        stderr(&doctor).contains("unexpected argument '--frobnicate'"),
        "{}",
        stderr(&doctor)
    );
    // The refusal happens before any check runs.
    assert!(
        !stdout(&doctor).contains("Doctor"),
        "doctor ran anyway: {}",
        stdout(&doctor)
    );

    for args in [
        vec!["status", "--bogus"],
        vec!["proxy", "list", "--json"],
        vec!["keys", "list", "--json"],
        vec!["adapters", "--json"],
        vec!["version", "--json"],
        vec!["stop", "--json"],
    ] {
        let out = sb.run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?} must refuse");
        assert!(
            stderr(&out).contains("unexpected argument"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

/// A typo gets the nearest known flag, in clap's own shape.
#[test]
fn a_misspelled_flag_suggests_the_real_one() {
    let sb = Sandbox::new("typo-flag");
    let out = sb.run(&["doctor", "--depp"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("tip: a similar argument exists: '--deep'"),
        "{err}"
    );
    assert!(err.contains("Usage: donsetch doctor"), "{err}");
}

/// `--json` is the machine contract: one parseable document, no colours.
#[test]
fn status_json_is_one_document() {
    let sb = Sandbox::new("status-json");
    let out = sb.run(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let doc: serde_json::Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("status --json must be one document ({e}): {text}"));
    let status = &doc["status"];
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert!(status["proxies"]["count"].is_u64(), "{status}");
    assert!(status["browser"]["available"].is_boolean(), "{status}");
    assert!(status["cache_bytes"].is_u64() || status["cache_bytes"].is_null());
    // No TUI leak: the human render is the other branch, not a prefix.
    assert!(!text.contains('\u{1b}'), "ANSI in machine output");
    assert!(!text.contains("DonSeTch "), "human title in machine output");
}

/// The browser health line names only what still needs the browser:
/// screenshots.
#[test]
fn status_browser_warning_names_only_what_uses_the_browser() {
    let sb = Sandbox::new("status-browser");
    let missing = sb.root.join("no-such-chrome");
    let out = sb.run_with(
        &["status"],
        &[(
            "DONSETCH_BROWSER__CHROMIUM_PATH",
            missing.to_str().expect("utf-8 sandbox path"),
        )],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let health = text
        .lines()
        .find(|l| l.contains("browser unavailable"))
        .unwrap_or_else(|| panic!("no browser warning in: {text}"));
    assert!(health.contains("screenshot will not work"), "{health}");
    assert!(!health.contains("actions"), "{health}");

    // Negative case: with a browser present there is no warning at all.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let stub = sb.root.join("chrome-stub");
        std::fs::write(&stub, "#!/bin/sh\necho 'Chromium 140.0.0.0'\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = sb.run_with(
            &["status"],
            &[(
                "DONSETCH_BROWSER__CHROMIUM_PATH",
                stub.to_str().expect("utf-8 sandbox path"),
            )],
        );
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        let text = stdout(&out);
        assert!(text.contains("browser ready"), "{text}");
        assert!(!text.contains("will not work"), "{text}");
    }
}

/// `donsetch help <cmd>` and `donsetch <cmd> --help` are one text. The
/// copy in main.rs had gone stale (`proxy fetch|crawl on|off` was missing
/// from it), which is the whole reason the routes are pinned together.
#[test]
fn the_two_help_routes_cannot_drift() {
    let sb = Sandbox::new("help-routes");
    for cmd in [
        "settings",
        "proxy",
        "keys",
        "status",
        "doctor",
        "adapters",
        "rules",
        "search-bench",
        "update",
        "rollback",
        "version",
        "stop",
        "config",
        "mcp",
        "tools",
    ] {
        let routed = sb.run(&["help", cmd]);
        let direct = sb.run(&[cmd, "--help"]);
        assert_eq!(routed.status.code(), Some(0), "help {cmd}");
        assert_eq!(
            stdout(&routed),
            stdout(&direct),
            "`help {cmd}` and `{cmd} --help` disagree"
        );
    }
    // The drift guard itself: the stale copy predated the pool toggles.
    let proxy = stdout(&sb.run(&["help", "proxy"]));
    assert!(proxy.contains("fetch on|off"), "{proxy}");
    assert!(proxy.contains("crawl on|off"), "{proxy}");
}

/// A typo in the command itself is a usage error with a suggestion.
#[test]
fn an_unknown_command_is_a_usage_error() {
    let sb = Sandbox::new("unknown-command");
    let out = sb.run(&["fetc"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("unknown command 'fetc'")
            && stderr(&out).contains("tip: a similar command exists: 'fetch'"),
        "{}",
        stderr(&out)
    );

    let help = sb.run(&["help", "nonsense"]);
    assert_eq!(help.status.code(), Some(1), "help nonsense must not exit 0");
    assert!(stderr(&help).contains("unknown command 'nonsense'"));

    let sub = sb.run(&["proxy", "frobnicate"]);
    assert_eq!(sub.status.code(), Some(1));
    assert!(stderr(&sub).contains("unknown subcommand 'frobnicate'"));
}

/// `-V` is the conventional short form of `--version`.
#[test]
fn short_version_flag_works() {
    let sb = Sandbox::new("dash-V");
    let out = sb.run(&["-V"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains(env!("CARGO_PKG_VERSION")));
}

/// An empty export prints nothing on stdout (it is piped into other
/// tools) and says so on stderr, so silence stops reading as success.
#[test]
fn an_empty_export_explains_itself_on_stderr() {
    let sb = Sandbox::new("empty-export");
    let out = sb.run(&["proxy", "export"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).is_empty(), "stdout must stay machine-clean");
    assert!(
        stderr(&out).contains("no proxies configured"),
        "{}",
        stderr(&out)
    );
}

/// No stale Proxy-Seller line is printed on the CLI, and `DONSETCH_NO_SPONSOR`
/// stays reserved in the config layer without emitting warnings.
#[test]
fn no_stale_sponsor_line_and_opt_out_stays_reserved() {
    let sb = Sandbox::new("sponsor");
    std::fs::write(
        sb.root.join("donsetch.toml"),
        format!(
            "[browser]\nchromium_path = {:?}\n[proxy]\nfrom_environment = false\nhttps = \"http://127.0.0.1:0\"\nhttp = \"http://127.0.0.1:0\"\n",
            sb.root.join("missing-browser").to_string_lossy()
        ),
    )
    .unwrap();

    for args in [
        vec!["proxy", "--help"],
        vec!["proxy", "list"],
        vec!["proxy", "export"],
        vec!["status", "--json"],
        vec!["doctor", "--json"],
        vec!["tools"],
    ] {
        let text = stdout(&sb.run(&args));
        assert!(
            !text.contains("Proxy-Seller") && !text.contains("DON15") && !text.contains("Sponsor:"),
            "{args:?} still carries a sponsor line: {text}"
        );
    }

    // Opt-out env var remains reserved and silent.
    let off = sb.run_with(&["proxy", "list"], &[("DONSETCH_NO_SPONSOR", "1")]);
    assert!(
        !stderr(&off).contains("ignoring DONSETCH_NO_SPONSOR"),
        "the opt-out name must be reserved, not warned about: {}",
        stderr(&off)
    );
}

/// The pool toggles stay reachable through both routes after the help
/// consolidation, and `proxy fetch|crawl on|off` still validates its value.
#[test]
fn the_pool_toggle_shapes_are_intact() {
    let sb = Sandbox::new("pool-toggle");
    let out = sb.run(&["proxy", "crawl", "maybe"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("on|off"), "{}", stderr(&out));

    // Positional `on|off` is a value, not an unknown flag.
    let unknown = sb.run(&["proxy", "fetch", "--on"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("unexpected argument '--on'"),
        "{}",
        stderr(&unknown)
    );
}

#[test]
fn mcp_response_mode_reports_the_installed_source() {
    let sb = Sandbox::new("mcp-mode");
    let status = |extra_env: &[(&str, &str)]| {
        // Mode/source reporting does not depend on the update endpoint.
        let mut env = vec![
            ("DONSETCH_PROXY__FROM_ENVIRONMENT", "false"),
            ("DONSETCH_PROXY__HTTPS", "http://127.0.0.1:0"),
            ("DONSETCH_PROXY__NO_PROXY", ""),
        ];
        env.extend_from_slice(extra_env);
        let out = sb.run_with(&["status", "--json"], &env);
        assert!(out.status.success(), "{}", stderr(&out));
        let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        doc["status"]["mcp"].clone()
    };
    let default = status(&[]);
    assert_eq!(default["mode"], "text_only");
    assert_eq!(default["source"], "default");
    let env = status(&[("DONSETCH_MCP__TEXT_ONLY", "false")]);
    assert_eq!(env["mode"], "split");
    assert_eq!(env["source"], "DONSETCH_MCP__TEXT_ONLY");
    assert_eq!(env["known_clients_may_fold"], true);
    std::fs::write(sb.root.join("donsetch.toml"), "[mcp]\ntext_only = false\n").unwrap();
    let file = status(&[]);
    assert_eq!(file["mode"], "split");
    assert!(file["source"].as_str().unwrap().starts_with("file:"));
    let env = status(&[("DONSETCH_MCP__TEXT_ONLY", "true")]);
    assert_eq!(env["mode"], "text_only");
    assert_eq!(env["source"], "DONSETCH_MCP__TEXT_ONLY");
    let cfg = sb.run(&["config", "show", "--markdown"]);
    assert!(stdout(&cfg).contains("| `text_only` | bool | true |"));
    let legacy = sb.run(&["config", "show", "--legacy"]);
    assert!(!stdout(&legacy).contains("cut at v4"));
}

#[test]
fn settings_cli_set_get_reset_preset_list_and_alias_work_end_to_end() {
    let sb = Sandbox::new("settings-cli");

    // Non-TTY `donsetch settings` and `donsetch setting` print a clean summary.
    let summary = sb.run(&["settings"]);
    assert!(summary.status.success(), "{}", stderr(&summary));
    assert!(
        stdout(&summary).contains("Settings"),
        "{}",
        stdout(&summary)
    );

    let alias = sb.run(&["setting", "path"]);
    assert!(alias.status.success(), "{}", stderr(&alias));
    assert_eq!(
        stdout(&alias).trim(),
        sb.root.join("donsetch.toml").display().to_string()
    );

    // `settings set` / `settings get` / `settings reset` roundtrip on #478 knobs.
    let set_seq = sb.run(&["settings", "set", "search.sequential", "true"]);
    assert!(set_seq.status.success(), "{}", stderr(&set_seq));
    let set_wave = sb.run(&["settings", "set", "search.wave_size", "1"]);
    assert!(set_wave.status.success(), "{}", stderr(&set_wave));

    let got = sb.run(&["settings", "get", "search.sequential", "--json"]);
    assert!(got.status.success(), "{}", stderr(&got));
    let doc: serde_json::Value = serde_json::from_str(stdout(&got).trim()).unwrap();
    assert_eq!(doc["key"], "search.sequential");
    assert_eq!(doc["value"], "on");
    assert!(doc["origin"].as_str().unwrap().starts_with("file:"));

    // Invalid range is refused cleanly.
    let bad = sb.run(&["settings", "set", "search.wave_size", "99"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("out of range"), "{}", stderr(&bad));

    // `settings list --modified --json` returns the modified knobs without ANSI.
    let list = sb.run(&["settings", "list", "--modified", "--json"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(!stdout(&list).contains('\u{1b}'));
    let ldoc: serde_json::Value = serde_json::from_str(stdout(&list).trim()).unwrap();
    let keys: Vec<&str> = ldoc["settings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["key"].as_str())
        .collect();
    assert!(keys.contains(&"search.sequential"), "{keys:?}");
    assert!(keys.contains(&"search.wave_size"), "{keys:?}");

    // `settings preset slow-network` writes sequential + wave_size=1 + timeout_ms=15000.
    let preset = sb.run(&["settings", "preset", "slow-network", "--json"]);
    assert!(preset.status.success(), "{}", stderr(&preset));
    let got_t = sb.run(&["settings", "get", "search.timeout_ms", "--json"]);
    let tdoc: serde_json::Value = serde_json::from_str(stdout(&got_t).trim()).unwrap();
    assert_eq!(tdoc["value"], "15000 ms");

    // `settings reset` restores default.
    let reset = sb.run(&["settings", "reset", "search.sequential"]);
    assert!(reset.status.success(), "{}", stderr(&reset));
    let after = sb.run(&["settings", "get", "search.sequential", "--json"]);
    let adoc: serde_json::Value = serde_json::from_str(stdout(&after).trim()).unwrap();
    assert_eq!(adoc["value"], "off");
    assert_eq!(adoc["origin"], "default");

    // Unknown subcommand & unknown flag are refused.
    let unk_sub = sb.run(&["settings", "frobnicate"]);
    assert_eq!(unk_sub.status.code(), Some(1));
    let unk_flag = sb.run(&["settings", "list", "--bogus"]);
    assert_eq!(unk_flag.status.code(), Some(1));
}

#[test]
fn issue_477_search_failure_receipt_reports_byok_census_on_cli() {
    // Reproduces Cases A, B, C, E from #477 deterministically using a dead
    // loopback proxy port so no live network traffic is needed.
    let dead_env: &[(&str, &str)] = &[
        ("HTTPS_PROXY", "http://127.0.0.1:9"),
        ("HTTP_PROXY", "http://127.0.0.1:9"),
        ("ALL_PROXY", "http://127.0.0.1:9"),
        ("DONSETCH_SEARCH__TIMEOUT_MS", "1000"),
    ];

    // Case B: BYOK-first configured (`parallel` default, `tavily` key), explicit `--lang fr`.
    let sb_b = Sandbox::new("i477-case-b");
    std::fs::write(
        sb_b.root.join("cache").join("byok-keys.json"),
        r#"{"default":"parallel","providers":[{"name":"tavily","keys":[{"key":"tvly-dev-dummy0000000000000000000000","state":"active","ts":0}]}]}"#,
    )
    .unwrap();
    let out_b = sb_b.run_with(
        &["search", "rapport cuir France", "--lang", "fr", "--json"],
        dead_env,
    );
    assert_eq!(out_b.status.code(), Some(2), "{}", stderr(&out_b));
    let doc_b: serde_json::Value = serde_json::from_str(stdout(&out_b).trim())
        .unwrap_or_else(|e| panic!("invalid json ({e}): {}", stdout(&out_b)));
    assert_eq!(doc_b["meta"]["sources"][0]["id"], "byok", "{doc_b}");
    assert_eq!(
        doc_b["meta"]["sources"][0]["status"], "skipped: explicit lang",
        "{doc_b}"
    );
    assert_eq!(doc_b["meta"]["sources"][0]["state"], "not asked", "{doc_b}");
    assert!(
        !doc_b["meta"]["next_action"]
            .as_str()
            .unwrap()
            .contains("donsetch keys add"),
        "Case B must not tell caller to add a key: {doc_b}"
    );
    assert!(
        doc_b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("byok skipped: explicit lang"),
        "{doc_b}"
    );

    // Case E: BYOK-first configured, no `--lang` (provider attempted and failed).
    let sb_e = Sandbox::new("i477-case-e");
    std::fs::write(
        sb_e.root.join("cache").join("byok-keys.json"),
        r#"{"default":"parallel","providers":[{"name":"tavily","keys":[{"key":"tvly-dev-dummy0000000000000000000000","state":"active","ts":0}]}]}"#,
    )
    .unwrap();
    let out_e = sb_e.run_with(&["search", "rapport cuir France", "--json"], dead_env);
    assert_eq!(out_e.status.code(), Some(2), "{}", stderr(&out_e));
    let doc_e: serde_json::Value = serde_json::from_str(stdout(&out_e).trim())
        .unwrap_or_else(|e| panic!("invalid json ({e}): {}", stdout(&out_e)));
    assert_eq!(doc_e["meta"]["sources"][0]["id"], "byok", "{doc_e}");
    assert_eq!(doc_e["meta"]["sources"][0]["state"], "failed", "{doc_e}");
    assert!(
        doc_e["meta"]["sources"][0]["status"]
            .as_str()
            .unwrap()
            .contains("tavily"),
        "{doc_e}"
    );
    assert_eq!(doc_e["meta"]["escalation"][0]["stage"], "byok", "{doc_e}");
    assert!(
        !doc_e["meta"]["next_action"]
            .as_str()
            .unwrap()
            .contains("donsetch keys add"),
        "Case E must not tell caller to add a key: {doc_e}"
    );

    // Case A: no key configured keeps the clean unconfigured envelope.
    let sb_a = Sandbox::new("i477-case-a");
    let out_a = sb_a.run_with(
        &["search", "rapport cuir France", "--lang", "fr", "--json"],
        dead_env,
    );
    assert_eq!(out_a.status.code(), Some(2), "{}", stderr(&out_a));
    let doc_a: serde_json::Value = serde_json::from_str(stdout(&out_a).trim()).unwrap();
    assert!(doc_a["meta"].get("sources").is_none(), "{doc_a}");
    assert!(
        doc_a["meta"]["next_action"]
            .as_str()
            .unwrap()
            .contains("donsetch keys add"),
        "Case A should still recommend adding a key: {doc_a}"
    );
}
