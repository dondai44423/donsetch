//! The daemon boot contract: `donsetch mcp --supervised` must stay
//! alive and answer an initialize, not exit during startup. The
//! config consolidation wave installed the layered config once for
//! every command and the mcp arm installed it again; the second
//! install hit the one-shot check and exited the daemon before the
//! first byte. This test drives the real binary over stdio and pins
//! both the response and the absence of the double-install error.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn mcp_daemon_boots_and_answers_initialize() {
    // A one-shot MCP client over stdio.
    let mut child = Command::new(env!("CARGO_BIN_EXE_donsetch"))
        .arg("mcp")
        .arg("--supervised")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("daemon spawns");
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
    // daemon. It must not appear anywhere on stderr.
    drop(stdin);
    let mut err_text = String::new();
    let _ = BufReader::new(stderr).read_line(&mut err_text);
    let _ = child.wait();
    assert!(
        !err_text.contains("already installed"),
        "double-install regression: {err_text}"
    );
}
