//! Browser fingerprint profiles as data.
//!
//! Captured live from Chromium 150 via tls.peet.ws/api/all (2026-07-30).
//! New browser version = new table, not new code.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Windows,
    MacOs,
}

impl Platform {
    pub fn host() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    /// Sec-CH-UA-Platform value.
    pub fn ch_platform(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::Windows => "Windows",
            Self::MacOs => "macOS",
        }
    }

    /// UA platform token.
    fn ua_token(self) -> &'static str {
        match self {
            Self::Linux => "X11; Linux x86_64",
            Self::Windows => "Windows NT 10.0; Win64; x64",
            Self::MacOs => "Macintosh; Intel Mac OS X 10_15_7",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TlsProfile {
    /// TLS <= 1.2 cipher list (SSL_CTX_set_cipher_list), Chrome order.
    /// (TLS 1.3 suites need no config: BoringSSL's default order IS
    /// Chrome's 4865-4866-4867.)
    pub ciphers_12: &'static str,
    /// Supported groups / key shares, Chrome order.
    pub groups: &'static str,
    /// Signature algorithms.
    pub sigalgs: &'static str,
    /// ALPN wire format.
    pub alpn: &'static [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct H2Profile {
    pub header_table_size: u32,
    pub enable_push: u32,
    pub initial_window_size: u32,
    pub max_header_list_size: u32,
    /// Connection-level WINDOW_UPDATE increment sent after preface.
    pub conn_window_update: u32,
}

#[derive(Clone, Debug)]
pub struct BrowserProfile {
    #[allow(dead_code)]
    pub name: &'static str,
    pub tls: TlsProfile,
    pub h2: H2Profile,
    pub user_agent: String,
    pub sec_ch_ua: String,
    pub platform: Platform,
}

impl BrowserProfile {
    /// Chrome 150 on the given platform. Ground truth: Chromium 150 capture, 2026-07-30.
    pub fn chrome_150(platform: Platform) -> Self {
        Self::chrome(150, platform)
    }

    /// Chrome `major` on the given platform. The TLS/H2 tables are
    /// the Chrome 150 capture (stable across adjacent versions);
    /// the UA and client hints carry the real version so the ghost
    /// browser and tier 1 advertise the SAME identity — clearance
    /// cookies are bound to it, and a ghost solving on Chromium 151
    /// while tier 1 claims 150 gets its replays rejected.
    pub fn chrome(major: u32, platform: Platform) -> Self {
        Self {
            name: "chrome-150",
            tls: TlsProfile {
                // 4865-4866-4867 then 49195-49199-49196-49200-52393-52392-49171-49172-156-157-47-53
                ciphers_12: "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
                             ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
                             ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
                             ECDHE-RSA-AES128-SHA:ECDHE-RSA-AES256-SHA:\
                             AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
                groups: "X25519MLKEM768:X25519:P-256:P-384",
                sigalgs: "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:\
                          ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:\
                          rsa_pss_rsae_sha512:rsa_pkcs1_sha512",
                alpn: b"\x02h2\x08http/1.1",
            },
            h2: H2Profile {
                header_table_size: 65536,
                enable_push: 0,
                initial_window_size: 6291456,
                max_header_list_size: 262144,
                conn_window_update: 15663105,
            },
            user_agent: format!(
                "Mozilla/5. ({}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36",
                platform.ua_token()
            ),
            sec_ch_ua: format!(
                "\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"{major}\", \"Google Chrome\";v=\"{major}\""
            ),
            platform,
        }
    }

    /// Default: host-coherent identity (a Windows agent looks like Windows Chrome).
    /// The major version is probed from the INSTALLED browser when
    /// one exists (ghost + tier 1 must claim the same version).
    pub fn host_default() -> Self {
        match probe_installed_major() {
            Some(major) => Self::chrome(major, Platform::host()),
            None => Self::chrome_150(Platform::host()),
        }
    }

    /// Ordered header template for a document GET, Chrome order.
    /// (name, value-or-placeholder). Placeholders filled by caller.
    pub fn h1_headers(&self, host: &str, path: &str) -> Vec<(String, String)> {
        vec![
            ("host".into(), host.into()),
            ("connection".into(), "keep-alive".into()),
            ("sec-ch-ua".into(), self.sec_ch_ua.clone()),
            ("sec-ch-ua-mobile".into(), "?0".into()),
            ("sec-ch-ua-platform".into(), format!("\"{}\"", self.platform.ch_platform())),
            ("upgrade-insecure-requests".into(), "1".into()),
            ("user-agent".into(), self.user_agent.clone()),
            ("accept".into(), "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7".into()),
            ("sec-fetch-site".into(), "none".into()),
            ("sec-fetch-mode".into(), "navigate".into()),
            ("sec-fetch-user".into(), "?1".into()),
            ("sec-fetch-dest".into(), "document".into()),
            ("accept-encoding".into(), "gzip, deflate, br, zstd".into()),
            ("accept-language".into(), accept_language_for(host, path).to_string()),
            ("priority".into(), "u=0, i".into()),
        ]
    }
}

/// v3 F5: Accept-Language coherent with the target's locale.
/// Many sites gate localized content on this header — an en-US
/// header on a .ru page is served the English stub (and is a mild
/// incoherence signal). Derived from the host TLD and non-Latin
/// script in the path; everything else stays Chrome-default en-US.
pub fn accept_language_for(host: &str, path: &str) -> &'static str {
    let tld = host.rsplit('.').next().unwrap_or("");
    let lang = match tld {
        "ru" | "su" => Some("ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7"),
        "de" => Some("de-DE,de;q=0.9,en-US;q=0.8,en;q=0.7"),
        "fr" => Some("fr-FR,fr;q=0.9,en-US;q=0.8,en;q=0.7"),
        "jp" | "ne" => Some("ja-JP,ja;q=0.9,en-US;q=0.8,en;q=0.7"),
        "cn" => Some("zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7"),
        "tw" | "hk" | "mo" => Some("zh-TW,zh;q=0.9,en-US;q=0.8,en;q=0.7"),
        "kr" => Some("ko-KR,ko;q=0.9,en-US;q=0.8,en;q=0.7"),
        "it" => Some("it-IT,it;q=0.9,en-US;q=0.8,en;q=0.7"),
        "es" => Some("es-ES,es;q=0.9,en-US;q=0.8,en;q=0.7"),
        "pt" => Some("pt-PT,pt;q=0.9,en-US;q=0.8,en;q=0.7"),
        "pl" => Some("pl-PL,pl;q=0.9,en-US;q=0.8,en;q=0.7"),
        "tr" => Some("tr-TR,tr;q=0.9,en-US;q=0.8,en;q=0.7"),
        "nl" => Some("nl-NL,nl;q=0.9,en-US;q=0.8,en;q=0.7"),
        "cz" | "cs" => Some("cs-CZ,cs;q=0.9,en-US;q=0.8,en;q=0.7"),
        "se" => Some("sv-SE,sv;q=0.9,en-US;q=0.8,en;q=0.7"),
        _ => None,
    };
    if let Some(l) = lang {
        return l;
    }
    // Cyrillic (percent-encoded UTF-8 %D0-%D4 lead bytes) or CJK
    // in the path ⇒ locale hint even on a .com.
    if path.contains("%D0") || path.contains("%D1") {
        return "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7";
    }
    if path.contains("%E4") || path.contains("%E5") || path.contains("%E6") || path.contains("%E7")
    {
        return "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7";
    }
    if path.contains("%E3") {
        return "ja-JP,ja;q=0.9,en-US;q=0.8,en;q=0.7";
    }
    "en-US,en;q=0.9"
}

/// Probe the installed browser's major version. Cached after first call.
///
/// Two strategies, tried in order:
///
/// 1. **Registry read (Windows).** Chromium-family browsers persist
///    their own version under `HKCU\Software\<Name>\BLBeacon\version`
///    (Google Chrome, Chromium, Microsoft Edge, Thorium, …). Reading
///    it costs zero browser launches — no window, no hang, no orphaned
///    processes. This is the tier-1-friendly path: tier 1 never needs
///    to spawn a real browser just to know what version to claim.
/// 2. **Spawned `--version` probe, hard timeboxed.** Only reached
///    when the registry has nothing (custom forks with no BLBeacon
///    entry). The child is killed — whole tree on Windows — if it
///    does not answer within `PROBE_SPAWN_TIMEOUT`, so a wedged
///    browser can never block tier 1 startup or leave processes behind.
///
/// The result is cached in a `OnceLock` so the probe runs at most
/// once per process.
static PROBED_MAJOR: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

/// Hard cap on the spawned `--version` probe. Chrome 129 on Windows
/// is known to hang (or crash-loop its network/GPU services) under
/// `--headless=new`; without this cap tier 1 blocks forever at boot.
const PROBE_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

pub fn probe_installed_major() -> Option<u32> {
    *PROBED_MAJOR.get_or_init(|| {
        // Fast path: read what browser wrote about itself. No spawn.
        if let Some(major) = probe_registry_major() {
            return Some(major);
        }
        // Fallback: ask the binary, but never let it hang or orphan.
        probe_spawned_major()
    })
}

/// Windows: read the major version from the browser's own
/// `BLBeacon\version` registry value. Honours `DONGHOST_CHROME` by
/// probing the browser family it names first.
#[cfg(windows)]
fn probe_registry_major() -> Option<u32> {
    use windows_sys::Win32::System::Registry as reg;

    // Candidate registry paths, in preference order. `DONGHOST_CHROME`
    // names one binary — probe its family first so a Thorium fork is
    // read from the Thorium key, not from whichever Chrome is installed.
    let mut keys: Vec<&str> = vec![
        "Software\\Google\\Chrome\\BLBeacon",
        "Software\\Chromium\\BLBeacon",
        "Software\\Microsoft\\Edge\\BLBeacon",
        "Software\\Thorium\\BLBeacon",
    ];
    // Sort DONGHOST_CHROME's family to the front if it doesn't already
    // lead — cheap, and makes the explicit choice authoritative.
    if let Some(p) = std::env::var_os("DONGHOST_CHROME") {
        let p = p.to_string_lossy().to_lowercase();
        for (family, key) in [
            ("thorium", "Software\\Thorium\\BLBeacon"),
            ("chrome", "Software\\Google\\Chrome\\BLBeacon"),
            ("chromium", "Software\\Chromium\\BLBeacon"),
            ("edge", "Software\\Microsoft\\Edge\\BLBeacon"),
        ] {
            if p.contains(family) {
                if let Some(pos) = keys.iter().position(|k| *k == key) {
                    keys.remove(pos);
                    keys.insert(0, key);
                }
                break;
            }
        }
    }

    for key in keys {
        if let Some(v) = registry_string(reg::HKEY_CURRENT_USER, key, "version")
            && let Some(major) = parse_version_major(&v)
        {
            return Some(major);
        }
    }
    None
}

/// Non-Windows: there is no registry to consult — fall through to
/// the spawned probe directly. (Stub keeps `probe_installed_major`
/// platform-symmetric.)
#[cfg(not(windows))]
fn probe_registry_major() -> Option<u32> {
    None
}

/// Read a REG_SZ value from the Windows registry without spawning
/// anything. Returns `None` on any failure (missing key, wrong type,
/// access denied).
#[cfg(windows)]
fn registry_string(
    hive: windows_sys::Win32::System::Registry::HKEY,
    key_path: &str,
    value_name: &str,
) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry as reg;

    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let path = wide(key_path);
    let name = wide(value_name);

    let mut hkey = std::ptr::null_mut();
    // SAFETY: RegOpenKeyExW with a nul-terminated wide path and a
    // valid out-param. KEY_READ only — no write access requested.
    let rc = unsafe { reg::RegOpenKeyExW(hive, path.as_ptr(), 0, reg::KEY_READ, &mut hkey) };
    if rc != 0 || hkey.is_null() {
        return None;
    }

    let mut buf = [0u8; 256];
    let mut size = buf.len() as u32;
    let mut typ: u32 = 0;
    // SAFETY: RegQueryValueExW into a fixed buffer with size in/out.
    let rc = unsafe {
        reg::RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &mut typ,
            buf.as_mut_ptr(),
            &mut size,
        )
    };
    // SAFETY: the handle is valid; close it in all paths.
    unsafe { reg::RegCloseKey(hkey) };

    if rc != 0 || typ != reg::REG_SZ {
        return None;
    }
    // REG_SZ is stored as UTF-16LE (one NUL-terminated wchar per
    // char). Treat the bytes as UTF-16, not UTF-8 — reading them raw
    // yields interleaved NULs that break version parsing.
    let sz = (size as usize).min(buf.len());
    let units: Vec<u16> = buf[..sz]
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16_lossy(&units)
        .trim_end_matches('\0')
        .to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Spawned `--version` probe, hard-capped by `PROBE_SPAWN_TIMEOUT`.
/// On timeout the whole process tree is killed so no orphaned browser
/// processes survive.
pub(crate) fn probe_spawned_major() -> Option<u32> {
    let bin = crate::ghost::chrome_binary().ok()?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--version");
    // On Windows and macOS, `--version` without `--headless` opens a
    // real GUI window. Pass `--headless=new` + a scratch profile so any
    // spawn that does happen stays invisible. Harmless on Linux (the
    // flag is ignored alongside `--version`).
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let tmp = std::env::temp_dir().join("donsetch-chrome-probe");
        let _ = std::fs::create_dir_all(&tmp);
        cmd.arg("--headless=new");
        cmd.arg(format!("--user-data-dir={}", tmp.display()));
        cmd.arg("--no-first-run");
        cmd.arg("--no-default-browser-check");
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    spawn_probe_with_timeout(cmd)
}

/// Run the built `--version` command, read its stdout, and return the
/// first plausible major version. Never runs longer than
/// `PROBE_SPAWN_TIMEOUT`; on timeout, kills the whole process tree.
fn spawn_probe_with_timeout(mut cmd: std::process::Command) -> Option<u32> {
    use std::io::Read;

    let mut child = cmd.spawn().ok()?;

    // Read stdout on a side thread so we can still enforce the timeout
    // if the browser never exits (the read would otherwise block us).
    let pid = child.id();
    let pipe = match child.stdout.take() {
        Some(p) => p,
        None => {
            // Can't happen (stdout is always piped above), but never
            // leave a spawned child running on the early-out path.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let stdout = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut rd = pipe;
        let _ = rd.read_to_string(&mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + PROBE_SPAWN_TIMEOUT;
    let mut reaped: Option<std::process::ExitStatus> = None;
    while reaped.is_none() {
        match child.try_wait() {
            Ok(Some(status)) => reaped = Some(status),
            Ok(None) => {}
            Err(_) => break,
        }
        if reaped.is_none() && std::time::Instant::now() >= deadline {
            // Wedged browser — kill the whole tree, not just the parent.
            kill_probe_tree(Some(pid));
            let _ = child.kill();
            reaped = child.wait().ok();
            break;
        }
        if reaped.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    if reaped.is_none() {
        // Err branch above killed nothing; be safe.
        let _ = child.kill();
        let _ = child.wait();
    }
    let out = stdout.join().unwrap_or_default();

    parse_version_major(&out)
}

/// Kill the probe process and its children (Windows: taskkill /T so
/// the whole tree dies; Unix: kill the process group-less child — its
/// renderers exit when the browser dies).
fn kill_probe_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    {
        // taskkill /T /F kills the process and all descendants.
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Parse the first plausible major version out of a `<name> <major>` line.
pub(crate) fn parse_version_major(line: &str) -> Option<u32> {
    let line = line.trim();
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for t in tokens {
        if let Some(first) = t.split('.').next()
            && let Ok(major) = first.parse::<u32>()
            && (20..=400).contains(&major)
        {
            return Some(major);
        }
    }
    None
}

#[cfg(test)]
mod locale_tests {
    use super::accept_language_for;

    #[test]
    fn tld_drives_locale() {
        assert!(accept_language_for("69shuba.com", "/").starts_with("en-US"));
        assert!(accept_language_for("www.69shuba.ru", "/book/1").starts_with("ru-RU"));
        assert!(accept_language_for("example.co.jp", "/").starts_with("ja-JP"));
        assert!(accept_language_for("example.com.cn", "/").starts_with("zh-CN"));
    }

    #[test]
    fn path_script_hint() {
        // Percent-encoded Cyrillic in the path ⇒ ru even on .com.
        assert!(accept_language_for("example.com", "/tags/%D0%A4%D0%B0").starts_with("ru-RU"));
        // CJK lead byte ⇒ zh.
        assert!(accept_language_for("example.com", "/wiki/%E4%B8%AD").starts_with("zh-CN"));
        // Plain path stays default.
        assert_eq!(
            accept_language_for("example.com", "/docs"),
            "en-US,en;q=0.9"
        );
    }
}

#[cfg(test)]
mod probe_tests {
    use super::parse_version_major;

    #[test]
    fn parses_known_banner_shapes() {
        assert_eq!(
            parse_version_major("Chromium 151.0.7922.108 Arch Linux"),
            Some(151)
        );
        assert_eq!(
            parse_version_major("Google Chrome 150.0.7204.184"),
            Some(150)
        );
        assert_eq!(
            parse_version_major("Microsoft Edge 151.0.7922.72"),
            Some(151)
        );
        // Registry shape: bare version string.
        assert_eq!(parse_version_major("151.0.7922.72"), Some(151));
    }

    #[test]
    fn rejects_non_versions() {
        assert_eq!(parse_version_major(""), None);
        assert_eq!(parse_version_major("no numbers here"), None);
        // Plausibility band: 20..=400 — rejects years, build ids, ports.
        assert_eq!(parse_version_major("Chrome 1985.1"), None);
        assert_eq!(parse_version_major("Chrome 100000"), None);
    }
}
