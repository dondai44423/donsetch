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
//! with a poll timeout, so an idle crash is caught within 500ms
//! and any bytes read-but-not-yet-forwarded when a child died are
//! held as `pending` and written to the NEXT child : a request is
//! never silently dropped. The MCP surface is stateless here (the
//! daemon answers requests without gating on `initialize`), so a
//! restarted child resumes the session as-is.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
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
    let mut pending: Vec<u8> = Vec::new();
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

    let mut child: Option<(Child, std::process::ChildStdin, Instant)> = None;
    loop {
        // (Re)spawn if needed.
        if child.is_none() {
            if !pending.is_empty() {
                eprintln!(
                    "[supervisor] replaying {} held bytes to the new daemon",
                    pending.len()
                );
            }
            let mut c = child_cmd()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let mut stdin = c.stdin.take().expect("child stdin");
            let mut stdout = c.stdout.take().expect("child stdout");
            let out = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
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
            });
            // Held bytes first : they predate this child.
            // (Write failure: this child already died; keep pending.)
            if !pending.is_empty() && stdin.write_all(&pending).is_ok() {
                let _ = stdin.flush();
                pending.clear();
            }
            child = Some((c, stdin, Instant::now()));
        }

        let (c, stdin, born) = child.as_mut().expect("child");
        // Multiplex: new input vs idle child death.
        match rx.recv_timeout(POLL) {
            Ok(In::Data(bytes)) => {
                if stdin.write_all(&bytes).is_ok() {
                    let _ = stdin.flush();
                } else {
                    // Child died under this write : hold the bytes
                    // for its replacement, never drop them.
                    pending = bytes;
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
                if let Some((c, stdin, _)) = child.take() {
                    drop(stdin);
                    drain(c);
                }
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: is the child still alive?
                if let Ok(Some(_status)) = c.try_wait() {
                    eprintln!("[supervisor] daemon died while idle : restarting");
                    restart_child(c, &mut restarts, born.elapsed());
                    child = None;
                }
            }
        }
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

    /// A client that sends one request after a delay long enough
    /// for the first child to have died, then EOF.
    #[cfg(unix)]
    struct DelayedOnce(&'static [u8], bool);

    #[cfg(unix)]
    impl Read for DelayedOnce {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1 {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(300));
            self.1 = true;
            buf[..self.0.len()].copy_from_slice(self.0);
            Ok(self.0.len())
        }
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
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                // First child dies instantly; its replacement serves.
                c.args(["-c", if *n == 1 { "exit 0" } else { "cat" }]);
                c
            },
            DelayedOnce(b"ping\n", false),
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
            "the held request must replay to the restarted child"
        );
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
