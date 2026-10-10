//! The runtime off switch for local rules: `--ignore-rules` and
//! `DONSETCH_RULES__MODE=off` must reach the ruleset the guards use,
//! not only the loaded config that doctor and `rules test` report.
//! Drives the real binary against a deny rule on a loopback literal,
//! so no request leaves the machine: with the rule on, the rule
//! refuses the fetch; with it off, the SSRF guard refuses the same URL.

use std::process::Command;

const URL: &str = "http://127.0.0.1:9/owned";

/// `donsetch fetch URL --json` with only the given config file, cache
/// root and DonSeTch variables; the developer's own config and
/// environment never reach the child.
fn fetch_code(root: &std::path::Path, extra_args: &[&str], extra_env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("DONSETCH_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("DONSETCH_CONFIG", root.join("donsetch.toml"))
        .env("DONSETCH_CACHE_DIR", root.join("cache"))
        .args(["fetch", URL, "--json"])
        .args(extra_args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("donsetch runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "--json output must parse ({e}): {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    envelope["meta"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no meta.code: {envelope}"))
        .to_string()
}

#[test]
fn the_off_switches_reach_the_guards() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir()
        .join("donsetch-test")
        .join(format!("rules-off-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("donsetch.toml"),
        "[rules.url.\"127.0.0.1\"]\naction = \"deny\"\nmessage = \"owned test rule\"\n",
    )
    .unwrap();

    // Negative: with the rule on, the rule refuses before the SSRF guard.
    assert_eq!(fetch_code(&root, &[], &[]), "policy.denied.unspecified");
    assert_eq!(fetch_code(&root, &["--ignore-rules"], &[]), "guard.ssrf");
    assert_eq!(
        fetch_code(&root, &[], &[("DONSETCH_RULES__MODE", "off")]),
        "guard.ssrf"
    );

    let _ = std::fs::remove_dir_all(&root);
}
