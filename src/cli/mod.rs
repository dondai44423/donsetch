//! CLI subcommands: `--version`, `--update`, `--doctor`.
//!
//! Clean TUI with ANSI colours, status icons, and a download
//! spinner. Cross-platform: Linux, macOS, Windows (virtual
//! terminal processing enabled on Windows 10+).

pub mod adapters;
pub mod doctor;
pub mod keys;
pub mod login;
pub mod proxy;
pub mod rollback;
pub mod status;
pub mod stop;
pub mod tool;
pub mod update;
pub mod version;

// ── Init ─────────────────────────────────────────────────────

/// Enable ANSI escape processing on Windows 10+. On Unix this is
/// a no-op (terminals already support ANSI).
pub fn init() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            for handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let h = GetStdHandle(handle);
                let mut mode: u32 = 0;
                if GetConsoleMode(h, &mut mode) != 0 {
                    SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                }
            }
        }
    }
}

// ── Colour helpers ───────────────────────────────────────────

use std::io::IsTerminal;

fn colours() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn paint(colour: &str, text: &str) -> String {
    if colours() {
        format!("{colour}{text}{RESET}")
    } else {
        text.to_string()
    }
}

pub fn green(s: &str) -> String {
    paint(GREEN, s)
}
pub fn red(s: &str) -> String {
    paint(RED, s)
}
pub fn yellow(s: &str) -> String {
    paint(YELLOW, s)
}
pub fn dim(s: &str) -> String {
    paint(DIM, s)
}
pub fn bold(s: &str) -> String {
    paint(BOLD, s)
}

// ── Icons ─────────────────────────────────────────────────────

pub fn icon_pass() -> String {
    green("\u{2713}")
}
pub fn icon_fail() -> String {
    red("\u{2717}")
}
pub fn icon_warn() -> String {
    yellow("\u{26A0}")
}

// ── Layout ────────────────────────────────────────────────────

/// Print a title and a dim divider rule.
pub fn print_title(title: &str) {
    println!("{}", bold(title));
    println!("{}", dim(&"\u{2500}".repeat(57)));
}

pub fn print_footer() {
    println!("{}", dim(&"\u{2500}".repeat(57)));
}

/// Print an aligned key-value pair: `  key       value`.
pub fn print_kv(key: &str, value: &str) {
    let padded = format!("{:<10}", key);
    println!("  {} {}", dim(&padded), value);
}

/// `  \u{2713} Check name              detail`
pub fn check_pass(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!("  {} {} {}", icon_pass(), padded, dim(detail));
}

/// `  \u{26A0} Check name              detail`
pub fn check_warn(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!("  {} {} {}", icon_warn(), padded, dim(detail));
}

/// `  \u{2717} Check name              detail`
///     `    instructions`
pub fn check_fail(name: &str, detail: &str, instructions: &str) {
    let padded = format!("{:<26}", name);
    println!("  {} {} {}", icon_fail(), padded, dim(detail));
    if !instructions.is_empty() {
        println!("    {}", dim(instructions));
    }
}

/// `    Check name              detail`  (no icon, informational)
pub fn check_dim(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!("    {} {}", padded, dim(detail));
}

/// `  \u{2713} Check name              fixed: detail`
pub fn check_fixed(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!(
        "  {} {} {}",
        icon_pass(),
        padded,
        dim(&format!("fixed: {detail}"))
    );
}

// ── Spinner ──────────────────────────────────────────────────

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Animated braille spinner on stderr. Silently degrades to a
/// static line when stderr is not a TTY (piped to a file, etc.).
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn new(msg: &str) -> Self {
        if !std::io::stderr().is_terminal() {
            eprintln!("  {} {}", dim("..."), msg);
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let msg = msg.to_string();
        let handle = thread::spawn(move || {
            let frames = [
                '\u{2807}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}',
                '\u{2827}', '\u{2807}', '\u{280F}',
            ];
            let mut i = 0;
            while !stop_clone.load(Ordering::Relaxed) {
                eprint!("\r  {} {}   ", frames[i], msg);
                i = (i + 1) % frames.len();
                thread::sleep(Duration::from_millis(80));
            }
            eprint!("\r\x1b[K"); // Erase to end of line.
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
