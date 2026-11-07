//! The locally installed browser, for capture-style work only.
//!
//! After the v5 swap no fetch touches a browser: the in-tree engine does
//! all fetching. This module keeps the small remainder that needs real
//! pixels or a real login session:
//!
//! - `donsetch login`: a visible browser on the operator's display; the
//!   session is harvested with one CDP call after the human is done.
//! - `web_screenshot`: one short headless capture per call.
//! - `doctor --stealth --parity`: the scorecard echo rendered by the
//!   installed browser, the always-current reference the local fetch is
//!   diffed against.
//!
//! One launch per call, a throwaway profile, no pool, no persistence.
//! The resolution machinery (system paths, snap, distro launchers,
//! Playwright caches) is carried over unchanged from the ghost era: it
//! exists so login and doctor can always name the real binary.
//! `donsetch stop` sweeps crash leftovers of these profiles.

pub mod cdp;
pub mod proc;

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Value, json};

use crate::error::FetchError;

// ────────────────────────── resolution ──────────────────────────

/// A resolved local browser binary.
#[derive(Clone, Debug)]
pub struct BrowserResolution {
    pub path: PathBuf,
    pub source: &'static str,
    /// Full dotted build (e.g. 151.0.7922.108), probed once per
    /// binary per process. Keeps doctor/status/describe on one
    /// honest number.
    pub version: Option<String>,
}

impl BrowserResolution {
    pub fn describe(&self) -> String {
        let version = self.version.clone().unwrap_or_else(|| "unknown".into());
        format!(
            "source={} path={} version={version}",
            self.source,
            self.path.display()
        )
    }
}

/// Resolve the installed Chromium-family binary.
pub fn resolve_browser() -> Result<BrowserResolution, String> {
    resolve_browser_without_download()
}

/// Resolve the installed browser without downloading anything.
/// (Kept as a distinct name: nothing is ever downloaded now.)
pub fn resolve_browser_without_download() -> Result<BrowserResolution, String> {
    let path = chromium_binary()?;
    let version = crate::profile::probe_version_string_at_path(&path);
    let configured = &crate::config::cfg().browser.chromium_path;
    let source = if crate::config::browser_string_is_supplied(
        "browser.chromium_path",
        configured,
        "DONGHOST_CHROME",
    ) {
        "explicit"
    } else {
        "system"
    };
    Ok(BrowserResolution {
        path: PathBuf::from(path),
        source,
        version,
    })
}

fn chromium_binary() -> Result<String, String> {
    // `browser.chromium_path` already layers the legacy DONGHOST_CHROME
    // env var, so the config value is the single explicit source here.
    let configured = &crate::config::cfg().browser.chromium_path;
    if crate::config::browser_string_is_supplied(
        "browser.chromium_path",
        configured,
        "DONGHOST_CHROME",
    ) {
        let path = PathBuf::from(configured);
        if !is_executable(&path) {
            return Err(format!(
                "browser.chromium_path (legacy DONGHOST_CHROME) is not an executable: {}",
                path.display()
            ));
        }
        return Ok(path.to_string_lossy().into_owned());
    }
    for path in known_chrome_paths() {
        if is_executable(&path) {
            return Ok(resolve_past_launchers(&path).to_string_lossy().into_owned());
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in chrome_names() {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Ok(resolve_past_launchers(&candidate)
                        .to_string_lossy()
                        .into_owned());
                }
            }
        }
    }
    Err("no chromium/chrome binary found (set DONGHOST_CHROME)".into())
}

#[cfg(linux_like)]
fn known_chrome_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/google-chrome-stable"),
        // Snap Chromium: the real binary inside the snap mount.
        // The /snap/bin/chromium wrapper is a symlink to /usr/bin/snap
        // and doesn't reliably pass CDP flags through Snap's confinement.
        // Prefer the real binary; fall back to the wrapper below.
        PathBuf::from("/snap/chromium/current/usr/lib/chromium-browser/chrome"),
        PathBuf::from("/snap/bin/chromium"),
    ];
    // Playwright cache: ~/.cache/ms-playwright/chromium-*/,
    // chrome-linux64 and chrome-linux layouts. Many devs have
    // Chromium via `npx playwright install` but not as a system
    // package. Auto-discover it so `donsetch doctor` just works.
    paths.extend(playwright_candidates());
    // Termux: $PREFIX/bin/chromium-browser or chromium.
    // $PREFIX is /data/data/com.termux/files/usr.
    if let Some(prefix) = std::env::var_os("PREFIX") {
        let prefix = PathBuf::from(prefix);
        let p1 = prefix.join("bin/chromium-browser");
        let p2 = prefix.join("bin/chromium");
        // Insert at front so Termux paths are tried first.
        paths.insert(0, p1);
        paths.insert(1, p2);
    }
    paths
}

/// If `path` is a Snap wrapper (canonicalizes to /usr/bin/snap or
/// similar), resolve to the real Chromium binary inside the snap
/// mount. Returns None if `path` is already a real binary or if the
/// real binary can't be found.
#[cfg(linux_like)]
fn resolve_snap_chrome(path: &std::path::Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    let real_str = real.to_string_lossy();
    // Snap wrappers resolve to the snap command itself.
    if !real_str.ends_with("/snap") {
        return None; // Already a real binary.
    }
    // Extract the snap package name from the original path.
    // /snap/bin/chromium → "chromium", /snap/bin/firefox → "firefox"
    let orig = path.to_string_lossy();
    let snap_name = orig.strip_prefix("/snap/bin/").unwrap_or("chromium");
    // Look for the real binary inside the snap mount.
    for candidate in [
        format!("/snap/{snap_name}/current/usr/lib/chromium-browser/chrome"),
        format!("/snap/{snap_name}/current/usr/lib/{snap_name}/chromium"),
    ] {
        let p = PathBuf::from(&candidate);
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(not(linux_like))]
fn resolve_snap_chrome(_path: &std::path::Path) -> Option<PathBuf> {
    None
}

/// Largest file still treated as a launcher wrapper. Distro launchers
/// are kilobytes (Arch's ELF launcher is ~14 KB, Void's shell script
/// 205 B); real Chromium-family browsers are 100 MB and up, so
/// anything bigger is never even read.
#[cfg(linux_like)]
const LAUNCHER_MAX_BYTES: u64 = 1024 * 1024;

/// Smallest file accepted as the real browser behind a launcher.
/// Guards the resolution against latching onto a small helper that
/// lives in the same directory (`chrome-sandbox` is an executable ELF).
#[cfg(linux_like)]
const REAL_BROWSER_MIN_BYTES: u64 = 8 * 1024 * 1024;

/// If `path` is a distro launcher wrapper, resolve the real browser
/// binary it execs. Several distros install `/usr/bin/chromium` as a
/// launcher that PREPENDS host flags before exec'ing the browser:
/// Arch's (a small ELF) reads `/etc/chromium-flags.conf` and
/// `$XDG_CONFIG_HOME/chromium-flags.conf`, Void and Debian ship
/// shell scripts. Those flags are desktop preferences a controlled
/// launch must never inherit: `--ozone-platform=wayland` swaps the
/// platform Chromium would pick in headless mode (it only defaults
/// to `--ozone-platform=headless` when the switch is unset), the
/// GPU process then keeps failing until Chromium intentionally
/// aborts (`GPU process isn't usable. Goodbye.`, issue #321), and
/// `--load-extension` entries are host fingerprints.
///
/// A wrapper is recognized by shape: a small file carrying an
/// absolute path, written out or, in a script, built from the
/// script's own plain variable assignments. Every candidate target
/// must be an executable ELF of real-browser size whose file name
/// looks like a Chromium binary, so a mis-parse falls back to `path`
/// (today's behavior) rather than to something random.
#[cfg(linux_like)]
fn resolve_launcher_binary(path: &std::path::Path) -> Option<PathBuf> {
    use std::io::Read;

    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > LAUNCHER_MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(path)
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;

    // ASCII path-shaped tokens: bytes that can live in a path,
    // starting at a slash. Quotes, shell metacharacters and
    // whitespace end a token, which both splits `VAR=/path` shapes
    // correctly and keeps trailing syntax out.
    // (offset, path): where in the file the reference ends, so
    // written-out and variable-built references keep one order.
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    let mut token = String::new();
    for (at, &b) in bytes.iter().chain(std::iter::once(&0u8)).enumerate() {
        if is_path_byte(b) {
            token.push(b as char);
            continue;
        }
        if let Some(pos) = token.find('/') {
            let cand = &token[pos..];
            if cand.len() <= 300 && !found.iter().any(|(_, p)| p.as_os_str() == cand) {
                found.push((at, PathBuf::from(cand)));
            }
        }
        token.clear();
    }
    // Debian's script never writes the browser's path: it sets
    // `LIBDIR=/usr/lib/$APPNAME` and execs `$LIBDIR/$APPNAME`.
    for (at, cand) in variable_built_paths(&bytes) {
        if !found.iter().any(|(_, p)| p == &cand) {
            found.push((at, cand));
        }
    }
    found.sort_by_key(|(at, _)| *at);

    // The operative exec is the LAST absolute path a launcher
    // touches (scripts exec on their final line; earlier mentions
    // are comments, env exports or helper paths).
    for (_, cand) in found.iter().rev() {
        let Ok(meta) = std::fs::metadata(cand) else {
            continue;
        };
        if !meta.is_file() || !is_executable(cand) || meta.len() < REAL_BROWSER_MIN_BYTES {
            continue;
        }
        let chromium_shaped = cand
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase().contains("chrom"))
            .unwrap_or(false);
        if !chromium_shaped {
            continue;
        }
        let mut magic = [0u8; 4];
        let is_elf = std::fs::File::open(cand)
            .and_then(|mut f| f.read_exact(&mut magic))
            .is_ok()
            && &magic == b"\x7fELF";
        if is_elf {
            return Some(cand.clone());
        }
    }
    None
}

#[cfg(not(linux_like))]
fn resolve_launcher_binary(_path: &std::path::Path) -> Option<PathBuf> {
    None
}

/// True for a byte that can be part of a path token in a launcher:
/// printable ASCII that is not shell or string punctuation.
#[cfg(linux_like)]
fn is_path_byte(b: u8) -> bool {
    let printable = (0x21..=0x7e).contains(&b);
    let delimiter = matches!(
        b,
        b'"' | b'\''
            | b'`'
            | b'$'
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'|'
            | b'&'
            | b';'
            | b'='
            | b','
            | b'\\'
            | b':'
            | b'*'
            | b'?'
            | b'['
            | b']'
            | b'{'
            | b'}'
    );
    printable && !delimiter
}

/// Absolute paths a launcher script builds from its own variables,
/// each with the byte offset of the end of the line that uses it.
/// Only plain assignments count, applied top-down; a value that is
/// computed (`$(...)`, backticks, `${X:-...}`) or that refers to an
/// unknown name leaves its variable unknown, and a token that touches
/// an unknown variable yields no path. Empty for anything that is
/// not a `#!` script.
#[cfg(linux_like)]
fn variable_built_paths(bytes: &[u8]) -> Vec<(usize, PathBuf)> {
    // Marks an unknown variable inside an expanded line: it stays in
    // its token so the token can be dropped whole.
    const UNKNOWN: char = '\u{1}';

    fn is_name(s: &str) -> bool {
        let mut chars = s.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    // `None` when the text cannot be expanded at all (a `${...}` form
    // that is not a bare name, or an unreasonable length).
    fn expand(text: &str, vars: &std::collections::HashMap<String, String>) -> Option<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find('$') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            let (name, used) = if let Some(braced) = after.strip_prefix('{') {
                let close = braced.find('}')?;
                if !is_name(&braced[..close]) {
                    return None;
                }
                (&braced[..close], close + 2)
            } else {
                let len = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..len], len)
            };
            if is_name(name) {
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push(UNKNOWN),
                }
                rest = &after[used..];
            } else {
                // `$@`, `$1`, `$(`, `$$`: not a variable of the script.
                out.push(' ');
                rest = after;
            }
            if out.len() > 4096 {
                return None;
            }
        }
        out.push_str(rest);
        Some(out)
    }

    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    if !text.starts_with("#!") {
        return Vec::new();
    }
    let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    let mut end = 0usize;
    for line in text.split_inclusive('\n') {
        end += line.len();
        let stmt = line.trim();
        if stmt.is_empty() || stmt.starts_with('#') || !stmt.contains(['$', '=']) {
            continue;
        }
        if stmt.contains('$')
            && let Some(expanded) = expand(stmt, &vars)
        {
            let mut token = String::new();
            for c in expanded.chars().chain(std::iter::once(' ')) {
                if c == UNKNOWN || (c.is_ascii() && is_path_byte(c as u8)) {
                    token.push(c);
                    continue;
                }
                if token.starts_with('/') && token.len() <= 300 && !token.contains(UNKNOWN) {
                    out.push((end, PathBuf::from(&token)));
                }
                token.clear();
            }
        }
        let decl = stmt
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(stmt);
        let Some((name, raw)) = decl.split_once('=') else {
            continue;
        };
        if !is_name(name) {
            continue;
        }
        let value = match raw.chars().next() {
            Some(quote @ ('"' | '\'')) => raw[1..]
                .find(quote)
                .map(|close| (&raw[1..1 + close], quote == '\'')),
            _ => Some((
                raw.split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(';'),
                false,
            )),
        };
        let known = match value {
            Some((v, true)) => Some(v.to_string()),
            Some((v, false)) if !v.contains("$(") && !v.contains('`') => {
                expand(v, &vars).filter(|e| !e.contains(UNKNOWN))
            }
            _ => None,
        };
        match known {
            Some(v) if v.len() <= 1024 => {
                vars.insert(name.to_string(), v);
            }
            _ => {
                vars.remove(name);
            }
        }
    }
    out
}

/// Discovery-level wrapper resolution: snap confinement and distro
/// launcher wrappers both hide the real browser behind a shim that
/// cannot carry our flags cleanly. Callers fall back to `path` itself
/// when nothing better is found.
fn resolve_past_launchers(path: &std::path::Path) -> PathBuf {
    if let Some(real) = resolve_snap_chrome(path) {
        return real;
    }
    if let Some(real) = resolve_launcher_binary(path) {
        return real;
    }
    path.to_path_buf()
}

/// Playwright keeps every historical Chromium layout; version
/// bumps moved the binary between `chrome-linux`, `chrome-linux64`,
/// `chrome-win64` and `chrome-mac-arm64` dirs. Probe each revision
/// entry for every known layout. The headless-shell registry dirs
/// (`chromium_headless_shell-*`) are deliberately excluded: the
/// legacy headless shell is a strictly weaker CDP target than the
/// full browser.
fn playwright_entry_suffixes() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
            "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
        ]
    }
    #[cfg(all(windows, not(target_os = "macos")))]
    {
        &["chrome-win64/chrome.exe", "chrome-win/chrome.exe"]
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        &["chrome-linux64/chrome", "chrome-linux/chrome"]
    }
}

/// Playwright registry roots: the explicit override first, then the
/// platform cache dir Playwright itself uses, then the XDG cache on
/// Linux (Playwright honors XDG_CACHE_HOME when set).
fn playwright_registry_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // The config knob wins when set; otherwise the ambient standard
    // var still applies (mirror-when-set semantics).
    let cfg_root = &crate::config::cfg().browser.playwright_path;
    if let Some(root) = crate::config::mirrored_os(
        "browser.playwright_path",
        cfg_root,
        "PLAYWRIGHT_BROWSERS_PATH",
    ) && !root.is_empty()
    {
        roots.push(PathBuf::from(root));
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        #[cfg(target_os = "macos")]
        roots.push(home.join("Library/Caches/ms-playwright"));
        #[cfg(not(target_os = "macos"))]
        roots.push(home.join(".cache/ms-playwright"));
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        roots.push(xdg.join("ms-playwright"));
    }
    #[cfg(windows)]
    if let Some(la) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        roots.push(la.join("ms-playwright"));
    }
    roots
}

/// All Playwright-managed Chromium binaries across registry roots.
/// Shared by the macOS/Linux/Windows `known_chrome_paths` so every
/// platform follows the same layout-evolution rules.
fn playwright_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in playwright_registry_roots() {
        out.extend(playwright_candidates_for_root(&root));
    }
    out
}

/// Chromium binaries inside one registry root. Exposed separately
/// so the discovery rules are unit-testable against a synthetic
/// cache dir without touching the real HOME.
fn playwright_candidates_for_root(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort(); // deterministic across filesystem orderings
    for entry in names {
        let is_chromium_entry = entry
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("chromium-"))
            .unwrap_or(false);
        if !is_chromium_entry {
            // firefox-*/webkit-* registries share the cache dir,
            // and chromium_headless_shell-* is excluded on purpose
            // (see playwright_entry_suffixes).
            continue;
        }
        for suffix in playwright_entry_suffixes() {
            let candidate = entry.join(suffix);
            if candidate.is_file() {
                out.push(candidate);
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn known_chrome_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    // Playwright cache: ~/Library/Caches/ms-playwright, including
    // the Apple Silicon layout (chrome-mac-arm64). Auto-discovered
    // so `npx playwright install` users need no DONGHOST_CHROME.
    paths.extend(playwright_candidates());
    paths
}

#[cfg(windows)]
fn known_chrome_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // Chrome (system + per-user installs).
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            paths.push(PathBuf::from(&pf).join("Google\\Chrome\\Application\\chrome.exe"));
        }
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        let la = PathBuf::from(&la);
        paths.push(la.join("Google\\Chrome\\Application\\chrome.exe"));
        paths.push(la.join("Chromium\\Application\\chrome.exe"));
        // Playwright cache (shared layouts: chrome-win64 first):
        // %LOCALAPPDATA%\ms-playwright\chromium-*\chrome-win64\chrome.exe
        paths.extend(playwright_candidates());
    }
    // Edge: Chromium-based and pre-installed on Windows : often the
    // ONLY CDP-capable browser on a stock box. Its directory is never
    // on PATH, so it must be probed explicitly.
    if let Some(pfx86) = std::env::var_os("ProgramFiles(x86)") {
        paths.push(PathBuf::from(&pfx86).join("Microsoft\\Edge\\Application\\msedge.exe"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        paths.push(PathBuf::from(&pf).join("Microsoft\\Edge\\Application\\msedge.exe"));
    }
    paths
}

#[cfg(windows)]
fn chrome_names() -> &'static [&'static str] {
    // Edge is Chromium-based, pre-installed on Windows : often
    // the only available CDP-capable browser.
    &["chrome.exe", "msedge.exe", "chromium.exe"]
}

#[cfg(not(windows))]
fn chrome_names() -> &'static [&'static str] {
    &[
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ]
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

// ────────────────────────── cookies & captures ──────────────────────────

/// Parse the `cookies` array of a Storage.getCookies CDP response
/// into vault records. Shared by the login harvest and any future
/// CDP cookie reader (all callers must store identical shapes).
pub fn parse_cdp_cookies(res: &Value) -> Vec<crate::state::CookieRecord> {
    let mut out = Vec::new();
    if let Some(arr) = res.get("cookies").and_then(Value::as_array) {
        for c in arr {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("");
            let value = c.get("value").and_then(Value::as_str).unwrap_or("");
            let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
            let expires = c
                .get("expires")
                .and_then(|v| v.as_f64())
                .filter(|&e| e > 0.0)
                .map(|e| e as u64);
            if !name.is_empty() {
                out.push(crate::state::CookieRecord {
                    name: name.to_string(),
                    value: value.to_string(),
                    domain: domain.to_string(),
                    path: c
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    expires_at: expires,
                    secure: c.get("secure").and_then(Value::as_bool).unwrap_or(false),
                    http_only: c.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
                    same_site: c
                        .get("sameSite")
                        .and_then(Value::as_str)
                        .unwrap_or("Lax")
                        .to_string(),
                });
            }
        }
    }
    out
}

/// Writes a captured image to `dest`, readable by its owner only.
pub(crate) fn save_screenshot(dest: &std::path::Path, bytes: &[u8]) -> Result<(), FetchError> {
    // A capture can show a logged-in page: same seal as the stores
    // that hold the session itself.
    crate::config::write_private(dest, bytes)
        .map_err(|e| FetchError::browser(format!("screenshot: {e}")))
}

/// Decodes the `data` field of a CDP `Page.captureScreenshot` reply
/// (standard alphabet, padded) into the image bytes. Malformed input
/// is an error, never a partial image.
fn decode_screenshot_data(data: &str) -> Result<Vec<u8>, FetchError> {
    // Strict decode: Chromium emits canonical padded base64 with no
    // line breaks, so anything else means the reply is not an image.
    BASE64_STANDARD
        .decode(data)
        .map_err(|e| FetchError::browser(format!("screenshot data is not base64: {e}")))
}

// ────────────────────────── one-shot headless sessions ──────────────────────────

/// How long a page may take to reach `document.readyState === complete`.
const LOAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a running browser may take to open its DevTools page target.
const START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Stderr lines a launch-failure message carries at most.
const STDERR_TAIL_LINES: usize = 12;

/// Characters kept of each stderr line a launch-failure message carries.
const STDERR_LINE_CHARS: usize = 200;

/// Bytes of one stderr line held while reading it; the rest of a longer
/// line is discarded.
const STDERR_LINE_BYTES: usize = 4096;

/// How long a launch that saw the browser exit waits for the rest of its
/// stderr.
const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

/// A throwaway headless browser. One per capture; killed on drop.
pub struct HeadlessSession {
    child: tokio::process::Child,
    cdp: cdp::Cdp,
    profile: PathBuf,
    /// Keeps the browser's stderr drained until the session ends.
    _stderr: StderrTail,
}

impl HeadlessSession {
    pub fn cdp(&self) -> &cdp::Cdp {
        &self.cdp
    }

    /// Navigate and wait until the main document is complete, bounded.
    pub async fn navigate(&self, url: &str) -> Result<(), String> {
        self.cdp
            .call(None, "Page.navigate", json!({ "url": url }))
            .await
            .map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        loop {
            let ready = self
                .cdp
                .call(
                    None,
                    "Runtime.evaluate",
                    json!({
                        "expression": "document.readyState === 'complete' && location.href !== 'about:blank'",
                        "returnByValue": true
                    }),
                )
                .await
                .ok()
                .and_then(|v| v.pointer("/result/value").and_then(Value::as_bool))
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
            if started.elapsed() >= LOAD_DEADLINE {
                return Err(format!(
                    "page did not finish loading within {}s",
                    LOAD_DEADLINE.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }
    }

    /// The page's visible text (`document.body.innerText`).
    pub async fn page_text(&self) -> Result<String, String> {
        self.cdp
            .call(
                None,
                "Runtime.evaluate",
                json!({
                    "expression": "document.body ? document.body.innerText : ''",
                    "returnByValue": true
                }),
            )
            .await
            .map_err(|e| e.to_string())?
            .pointer("/result/value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "page text unavailable".to_string())
    }

    /// The main document's current URL as the browser sees it.
    pub async fn current_url(&self) -> Option<String> {
        self.cdp
            .call(
                None,
                "Runtime.evaluate",
                json!({ "expression": "location.href", "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|v| {
                v.pointer("/result/value")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|u| u != "about:blank")
    }

    /// Close the browser and remove its throwaway profile.
    pub async fn close(mut self) {
        let _ = self.cdp.call(None, "Browser.close", json!({})).await;
        if tokio::time::timeout(std::time::Duration::from_secs(6), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// Launch the resolved browser headless with a throwaway profile and
/// connect to its first page target. A browser that exits unsuccessfully
/// before its DevTools endpoint is up fails the launch at once, with the
/// last lines it wrote to stderr; otherwise the launch waits up to
/// `START_DEADLINE` for the endpoint.
pub async fn launch_headless(binary: &Path) -> Result<HeadlessSession, String> {
    launch_headless_within(binary, START_DEADLINE).await
}

/// `launch_headless` with the DevTools deadline as a parameter.
async fn launch_headless_within(
    binary: &Path,
    deadline: std::time::Duration,
) -> Result<HeadlessSession, String> {
    let profile = temporary_profile_dir();
    std::fs::create_dir_all(&profile).map_err(|e| format!("browser profile: {e}"))?;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("port: {e}"))?;
        l.local_addr()
            .map_err(|e| format!("port addr: {e}"))?
            .port()
    };
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--headless=new")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--no-crash-upload")
        .arg("--disable-breakpad")
        .arg("--disable-extensions")
        .arg("--disable-sync")
        .arg("--disable-translate")
        .arg("--mute-audio")
        .arg("--window-size=1280,800")
        .arg("about:blank")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("launch browser: {e}"))?;
    let cleanup = |child: &mut tokio::process::Child, profile: &Path| {
        let _ = child.start_kill();
        let _ = std::fs::remove_dir_all(profile);
    };
    let Some(pipe) = child.stderr.take() else {
        cleanup(&mut child, &profile);
        return Err("launch browser: no stderr pipe".into());
    };
    let mut stderr = StderrTail::spawn(pipe);
    let ws = match page_ws(port, &mut child, &mut stderr, deadline).await {
        Ok(ws) => ws,
        Err(e) => {
            cleanup(&mut child, &profile);
            return Err(e);
        }
    };
    let cdp = match cdp::Cdp::connect(&ws).await {
        Ok(cdp) => cdp,
        Err(e) => {
            cleanup(&mut child, &profile);
            return Err(e.to_string());
        }
    };
    Ok(HeadlessSession {
        child,
        cdp,
        profile,
        _stderr: stderr,
    })
}

/// The DevTools ws URL of the first page target (`/json/list`). Fails as
/// soon as `child` has exited unsuccessfully, naming its exit status and
/// what it wrote to stderr last, or once `deadline` has passed. The
/// client here runs inside the tokio runtime driving the caller, so it
/// must stay async.
async fn page_ws(
    port: u16,
    child: &mut tokio::process::Child,
    stderr: &mut StderrTail,
    deadline: std::time::Duration,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let start = std::time::Instant::now();
    loop {
        if let Ok(resp) = client.get(&endpoint).send().await
            && let Ok(list) = resp.json::<Value>().await
            && let Some(ws) = list.as_array().and_then(|targets| {
                targets
                    .iter()
                    .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                    .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
            })
        {
            return Ok(ws.to_string());
        }
        // A browser that died can never open DevTools: report it now,
        // with its own words, instead of polling a dead port until the
        // deadline. A clean exit keeps polling, because a launcher that
        // hands the browser to a background process and exits 0 still
        // gets its endpoint.
        if let Ok(Some(status)) = child.try_wait()
            && !status.success()
        {
            stderr.settle(STDERR_SETTLE).await;
            return Err(format!(
                "browser exited before opening DevTools ({status}); {}",
                stderr.describe()
            ));
        }
        if start.elapsed() > deadline {
            return Err("timeout waiting for the browser to start".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// A launched browser's stderr, read for as long as the browser runs so
/// it never blocks on a full pipe. Keeps only the last
/// `STDERR_TAIL_LINES` non-empty lines, each cut to `STDERR_LINE_CHARS`
/// characters. Dropping it stops the reader.
struct StderrTail {
    lines: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl StderrTail {
    fn spawn<R>(stderr: R) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(STDERR_TAIL_LINES),
        ));
        let sink = std::sync::Arc::clone(&lines);
        let reader = tokio::spawn(async move {
            let mut stderr = tokio::io::BufReader::new(stderr);
            let mut line = Vec::new();
            while let Ok(true) = read_capped_line(&mut stderr, &mut line).await {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let mut lines = sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if lines.len() == STDERR_TAIL_LINES {
                    lines.pop_front();
                }
                lines.push_back(text.chars().take(STDERR_LINE_CHARS).collect());
            }
        });
        Self { lines, reader }
    }

    /// Wait at most `grace` for the reader to reach the end of the
    /// stream, so lines written just before an exit are kept.
    async fn settle(&mut self, grace: std::time::Duration) {
        if !self.reader.is_finished() {
            let _ = tokio::time::timeout(grace, &mut self.reader).await;
        }
    }

    /// `browser said: <line> | <line>`, oldest first, or `browser wrote
    /// nothing to stderr` when no non-empty line was read.
    fn describe(&self) -> String {
        let lines = self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.is_empty() {
            "browser wrote nothing to stderr".into()
        } else {
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            format!("browser said: {}", lines.join(" | "))
        }
    }
}

impl Drop for StderrTail {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Read one `\n`-terminated line into `line`, keeping at most
/// `STDERR_LINE_BYTES` of it and discarding the rest. `Ok(false)` at the
/// end of the stream.
async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    use tokio::io::AsyncBufReadExt as _;
    line.clear();
    let mut any = false;
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(any);
        }
        any = true;
        let end = bytes.iter().position(|&b| b == b'\n');
        let taken = end.map_or(bytes.len(), |i| i + 1);
        let room = STDERR_LINE_BYTES.saturating_sub(line.len());
        line.extend_from_slice(&bytes[..taken.min(room)]);
        reader.consume(taken);
        if end.is_some() {
            return Ok(true);
        }
    }
}

/// Crash-leftover-safe throwaway profile dir (`donsetch stop` sweeps
/// ones whose owning pid is gone).
fn temporary_profile_dir() -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "donsetch-browse-{}-{nonce}-{sequence}",
        std::process::id()
    ))
}

/// One-shot capture: navigate, settle `wait_ms`, return (png, final
/// url). Used by the `web_screenshot` tool.
pub async fn capture_page_png(
    url: &str,
    full_page: bool,
    wait_ms: u64,
) -> Result<(Vec<u8>, String), String> {
    let resolved = resolve_browser_without_download()?;
    let session = launch_headless(&resolved.path).await?;
    let result = async {
        session.navigate(url).await?;
        if wait_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
        let res = session
            .cdp()
            .call(
                None,
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": full_page }),
            )
            .await
            .map_err(|e| e.to_string())?;
        let data = res
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| "screenshot reply carried no image data".to_string())?;
        let png = decode_screenshot_data(data).map_err(|e| e.to_string())?;
        let final_url = session
            .current_url()
            .await
            .unwrap_or_else(|| url.to_string());
        Ok((png, final_url))
    }
    .await;
    session.close().await;
    result
}

#[cfg(test)]
mod resolution_tests {
    use super::*;

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn playwright_discovers_chrome_linux64_layout() {
        // Regression for issue #84: Playwright moved Chromium into
        // chrome-linux64/; the old discovery only probed the legacy
        // chrome-linux/ layout and reported "Chromium not found".
        let dir = std::env::temp_dir().join(format!("donsetch-pw-test-{}", std::process::id()));
        let entry = dir.join("ms-playwright/chromium-1234/chrome-linux64");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("chrome"), b"fake").unwrap();
        let root = dir.join("ms-playwright");
        let found = playwright_candidates_for_root(&root);
        assert!(
            found.contains(&entry.join("chrome")),
            "chrome-linux64 layout must be discovered, got: {found:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playwright_skips_headless_shell_and_other_browsers() {
        let dir = std::env::temp_dir().join(format!("donsetch-pw-skip-{}", std::process::id()));
        let base = dir.join("ms-playwright");
        for rel in [
            "chromium_headless_shell-1234/chrome-linux/headless_shell",
            "firefox-1234/firefox",
            "webkit-1234/pw_run",
        ] {
            let f = base.join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, b"fake").unwrap();
        }
        assert!(
            playwright_candidates_for_root(&base).is_empty(),
            "headless shell, firefox and webkit must never be discovered as Chrome"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn known_chrome_paths_includes_hardcoded_app_bundles() {
        // Regression: known_chrome_paths() failed to compile on
        // macOS (E0425: cannot find value `paths`) after the
        // Playwright-discovery addition dropped the `let mut paths
        // =` binding on the hardcoded-paths collect().
        let paths = known_chrome_paths();
        for expected in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ] {
            assert!(
                paths.iter().any(|p| p == std::path::Path::new(expected)),
                "missing hardcoded path {expected}, got: {paths:?}"
            );
        }
    }

    // ── Launcher resolution (issue #321) ──
    //
    // Distro chromium packages install a launcher at the discovered
    // path that PREPENDS host flags before exec'ing the real browser
    // (Arch: a tiny ELF reading chromium-flags.conf; Void/Debian:
    // shell scripts). The launch must use the real binary instead.
    // Fixtures use a sparse stand-in for the real browser so the
    // "browser-sized ELF" acceptance check is exercised without
    // writing 100 MB.

    #[cfg(linux_like)]
    fn fake_real_browser(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"\x7fELF").unwrap();
        f.seek(SeekFrom::Start(9_000_000)).unwrap();
        f.write_all(&[0]).unwrap();
        drop(f);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(linux_like)]
    fn launcher_fixture_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("donsetch-launcher-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(linux_like)]
    #[test]
    fn launcher_script_resolves_to_the_real_binary() {
        // Void's shape: export the wrapper env, prepend CHROME_FLAGS,
        // exec the real binary on the last line.
        let dir = launcher_fixture_dir("script");
        let real = fake_real_browser(&dir, "chromium");
        let script = dir.join("wrap-chromium");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nexport CHROME_WRAPPER={p}\nexport CHROME_DESKTOP=chromium.desktop\nCHROME_FLAGS=\"--enable-gpu-rasterization $CHROME_FLAGS\"\nexec {p} $CHROME_FLAGS \"$@\"\n",
                p = real.display()
            ),
        )
        .unwrap();
        assert_eq!(resolve_launcher_binary(&script), Some(real));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(linux_like)]
    #[test]
    fn elf_launcher_resolves_from_embedded_paths() {
        // Arch's shape: no shebang, a small ELF whose rodata carries
        // the real binary's absolute path.
        let dir = launcher_fixture_dir("elf");
        let real = fake_real_browser(&dir, "chromium");
        let launcher = dir.join("launcher-elf");
        let mut bytes = b"\x7fELF\x02\x01\x01\x00junk".to_vec();
        bytes.extend_from_slice(real.display().to_string().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(b"\x00tail");
        std::fs::write(&launcher, &bytes).unwrap();
        assert_eq!(resolve_launcher_binary(&launcher), Some(real));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(linux_like)]
    #[test]
    fn launcher_resolution_takes_the_last_reference_that_checks_out() {
        let dir = launcher_fixture_dir("last");
        let decoy = fake_real_browser(&dir, "chromium-old");
        let real = fake_real_browser(&dir, "chromium");
        let script = dir.join("wrap-chromium");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n# older reference first\nexec {decoy} \"$@\"\nexec {real} \"$@\"\n",
                decoy = decoy.display(),
                real = real.display()
            ),
        )
        .unwrap();
        assert_eq!(
            resolve_launcher_binary(&script),
            Some(real),
            "the final exec is the operative one"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Debian's /usr/bin/chromium (154.0.8037.57-1) never writes the
    // browser's path: it builds it from two variables
    // (`APPNAME=chromium`, `LIBDIR=/usr/lib/$APPNAME`,
    // `exec $LIBDIR/$APPNAME $CHROMIUM_FLAGS "$@"`), and sources
    // /etc/chromium.d/* for host flags on the way. Same shape here,
    // with the fixture dir in place of /usr/lib.
    #[cfg(linux_like)]
    #[test]
    fn launcher_script_resolves_a_path_built_from_variables() {
        let dir = launcher_fixture_dir("vars");
        let libdir = dir.join("chromium");
        std::fs::create_dir_all(&libdir).unwrap();
        let real = fake_real_browser(&libdir, "chromium");
        let script = dir.join("wrap-chromium");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nAPPNAME=chromium\nLIBDIR={base}/$APPNAME\nCHROMIUM_FLAGS=\"\"\nfor file in /etc/chromium.d/*; do . $file; done\nexport CHROMIUM_FLAGS=\"$CHROMIUM_FLAGS --disable-extensions-except=$(echo $CHROMIUM_FLAGS | cut -d= -f2)\"\nexport CHROME_WRAPPER=\"/usr/bin/$APPNAME\"\nif [ $want_temp_profile -eq 0 ] ; then\n    exec $LIBDIR/$APPNAME $CHROMIUM_FLAGS \"$@\"\nelse\n    ${{LIBDIR}}/${{APPNAME}} $CHROMIUM_FLAGS \"$@\"\nfi\n",
                base = dir.display()
            ),
        )
        .unwrap();
        assert_eq!(resolve_launcher_binary(&script), Some(real));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Expansion adds candidates, never trust: a variable-built path
    // goes through the same proof as a literal one, an undefined or
    // computed variable resolves nothing, and a self-referencing
    // assignment cannot loop.
    #[cfg(linux_like)]
    #[test]
    fn launcher_variable_expansion_proves_its_targets_like_literals() {
        let dir = launcher_fixture_dir("vars-reject");
        let foreign = fake_real_browser(&dir, "somewhere-else");
        let script = dir.join("wrap-chromium");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nNAME=somewhere-else\nDIR={base}\nLOOP=$LOOP/x\nHERE=$(dirname \"$0\")\nexec $DIR/$NAME \"$@\"\nexec $HERE/chromium \"$@\"\nexec $UNSET/chromium \"$@\"\nexec $LOOP/chromium \"$@\"\n",
                base = dir.display()
            ),
        )
        .unwrap();
        assert!(foreign.is_file());
        assert_eq!(
            resolve_launcher_binary(&script),
            None,
            "a name that is not chromium-shaped stays unproven, expanded or not"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(linux_like)]
    #[test]
    fn launcher_resolution_refuses_targets_it_cannot_prove() {
        use std::os::unix::fs::PermissionsExt;
        let dir = launcher_fixture_dir("reject");
        // A small executable ELF in the same directory (the
        // chrome-sandbox shape): the size gate must refuse it even
        // though the name is chromium-ish.
        let helper = dir.join("chromium-sandbox-helper");
        std::fs::write(&helper, b"\x7fELFsmall").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A browser-sized ELF that is not chromium-shaped.
        let foreign = fake_real_browser(&dir, "somewhere-else");
        // A target that does not exist at all.
        let script = dir.join("wrap-chromium");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nexec {missing} \"$@\"\nexec {helper} \"$@\"\nexec {foreign} \"$@\"\n",
                missing = dir.join("nowhere").display(),
                helper = helper.display(),
                foreign = foreign.display()
            ),
        )
        .unwrap();
        assert_eq!(
            resolve_launcher_binary(&script),
            None,
            "unproven targets must fall back to the wrapper"
        );
        // And a real-sized browser binary is never re-scanned.
        let real = fake_real_browser(&dir, "chromium");
        assert_eq!(resolve_launcher_binary(&real), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod screenshot_data_tests {
    use super::*;

    #[test]
    fn decodes_rfc4648_vectors_including_the_padded_tail() {
        // The hand-rolled decoder this replaced skipped a final group
        // shorter than four symbols once `=` was stripped, so any image
        // whose length was not a multiple of 3 lost its last 1-2 bytes.
        for (encoded, raw) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(
                decode_screenshot_data(encoded).unwrap(),
                raw.as_bytes(),
                "{encoded}"
            );
        }
    }

    // A capture can show a logged-in page; the file must stay private.
    #[cfg(unix)]
    #[test]
    fn a_saved_screenshot_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("donsetch-shot-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("page.png");
        save_screenshot(&dest, b"\x89PNG\r\n\x1a\n").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn png_signature_round_trips() {
        let png_head = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let encoded = BASE64_STANDARD.encode(png_head);
        assert_eq!(decode_screenshot_data(&encoded).unwrap(), png_head);
    }

    #[test]
    fn malformed_data_is_an_error_not_a_partial_image() {
        // The old decoder mapped unknown symbols to 0 and returned
        // garbage bytes; a bad reply must now fail the capture.
        assert!(decode_screenshot_data("Zm9v!!!!").is_err());
        assert!(decode_screenshot_data("Zm9vYg").is_err(), "missing padding");
        assert!(decode_screenshot_data("Zm9v\nYmFy").is_err(), "line break");
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    use std::time::Duration;

    /// An executable stand-in for the browser that runs `body` under sh,
    /// alone in a directory of its own.
    #[cfg(unix)]
    fn stub_browser(tag: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("donsetch-launch-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("chromium");
        std::fs::write(&stub, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    // The browser dies before DevTools comes up, the way Chromium does
    // when its sandbox cannot start. The launch must say so at once, in
    // the browser's own words, instead of timing out 30 s later.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_browser_that_dies_at_start_is_reported_at_once_with_its_stderr() {
        let stub = stub_browser(
            "dies",
            "echo 'FATAL: No usable sandbox! (stub)' >&2\nexit 3",
        );
        let launched = tokio::time::timeout(Duration::from_secs(10), launch_headless(&stub)).await;
        std::fs::remove_dir_all(stub.parent().unwrap()).ok();
        let error = match launched {
            Err(_) => panic!("a browser that exited at start was still unreported after 10 s"),
            Ok(Ok(_)) => panic!("a stub that exits cannot start a browser session"),
            Ok(Err(error)) => error,
        };
        assert!(
            error.contains("FATAL: No usable sandbox! (stub)"),
            "{error}"
        );
        assert!(error.contains("exit status: 3"), "{error}");
    }

    // Negative: a browser that is still running but slow keeps the whole
    // start deadline and ends as a timeout, even when it writes to
    // stderr meanwhile.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_running_browser_without_devtools_still_times_out() {
        let stub = stub_browser("alive", "echo 'still starting (stub)' >&2\nexec sleep 30");
        let deadline = Duration::from_secs(1);
        let started = std::time::Instant::now();
        let launched = launch_headless_within(&stub, deadline).await;
        let waited = started.elapsed();
        std::fs::remove_dir_all(stub.parent().unwrap()).ok();
        let Err(error) = launched else {
            panic!("a stub without DevTools cannot start a browser session");
        };
        assert_eq!(error, "timeout waiting for the browser to start");
        assert!(waited >= deadline, "gave up after {waited:?}");
    }

    // Negative: a launcher that hands the browser to a background process
    // exits 0 while the endpoint may still come up; that exit is not a
    // crash and the launch keeps waiting.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_clean_exit_during_start_keeps_waiting_for_devtools() {
        let stub = stub_browser("handoff", "echo 'handed off (stub)' >&2\nexit 0");
        let deadline = Duration::from_secs(1);
        let started = std::time::Instant::now();
        let launched = launch_headless_within(&stub, deadline).await;
        let waited = started.elapsed();
        std::fs::remove_dir_all(stub.parent().unwrap()).ok();
        let Err(error) = launched else {
            panic!("a stub without DevTools cannot start a browser session");
        };
        assert_eq!(error, "timeout waiting for the browser to start");
        assert!(waited >= deadline, "gave up after {waited:?}");
    }

    // A flood, an endless line and non-UTF-8 bytes all stay inside the
    // tail's bounds, and the reader still reaches what follows them.
    #[tokio::test]
    async fn the_stderr_tail_stays_bounded() {
        let mut wire = Vec::new();
        for n in 0..20 {
            wire.extend_from_slice(format!("line {n}\n").as_bytes());
        }
        wire.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
        wire.extend_from_slice(b"\nlast \xff line without newline");
        let mut tail = StderrTail::spawn(std::io::Cursor::new(wire));
        tail.settle(Duration::from_secs(5)).await;
        assert!(tail.reader.is_finished(), "the reader must reach the end");
        let lines = tail.lines.lock().unwrap();
        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        assert_eq!(lines[0], "line 10");
        assert_eq!(lines[10], "x".repeat(STDERR_LINE_CHARS));
        assert_eq!(lines[11], "last \u{fffd} line without newline");
    }
}
