//! CLI presentation, Crimson-Noir theme, layout primitives, and kinetic TUI.
//!
//! Cross-platform across Linux, macOS, and Windows:
//! - Enables UTF-8 console codepage (`CP_UTF8`) and Virtual Terminal ANSI
//!   processing on Windows 10/11, degrading cleanly to plain text if a legacy
//!   host refuses VT mode.
//! - Auto-detects 24-bit TrueColor vs 256-color (e.g. macOS `Terminal.app`) vs
//!   16-color ANSI, and strips all escapes on pipes, `--json`, `NO_COLOR`, or
//!   `TERM=dumb`.
//! - Restores cursor visibility (`\x1b[?25h`) on normal completion, `Drop`,
//!   and `Ctrl+C` (`SIGINT`).

pub mod adapters;
pub mod args;
pub mod doctor;
pub mod download;
pub mod keys;
pub mod proxy;
pub mod rollback;
pub mod rules;
pub mod status;
pub mod stop;
pub mod tool;
pub mod update;
pub mod version;

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ── Init & Terminal Capabilities ─────────────────────────────

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static VT_OUT_OK: AtomicBool = AtomicBool::new(true);
static VT_ERR_OK: AtomicBool = AtomicBool::new(true);
static CURSOR_GUARD_ARMED: AtomicBool = AtomicBool::new(false);
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Enable UTF-8 output and ANSI escape processing on Windows 10+.
/// Safe and idempotent on all platforms.
pub fn init() {
    if !INITIALIZED.swap(true, Ordering::Relaxed) {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::*;
            unsafe {
                let _ = SetConsoleOutputCP(65001);
                for (handle, slot) in [
                    (STD_OUTPUT_HANDLE, &VT_OUT_OK),
                    (STD_ERROR_HANDLE, &VT_ERR_OK),
                ] {
                    let h = GetStdHandle(handle);
                    let mut mode: u32 = 0;
                    if GetConsoleMode(h, &mut mode) != 0
                        && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
                    {
                        slot.store(false, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

/// Ensure `Ctrl+C` restores a hidden terminal cursor before exiting.
fn arm_ctrlc_cursor_guard() {
    if CURSOR_GUARD_ARMED.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                if CURSOR_HIDDEN.swap(false, Ordering::Relaxed) {
                    crate::diag_raw!("\r\x1b[K\x1b[?25h\n");
                }
                std::process::exit(130);
            }
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColorTier {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

fn detect_env_tier() -> ColorTier {
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if colorterm == "truecolor"
        || colorterm == "24bit"
        || std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("ITERM_SESSION_ID").is_some()
    {
        return ColorTier::TrueColor;
    }
    let prog = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        prog.as_str(),
        "wezterm" | "vscode" | "ghostty" | "hyper" | "alacritty" | "zed"
    ) {
        return ColorTier::TrueColor;
    }
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if term.contains("256color") || cfg!(target_os = "macos") || cfg!(windows) {
        return ColorTier::Ansi256;
    }
    ColorTier::Ansi16
}

pub fn color_tier_out() -> ColorTier {
    use crate::config::ColorMode;
    match crate::config::cfg().cli.color {
        ColorMode::Always => detect_env_tier(),
        ColorMode::Never => ColorTier::None,
        ColorMode::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return ColorTier::None;
            }
            if std::env::var("TERM")
                .map(|t| t.eq_ignore_ascii_case("dumb"))
                .unwrap_or(false)
            {
                return ColorTier::None;
            }
            if !VT_OUT_OK.load(Ordering::Relaxed) || !std::io::stdout().is_terminal() {
                return ColorTier::None;
            }
            detect_env_tier()
        }
    }
}

pub fn color_tier_err() -> ColorTier {
    use crate::config::ColorMode;
    match crate::config::cfg().cli.color {
        ColorMode::Always => detect_env_tier(),
        ColorMode::Never => ColorTier::None,
        ColorMode::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return ColorTier::None;
            }
            if std::env::var("TERM")
                .map(|t| t.eq_ignore_ascii_case("dumb"))
                .unwrap_or(false)
            {
                return ColorTier::None;
            }
            if !VT_ERR_OK.load(Ordering::Relaxed) || !std::io::stderr().is_terminal() {
                return ColorTier::None;
            }
            detect_env_tier()
        }
    }
}

pub fn colours() -> bool {
    color_tier_out() != ColorTier::None
}

pub fn colours_err() -> bool {
    color_tier_err() != ColorTier::None
}

/// Detect usable terminal column width across Linux, macOS, and Windows.
pub fn term_width() -> usize {
    if let Some(cols) = std::env::var("COLUMNS")
        .ok()
        .and_then(|w| w.parse::<usize>().ok())
        .filter(|&c| c > 0)
    {
        return cols.clamp(60, 120);
    }
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                    return (ws.ws_col as usize).clamp(60, 120);
                }
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            for handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let h = GetStdHandle(handle);
                let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
                if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                    let cols = (info.srWindow.Right as isize) - (info.srWindow.Left as isize) + 1;
                    if cols > 0 {
                        return (cols as usize).clamp(60, 120);
                    }
                }
            }
        }
    }
    84
}

// ── Crimson-Noir Palette Helpers ─────────────────────────────

const RESET: &str = "\x1b[0m";

fn paint_tier(tier: ColorTier, tc: &str, c256: &str, c16: &str, text: &str) -> String {
    match tier {
        ColorTier::None => text.to_string(),
        ColorTier::TrueColor => format!("{tc}{text}{RESET}"),
        ColorTier::Ansi256 => format!("{c256}{text}{RESET}"),
        ColorTier::Ansi16 => format!("{c16}{text}{RESET}"),
    }
}

pub fn crimson(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;229;43;49m",
        "\x1b[38;5;196m",
        "\x1b[91m",
        s,
    )
}

pub fn ember(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;255;107;74m",
        "\x1b[38;5;203m",
        "\x1b[31m",
        s,
    )
}

pub fn green(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;46;204;113m",
        "\x1b[38;5;42m",
        "\x1b[32m",
        s,
    )
}

pub fn red(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;239;68;68m",
        "\x1b[38;5;196m",
        "\x1b[31m",
        s,
    )
}

pub fn yellow(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;245;166;35m",
        "\x1b[38;5;214m",
        "\x1b[33m",
        s,
    )
}

pub fn cyan(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;86;182;194m",
        "\x1b[38;5;73m",
        "\x1b[36m",
        s,
    )
}

pub fn dim(s: &str) -> String {
    paint_tier(
        color_tier_out(),
        "\x1b[38;2;132;138;152m",
        "\x1b[38;5;245m",
        "\x1b[2m",
        s,
    )
}

pub fn bold(s: &str) -> String {
    paint_tier(color_tier_out(), "\x1b[1m", "\x1b[1m", "\x1b[1m", s)
}

fn paint_err(tc: &str, c256: &str, c16: &str, text: &str) -> String {
    paint_tier(color_tier_err(), tc, c256, c16, text)
}

pub fn crimson_err(s: &str) -> String {
    paint_err("\x1b[38;2;229;43;49m", "\x1b[38;5;196m", "\x1b[91m", s)
}
pub fn ember_err(s: &str) -> String {
    paint_err("\x1b[38;2;255;107;74m", "\x1b[38;5;203m", "\x1b[31m", s)
}
pub fn green_err(s: &str) -> String {
    paint_err("\x1b[38;2;46;204;113m", "\x1b[38;5;42m", "\x1b[32m", s)
}
pub fn yellow_err(s: &str) -> String {
    paint_err("\x1b[38;2;245;166;35m", "\x1b[38;5;214m", "\x1b[33m", s)
}
pub fn cyan_err(s: &str) -> String {
    paint_err("\x1b[38;2;86;182;194m", "\x1b[38;5;73m", "\x1b[36m", s)
}
pub fn dim_err(s: &str) -> String {
    paint_err("\x1b[38;2;132;138;152m", "\x1b[38;5;245m", "\x1b[2m", s)
}
pub fn bold_err(s: &str) -> String {
    paint_err("\x1b[1m", "\x1b[1m", "\x1b[1m", s)
}

// ── Icons ────────────────────────────────────────────────────

pub fn icon_pass() -> String {
    green("\u{2713}")
}
pub fn icon_fail() -> String {
    red("\u{2717}")
}
pub fn icon_warn() -> String {
    yellow("\u{26A0}")
}
pub fn icon_brand() -> String {
    crimson("\u{25C6}")
}
pub fn icon_step() -> String {
    crimson("\u{25B8}")
}

// ── Layout Primitives ────────────────────────────────────────

fn rule_line(width: usize) -> String {
    let total = width.clamp(48, 76);
    let accent = 6.min(total);
    let rest = total.saturating_sub(accent);
    format!(
        "  {}{}",
        crimson(&"\u{2501}".repeat(accent)),
        dim(&"\u{2500}".repeat(rest))
    )
}

/// Print a signature Crimson-Noir title banner and two-tone rule.
pub fn print_title(title: &str) {
    println!("  {} {}", icon_brand(), bold(title));
    println!("{}", rule_line(62));
}

/// Print a header with a right-side contextual badge (`v5.5.5`, `linux-x64`, etc.).
pub fn print_header(title: &str, badge: &str) {
    if badge.is_empty() {
        print_title(title);
    } else {
        println!("  {} {}  {}", icon_brand(), bold(title), dim(badge));
        println!("{}", rule_line(62));
    }
}

/// Print a section heading inside a multi-part CLI view.
pub fn print_section(label: &str) {
    println!("\n  {} {}", icon_step(), bold(label));
}

pub fn print_footer() {
    println!("  {}", dim(&"\u{2500}".repeat(62)));
}

/// Print a compact navigation strip of follow-up commands.
pub fn print_nav(hints: &[(&str, &str)]) {
    if hints.is_empty() {
        return;
    }
    let parts: Vec<String> = hints
        .iter()
        .map(|(cmd, desc)| {
            if desc.is_empty() {
                cyan(cmd)
            } else {
                format!("{} {}", cyan(cmd), dim(&format!("({desc})")))
            }
        })
        .collect();
    println!("  {} {}", dim("next:"), parts.join(&dim(" \u{00B7} ")));
}

/// Print an aligned key-value pair (`  key               value`).
pub fn print_kv(key: &str, value: &str) {
    let padded = format!("{:<16}", key);
    println!("  {} {}", dim(&padded), value);
}

/// Print an aligned option/flag row (`    --flag          Description`),
/// padding BEFORE applying ANSI colors so columns stay straight.
pub fn print_opt(flag: &str, desc: &str) {
    let padded = format!("{:<18}", flag);
    println!("    {} {}", cyan(&padded), desc);
}

/// Proportional segmented meter bar (`██████████▓▓░░`) for health & probe summaries.
pub fn meter_bar(pass: u32, warn: u32, fail: u32, width: usize) -> String {
    let total = pass + warn + fail;
    if total == 0 || width == 0 {
        return dim(&"\u{2591}".repeat(width));
    }
    let mut p_cells = ((pass as usize) * width) / (total as usize);
    let mut w_cells = ((warn as usize) * width) / (total as usize);
    let mut f_cells = ((fail as usize) * width) / (total as usize);
    if fail > 0 && f_cells == 0 {
        f_cells = 1;
    }
    if warn > 0 && w_cells == 0 && f_cells < width {
        w_cells = 1;
    }
    let used = p_cells + w_cells + f_cells;
    if used < width && pass > 0 {
        p_cells += width - used;
    } else if used > width {
        p_cells = width.saturating_sub(w_cells + f_cells);
    }
    let rem = width.saturating_sub(p_cells + w_cells + f_cells);
    let mut out = String::new();
    if p_cells > 0 {
        out.push_str(&green(&"\u{2588}".repeat(p_cells)));
    }
    if w_cells > 0 {
        out.push_str(&yellow(&"\u{2593}".repeat(w_cells)));
    }
    if f_cells > 0 {
        out.push_str(&red(&"\u{2592}".repeat(f_cells)));
    }
    if rem > 0 {
        out.push_str(&dim(&"\u{2591}".repeat(rem)));
    }
    out
}

/// Format byte sizes cleanly (`24.6 MB`, `512 KB`, `840 B`).
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

/// `  ✓ Check name              detail`
pub fn check_pass(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!("  {} {} {}", icon_pass(), padded, dim(detail));
}

/// `  ⚠ Check name              detail`
pub fn check_warn(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!("  {} {} {}", icon_warn(), padded, dim(detail));
}

/// `  ✗ Check name              detail`
pub fn check_fail(name: &str, detail: &str, instructions: &str) {
    let padded = format!("{:<26}", name);
    println!("  {} {} {}", icon_fail(), padded, dim(detail));
    if !instructions.is_empty() {
        println!("    {}", yellow(instructions));
    }
}

/// `    Check name              detail` (informational)
pub fn check_dim(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!("    {} {}", padded, dim(detail));
}

/// `  ✓ Check name              fixed: detail`
pub fn check_fixed(name: &str, detail: &str) {
    let padded = format!("{:<26}", name);
    println!(
        "  {} {} {}",
        icon_pass(),
        padded,
        dim(&format!("fixed: {detail}"))
    );
}

// ── Kinetic Animations (Stderr TTY, zero-escape fallback) ────

fn stderr_can_animate() -> bool {
    if !std::io::stderr().is_terminal() || !VT_ERR_OK.load(Ordering::Relaxed) {
        return false;
    }
    !std::env::var("TERM")
        .map(|t| t.eq_ignore_ascii_case("dumb"))
        .unwrap_or(false)
}

fn render_comet_scanner(tick: usize) -> String {
    const SPAN: usize = 8;
    const CYCLE: usize = (SPAN - 1) * 2;
    let phase = tick % CYCLE;
    let pos = if phase < SPAN { phase } else { CYCLE - phase };
    let tier = color_tier_err();
    let mut cells = String::new();
    for i in 0..SPAN {
        let dist = i.abs_diff(pos);
        match dist {
            0 => cells.push_str(&paint_tier(
                tier,
                "\x1b[38;2;229;43;49m",
                "\x1b[38;5;196m",
                "\x1b[91m",
                "\u{25B0}",
            )),
            1 => cells.push_str(&paint_tier(
                tier,
                "\x1b[38;2;255;107;74m",
                "\x1b[38;5;203m",
                "\x1b[31m",
                "\u{25B1}",
            )),
            2 => cells.push_str(&paint_tier(
                tier,
                "\x1b[38;2;135;34;40m",
                "\x1b[38;5;88m",
                "\x1b[2m",
                "\u{2022}",
            )),
            _ => cells.push_str(&paint_tier(
                tier,
                "\x1b[38;2;64;68;80m",
                "\x1b[38;5;239m",
                "\x1b[2m",
                "\u{00B7}",
            )),
        }
    }
    format!("{}{}{}", dim_err("\u{27E6}"), cells, dim_err("\u{27E7}"))
}

/// Crimson-Noir Comet Scanner on stderr with live elapsed readout and dynamic
/// stage updates. Silently degrades to a static status line when non-TTY.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    msg: Arc<Mutex<String>>,
    handle: Option<JoinHandle<()>>,
    animated: bool,
}

impl Spinner {
    pub fn new(msg: &str) -> Self {
        Self::start(msg, true)
    }

    /// Quiet variant: animates on an interactive TTY stderr, emits nothing at
    /// all when stderr is piped/redirected (ideal for `fetch`/`search`/`crawl`
    /// where piped stderr must contain only the final receipt).
    pub fn new_tty_only(msg: &str) -> Self {
        Self::start(msg, false)
    }

    fn start(initial: &str, emit_when_non_tty: bool) -> Self {
        let msg = Arc::new(Mutex::new(initial.to_string()));
        if !stderr_can_animate() {
            if emit_when_non_tty {
                crate::diag!("  {} {}", dim("..."), initial);
            }
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                msg,
                handle: None,
                animated: false,
            };
        }

        arm_ctrlc_cursor_guard();
        CURSOR_HIDDEN.store(true, Ordering::Relaxed);
        crate::diag_raw!("\x1b[?25l");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let msg_clone = Arc::clone(&msg);
        let handle = thread::spawn(move || {
            let started = Instant::now();
            let mut tick = 0usize;
            while !stop_clone.load(Ordering::Relaxed) {
                let label = msg_clone
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let scanner = render_comet_scanner(tick);
                let elapsed = started.elapsed().as_secs_f64();
                let timer = if elapsed >= 0.2 {
                    dim_err(&format!("  {elapsed:.1}s"))
                } else {
                    String::new()
                };
                crate::diag_raw!("\r\x1b[K  {scanner}  {label}{timer}");
                tick = tick.wrapping_add(1);
                thread::sleep(Duration::from_millis(65));
            }
            crate::diag_raw!("\r\x1b[K\x1b[?25h");
            CURSOR_HIDDEN.store(false, Ordering::Relaxed);
        });

        Self {
            stop,
            msg,
            handle: Some(handle),
            animated: true,
        }
    }

    /// Update the live message in-place without restarting the animation.
    pub fn set_message(&self, next: &str) {
        if let Ok(mut guard) = self.msg.lock() {
            guard.clear();
            guard.push_str(next);
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        } else if self.animated {
            CURSOR_HIDDEN.store(false, Ordering::Relaxed);
            crate::diag_raw!("\r\x1b[K\x1b[?25h");
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Multi-Lane Conduit Bar (8-lane parallel range visualizer) ─

const SUB_CELLS: [char; 9] = [
    ' ', '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}', '\u{258B}', '\u{258A}', '\u{2589}',
    '\u{2588}',
];
const LANE_LEVELS: [char; 9] = [
    '\u{00B7}', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

fn render_lane_matrix(lanes: &[f32; download::PARALLEL_CHUNKS]) -> String {
    let tier = color_tier_err();
    let mut cells = String::new();
    for &ratio in lanes {
        let clamped = ratio.clamp(0.0, 1.0);
        let idx = (clamped * 8.0).round() as usize;
        let ch = LANE_LEVELS[idx.min(8)];
        let s = ch.to_string();
        let painted = if idx >= 8 {
            paint_tier(
                tier,
                "\x1b[38;2;46;204;113m",
                "\x1b[38;5;42m",
                "\x1b[32m",
                &s,
            )
        } else if idx >= 4 {
            paint_tier(
                tier,
                "\x1b[38;2;229;43;49m",
                "\x1b[38;5;196m",
                "\x1b[91m",
                &s,
            )
        } else if idx >= 1 {
            paint_tier(
                tier,
                "\x1b[38;2;255;107;74m",
                "\x1b[38;5;203m",
                "\x1b[31m",
                &s,
            )
        } else {
            paint_tier(tier, "\x1b[38;2;64;68;80m", "\x1b[38;5;239m", "\x1b[2m", &s)
        };
        cells.push_str(&painted);
    }
    format!("{}{}{}", dim_err("\u{27E6}"), cells, dim_err("\u{27E7}"))
}

fn render_sub_bar(ratio: f64, width: usize, tick: usize) -> String {
    let tier = color_tier_err();
    let clamped = ratio.clamp(0.0, 1.0);
    let eighths = (clamped * (width as f64) * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let gleam = if full > 2 {
        Some(tick % (full + 4))
    } else {
        None
    };
    let mut out = String::new();
    for i in 0..width {
        if i < full {
            if gleam.is_some_and(|g| i == g || i + 1 == g) {
                out.push_str(&paint_tier(
                    tier,
                    "\x1b[38;2;245;166;35m",
                    "\x1b[38;5;214m",
                    "\x1b[33m",
                    "\u{2588}",
                ));
            } else if i + 2 >= full {
                out.push_str(&paint_tier(
                    tier,
                    "\x1b[38;2;255;107;74m",
                    "\x1b[38;5;203m",
                    "\x1b[31m",
                    "\u{2588}",
                ));
            } else {
                out.push_str(&paint_tier(
                    tier,
                    "\x1b[38;2;229;43;49m",
                    "\x1b[38;5;196m",
                    "\x1b[91m",
                    "\u{2588}",
                ));
            }
        } else if i == full && rem > 0 {
            let s = SUB_CELLS[rem].to_string();
            out.push_str(&paint_tier(
                tier,
                "\x1b[38;2;255;107;74m",
                "\x1b[38;5;203m",
                "\x1b[31m",
                &s,
            ));
        } else {
            out.push_str(&paint_tier(
                tier,
                "\x1b[38;2;62;66;78m",
                "\x1b[38;5;239m",
                "\x1b[2m",
                "\u{2591}",
            ));
        }
    }
    out
}

/// Interactive multi-lane progress bar for `donsetch update` downloads.
/// Throttles redraws to ~60ms on TTY stderr and emits zero escape codes
/// when stderr is non-interactive.
pub struct ConduitBar {
    animated: bool,
    started: Instant,
    last_draw: Instant,
    last_bytes: usize,
    last_rate_at: Instant,
    smooth_bps: f64,
    tick: usize,
}

impl ConduitBar {
    pub fn new(label: &str) -> Self {
        let animated = stderr_can_animate();
        let now = Instant::now();
        if animated {
            arm_ctrlc_cursor_guard();
            CURSOR_HIDDEN.store(true, Ordering::Relaxed);
            let scanner = render_comet_scanner(0);
            crate::diag_raw!("\x1b[?25l\r\x1b[K  {scanner}  {label}");
        } else {
            println!("  {} {label}", dim("..."));
        }
        Self {
            animated,
            started: now,
            last_draw: now.checked_sub(Duration::from_millis(100)).unwrap_or(now),
            last_bytes: 0,
            last_rate_at: now,
            smooth_bps: 0.0,
            tick: 0,
        }
    }

    pub fn update(&mut self, p: &download::Progress<'_>) {
        if !self.animated {
            return;
        }
        let now = Instant::now();
        let is_done = p.total.is_some_and(|t| p.bytes as u64 >= t);
        if !is_done && now.duration_since(self.last_draw) < Duration::from_millis(55) {
            return;
        }
        let dt = now.duration_since(self.last_rate_at).as_secs_f64();
        if dt >= 0.08 {
            if p.bytes >= self.last_bytes {
                let inst = (p.bytes - self.last_bytes) as f64 / dt;
                self.smooth_bps = if self.smooth_bps > 0.0 {
                    self.smooth_bps * 0.72 + inst * 0.28
                } else {
                    inst
                };
            } else {
                self.smooth_bps = 0.0;
            }
            self.last_bytes = p.bytes;
            self.last_rate_at = now;
        }
        self.last_draw = now;
        self.tick = self.tick.wrapping_add(1);

        let badge = match &p.lanes {
            Some(lanes) => render_lane_matrix(lanes),
            None => render_comet_scanner(self.tick),
        };
        let cols = term_width();
        let bar_w = if cols >= 92 { 20 } else { 12 };

        let line = if let Some(total) = p.total.filter(|&t| t > 0) {
            let ratio = (p.bytes as f64) / (total as f64);
            let pct = format!("{:>3}%", ((ratio * 100.0).floor() as u32).min(100));
            let bar = render_sub_bar(ratio, bar_w, self.tick);
            let size_txt = format!("{}/{}", format_bytes(p.bytes as u64), format_bytes(total));
            let speed_txt = if self.smooth_bps > 1024.0 {
                format!(" \u{00B7} {}/s", format_bytes(self.smooth_bps as u64))
            } else {
                String::new()
            };
            let eta_txt = if self.smooth_bps > 4096.0 && (p.bytes as u64) < total {
                let rem = (total - p.bytes as u64) as f64 / self.smooth_bps;
                if rem < 3600.0 {
                    format!(" \u{00B7} eta {rem:.1}s")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            let route_txt = if cols >= 96 && !p.route.is_empty() {
                format!(" \u{00B7} {}", p.route)
            } else {
                String::new()
            };
            format!(
                "  {badge} {bar} {}  {}",
                bold_err(&pct),
                dim_err(&format!("{size_txt}{speed_txt}{eta_txt}{route_txt}"))
            )
        } else {
            let speed_txt = if self.smooth_bps > 1024.0 {
                format!(" \u{00B7} {}/s", format_bytes(self.smooth_bps as u64))
            } else {
                String::new()
            };
            format!(
                "  {badge}  downloading  {}",
                dim_err(&format!("{}{speed_txt}", format_bytes(p.bytes as u64)))
            )
        };
        crate::diag_raw!("\r\x1b[K{line}");
    }

    /// Print a transient route/retry warning cleanly without corrupting the active bar.
    pub fn warn(&mut self, msg: &str) {
        if self.animated {
            crate::diag_raw!("\r\x1b[K");
        }
        println!("  {} {}", icon_warn(), yellow(msg));
    }

    pub fn finish(&mut self) -> Duration {
        let elapsed = self.started.elapsed();
        if self.animated {
            self.animated = false;
            CURSOR_HIDDEN.store(false, Ordering::Relaxed);
            crate::diag_raw!("\r\x1b[K\x1b[?25h");
        }
        elapsed
    }
}

impl Drop for ConduitBar {
    fn drop(&mut self) {
        if self.animated {
            self.animated = false;
            CURSOR_HIDDEN.store(false, Ordering::Relaxed);
            crate::diag_raw!("\r\x1b[K\x1b[?25h");
        }
    }
}

/// Default MCP response posture; handshake compatibility can still fold a session.
pub(crate) fn mcp_output_status() -> serde_json::Value {
    serde_json::json!({
        "mode": if crate::config::cfg().mcp.text_only { "text_only" } else { "split" },
        "source": crate::config::mcp_text_only_source(),
        "known_clients_may_fold": true,
    })
}

pub(crate) fn mcp_output_summary() -> String {
    let state = mcp_output_status();
    format!(
        "{} ({}); known clients may use compatibility folding",
        state["mode"].as_str().unwrap_or("unknown"),
        state["source"].as_str().unwrap_or("unknown")
    )
}
