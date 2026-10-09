//! An MCP tool call must answer, not abort.
//!
//! The MCP server runs each tool call on a runtime worker thread, and a
//! worker gets 2 MiB of stack by default. The fetch path needs more than
//! that in an unoptimized build: measured on the `fast` profile, 2 MiB and
//! 2.5 MiB both overflowed and 2.75 MiB was the first size that passed. A
//! stack overflow is an abort with no error envelope, so the whole session
//! died on the first fetch while the same fetch through the CLI was fine,
//! because the CLI runs on the 8 MiB main thread. This drives the real
//! binary over stdio against a local page and pins the answer: on the
//! `fast` profile it goes red if the worker stack is left at the default.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A one-page server on loopback: enough HTML that the fetch path runs
/// its real course, nothing else.
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
            let body = format!(
                "<html><head><title>Stack probe</title></head><body>\
                 <article><h1>Stack probe</h1><p>{}</p></article></body></html>",
                "The worker thread answered with readable evidence. ".repeat(40)
            );
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

#[test]
fn mcp_fetch_answers_instead_of_aborting() {
    let url = html_server();
    // Hermetic: the child must not read a developer's real config or
    // cache, and a loopback target needs the SSRF override.
    let dir = std::env::temp_dir().join(format!("donsetch-stack-{}", std::process::id()));
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

    let mut child = cmd.spawn().expect("mcp daemon spawns");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Every line the daemon writes, forwarded to the test thread. EOF
    // (the abort case) closes the channel, so a dead process cannot
    // make this hang.
    let (tx, rx) = mpsc::channel::<String>();
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
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli","version":"2.0.16"}}}"#.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"web_fetch","arguments":{{"url":"{url}","tier":"1","max_chars":600}}}}}}"#
        ),
    ] {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    stdin.flush().unwrap();

    let mut answer = String::new();
    // Either end of stream (the process died before answering) or the
    // deadline ends the wait: there is no answer to wait for.
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(60)) {
        if line.contains("\"id\":2") {
            answer = line;
            break;
        }
    }

    drop(stdin);
    let mut err_text = String::new();
    let _ = BufReader::new(stderr).read_to_string(&mut err_text);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !err_text.contains("stack overflow"),
        "the tool call aborted on a stack overflow: {err_text}"
    );
    assert!(
        answer.contains("\"result\"") || answer.contains("\"error\""),
        "no JSON-RPC answer to tools/call; stderr was: {err_text}"
    );
    let reply: serde_json::Value = serde_json::from_str(&answer).unwrap();
    let result = &reply["result"];
    assert!(result.get("structuredContent").is_none(), "{reply}");
    let content = result["content"].as_array().expect("MCP content blocks");
    let meta = content[0]["text"]
        .as_str()
        .unwrap()
        .strip_prefix("[meta] ")
        .expect("unknown client receives self-contained text by default");
    let state: serde_json::Value = serde_json::from_str(meta).unwrap();
    assert_eq!(state["content_ok"], true, "{reply}");
    assert!(state["next_offset"].as_u64().unwrap() > 0, "{reply}");
    assert!(
        content.iter().any(|block| block["text"]
            .as_str()
            .is_some_and(|text| text.contains("The worker thread answered"))),
        "client must receive page evidence alongside continuation: {reply}"
    );
}
