//! web_fetch's views, end to end: the real binary over MCP stdio and the
//! CLI, against a docs site on loopback. Each view is checked on what the
//! agent receives ([meta] state and the evidence text), and every
//! argument mistake is checked to answer before anything is fetched.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

const GUIDE: &str = r#"<html><head><title>Widget Guide</title>
<link rel="next" href="/guide?page=2"></head><body><main><article>
<h1>Widget Guide</h1>
<p>This guide covers installing widgets, the limits that apply to every account, and the answers to common questions about them.</p>
<h2>Install</h2>
<p>Download the installer from the release page and run it with administrator rights on the target machine.</p>
<h3>Linux</h3>
<p>On Linux, unpack the tarball into /opt/widget and add the bin directory to your PATH before starting the daemon.</p>
<h3>Windows</h3>
<p>On Windows, run the MSI package; it registers the widget service and starts it automatically after installation.</p>
<h2>Limits</h2>
<p>Every account is subject to a <strong>rate</strong> limit of 100 requests per minute; bursts above it return HTTP 429.</p>
<table><tr><th>Plan</th><th>Requests per minute</th></tr><tr><td>Free</td><td>100</td></tr><tr><td>Enterprise</td><td>10000</td></tr></table>
<h2>FAQ</h2>
<p>Why is my request slow? Requests are served from the nearest region; check the region setting in your profile first.</p>
</article></main></body></html>"#;

const OTHER: &str = r#"<html><head><title>Changelog</title></head><body><main><article>
<h1>Changelog</h1>
<h2>2.0</h2>
<p>Breaking change: the v1 endpoints are removed and every client must move to the v2 API before upgrading.</p>
<h2>1.9</h2>
<p>Fixed CVE-2026-12345, a path traversal in the archive extractor that affected every earlier release.</p>
</article></main></body></html>"#;

/// The docs site: every request counted, so a refused call can prove it
/// fetched nothing.
fn site() -> (String, Arc<AtomicUsize>) {
    let listener = (0..5)
        .find_map(|_| TcpListener::bind("127.0.0.1:0").ok())
        .expect("no free port");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                counter.fetch_add(1, Ordering::SeqCst);
                let body = if path.starts_with("/changelog") {
                    OTHER
                } else {
                    GUIDE
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}"), hits)
}

fn hermetic(cmd: &mut Command, dir: &std::path::Path) {
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("DONSETCH_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("DONSETCH_NO_CONFIG_FILE", "1")
        .env("DONSETCH_ALLOW_PRIVATE_EGRESS", "1")
        .env("DONSETCH_CACHE_DIR", dir);
}

/// One MCP session over stdio.
struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Session {
    fn open(dir: &std::path::Path) -> Session {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
        cmd.arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hermetic(&mut cmd, dir);
        let mut child = cmd.spawn().expect("mcp daemon spawns");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut session = Session {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        session.send(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli","version":"2.0.16"}}}));
        session.wait(0);
        session.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        session
    }

    fn send(&mut self, msg: &Value) {
        self.stdin.write_all(msg.to_string().as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    fn wait(&mut self, id: u64) -> Value {
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(60)) {
            let reply: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if reply["id"] == id {
                return reply;
            }
        }
        panic!("no answer to request {id}");
    }

    /// Call web_fetch: the [meta] state and the evidence text.
    fn fetch(&mut self, arguments: Value) -> (Value, String) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":"web_fetch","arguments":arguments}}));
        let reply = self.wait(id);
        let content = reply["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("no content: {reply}"));
        let first = content[0]["text"].as_str().unwrap();
        let (meta, rest) = match first.strip_prefix("[meta] ") {
            Some(tail) => tail.split_once('\n').unwrap_or((tail, "")),
            None => panic!("no [meta] line: {reply}"),
        };
        let mut text = rest.to_string();
        for block in &content[1..] {
            text.push('\n');
            text.push_str(block["text"].as_str().unwrap_or(""));
        }
        (serde_json::from_str(meta).unwrap(), text)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("donsetch-views-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn every_view_answers_its_question_over_mcp() {
    let (base, hits) = site();
    let dir = temp_dir("mcp");
    let mut s = Session::open(&dir);
    let guide = format!("{base}/guide");

    // must_contain: per-term answers, never the page.
    let (meta, text) = s.fetch(
        json!({"url": guide, "must_contain": ["rate limit", "/HTTP\\s+\\d{3}/", "GraphQL"]}),
    );
    assert_eq!(meta["read_status"], "probe", "{meta}");
    assert_eq!(
        meta["must_contain"],
        json!({"verdict": "partial", "found": 2, "of": 3}),
        "{meta}"
    );
    assert!(text.contains("must_contain: PARTIAL 2/3"), "{text}");
    assert!(text.contains("+ \"rate limit\" ×1 in §s"), "{text}");
    assert!(text.contains("- \"GraphQL\": not found"), "{text}");
    assert!(
        !text.contains("administrator rights"),
        "the page leaked into a probe: {text}"
    );
    assert!(text.len() < 900, "{text}");

    // focus: the passage that answers, under its section path.
    let (meta, text) = s.fetch(json!({"url": guide, "focus": "enterprise plan requests"}));
    assert_eq!(meta["read_status"], "focus", "{meta}");
    assert!(meta["focus"]["passages"].as_u64().unwrap() >= 1, "{meta}");
    assert!(text.contains("| Enterprise | 10000 |"), "{text}");
    assert!(text.contains("› Limits"), "{text}");
    assert!(!text.contains("MSI package"), "{text}");

    // A focus miss: the outline and the absent words, not the page.
    let (meta, text) = s.fetch(json!({"url": guide, "focus": "kubernetes helm"}));
    assert_eq!(meta["focus"]["passages"], 0, "{meta}");
    assert!(
        text.contains("not on the page: \"kubernetes\", \"helm\""),
        "{text}"
    );
    assert!(text.contains("] Limits"), "{text}");
    assert!(!text.contains("administrator rights"), "{text}");

    // toc, then one section by its id.
    let (meta, text) = s.fetch(json!({"url": guide, "toc": true}));
    assert_eq!(meta["read_status"], "outline", "{meta}");
    let windows = text
        .lines()
        .find(|l| l.contains("] Windows"))
        .unwrap_or_else(|| panic!("{text}"));
    let id = windows
        .split('[')
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let (meta, text) = s.fetch(json!({"url": guide, "section": id}));
    assert_eq!(meta["section"], id, "{meta}");
    assert!(text.contains("MSI package"), "{text}");
    assert!(!text.contains("tarball"), "{text}");
    assert_eq!(meta["content_complete"], false, "{meta}");

    // A section that is not there: the outline.
    let (meta, text) = s.fetch(json!({"url": guide, "section": "Billing"}));
    assert_eq!(meta["section"], "not_found", "{meta}");
    assert!(text.contains("no heading matches"), "{text}");

    // The page, its window and continuation, and the next part.
    let (meta, first) = s.fetch(json!({"url": guide, "max_chars": 400}));
    assert_eq!(meta["read_status"], "partial", "{meta}");
    assert_eq!(meta["next_page"], format!("{base}/guide?page=2"), "{meta}");
    let next = meta["next_offset"].as_u64().unwrap();
    let (meta, rest) = s.fetch(json!({"url": guide, "offset": next, "max_chars": 100000}));
    assert!(rest.contains("nearest region"), "{rest}");
    assert!(meta.get("next_offset").is_none(), "{meta}");
    assert!(!first.contains("nearest region"));

    // A batch: one verdict per page.
    let (meta, text) = s.fetch(
        json!({"url": [guide, format!("{base}/changelog")], "must_contain": "/CVE-\\d{4}-\\d+/"}),
    );
    let rows = meta["results"].as_array().unwrap();
    assert_eq!(rows[0]["must_contain"]["verdict"], "no_match", "{meta}");
    assert_eq!(rows[1]["must_contain"]["verdict"], "match", "{meta}");
    assert!(text.contains("CVE-2026-12345"), "{text}");

    // Argument mistakes answer before any request is made.
    let before = hits.load(Ordering::SeqCst);
    for (arguments, expect) in [
        (
            json!({"url": guide, "must_contain": "x", "focus": "y"}),
            "must_contain and focus cannot be combined",
        ),
        (
            json!({"url": guide, "must_contain": "/([bad/"}),
            "not a valid regex",
        ),
        (
            json!({"url": guide, "since_last": true, "section": "s1"}),
            "since_last and section cannot be combined",
        ),
        (
            json!({"url": guide, "toc": true, "focus": "y"}),
            "toc and focus cannot be combined",
        ),
        (
            json!({"url": guide, "budget_tokens": 900}),
            "budget_tokens was removed",
        ),
        (
            json!({"url": guide, "selector": "main"}),
            "selector was removed",
        ),
        (
            json!({"url": guide, "must_contain": ["a","b","c","d","e","f","g","h","i"]}),
            "must_contain",
        ),
    ] {
        let id = s.next_id;
        s.next_id += 1;
        s.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"web_fetch","arguments":arguments}}));
        let reply = s.wait(id).to_string();
        assert!(reply.contains(expect), "{arguments}: {reply}");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        before,
        "a refused call fetched"
    );

    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_cli_speaks_the_same_views() {
    let (base, _) = site();
    let dir = temp_dir("cli");
    let run = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
        cmd.arg("fetch").args(args).arg("--quiet");
        hermetic(&mut cmd, &dir);
        let out = cmd.output().expect("cli runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let guide = format!("{base}/guide");
    let (ok, out, err) = run(&[
        &guide,
        "--must-contain",
        "rate limit",
        "--must-contain",
        "MSI",
    ]);
    assert!(ok, "{err}");
    assert!(out.contains("must_contain: MATCH 2/2"), "{out}");
    let (ok, out, err) = run(&[&guide, "--focus", "linux tarball", "--json"]);
    assert!(ok, "{err}");
    let envelope: Value = serde_json::from_str(&out).unwrap();
    let text = envelope.to_string();
    assert!(text.contains("unpack the tarball"), "{out}");
    let (ok, out, err) = run(&[&guide, "--toc"]);
    assert!(ok, "{err}");
    assert!(out.contains("] Install"), "{out}");
    let (ok, _, err) = run(&[&guide, "--stitch"]);
    assert!(!ok && err.contains("--stitch"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
