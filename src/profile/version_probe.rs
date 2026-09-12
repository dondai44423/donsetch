//! Bounded browser version-probe capture.
//!
//! One deadline covers both process completion and stdout capture. Once the
//! direct child exits, all bytes it wrote are already in the pipe; descendants
//! may still own inherited write handles, so EOF is not a completion signal.

use std::io::{self, Read};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) fn run(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut tree = ProbeTree::prepare(&mut cmd)?;
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn browser version probe: {e}"))?;
    // Process creation and teardown are OS operations and are not promised to
    // be hard real-time. Everything after spawn shares this deadline.
    let deadline = Instant::now() + timeout;

    let result = (|| {
        tree.attach_and_resume(&child)?;
        let mut pipe = child
            .stdout
            .take()
            .ok_or("browser version probe had no stdout pipe")?;
        prepare_pipe(&pipe).map_err(|e| format!("prepare browser version pipe: {e}"))?;
        capture(&mut child, &mut pipe, deadline)
    })();

    // This also cleans up descendants after the direct child has exited.
    drop(tree);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn capture(child: &mut Child, pipe: &mut ChildStdout, deadline: Instant) -> Result<String, String> {
    let mut output = Vec::new();
    let mut eof = false;
    let mut status: Option<ExitStatus> = None;
    let mut buffer = [0_u8; 4096];

    loop {
        if Instant::now() >= deadline {
            return Err(
                "browser version probe timed out before process exit or output completion".into(),
            );
        }

        // Observe exit before the final drain. If exit became visible after a
        // WouldBlock result, using that stale result could miss bytes written
        // by the child immediately before it exited.
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|e| format!("wait browser version probe: {e}"))?;
        }

        let mut made_progress = false;
        let mut drained = eof;
        if !eof {
            match read_available(pipe, &mut buffer) {
                Ok(0) => {
                    eof = true;
                    drained = true;
                }
                Ok(count) => {
                    if count > OUTPUT_LIMIT.saturating_sub(output.len()) {
                        return Err(format!(
                            "browser version probe exceeded {OUTPUT_LIMIT} output bytes"
                        ));
                    }
                    output.extend_from_slice(&buffer[..count]);
                    made_progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => drained = true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("read browser version pipe: {error}")),
            }
        }

        // An exited direct child cannot write more. Parse once every byte
        // currently readable has been drained; waiting for EOF
        // would let an unrelated inherited handle hold the probe hostage.
        if drained && let Some(status) = status {
            if !status.success() {
                return Err(format!("browser version probe exited with {status}"));
            }
            let text = std::str::from_utf8(&output)
                .map_err(|_| "browser version probe returned invalid UTF-8")?;
            return super::parse_version_string(text)
                .ok_or_else(|| "browser version probe returned no version token".into());
        }

        if !made_progress {
            std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

#[cfg(unix)]
fn prepare_pipe(pipe: &ChildStdout) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let fd = pipe.as_raw_fd();
    // SAFETY: `fd` remains owned by `pipe`; preserve all existing status bits.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_pipe(_: &ChildStdout) -> io::Result<()> {
    // Rust 1.98 (the pinned toolchain) creates the parent's Windows pipe end
    // for asynchronous I/O. That keeps PeekNamedPipe below non-blocking; the
    // Windows lifecycle tests guard this implementation-level dependency.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn prepare_pipe(_: &ChildStdout) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(unix)]
fn read_available(pipe: &mut ChildStdout, buffer: &mut [u8]) -> io::Result<usize> {
    pipe.read(buffer)
}

#[cfg(windows)]
fn read_available(pipe: &mut ChildStdout, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{Foundation, System::Pipes::PeekNamedPipe};

    let mut available = 0_u32;
    // SAFETY: this function is the only reader. It reads no more than the
    // number of bytes observed as available on this pipe.
    unsafe {
        if PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        ) == 0
        {
            let error = Foundation::GetLastError();
            if error == Foundation::ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    if available == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let count = buffer.len().min(available as usize);
    pipe.read(&mut buffer[..count])
}

#[cfg(not(any(unix, windows)))]
fn read_available(_: &mut ChildStdout, _: &mut [u8]) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}

struct ProbeTree {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    job: crate::ghost::proc::KillOnCloseJob,
}

impl ProbeTree {
    fn prepare(cmd: &mut Command) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            Ok(Self { pid: None })
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading;

            // `Command` does not retain the initial thread handle. Suspending
            // closes the race in which descendants could escape before the
            // process is assigned to the kill-on-close Job Object.
            cmd.creation_flags(Threading::CREATE_SUSPENDED | Threading::CREATE_NO_WINDOW);
            crate::ghost::proc::KillOnCloseJob::new().map(|job| Self { job })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = cmd;
            Err("version probe unsupported on this platform".into())
        }
    }

    fn attach_and_resume(&mut self, child: &Child) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.pid = Some(child.id());
            Ok(())
        }

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;

            self.job
                .assign(child.as_raw_handle())
                .map_err(|error| format!("assign suspended probe to Job Object: {error}"))?;
            crate::ghost::proc::resume_process(child.as_raw_handle())
                .map_err(|status| format!("resume probe process: NTSTATUS {status:#x}"))
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Err("version probe unsupported on this platform".into())
        }
    }
}

impl Drop for ProbeTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: the command was put in a fresh process group; the
            // negative PID therefore cannot address our own group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn script(source: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", source]);
        cmd
    }

    #[test]
    fn exited_parent_output_is_complete_without_descendant_eof() {
        let start = Instant::now();
        assert_eq!(
            run(
                script("sleep 30 & printf 'Chromium 151.0.0.0\\n'; exit 0"),
                Duration::from_millis(250),
            )
            .unwrap(),
            "151.0.0.0"
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn live_parent_and_closed_stdout_both_remain_bounded() {
        for source in ["sleep 30", "exec 1>&-; sleep 30"] {
            let start = Instant::now();
            let error = run(script(source), Duration::from_millis(250)).unwrap_err();
            assert!(error.contains("timed out"), "{error}");
            assert!(start.elapsed() < Duration::from_secs(3));
        }
    }

    #[test]
    fn bounded_capture_handles_valid_invalid_and_failed_output() {
        assert_eq!(
            run(
                script("printf 'Chromium 151.0.0.0\\n'"),
                Duration::from_secs(2)
            )
            .unwrap(),
            "151.0.0.0"
        );
        assert!(
            run(script("printf 'not a version'"), Duration::from_secs(2))
                .unwrap_err()
                .contains("no version")
        );
        assert!(
            run(
                script("printf 'Chromium 151.0.0.0\\n'; exit 2"),
                Duration::from_secs(2)
            )
            .unwrap_err()
            .contains("exited")
        );
        assert!(
            run(script("printf '\\377'"), Duration::from_secs(2))
                .unwrap_err()
                .contains("invalid UTF-8")
        );
    }

    #[test]
    fn output_limit_accepts_the_boundary_and_rejects_more() {
        let exact = "printf 'Chromium 151.0.0.0\\n'; \
                     dd if=/dev/zero bs=65517 count=1 2>/dev/null";
        assert_eq!(
            run(script(exact), Duration::from_secs(2)).unwrap(),
            "151.0.0.0"
        );

        let oversized = "printf 'Chromium 151.0.0.0\\n'; \
                         dd if=/dev/zero bs=65518 count=1 2>/dev/null";
        let error = run(script(oversized), Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("65536"), "{error}");

        let error = run(script("yes 'Chromium 151.0.0.0'"), Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("65536"), "{error}");
    }

    #[test]
    fn successful_probe_reaps_a_descendant_that_closed_stdout() {
        let pid_file = std::env::temp_dir().join(format!(
            "donsetch-version-probe-descendant-{}.pid",
            std::process::id()
        ));
        let mut cmd = script(
            "sleep 30 >/dev/null 2>&1 & \
             echo $! > \"$DONSETCH_PROBE_PID_FILE\"; \
             printf 'Chromium 151.0.0.0\\n'",
        );
        cmd.env("DONSETCH_PROBE_PID_FILE", &pid_file);

        assert_eq!(run(cmd, Duration::from_secs(2)).unwrap(), "151.0.0.0");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(pid_file);

        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "descendant {pid} survived"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn fixture(role: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "profile::version_probe::windows_tests::fixture_process",
            "--nocapture",
        ])
        .env("DONSETCH_REVIEW_PROBE_ROLE", role);
        cmd
    }

    #[test]
    // These descendants deliberately outlive the fixture parent: the tests
    // below verify that closing the probe's Job Object reaps them instead.
    #[allow(clippy::zombie_processes)]
    fn fixture_process() {
        let Ok(role) = std::env::var("DONSETCH_REVIEW_PROBE_ROLE") else {
            return;
        };
        match role.as_str() {
            "descendant" => std::thread::sleep(Duration::from_secs(30)),
            "parent-exits" => {
                let _descendant = fixture("descendant")
                    .stdout(Stdio::inherit())
                    .spawn()
                    .unwrap();
                println!("Chromium 151.0.0.0");
            }
            "parent-waits" => {
                fixture("descendant")
                    .stdout(Stdio::inherit())
                    .status()
                    .unwrap();
            }
            "detached" => {
                let _descendant = fixture("descendant").stdout(Stdio::null()).spawn().unwrap();
                println!("Chromium 151.0.0.0");
            }
            "flood" => {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                loop {
                    stdout.write_all(&[b'x'; 4096]).unwrap();
                }
            }
            "valid" => println!("Chromium 151.0.0.0"),
            _ => panic!("unknown fixture role"),
        }
    }

    #[test]
    fn job_accepts_exited_parent_output_and_bounds_a_live_parent() {
        let start = Instant::now();
        assert_eq!(
            run(fixture("parent-exits"), Duration::from_secs(10)).unwrap(),
            "151.0.0.0"
        );
        assert!(start.elapsed() < Duration::from_secs(5));

        let start = Instant::now();
        let error = run(fixture("parent-waits"), Duration::from_millis(500)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn job_capture_accepts_finite_output_and_bounds_floods() {
        assert_eq!(
            run(fixture("valid"), Duration::from_secs(10)).unwrap(),
            "151.0.0.0"
        );
        assert!(
            run(fixture("flood"), Duration::from_secs(10))
                .unwrap_err()
                .contains("65536")
        );
        assert_eq!(
            run(fixture("detached"), Duration::from_secs(10)).unwrap(),
            "151.0.0.0"
        );
    }
}
