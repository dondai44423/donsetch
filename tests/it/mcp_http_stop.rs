//! Stopping `donsetch mcp --http` with SIGTERM and SIGINT.
//!
//! The first signal drains: the tool calls in flight are answered, the
//! GET /mcp streams (keep-alives only, nothing to wait for) are ended,
//! and the server exits. A second signal stops the wait. These drive
//! the real binary with real signals; Unix only, for `kill`.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a stopping server may take to exit before the test calls
/// it stuck.
const EXIT_WITHIN: Duration = Duration::from_secs(10);

/// How long an open GET stream must stay silent and open when no signal
/// comes; shorter than the stream's 15 s keep-alive interval.
const STREAM_QUIET: Duration = Duration::from_secs(2);

struct Server {
    child: Child,
    port: u16,
    stderr: mpsc::Receiver<String>,
    seen: Vec<String>,
    dir: PathBuf,
}

impl Server {
    /// The real binary on a free loopback port, hermetic (no config
    /// file, a private cache) and allowed to fetch loopback pages.
    ///
    /// The port is free when picked but released before the child binds
    /// it, and parallel tests pick ports too: a child that loses its port
    /// exits at startup, and the next attempt picks another.
    fn start(tag: &str) -> Self {
        for _ in 0..5 {
            if let Some(server) = Self::try_start(tag) {
                return server;
            }
        }
        panic!("the server lost its port five times");
    }

    /// `None` when the child could not bind the port it was given.
    fn try_start(tag: &str) -> Option<Self> {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let dir =
            std::env::temp_dir().join(format!("donsetch-http-stop-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
        cmd.args(["mcp", "--http", "--host", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("DONSETCH_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("DONSETCH_NO_CONFIG_FILE", "1")
            .env("DONSETCH_ALLOW_PRIVATE_EGRESS", "1")
            .env("DONSETCH_CACHE_DIR", &dir);
        let mut child = cmd.spawn().expect("the HTTP server spawns");
        let err = child.stderr.take().unwrap();
        let (tx, stderr) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            port,
            stderr,
            seen: Vec::new(),
            dir,
        };
        // Only this child's own line proves the port is its: a health
        // answer alone may come from another test's server.
        let listening = format!("listening on http://127.0.0.1:{port}");
        if !server.wait_log(&listening, Duration::from_secs(60)) {
            let log = server.log();
            if server.exited_within(Duration::from_secs(5)).is_some()
                && log.to_ascii_lowercase().contains("in use")
            {
                return None;
            }
            panic!("the server did not come up: {log}");
        }
        let up = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = s.write_all(
                    b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                );
                let mut answer = String::new();
                let _ = s.read_to_string(&mut answer);
                if answer.starts_with("HTTP/1.1 200") {
                    return Some(server);
                }
            }
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!("the server exited at startup ({status}): {}", server.log());
            }
            assert!(
                Instant::now() < up,
                "the server did not come up: {}",
                server.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: a plain kill(2) on the child this test spawned and has
        // not reaped yet.
        let rc = unsafe { libc::kill(self.child.id() as libc::pid_t, sig) };
        assert_eq!(rc, 0, "kill({sig}) failed");
    }

    /// The exit status, or None if the process still runs after `within`.
    fn exited_within(&mut self, within: Duration) -> Option<ExitStatus> {
        let end = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for a stderr line containing `needle`.
    fn wait_log(&mut self, needle: &str, within: Duration) -> bool {
        let end = Instant::now() + within;
        if self.seen.iter().any(|l| l.contains(needle)) {
            return true;
        }
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match self.stderr.recv_timeout(left) {
                Ok(line) => {
                    let hit = line.contains(needle);
                    self.seen.push(line);
                    if hit {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }

    /// Everything the server wrote to stderr so far.
    fn log(&mut self) -> String {
        while let Ok(line) = self.stderr.try_recv() {
            self.seen.push(line);
        }
        self.seen.join(" | ")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Opens GET /mcp and reads the response head, leaving the stream open.
fn open_get_stream(port: u16) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!("GET /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        assert_eq!(
            s.read(&mut b).unwrap(),
            1,
            "GET /mcp closed before its head"
        );
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("text/event-stream"), "{head}");
    s
}

/// A loopback page that answers `delay` after its request arrives, and
/// reports each arrival on the returned channel.
fn slow_page(delay: Duration) -> (String, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, hits) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = tx.send(());
                std::thread::sleep(delay);
                let body = format!(
                    "<html><head><title>Drain probe</title></head><body><article>\
                     <h1>Drain probe</h1><p>{}</p></article></body></html>",
                    "The answer arrived while the server was draining. ".repeat(20)
                );
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    (format!("http://{addr}/"), hits)
}

/// POSTs a web_fetch of `url` on its own thread; the handle yields the
/// whole HTTP response, or the read error if none came.
fn fetch_call(port: u16, url: &str) -> std::thread::JoinHandle<std::io::Result<String>> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"web_fetch","arguments":{{"url":"{url}","max_chars":600}}}}}}"#
    );
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        s.write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
                 Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )?;
        let mut answer = String::new();
        s.read_to_string(&mut answer)?;
        Ok(answer)
    })
}

#[test]
fn sigterm_ends_an_open_get_stream_and_the_server_exits() {
    let mut server = Server::start("stream");
    let mut stream = open_get_stream(server.port);
    // Without a signal the stream stays open: nothing to read, no end.
    stream.set_read_timeout(Some(STREAM_QUIET)).unwrap();
    let mut b = [0u8; 64];
    match stream.read(&mut b) {
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        got => panic!(
            "the GET stream did not stay open with no signal: {:?}",
            got.map(|n| String::from_utf8_lossy(&b[..n]).into_owned())
        ),
    }
    assert!(
        server.child.try_wait().expect("try_wait").is_none(),
        "the server exited with no signal: {}",
        server.log()
    );
    stream.set_read_timeout(Some(EXIT_WITHIN)).unwrap();

    server.signal(libc::SIGTERM);
    let status = server.exited_within(EXIT_WITHIN);
    assert!(
        status.is_some(),
        "the server still runs {EXIT_WITHIN:?} after SIGTERM with a GET stream open: {}",
        server.log()
    );
    assert!(status.unwrap().success(), "{}", server.log());
    // The stream was ended (the chunked body's last chunk), not cut.
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    assert!(
        rest.ends_with(b"0\r\n\r\n"),
        "the GET stream did not end cleanly: {:?}",
        String::from_utf8_lossy(&rest)
    );
}

// The drain still does its job: a tool call that is in flight when the
// signal lands is answered before the server exits.
#[test]
fn sigterm_still_answers_a_tool_call_in_flight() {
    let (url, hits) = slow_page(Duration::from_secs(3));
    let mut server = Server::start("drain");
    let call = fetch_call(server.port, &url);
    hits.recv_timeout(Duration::from_secs(60))
        .expect("the fetch never reached the page");

    server.signal(libc::SIGTERM);
    assert!(
        server.wait_log("shutdown signal received", EXIT_WITHIN),
        "{}",
        server.log()
    );
    let answer = call
        .join()
        .unwrap()
        .unwrap_or_else(|e| panic!("no answer to the call in flight ({e}): {}", server.log()));
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let json = &answer[answer.find("\r\n\r\n").expect("a response head") + 4..];
    let reply: serde_json::Value = serde_json::from_str(json).expect(json);
    assert!(reply.get("error").is_none(), "{reply}");
    assert!(
        reply["result"]
            .to_string()
            .contains("while the server was draining"),
        "{reply}"
    );
    let status = server.exited_within(EXIT_WITHIN);
    assert!(
        status.is_some_and(|s| s.success()),
        "{status:?}: {}",
        server.log()
    );
}

// A second signal is the way out of a drain that waits for a slow call.
// The first one alone must keep waiting.
#[test]
fn a_second_signal_stops_waiting_for_the_drain() {
    let (url, hits) = slow_page(Duration::from_secs(120));
    let mut server = Server::start("second");
    let call = fetch_call(server.port, &url);
    hits.recv_timeout(Duration::from_secs(60))
        .expect("the fetch never reached the page");

    server.signal(libc::SIGINT);
    assert!(
        server.wait_log("shutdown signal received", EXIT_WITHIN),
        "{}",
        server.log()
    );
    assert!(
        server.exited_within(Duration::from_secs(2)).is_none(),
        "one signal must wait for the call in flight: {}",
        server.log()
    );

    server.signal(libc::SIGINT);
    let status = server.exited_within(EXIT_WITHIN);
    assert!(
        status.is_some(),
        "the server still runs {EXIT_WITHIN:?} after a second SIGINT: {}",
        server.log()
    );
    // The call it gave up on is closed unanswered.
    let got = call.join().unwrap();
    assert!(!got.as_ref().is_ok_and(|a| !a.is_empty()), "{got:?}");
}
