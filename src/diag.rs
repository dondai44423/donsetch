//! Diagnostics that cannot kill the process.
//!
//! std's stderr printing macros panic when the write fails, and the
//! profile the binary ships is `panic = "abort"` (`Cargo.toml`): a
//! stderr pipe whose reader is gone — an MCP client that closed its
//! read end, a CLI run whose holder exited — turned a diagnostic line
//! into SIGABRT in the middle of a session (#364). A dropped log line
//! is a lost log line; it is never a reason to die.
//!
//! Every diagnostic in this crate goes through [`diag!`], and
//! `[lints.clippy] print_stderr = "deny"` keeps a raw stderr print
//! macro from coming back (§ the `Cargo.toml` entry).
//!
//! One case stays fatal by design: a CLI command other than `mcp`
//! restores SIGPIPE's default disposition (`main.rs`), so a pipeline
//! that closes the pipe kills the process with exit 141, the way
//! `rg` and `curl` do. That is the shell convention, not a panic,
//! and `diag!` cannot reach it: the kernel delivers the signal
//! during the write, before any error is returned.

/// Write a diagnostic line to stderr, dropping a failed write.
///
/// Drop-in for the stderr print macro: same format arguments, same
/// bytes on a working stderr, and no panic when the write fails.
#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr(), $($arg)*);
    }};
}

/// [`diag!`] without the trailing newline: a line a caller rewrites in
/// place with `\r` (kinetic CLI progress indicators). Locks and flushes
/// stderr as one frame so escape sequences never interleave or buffer.
#[macro_export]
macro_rules! diag_raw {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let mut err = ::std::io::stderr().lock();
        let _ = ::std::write!(err, $($arg)*);
        let _ = err.flush();
    }};
}
