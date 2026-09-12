//! The daemon boot contract: `donsetch mcp --supervised` must stay
//! alive and answer an initialize, not exit during startup. The
//! config consolidation wave installed the layered config once for
//! every command and the mcp arm installed it again; the second
//! install hit the one-shot check and exited the daemon before the
//! first byte. This test drives the real binary over stdio and pins
//! both the response and the absence of the double-install error.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn hermetic_command() -> Command {
    // The child must not inherit a developer's real config: a real
    // donsetch.toml with [transport] kind = "http" (or
    // DONSETCH_TRANSPORT=http) would boot an HTTP server instead of
    // the stdio daemon this test drives. DONSETCH_NO_CONFIG_FILE=1
    // skips the file layer; every other DONSETCH_* var goes too,
    // including DONSETCH_CONFIG (NO_CONFIG_FILE + CONFIG = a hard
    // conflict error that would kill the boot for the wrong reason).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
    cmd.arg("mcp")
        .arg("--supervised")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("DONSETCH_NO_CONFIG_FILE", "1");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("DONSETCH_") {
            cmd.env_remove(&k);
        }
    }
    cmd
}

#[test]
fn mcp_daemon_boots_and_answers_initialize() {
    // A one-shot MCP client over stdio.
    let mut child = hermetic_command().spawn().expect("daemon spawns");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"boot-probe\",\"version\":\"0\"}}}\n",
        )
        .unwrap();
    stdin.flush().unwrap();

    // The response must arrive while the process is still alive:
    // a process that exited at startup serves EOF, not a result.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut reader = BufReader::new(stdout);
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
    });
    let answer = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the daemon must answer initialize");
    assert!(
        answer.contains("\"id\":1") && answer.contains("\"result\""),
        "unexpected initialize answer: {answer}"
    );

    // The regression signature: the double install error killed the
    // daemon. It must not appear anywhere on stderr. Read to EOF so
    // a first-line warning can never mask a later line.
    drop(stdin);
    let mut err_text = String::new();
    let _ = BufReader::new(stderr).read_to_string(&mut err_text);
    let _ = child.wait();
    assert!(
        !err_text.contains("already installed"),
        "double-install regression: {err_text}"
    );
}
