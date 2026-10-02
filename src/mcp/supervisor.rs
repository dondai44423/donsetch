//! Crash-only supervisor (v3): a panic anywhere in the daemon is a
//! blip, not a death.
//!
//! `donsetch mcp --supervised` spawns the real daemon as a child
//! and proxies stdio. Release builds run `panic = "abort"` : a
//! hostile page that trips an unguarded path would otherwise take
//! the whole MCP session down. Under the supervisor the child
//! restarts (500ms backoff, honest give-up after 5 rapid
//! crashes), reloads its persistent state from disk, and keeps
//! serving.
//!
//! Structure: our stdin is drained by a reader thread into a
//! channel; the main loop multiplexes (new input | child death)
//! with a poll timeout, so an idle crash is caught within 500ms.
//! The replay window is REQUEST-level: every line the client sends
//! with an id is held until its response comes back through the
//! stdout forwarder (or the client cancels it), and a restart
//! replays exactly what the dead child never answered. A request
//! is never silently dropped, and an answered one never runs a
//! second time (issue #281: byte-level replay re-ran whole
//! finished sessions). The MCP surface is stateless here (the
//! daemon answers requests without gating on `initialize`), so a
//! restarted child resumes the session as-is; the one thing the
//! handshake decided, the client's compat mode, reaches each
//! replacement as the client's name in `CLIENT_NAME_ENV`.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_RAPID_RESTARTS: u32 = 5;
const BACKOFF_MS: u64 = 500;
const POLL: Duration = Duration::from_millis(500);
/// A child that served this long before dying was not part of a
/// crash loop: the rapid-restart counter starts over. Without
/// this the counter only ever grew, and a long-lived session gave
/// up on its fifth crash in a month.
const RAPID_WINDOW: Duration = Duration::from_secs(60);
/// After our client closes stdin, how long the daemon gets to
/// answer its in-flight requests and shut down cleanly before
/// it is killed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The client's `clientInfo.name` from its `initialize`, handed to
/// every replacement child. The first child answered the handshake,
/// so the replay window no longer holds it, and a child that never
/// sees it would answer in the default shape for the rest of the
/// session (the #27 fold is chosen by that name).
pub(crate) const CLIENT_NAME_ENV: &str = "DONSETCH_MCP_CLIENT_NAME";

enum In {
    Data(Vec<u8>),
    Eof,
}

pub fn run() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    run_with(
        move || {
            let mut c = Command::new(&exe);
            c.arg("mcp");
            c
        },
        std::io::stdin(),
        std::io::stdout(),
    )
}

/// The supervisor loop over an arbitrary child command, input and
/// output (the real thing uses `donsetch mcp` and our own stdio).
/// Returns once the client has closed `input` AND the daemon has
/// finished: every response it produces on the way out reaches
/// `output`.
fn run_with<R, W>(
    mut child_cmd: impl FnMut() -> Command,
    input: R,
    output: W,
) -> std::io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // main() restores SIGPIPE's default disposition so piped CLI
    // output dies quietly : this process must not. The crash
    // contract below depends on a write to a dead child's stdin
    // coming back as an EPIPE error (hold the bytes, restart,
    // replay) rather than a signal that kills the supervisor; and
    // a broken output pipe just means the client left (handled at
    // the write). The child daemon is unaffected : it makes its
    // own choice in its own main().
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let mut restarts: u32 = 0;
    // Bytes to write to the NEXT child at its spawn: what the dead
    // child never answered (see ReplayState). Empty in steady state.
    let mut pending: Vec<u8> = Vec::new();
    // What the CURRENT child has not answered yet, shared with its
    // stdout forwarder, which retires a request the moment its
    // response passes through (issue #281: replaying answered
    // requests made the replacement re-run finished work).
    let replay = Arc::new(Mutex::new(ReplayState::default()));
    let output = Arc::new(Mutex::new(output));

    // Drain OUR stdin from a thread so the main loop can also
    // watch for child death while the client is idle.
    let (tx, rx) = mpsc::channel::<In>();
    std::thread::spawn(move || {
        let mut input = input;
        let mut buf = [0u8; 16384];
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(In::Eof);
                    return;
                }
                Ok(n) => {
                    if tx.send(In::Data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut child: Option<RunningChild> = None;
    loop {
        // (Re)spawn if needed.
        if child.is_none() {
            if !pending.is_empty() {
                eprintln!(
                    "[supervisor] replaying {} held bytes to the new daemon",
                    pending.len()
                );
            }
            let mut cmd = child_cmd();
            if let Some(name) = replay
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .client_name()
            {
                cmd.env(CLIENT_NAME_ENV, name);
            }
            let mut c = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let mut stdin = c.stdin.take().expect("child stdin");
            let mut stdout = c.stdout.take().expect("child stdout");
            let out = Arc::clone(&output);
            let forwarder_done = Arc::new(AtomicBool::new(false));
            let done = Arc::clone(&forwarder_done);
            let replay_fwd = Arc::clone(&replay);
            std::thread::spawn(move || {
                let mut carry: Vec<u8> = Vec::new();
                let mut buf = [0u8; 16384];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Retire what this span answers: a JSON
                            // object with an id and no method is a
                            // response, and the request it answers
                            // must never run a second time (#281).
                            carry.extend_from_slice(&buf[..n]);
                            while let Some(pos) = carry.iter().position(|b| *b == b'\n') {
                                let line: Vec<u8> = carry.drain(..=pos).collect();
                                if let Some(id) = response_id_of(&line) {
                                    replay_fwd
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .retire(&id);
                                }
                            }
                            if carry.len() > REPLAY_WINDOW {
                                carry.clear(); // no id to match in a line this big
                            }
                            let mut out = out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if out.write_all(&buf[..n]).is_err() {
                                break; // our client is gone
                            }
                            let _ = out.flush();
                        }
                    }
                }
                done.store(true, Ordering::SeqCst);
            });
            // Held bytes first : they predate this child.
            // (Write failure: this child already died; keep pending.)
            if !pending.is_empty() && stdin.write_all(&pending).is_ok() {
                let _ = stdin.flush();
                // The replayed bytes are THIS child's unacked history
                // too: if it also dies before answering them (a crash
                // loop where each child buffers-then-dies), the idle
                // poll must replay them again, not find an empty
                // history and drop the request one restart deeper.
                // Seed the history with what we just wrote.
                let held = std::mem::take(&mut pending);
                replay
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .feed(&held);
            } else {
                replay
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
            }
            child = Some(RunningChild {
                child: c,
                stdin,
                born: Instant::now(),
                forwarder_done,
            });
        }

        let RunningChild {
            child: c,
            stdin,
            born,
            forwarder_done,
        } = child.as_mut().expect("child");
        // Multiplex: new input vs idle child death.
        match rx.recv_timeout(POLL) {
            Ok(In::Data(bytes)) => {
                if stdin.write_all(&bytes).is_ok() {
                    let _ = stdin.flush();
                    replay
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .feed(&bytes);
                } else {
                    // Child died under this write : hold this span
                    // of bytes for its replacement, never drop them.
                    // The span may be partially delivered (the kernel
                    // accepts a prefix into the dead child's pipe), so
                    // all of it is replayed, alongside every request
                    // the child never answered.
                    // Fold in anything already held: a replay that
                    // failed against the previous child left `pending`
                    // populated with the replay set cleared, and
                    // replacing it here would silently drop the held
                    // request one restart deeper (the exact loss this
                    // replay window exists to prevent).
                    let mut held = std::mem::take(&mut pending);
                    held.extend_from_slice(&bytes);
                    {
                        let mut st = replay
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        st.feed(&held);
                        pending = st.take_replay();
                    }
                    eprintln!("[supervisor] daemon died mid-write : holding request for restart");
                    restart_child(c, &mut restarts, born.elapsed());
                    child = None;
                }
            }
            // Our client closed stdin (or its reader thread died):
            // pass the EOF on and let the daemon finish its
            // in-flight work. Its answers travel through the
            // forwarder thread, which only lives as long as this
            // process : returning before the daemon exits would
            // drop every response still on the way out (and cut
            // its shutdown, browser cleanup included, short).
            Ok(In::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(rc) = child.take() {
                    drop(rc.stdin);
                    drain(rc.child);
                }
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: is the child still alive?
                if let Ok(Some(_status)) = c.try_wait() {
                    // The buffered-history case: writes that the
                    // kernel accepted are gone with the child, and
                    // no EPIPE ever fired. Replay what the child
                    // never answered so the replacement serves it
                    // (issue #281: an answered request stays
                    // answered). Let the stdout forwarder drain
                    // first: a response the child wrote before
                    // dying must retire its request before this
                    // snapshot, or the replacement runs it again.
                    await_forwarder(forwarder_done);
                    {
                        let mut st = replay
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        pending = st.take_replay();
                    }
                    if !pending.is_empty() {
                        eprintln!(
                            "[supervisor] daemon died while idle : replaying {} unanswered bytes",
                            pending.len()
                        );
                    }
                    eprintln!("[supervisor] daemon died while idle : restarting");
                    restart_child(c, &mut restarts, born.elapsed());
                    child = None;
                }
            }
        }
    }
}

/// A live daemon child plus what the death paths need to know.
struct RunningChild {
    child: Child,
    stdin: std::process::ChildStdin,
    born: Instant,
    /// Set by this child's stdout forwarder when it reaches EOF: the
    /// death path waits for it so responses already in the pipe
    /// retire their requests before the replay set is snapshotted.
    forwarder_done: Arc<AtomicBool>,
}

/// The unacked-request replay window: on a child death only the
/// requests it never answered are replayed. Holding raw byte
/// history instead (v3 through 4.2.9) re-ran everything the dead
/// child had already answered, so for a long session the
/// replacement re-executed most of its history: minutes of CPU,
/// gigabytes of RSS, and a duplicate response per request at the
/// client (issue #281). Duplicate delivery of an OPEN request
/// beats loss; an answered one is never delivered again.
const REPLAY_WINDOW: usize = 1 << 20;

/// One held line: raw bytes plus the id whose response retires it
/// (None for bytes this parser could not classify).
struct Held {
    id: Option<String>,
    bytes: Vec<u8>,
}

/// What the current child has not answered yet, shared with its
/// stdout forwarder (which retires responses) and the main loop
/// (which feeds forwarded bytes and snapshots the replay).
#[derive(Default)]
struct ReplayState {
    /// In arrival order; bounded to REPLAY_WINDOW total bytes.
    held: VecDeque<Held>,
    /// Bytes of a line still being received (no newline yet): the
    /// child got them, so a replacement must get the head too or
    /// the client's tail would arrive orphaned.
    partial: Vec<u8>,
    /// held bytes + partial bytes, maintained incrementally.
    total: usize,
    /// `clientInfo.name` from the client's `initialize`, for the
    /// children spawned after the one that answered it.
    client_name: Option<String>,
}

impl ReplayState {
    /// Bytes just forwarded to the current child.
    fn feed(&mut self, bytes: &[u8]) {
        let mut start = 0;
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                let partial = std::mem::take(&mut self.partial);
                let partial_len = partial.len();
                let mut line = partial;
                line.extend_from_slice(&bytes[start..=i]);
                start = i + 1;
                self.total -= partial_len; // the partial leaves partial-space
                self.push_line(line); // ...and line-space adds the whole line
            }
        }
        if start < bytes.len() {
            self.partial.extend_from_slice(&bytes[start..]);
            self.total += bytes.len() - start;
        }
        self.enforce_bound();
    }

    /// Classify one complete line.
    fn push_line(&mut self, line: Vec<u8>) {
        let val: Option<serde_json::Value> = serde_json::from_slice(&line).ok();
        let keep = match val.as_ref().and_then(|v| v.as_object()) {
            Some(obj) => {
                let method = obj.get("method").and_then(|m| m.as_str());
                match (method, obj.get("id")) {
                    // A cancellation retires its target and is not
                    // itself replayed.
                    (Some("notifications/cancelled"), _) => {
                        if let Some(rid) = obj
                            .get("params")
                            .and_then(|p| p.get("requestId"))
                            .filter(|v| !v.is_null())
                        {
                            self.retire(&rid.to_string());
                        }
                        None
                    }
                    // A client request: held until its response.
                    (Some("initialize"), Some(id)) if !id.is_null() => {
                        if let Some(name) = obj
                            .get("params")
                            .and_then(|p| p.get("clientInfo"))
                            .and_then(|c| c.get("name"))
                            .and_then(|n| n.as_str())
                            .map(str::trim)
                            .filter(|n| !n.is_empty())
                        {
                            self.client_name = Some(name.to_string());
                        }
                        Some(Some(id.to_string()))
                    }
                    (Some(_), Some(id)) if !id.is_null() => Some(Some(id.to_string())),
                    // A notification (no id): nothing will ever
                    // answer it, so it is not part of the replay
                    // window.
                    (Some(_), None) => None,
                    // Anything else (a client response, a null-id
                    // shape) is held conservatively : the daemon may
                    // accept what this parser does not classify.
                    _ => Some(None),
                }
            }
            // Not JSON (or not an object): hold it whole.
            None => Some(None),
        };
        if let Some(id) = keep {
            self.total += line.len();
            self.held.push_back(Held { id, bytes: line });
        }
    }

    /// A response passed through the forwarder (or a cancellation
    /// arrived): the request it answers must not replay.
    fn retire(&mut self, id: &str) {
        let mut removed = 0;
        self.held.retain(|h| {
            if h.id.as_deref() == Some(id) {
                removed += h.bytes.len();
                false
            } else {
                true
            }
        });
        self.total -= removed;
    }

    /// The client's name from its `initialize`, once seen.
    fn client_name(&self) -> Option<String> {
        self.client_name.clone()
    }

    /// Everything still open, as one replay buffer: held lines in
    /// arrival order, then the partial tail (it came last).
    fn take_replay(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.partial);
        for h in self.held.drain(..) {
            out.extend_from_slice(&h.bytes);
        }
        self.total = 0;
        out
    }

    fn clear(&mut self) {
        self.held.clear();
        self.partial.clear();
        self.total = 0;
    }

    /// Keep the window bounded: drop the OLDEST held lines first
    /// (the newest requests are the ones a replacement still
    /// needs), then trim a too-large partial.
    fn enforce_bound(&mut self) {
        while self.total > REPLAY_WINDOW && !self.held.is_empty() {
            if let Some(old) = self.held.pop_front() {
                self.total -= old.bytes.len();
            }
        }
        if self.total > REPLAY_WINDOW {
            let excess = self.total - REPLAY_WINDOW;
            let cut = excess.min(self.partial.len());
            self.partial.drain(..cut);
            self.total -= cut;
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.total
    }
}

/// The id a child line retires: a JSON object with a non-null id
/// and no method is a response to a tracked request.
fn response_id_of(line: &[u8]) -> Option<String> {
    if !line.windows(4).any(|w| w == b"\"id\"") {
        return None; // cheap skip: not a response shape
    }
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    let obj = v.as_object()?;
    if obj.contains_key("method") {
        return None;
    }
    let id = obj.get("id")?;
    if id.is_null() {
        return None;
    }
    Some(id.to_string())
}

/// Give a dead child's stdout forwarder a beat to drain what the
/// child wrote before dying: a response still sitting in the pipe
/// must retire its request before the replay set is snapshotted,
/// or the replacement runs an answered request (issue #281).
/// Bounded: a stalled output pipe must not stall the restart.
fn await_forwarder(done: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait for a child that has seen EOF to exit on its own, killing
/// it only if it overstays `DRAIN_TIMEOUT`.
fn drain(mut c: Child) {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    loop {
        match c.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => {
                eprintln!("[supervisor] daemon did not exit after stdin closed : killing it");
                let _ = c.kill();
                let _ = c.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The restart count after a child that lived `lived` died: a
/// crash loop counts up; a child that served a full
/// `RAPID_WINDOW` first resets the count to one.
fn next_restart_count(restarts: u32, lived: Duration) -> u32 {
    if lived >= RAPID_WINDOW {
        1
    } else {
        restarts + 1
    }
}

fn restart_child(c: &mut Child, restarts: &mut u32, lived: Duration) {
    let _ = c.kill();
    let _ = c.wait();
    *restarts = next_restart_count(*restarts, lived);
    if *restarts >= MAX_RAPID_RESTARTS {
        eprintln!(
            "[supervisor] {MAX_RAPID_RESTARTS} rapid crashes : giving up (the daemon needs a look)"
        );
        std::process::exit(1);
    }
    std::thread::sleep(Duration::from_millis(BACKOFF_MS));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared sink the test can inspect after `run_with` returns.
    /// (Unix-only with its test: the child is a `sh` one-liner.)
    #[cfg(unix)]
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    #[cfg(unix)]
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // A client that writes its request and closes stdin at once
    // (a one-shot script, `printf ... | donsetch mcp --supervised`)
    // used to get nothing back: the supervisor returned on EOF
    // and the process exit took the stdout forwarder with it
    // before the daemon had answered. Reproduced with the real
    // binary: `donsetch mcp` answered, `--supervised` did not.
    #[cfg(unix)]
    #[test]
    fn responses_after_client_eof_still_reach_the_output() {
        let sink = Sink::default();
        let input = std::io::Cursor::new(b"hello\n".to_vec());
        // A child that answers late: it echoes stdin only after
        // the client has long since closed it.
        run_with(
            || {
                let mut c = Command::new("sh");
                c.args(["-c", "sleep 0.5; cat"]);
                c
            },
            input,
            sink.clone(),
        )
        .unwrap();
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(String::from_utf8_lossy(&got), "hello\n");
    }

    /// A client that waits for the child to ANNOUNCE that its stdin is
    /// closed before writing, then waits to see its request come back
    /// through the replacement before closing. Both waits are on
    /// observable effects, never on a sleep: the announcement is written
    /// after the close, so the request is guaranteed to land on a closed
    /// pipe and come back EPIPE however long the runner took to start
    /// `sh`. The 300ms timer this replaces raced the shell on
    /// macos-x86_64, first as a failed spawn assert, then as a 30s
    /// nextest timeout.
    #[cfg(unix)]
    struct WaitsForReady {
        sink: Arc<Mutex<Vec<u8>>>,
        payload: &'static [u8],
        phase: u8,
    }

    #[cfg(unix)]
    impl Read for WaitsForReady {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.phase {
                0 => {
                    wait_for_marker(&self.sink, b"READY\n");
                    self.phase = 1;
                    buf[..self.payload.len()].copy_from_slice(self.payload);
                    Ok(self.payload.len())
                }
                1 => {
                    // Only the replacement echoes, so the payload
                    // showing up in the sink IS the replay: waiting for
                    // it keeps the final assert from racing the
                    // forwarder thread that writes it out.
                    wait_for_marker(&self.sink, self.payload);
                    self.phase = 2;
                    Ok(0)
                }
                _ => Ok(0),
            }
        }
    }

    /// Block until the sink shows `needle`, bounded so a regression fails
    /// the asserts below instead of hanging into nextest's slow-timeout
    /// (the shape this test took on macos-x86_64).
    #[cfg(unix)]
    fn wait_for_marker(sink: &Arc<Mutex<Vec<u8>>>, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if sink
                .lock()
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // A client whose request lands in the pipe buffer of a child
    // that dies before consuming it. The write succeeds while the
    // child is still alive, so the mid-write EPIPE arm never
    // fires: the death surfaces on the next idle poll, and only
    // the unacked-history replay saves the request. Child 1 must
    // outlive the write, then die; the client holds its EOF long
    // enough for the idle poll to see the death first.
    /// Serves the payload once, then holds the connection open for
    /// a caller-set delay before EOF: the death surfaces on the
    /// idle poll, and the EOF must outlast every poll + restart
    /// backoff the test needs to survive.
    #[cfg(unix)]
    struct WriteThenEofAfter(&'static [u8], bool, u64);

    #[cfg(unix)]
    impl Read for WriteThenEofAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1 {
                std::thread::sleep(Duration::from_millis(self.2));
                return Ok(0);
            }
            self.1 = true;
            buf[..self.0.len()].copy_from_slice(self.0);
            Ok(self.0.len())
        }
    }

    /// The macOS CI signature: the request was buffered into a
    /// dying child, the write reported success, and the request
    /// never reached the replacement.
    #[cfg(unix)]
    #[test]
    fn request_buffered_in_a_dying_child_replays_to_the_replacement() {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                // First child accepts the write, dies 300ms later
                // without consuming it; its replacement serves.
                c.args(["-c", if *n == 1 { "sleep 0.3; exit 0" } else { "cat" }]);
                c
            },
            // Hold EOF past one idle poll (500ms) + the restart
            // backoff (500ms) + a slow CI spawn: 900ms lost the race
            // on a loaded macOS runner (EOF drained the corpse before
            // the replacement existed). Same reasoning as the
            // two-death test below.
            WriteThenEofAfter(b"ping\n", false, 4000),
            sink.clone(),
        )
        .unwrap();
        assert!(
            *spawns.lock().unwrap() >= 2,
            "the dead child must have been replaced"
        );
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "ping\n",
            "the buffered request must replay to the restarted child"
        );
    }

    // The crash-loop case: the replacement ALSO buffers-then-dies
    // before consuming the replayed request. On respawn the replayed
    // `pending` is this child's unacked history too, so it must be
    // seeded into `written` rather than cleared. Without the seed the
    // second idle poll finds an empty history and drops the request
    // one restart deeper : the replay survives exactly one death.
    // Children 1 and 2 accept the write and die; child 3 serves.
    #[cfg(unix)]
    #[test]
    fn buffered_request_survives_two_consecutive_silent_deaths() {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                // First TWO children buffer the request and die; the
                // third finally consumes and echoes it.
                c.args(["-c", if *n <= 2 { "sleep 0.3; exit 0" } else { "cat" }]);
                c
            },
            // Hold EOF well past two deaths + their restart backoffs
            // so both are seen by the idle poll before the client
            // closes and the survivor is drained.
            WriteThenEofAfter(b"ping\n", false, 4000),
            sink.clone(),
        )
        .unwrap();
        assert!(
            *spawns.lock().unwrap() >= 3,
            "both dying children must have been replaced"
        );
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "ping\n",
            "the buffered request must survive a second silent death"
        );
    }

    // #281: a request the dead child ALREADY answered must not run
    // again in its replacement. The client sends one request; child
    // 1 consumes it, answers, and exits; the replacement is `cat`,
    // so anything replayed to it echoes into the sink and fails the
    // assert. RED on the byte-level replay: the finished request
    // came back a second time (the reported class: the replacement
    // re-ran the session and the client logged "unknown message
    // ID" for every duplicate).
    #[cfg(unix)]
    struct SendsThenWaitsForRestart {
        sink: Arc<Mutex<Vec<u8>>>,
        spawns: Arc<Mutex<u32>>,
        sent: bool,
    }

    #[cfg(unix)]
    impl Read for SendsThenWaitsForRestart {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let payload = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
                buf[..payload.len()].copy_from_slice(payload);
                return Ok(payload.len());
            }
            // Wait for the answer, then for the replacement to
            // spawn, then a beat for any replay to reach the sink
            // before EOF drains both. Both waits are on observable
            // effects, never on an assumed schedule alone.
            wait_for_marker(&self.sink, b"\"result\":{}");
            let deadline = Instant::now() + Duration::from_secs(8);
            while *self.spawns.lock().unwrap() < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(300));
            Ok(0) // EOF: the supervisor drains the replacement and returns
        }
    }

    // A replacement child never sees the client's `initialize` (the
    // first child answered it, so it retired from the replay window),
    // and its mode cell starts at Default: after one crash every
    // response to Claude Code, VS Code, OpenCode or pi came back in
    // the split shape for the rest of the session. The supervisor
    // passes the client's name to each replacement.
    #[cfg(unix)]
    struct InitThenRequestAfterRestart {
        sink: Arc<Mutex<Vec<u8>>>,
        spawns: Arc<Mutex<u32>>,
        step: u8,
    }

    #[cfg(unix)]
    impl Read for InitThenRequestAfterRestart {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.step {
                0 => {
                    self.step = 1;
                    let payload = b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"claude-code\",\"version\":\"1.0\"}}}\n";
                    buf[..payload.len()].copy_from_slice(payload);
                    Ok(payload.len())
                }
                1 => {
                    self.step = 2;
                    // The first child's answer, then its replacement.
                    wait_for_marker(&self.sink, b"\"result\":{}");
                    let deadline = Instant::now() + Duration::from_secs(8);
                    while *self.spawns.lock().unwrap() < 2 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let payload = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n";
                    buf[..payload.len()].copy_from_slice(payload);
                    Ok(payload.len())
                }
                _ => {
                    wait_for_marker(&self.sink, b"client=");
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(0)
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_restarted_child_is_told_the_clients_name() {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                c.args([
                    "-c",
                    if *n == 1 {
                        // Answers initialize, then dies.
                        "read l; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}'; sleep 0.3; exit 0"
                    } else {
                        // Reports what the supervisor told it.
                        "read l; printf 'client=%s\\n' \"$DONSETCH_MCP_CLIENT_NAME\""
                    },
                ]);
                c
            },
            InitThenRequestAfterRestart {
                sink: Arc::clone(&sink.0),
                spawns: Arc::clone(&spawns),
                step: 0,
            },
            sink.clone(),
        )
        .unwrap();
        let got = String::from_utf8_lossy(&sink.0.lock().unwrap().clone()).to_string();
        assert!(
            got.contains("client=claude-code\n"),
            "the replacement must learn the client's name; sink: {got:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_answered_request_is_not_replayed_to_the_replacement() {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                c.args([
                    "-c",
                    if *n == 1 {
                        "read line; printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; exit 0"
                    } else {
                        "cat"
                    },
                ]);
                c
            },
            SendsThenWaitsForRestart {
                sink: Arc::clone(&sink.0),
                spawns: Arc::clone(&spawns),
                sent: false,
            },
            sink.clone(),
        )
        .unwrap();
        assert!(
            *spawns.lock().unwrap() >= 2,
            "the dead child must have been replaced"
        );
        let got = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        assert_eq!(
            got, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n",
            "an answered request must not run a second time"
        );
    }

    // #281: the replay window is request-level. A response retires
    // its request, a cancellation retires its target, and a
    // notification (which nothing will ever answer) is not held.
    #[test]
    fn answered_requests_and_cancellations_leave_the_replay_window() {
        let mut st = ReplayState::default();
        st.feed(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n\
              {\"jsonrpc\":\"2.0\",\"id\":\"two\",\"method\":\"b\"}\n\
              {\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"c\"}\n",
        );
        st.retire("1");
        st.feed(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":3}}\n",
        );
        st.feed(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
        let out = st.take_replay();
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("\"id\":1"), "retired request: {text}");
        assert!(!text.contains("\"id\":3"), "cancelled request: {text}");
        assert!(
            !text.contains("initialized"),
            "notifications are not replayed: {text}"
        );
        assert!(
            text.contains("\"id\":\"two\""),
            "open request survives: {text}"
        );
        assert_eq!(st.bytes(), 0);
    }

    #[test]
    fn response_id_only_matches_a_response_shape() {
        assert_eq!(
            response_id_of(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n").as_deref(),
            Some("7")
        );
        // A server-initiated request (id + method) is not a response.
        assert_eq!(
            response_id_of(
                b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"sampling/createMessage\"}\n"
            ),
            None
        );
        assert_eq!(response_id_of(b"not json\n"), None);
        assert_eq!(
            response_id_of(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{}}\n"),
            None
        );
    }

    // Bounded window: a healthy child answers and retires, but an
    // ignored stream of requests must still not grow the history
    // without limit. ReplayState keeps only the newest
    // REPLAY_WINDOW bytes, so "bounded to 1 MiB" is true of the
    // live set, not only of the replay output.
    #[test]
    fn replay_state_bounds_the_unanswered_window() {
        let mut st = ReplayState::default();
        // Feed well past the window in request-sized lines.
        for i in 0..(REPLAY_WINDOW / 200 + 50) {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{i},\"method\":\"x\",\"pad\":\"{}\"}}\n",
                "z".repeat(120)
            );
            st.feed(line.as_bytes());
        }
        assert!(
            st.bytes() <= REPLAY_WINDOW,
            "held {} exceeded the {REPLAY_WINDOW}-byte window",
            st.bytes()
        );
        // The window keeps the TAIL: the newest request survives a
        // replacement, the oldest are the ones dropped.
        st.feed(b"{\"jsonrpc\":\"2.0\",\"id\":999999,\"method\":\"last\"}\n");
        let out = st.take_replay();
        assert!(
            out.ends_with(b"{\"jsonrpc\":\"2.0\",\"id\":999999,\"method\":\"last\"}\n"),
            "the newest request must survive"
        );
        assert!(out.len() <= REPLAY_WINDOW);
        assert_eq!(st.bytes(), 0);
    }

    // main() restores SIGPIPE's default disposition for the CLI
    // (quiet `donsetch --help | head` exits). The supervisor's
    // whole crash contract, though, is built on the write to a
    // dead child's stdin coming back as an EPIPE *error* (hold
    // the bytes, restart, replay): under SIG_DFL that write is a
    // SIGPIPE that kills the supervisor itself before write_all
    // returns. run_with must pin SIG_IGN for its own process no
    // matter what main() set. Without the fix this test does not
    // fail an assert : the test process dies by signal 13.
    #[cfg(unix)]
    #[test]
    fn crash_mid_write_restarts_even_with_cli_sigpipe_disposition() {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        let (spawns, got) = sigpipe_restart_case("exec 0<&-; echo READY");
        assert!(spawns >= 2, "the dead child must have been replaced");
        assert_eq!(
            got, "READY\nping\n",
            "the held request must replay to the restarted child"
        );
    }

    // The macos-x86_64 signature, reproduced on every platform: a shell
    // that is slow to reach its own `exec 0<&-`. A client that wrote on a
    // 300ms timer landed in a LIVE pipe, got no EPIPE, and never reached
    // the restart path: first as a failed spawn assert (#236), then as a
    // 30s nextest timeout (#239). The child's announcement makes the
    // ordering causal instead.
    #[cfg(unix)]
    #[test]
    fn mid_write_restart_does_not_race_a_slow_to_start_child() {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        let (spawns, got) = sigpipe_restart_case("sleep 1; exec 0<&-; echo READY");
        assert!(spawns >= 2, "the dead child must have been replaced");
        assert_eq!(
            got, "READY\nping\n",
            "the held request must replay to the restarted child"
        );
    }

    /// Drive the mid-write crash case. `first_child` must close its own
    /// stdin and announce it on stdout; its replacement is `cat`.
    /// Returns the spawn count and everything the client received.
    #[cfg(unix)]
    fn sigpipe_restart_case(first_child: &'static str) -> (u32, String) {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                c.args(["-c", if *n == 1 { first_child } else { "cat" }]);
                c
            },
            WaitsForReady {
                sink: Arc::clone(&sink.0),
                payload: b"ping\n",
                phase: 0,
            },
            sink.clone(),
        )
        .unwrap();
        let spawns = *spawns.lock().unwrap();
        let got = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        (spawns, got)
    }

    #[test]
    fn restart_counter_resets_after_a_long_lived_child() {
        assert_eq!(next_restart_count(0, Duration::from_millis(10)), 1);
        assert_eq!(next_restart_count(3, Duration::from_secs(5)), 4);
        // Five crashes spread over a long session are not a loop.
        assert_eq!(next_restart_count(4, RAPID_WINDOW), 1);
        assert_eq!(next_restart_count(4, Duration::from_secs(3600)), 1);
    }
}
