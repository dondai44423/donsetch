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
//! - `donsetch login` with no terminal launched a real browser before
//!   discovering the closed stdin.
//!
//! Plus the sponsor placement's hard lines: labelled, human-surface only,
//! and switchable off without the config layer complaining about the
//! switch's own name.
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

/// `donsetch help <cmd>` and `donsetch <cmd> --help` are one text. The
/// copy in main.rs had gone stale (`proxy fetch|crawl on|off` was missing
/// from it), which is the whole reason the routes are pinned together.
#[test]
fn the_two_help_routes_cannot_drift() {
    let sb = Sandbox::new("help-routes");
    for cmd in [
        "proxy", "keys", "login", "status", "doctor", "adapters", "rules",
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

/// The other branches of `login` are non-interactive; the interactive one
/// needs a human, and must say so before spawning anything.
#[test]
fn login_without_a_terminal_launches_no_browser() {
    let sb = Sandbox::new("login-notty");
    let out = sb.run(&["login"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("needs a terminal"),
        "{}",
        stderr(&out)
    );
    assert!(
        !stdout(&out).contains("Starting browser"),
        "a browser was launched for a login nobody could confirm: {}",
        stdout(&out)
    );
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

/// The sponsor placement: labelled, on the human `proxy` surface only,
/// and switchable off with an env var the config layer accepts.
#[test]
fn the_sponsor_line_is_labelled_human_only_and_switchable() {
    let sb = Sandbox::new("sponsor");
    // This tests output placement, not browser/network health. An explicit
    // missing browser and a refused loopback proxy keep doctor bounded on
    // offline runners and prevent discovery of a developer's browser.
    std::fs::write(
        sb.root.join("donsetch.toml"),
        format!(
            "[browser]\nchromium_path = {:?}\n[proxy]\nfrom_environment = false\nhttps = \"http://127.0.0.1:0\"\nhttp = \"http://127.0.0.1:0\"\n",
            sb.root.join("missing-browser").to_string_lossy()
        ),
    )
    .unwrap();

    // Present, labelled, with the URL, in the two human surfaces.
    for args in [vec!["proxy", "--help"], vec!["proxy", "list"]] {
        let text = stdout(&sb.run(&args));
        assert!(text.contains("Sponsor: Proxy-Seller"), "{args:?}: {text}");
        assert!(text.contains("https://proxy-seller.com/"), "{args:?}");
    }

    // Absent from every machine-facing output.
    let machine: &[(&str, &[&str])] = &[
        ("status --json", &["status", "--json"]),
        ("doctor --json", &["doctor", "--json"]),
        ("tools", &["tools"]),
        ("proxy export", &["proxy", "export"]),
        ("doctor --help", &["doctor", "--help"]),
        ("status --help", &["status", "--help"]),
    ];
    for (label, args) in machine {
        let text = stdout(&sb.run(args));
        assert!(
            !text.contains("Proxy-Seller") && !text.contains("DON15"),
            "{label} carries the sponsor line"
        );
    }

    // Switched off: no line, and no config warning about the switch's name.
    let off = sb.run_with(&["proxy", "list"], &[("DONSETCH_NO_SPONSOR", "1")]);
    let text = stdout(&off);
    assert!(!text.contains("Sponsor:"), "{text}");
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
