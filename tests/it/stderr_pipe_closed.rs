//! A diagnostic write must never kill the process (#364).
//!
//! std's stderr print macros panic when the write fails, and the profile
//! the binary ships is `panic = "abort"`: an MCP client that closed its
//! stderr read end turned a log line into SIGABRT mid-session. Every
//! diagnostic now goes through `diag!` (`src/diag.rs`), which drops a
//! failed write, and `[lints.clippy] print_stderr = "deny"` keeps a raw
//! stderr print from coming back.
//!
//! The children here are driven over stdio like a client, against a
//! loopback page, so nothing leaves the machine.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// A one-page server on loopback: enough HTML for the fetch path to run
/// its real course.
fn html_server() -> String {
    let listener = (0..5)
        .find_map(|_| TcpListener::bind("127.0.0.1:0").ok())
        .expect("no free port");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut s = stream;
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = "<html><head><title>closed stderr</title></head><body>\
                        <article><h1>closed stderr</h1><p>The page arrived while \
                        stderr was gone.</p></article></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    format!("http://{addr}/")
}

fn child(env_path: Option<PathBuf>) -> Child {
    let dir = std::env::temp_dir().join(format!("donsetch-364-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
    cmd.arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("DONSETCH_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("DONSETCH_NO_CONFIG_FILE", "1")
        .env("DONSETCH_ALLOW_PRIVATE_EGRESS", "1")
        .env("DONSETCH_CACHE_DIR", &dir);
    if let Some(path) = env_path {
        cmd.env("PATH", path);
    }
    cmd.spawn().expect("mcp daemon spawns")
}

/// `initialize` + a `tools/call`, and the id=2 answer if one ever comes.
/// `stderr_closed` drops the read end of the child's stderr pipe at once,
/// so every later write to fd 2 fails: that is the broken pipe of #364.
fn drive(child: &mut Child, url: &str, stderr_closed: bool) -> (Option<String>, String) {
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut err_text = String::new();
    // Closing the read end is the whole fixture: the child holds the
    // write end, and no reader exists from here on.
    let keep = if stderr_closed {
        drop(stderr);
        None
    } else {
        Some(stderr)
    };
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    for msg in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}"#.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"web_fetch","arguments":{{"url":"{url}","max_chars":600}}}}}}"#
        ),
    ] {
        let _ = stdin.write_all(msg.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let _ = stdin.flush();

    let mut answer = None;
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(90)) {
        if line.contains("\"id\":2") {
            answer = Some(line);
            break;
        }
    }
    drop(stdin);
    if let Some(mut stderr) = keep {
        let _ = stderr.read_to_string(&mut err_text);
    }
    (answer, err_text)
}

/// The startup banner is the first thing `donsetch mcp` writes to stderr.
/// With the read end gone, that write fails before any request is read:
/// pre-fix the process died here (SIGABRT under `panic = "abort"`, exit
/// 101 on the unwinding profiles), so no answer ever came back.
#[test]
fn mcp_startup_survives_a_closed_stderr_pipe() {
    let url = html_server();
    let mut c = child(None);
    let (answer, _) = drive(&mut c, &url, true);
    let status = wait(&mut c);
    assert!(
        answer.is_some(),
        "the daemon answered nothing after a closed-stderr startup: {status:?}"
    );
    assert!(
        status.code().is_some(),
        "the daemon was killed by a signal at startup: {status:?}"
    );
}

fn wait(c: &mut Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = c.try_wait().expect("try_wait") {
            return status;
        }
        if std::time::Instant::now() > deadline {
            let _ = c.kill();
            return c.wait().expect("wait after kill");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
