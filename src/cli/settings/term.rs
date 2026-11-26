//! Cross-platform raw terminal driver & Crimson-Noir TUI rendering helpers.
//!
//! Works natively on Linux, macOS, and Windows using zero external TUI crates:
//! - Unix: `termios` raw mode + `poll()` + ANSI/VT escape parsing + `TIOCGWINSZ`
//! - Windows: Win32 Console `ReadConsoleInputW` + `WaitForSingleObject` + VT output
//! - RAII [`TerminalGuard`] restores console mode, cursor visibility, and the
//!   main screen buffer on every exit path.

use std::io::{self, Write};

use crate::cli::{self, ColorTier};

/// Normalized keyboard input events consumed by the settings TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyEvent {
    Up,
    Down,
    Left,
    Right,
    ShiftLeft,
    ShiftRight,
    Tab,
    BackTab,
    Enter,
    Esc,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl(char),
    Char(char),
}

/// Detect `(cols, rows)` of the active terminal across Unix and Windows.
pub fn term_size() -> (usize, usize) {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0
                {
                    return ((ws.ws_col as usize).max(60), (ws.ws_row as usize).max(18));
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
                    let rows = (info.srWindow.Bottom as isize) - (info.srWindow.Top as isize) + 1;
                    if cols > 0 && rows > 0 {
                        return ((cols as usize).max(60), (rows as usize).max(18));
                    }
                }
            }
        }
    }
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .max(60);
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(30)
        .max(18);
    (cols, rows)
}

/// RAII raw-mode & alternate-screen guard.
pub struct TerminalGuard {
    #[cfg(unix)]
    orig_termios: libc::termios,
    #[cfg(windows)]
    in_handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    orig_in_mode: u32,
    active: bool,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        cli::init();
        #[cfg(unix)]
        {
            let mut orig: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut orig) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = orig;
            unsafe { libc::cfmakeraw(&mut raw) };
            // Keep OPOST so `\n` translates cleanly if needed, though our frame
            // renderer emits explicit `\r\n`.
            raw.c_oflag |= libc::OPOST;
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut out = io::stdout().lock();
            out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")?;
            out.flush()?;
            Ok(Self {
                orig_termios: orig,
                active: true,
            })
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::*;
            unsafe {
                let h = GetStdHandle(STD_INPUT_HANDLE);
                let mut mode: u32 = 0;
                if GetConsoleMode(h, &mut mode) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let raw = mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
                if SetConsoleMode(h, raw) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut out = io::stdout().lock();
                out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")?;
                out.flush()?;
                Ok(Self {
                    in_handle: h,
                    orig_in_mode: mode,
                    active: true,
                })
            }
        }
    }

    pub fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = io::stdout().write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
        #[cfg(unix)]
        unsafe {
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.orig_termios);
        }
        #[cfg(windows)]
        unsafe {
            let _ = windows_sys::Win32::System::Console::SetConsoleMode(
                self.in_handle,
                self.orig_in_mode,
            );
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Write an entire rendered frame to stdout in a single atomic flush to avoid
/// flicker during animations.
pub fn flush_frame(lines: &[String]) -> io::Result<()> {
    let mut buf = String::with_capacity(lines.len() * 160 + 64);
    buf.push_str("\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            buf.push_str("\r\n");
        }
        buf.push_str(line);
        buf.push_str("\x1b[0m\x1b[K");
    }
    buf.push_str("\x1b[J");
    let mut out = io::stdout().lock();
    out.write_all(buf.as_bytes())?;
    out.flush()
}

/// Poll for one key event, waiting up to `timeout_ms` milliseconds.
pub fn poll_key(timeout_ms: u32) -> Option<KeyEvent> {
    #[cfg(unix)]
    {
        unix_poll_key(timeout_ms)
    }
    #[cfg(windows)]
    {
        windows_poll_key(timeout_ms)
    }
}

#[cfg(unix)]
fn read_byte_timed(timeout_ms: i32) -> Option<u8> {
    let mut pfd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc <= 0 || (pfd.revents & libc::POLLIN) == 0 {
        return None;
    }
    let mut b = 0u8;
    let n = unsafe { libc::read(libc::STDIN_FILENO, (&mut b as *mut u8).cast(), 1) };
    if n == 1 { Some(b) } else { None }
}

#[cfg(unix)]
fn unix_poll_key(timeout_ms: u32) -> Option<KeyEvent> {
    let first = read_byte_timed(timeout_ms as i32)?;
    match first {
        0x1b => {
            // Distinguish lone Escape from CSI/SS3 escape sequence.
            let Some(second) = read_byte_timed(25) else {
                return Some(KeyEvent::Esc);
            };
            let mut seq = vec![second];
            if matches!(second, b'[' | b'O') {
                while seq.len() < 12 {
                    let Some(next) = read_byte_timed(10) else {
                        break;
                    };
                    seq.push(next);
                    if (0x40..=0x7e).contains(&next) {
                        break;
                    }
                }
            }
            Some(parse_ansi_sequence(&seq))
        }
        b'\r' | b'\n' => Some(KeyEvent::Enter),
        b'\t' => Some(KeyEvent::Tab),
        0x7f | 0x08 => Some(KeyEvent::Backspace),
        1..=26 => {
            let c = (b'a' + (first - 1)) as char;
            Some(KeyEvent::Ctrl(c))
        }
        b if b < 0x80 => Some(KeyEvent::Char(b as char)),
        lead => {
            // Assemble multi-byte UTF-8 sequence.
            let expected = if lead & 0xe0 == 0xc0 {
                2
            } else if lead & 0xf0 == 0xe0 {
                3
            } else if lead & 0xf8 == 0xf0 {
                4
            } else {
                return None;
            };
            let mut bytes = vec![lead];
            for _ in 1..expected {
                if let Some(next) = read_byte_timed(15) {
                    bytes.push(next);
                } else {
                    break;
                }
            }
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.chars().next())
                .map(KeyEvent::Char)
        }
    }
}

/// Pure ANSI sequence decoder (tested independently of TTY state).
#[cfg(any(unix, test))]
pub(crate) fn parse_ansi_sequence(seq: &[u8]) -> KeyEvent {
    match seq {
        b"[A" | b"OA" => KeyEvent::Up,
        b"[B" | b"OB" => KeyEvent::Down,
        b"[C" | b"OC" => KeyEvent::Right,
        b"[D" | b"OD" => KeyEvent::Left,
        b"[1;2C" | b"[1;5C" => KeyEvent::ShiftRight,
        b"[1;2D" | b"[1;5D" => KeyEvent::ShiftLeft,
        b"[Z" => KeyEvent::BackTab,
        b"[H" | b"OH" | b"[1~" => KeyEvent::Home,
        b"[F" | b"OF" | b"[4~" => KeyEvent::End,
        b"[3~" => KeyEvent::Delete,
        b"[5~" => KeyEvent::PageUp,
        b"[6~" => KeyEvent::PageDown,
        _ => KeyEvent::Esc,
    }
}

#[cfg(windows)]
fn windows_poll_key(timeout_ms: u32) -> Option<KeyEvent> {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Console::*;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        if WaitForSingleObject(h, timeout_ms) != WAIT_OBJECT_0 {
            return None;
        }
        let mut rec: INPUT_RECORD = std::mem::zeroed();
        let mut count: u32 = 0;
        if ReadConsoleInputW(h, &mut rec, 1, &mut count) == 0 || count == 0 {
            return None;
        }
        if u32::from(rec.EventType) != KEY_EVENT {
            return None;
        }
        let ke = rec.Event.KeyEvent;
        if ke.bKeyDown == 0 {
            return None;
        }
        let shift = (ke.dwControlKeyState & SHIFT_PRESSED) != 0;
        let ctrl = (ke.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED)) != 0;
        // Standard Win32 virtual key codes.
        match ke.wVirtualKeyCode {
            0x26 => return Some(KeyEvent::Up),
            0x28 => return Some(KeyEvent::Down),
            0x25 => {
                return Some(if shift || ctrl {
                    KeyEvent::ShiftLeft
                } else {
                    KeyEvent::Left
                });
            }
            0x27 => {
                return Some(if shift || ctrl {
                    KeyEvent::ShiftRight
                } else {
                    KeyEvent::Right
                });
            }
            0x09 => {
                return Some(if shift {
                    KeyEvent::BackTab
                } else {
                    KeyEvent::Tab
                });
            }
            0x0D => return Some(KeyEvent::Enter),
            0x1B => return Some(KeyEvent::Esc),
            0x08 => return Some(KeyEvent::Backspace),
            0x2E => return Some(KeyEvent::Delete),
            0x24 => return Some(KeyEvent::Home),
            0x23 => return Some(KeyEvent::End),
            0x21 => return Some(KeyEvent::PageUp),
            0x22 => return Some(KeyEvent::PageDown),
            _ => {}
        }
        let code = ke.uChar.UnicodeChar;
        if ctrl && (1..=26).contains(&code) {
            return Some(KeyEvent::Ctrl((b'a' + (code as u8 - 1)) as char));
        }
        if code >= 0x20
            && let Some(ch) = char::from_u32(u32::from(code))
        {
            return Some(KeyEvent::Char(ch));
        }
        None
    }
}

// ── Styling & Visual Primitives (Crimson-Noir Palette) ──────────────────────

const RESET: &str = "\x1b[0m";

pub fn paint(tc: &str, c256: &str, c16: &str, text: &str) -> String {
    match cli::color_tier_out() {
        ColorTier::None => text.to_string(),
        ColorTier::TrueColor => format!("{tc}{text}{RESET}"),
        ColorTier::Ansi256 => format!("{c256}{text}{RESET}"),
        ColorTier::Ansi16 => format!("{c16}{text}{RESET}"),
    }
}

pub fn fg_crimson(s: &str) -> String {
    paint("\x1b[38;2;229;43;49m", "\x1b[38;5;196m", "\x1b[91m", s)
}

pub fn fg_crimson_bold(s: &str) -> String {
    paint(
        "\x1b[1;38;2;229;43;49m",
        "\x1b[1;38;5;196m",
        "\x1b[1;91m",
        s,
    )
}

pub fn fg_ember(s: &str) -> String {
    paint("\x1b[38;2;255;107;74m", "\x1b[38;5;203m", "\x1b[31m", s)
}

pub fn fg_gold(s: &str) -> String {
    paint("\x1b[38;2;245;166;35m", "\x1b[38;5;214m", "\x1b[33m", s)
}

pub fn fg_emerald(s: &str) -> String {
    paint("\x1b[38;2;46;204;113m", "\x1b[38;5;42m", "\x1b[32m", s)
}

pub fn fg_cyan(s: &str) -> String {
    paint("\x1b[38;2;86;182;194m", "\x1b[38;5;73m", "\x1b[36m", s)
}

pub fn fg_white_bold(s: &str) -> String {
    paint(
        "\x1b[1;38;2;244;246;250m",
        "\x1b[1;38;5;255m",
        "\x1b[1;97m",
        s,
    )
}

pub fn fg_soft(s: &str) -> String {
    paint("\x1b[38;2;204;210;222m", "\x1b[38;5;252m", "\x1b[37m", s)
}

pub fn fg_dim(s: &str) -> String {
    paint("\x1b[38;2;126;132;148m", "\x1b[38;5;244m", "\x1b[2m", s)
}

pub fn fg_muted(s: &str) -> String {
    paint("\x1b[38;2;74;78;92m", "\x1b[38;5;239m", "\x1b[2m", s)
}

pub fn pill_active_tab(s: &str) -> String {
    paint(
        "\x1b[1;38;2;255;255;255;48;2;204;34;42m",
        "\x1b[1;38;5;255;48;5;160m",
        "\x1b[1;97;41m",
        s,
    )
}

pub fn pill_inactive_tab(s: &str) -> String {
    paint(
        "\x1b[38;2;156;162;178;48;2;30;32;40m",
        "\x1b[38;5;248;48;5;236m",
        "\x1b[2m",
        s,
    )
}

pub fn pill_on(flash: bool) -> String {
    if flash {
        paint(
            "\x1b[1;38;2;16;22;18;48;2;245;196;66m",
            "\x1b[1;38;5;16;48;5;220m",
            "\x1b[1;30;43m",
            " \u{2501}\u{25CF} ON  ",
        )
    } else {
        paint(
            "\x1b[1;38;2;14;28;20;48;2;46;204;113m",
            "\x1b[1;38;5;16;48;5;42m",
            "\x1b[1;30;42m",
            " \u{2501}\u{25CF} ON  ",
        )
    }
}

pub fn pill_off(flash: bool) -> String {
    if flash {
        paint(
            "\x1b[1;38;2;255;230;210;48;2;196;62;48m",
            "\x1b[1;38;5;255;48;5;160m",
            "\x1b[1;97;41m",
            " \u{25CB}\u{2501} OFF ",
        )
    } else {
        paint(
            "\x1b[38;2;150;156;172;48;2;44;48;58m",
            "\x1b[38;5;248;48;5;238m",
            "\x1b[2m",
            " \u{25CB}\u{2501} OFF ",
        )
    }
}

pub fn pill_badge_warn(s: &str) -> String {
    paint(
        "\x1b[1;38;2;24;18;8;48;2;245;166;35m",
        "\x1b[1;38;5;16;48;5;214m",
        "\x1b[1;30;43m",
        s,
    )
}

pub fn pill_badge_ok(s: &str) -> String {
    paint(
        "\x1b[1;38;2;14;28;20;48;2;46;204;113m",
        "\x1b[1;38;5;16;48;5;42m",
        "\x1b[1;30;42m",
        s,
    )
}

pub fn pill_badge_crimson(s: &str) -> String {
    paint(
        "\x1b[1;38;2;255;255;255;48;2;229;43;49m",
        "\x1b[1;38;5;255;48;5;196m",
        "\x1b[1;97;41m",
        s,
    )
}

pub fn bg_row_selected(s: &str) -> String {
    paint("\x1b[48;2;36;24;28m", "\x1b[48;5;236m", "\x1b[7m", s)
}

/// Animated Crimson-Noir comet scanner rendered inside the TUI header.
pub fn comet_scanner(tick: usize) -> String {
    const SPAN: usize = 7;
    const CYCLE: usize = (SPAN - 1) * 2;
    let phase = tick % CYCLE;
    let pos = if phase < SPAN { phase } else { CYCLE - phase };
    let mut cells = String::new();
    for i in 0..SPAN {
        match i.abs_diff(pos) {
            0 => cells.push_str(&fg_crimson("\u{25B0}")),
            1 => cells.push_str(&fg_ember("\u{25B1}")),
            2 => cells.push_str(&paint(
                "\x1b[38;2;135;34;40m",
                "\x1b[38;5;88m",
                "\x1b[2m",
                "\u{2022}",
            )),
            _ => cells.push_str(&fg_muted("\u{00B7}")),
        }
    }
    format!("{}{cells}{}", fg_dim("\u{27E6}"), fg_dim("\u{27E7}"))
}

/// Animated slider bar with crimson→ember gradient, thumb dot, and gleam flash.
pub fn render_slider(ratio: f64, width: usize, flash: bool, tick: usize) -> String {
    let w = width.max(6);
    let clamped = ratio.clamp(0.0, 1.0);
    let pos = ((clamped * (w - 1) as f64).round() as usize).min(w - 1);
    let gleam = if flash { Some(tick % w) } else { None };
    let mut out = String::new();
    out.push_str(&fg_muted("["));
    for i in 0..w {
        if i == pos {
            if flash {
                out.push_str(&fg_gold("\u{25CF}"));
            } else {
                out.push_str(&fg_white_bold("\u{25CF}"));
            }
        } else if i < pos {
            if gleam == Some(i) {
                out.push_str(&fg_gold("\u{2501}"));
            } else if i + 2 >= pos {
                out.push_str(&fg_ember("\u{2501}"));
            } else {
                out.push_str(&fg_crimson("\u{2501}"));
            }
        } else {
            out.push_str(&fg_muted("\u{2500}"));
        }
    }
    out.push_str(&fg_muted("]"));
    out
}

/// Animated horizontal sweep divider for the header (sweeps left→right on boot).
pub fn sweep_rule(width: usize, sweep_pos: Option<usize>) -> String {
    let mut out = String::new();
    let accent = 8.min(width);
    for i in 0..width {
        if let Some(pos) = sweep_pos {
            let d = i.abs_diff(pos);
            if d == 0 {
                out.push_str(&fg_gold("\u{2501}"));
                continue;
            } else if d <= 2 {
                out.push_str(&fg_ember("\u{2501}"));
                continue;
            } else if d <= 4 {
                out.push_str(&fg_crimson("\u{2501}"));
                continue;
            }
        }
        if i < accent {
            out.push_str(&fg_crimson("\u{2501}"));
        } else {
            out.push_str(&fg_muted("\u{2500}"));
        }
    }
    out
}

/// Strip ANSI CSI escapes to compute visible display width (ASCII/BMP cell width).
pub fn visible_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        len += 1;
    }
    len
}

/// Truncate plain text to at most `max_chars` characters with a trailing `…`.
pub fn truncate_plain(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars == 1 {
        return "\u{2026}".to_string();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('\u{2026}');
    out
}

/// Pad a styled string on the right with spaces to reach `target_cols` visible columns.
pub fn pad_right_styled(s: &str, target_cols: usize) -> String {
    let vis = visible_len(s);
    if vis >= target_cols {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(target_cols - vis))
    }
}

/// Wrap plain prose cleanly at word boundaries to fit `width` columns.
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let w = width.max(12);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let wlen = word.chars().count();
            if cur.is_empty() {
                if wlen > w {
                    cur.push_str(&truncate_plain(word, w));
                } else {
                    cur.push_str(word);
                }
            } else if cur.chars().count() + 1 + wlen <= w {
                cur.push(' ');
                cur.push_str(word);
            } else {
                lines.push(std::mem::take(&mut cur));
                if wlen > w {
                    cur.push_str(&truncate_plain(word, w));
                } else {
                    cur.push_str(word);
                }
            }
        }
        if !cur.is_empty() || para.is_empty() {
            lines.push(cur);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_sequence_decoder_maps_navigation_and_shift_arrows() {
        assert_eq!(parse_ansi_sequence(b"[A"), KeyEvent::Up);
        assert_eq!(parse_ansi_sequence(b"[B"), KeyEvent::Down);
        assert_eq!(parse_ansi_sequence(b"[C"), KeyEvent::Right);
        assert_eq!(parse_ansi_sequence(b"[D"), KeyEvent::Left);
        assert_eq!(parse_ansi_sequence(b"[1;2C"), KeyEvent::ShiftRight);
        assert_eq!(parse_ansi_sequence(b"[1;2D"), KeyEvent::ShiftLeft);
        assert_eq!(parse_ansi_sequence(b"[Z"), KeyEvent::BackTab);
        assert_eq!(parse_ansi_sequence(b"[3~"), KeyEvent::Delete);
        assert_eq!(parse_ansi_sequence(b"[5~"), KeyEvent::PageUp);
        assert_eq!(parse_ansi_sequence(b"[6~"), KeyEvent::PageDown);
    }

    #[test]
    fn visible_len_and_wrapping_ignore_ansi_escapes() {
        let styled = "\x1b[38;2;229;43;49mhello\x1b[0m world";
        assert_eq!(visible_len(styled), 11);
        let wrapped = wrap_plain("one two three four five", 10);
        assert_eq!(wrapped, vec!["one two", "three four", "five"]);
    }
}
