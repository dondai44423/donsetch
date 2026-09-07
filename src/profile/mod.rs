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
        Self::chrome(150, platform, true)
    }

    /// Chrome `major` on the given platform. The TLS/H2 tables are
    /// the Chrome 150 capture (stable across adjacent versions);
    /// the UA and client hints carry the real version so the ghost
    /// browser and tier 1 advertise the SAME identity : clearance
    /// cookies are bound to it, and a ghost solving on Chromium 151
    /// while tier 1 claims 150 gets its replays rejected.
    ///
    /// `branded` = the host binary is Google-Chrome-branded (it has a
    /// third Sec-CH-UA brand). Distro Chromium sends only
    /// `"Chromium";v=N, "Not=A?Brand";v=99` (greased version 99,
    /// Chromium first) : Chrome 151 ground truth, captured live.
    pub fn chrome(major: u32, platform: Platform, branded: bool) -> Self {
        let sec_ch_ua = if branded {
            format!(
                "\"Chromium\";v=\"{major}\", \"Not=A?Brand\";v=\"99\", \"Google Chrome\";v=\"{major}\""
            )
        } else {
            format!("\"Chromium\";v=\"{major}\", \"Not=A?Brand\";v=\"99\"")
        };
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
                // NOTE (v4 parity finding, 2026-09-08): the floor's
                // real Chromium 151 also pads sig-algs with three
                // GREASE codes (0x0904/05/06) at the front. boring's
                // public set_sigalgs_list validates against known
                // algorithms and refuses them ([INVALID_SIGNATURE_
                // ALGORITHM], live-proven), and upstream boringssl
                // has NO sig-alg grease hook (its grease indexes:
                // cipher/group/ext1/ext2/version/ticket/ech). Raw
                // ClientHello extension injection would be needed:
                // documented as the L1 evergreen work item in
                // design/v4.md, doctor --stealth --parity surfaces
                // the exact delta meanwhile.
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
                "Mozilla/5.0 ({}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36",
                platform.ua_token()
            ),
            sec_ch_ua,
            platform,
        }
    }

    /// Default: host-coherent identity (a Windows agent looks like Windows Chrome).
    /// The major version AND the brand set (distro Chromium vs Google Chrome)
    /// are probed from the INSTALLED browser (ghost + tier 1 must claim the
    /// same version and brand list).
    pub fn host_default() -> Self {
        let (major, branded) = match probe_installed() {
            Some((m, b)) => (Some(m), b),
            None => (None, true),
        };
        match major {
            Some(major) => Self::chrome(major, Platform::host(), branded),
            None => Self::chrome_150(Platform::host()),
        }
    }

    /// Ordered header template for a document GET, Chrome order.
    /// (name, value-or-placeholder). Placeholders filled by caller.
    pub fn h1_headers(&self, host: &str, path: &str) -> Vec<(String, String)> {
        self.h1_headers_for_class(host, path, RequestClass::Navigation)
    }

    /// Class-aware header sets (v4 phase 1.2). A real browser does
    /// not send one header set for everything: navigations carry
    /// upgrade-insecure-requests + sec-fetch-user + the document
    /// Accept; subresources carry per-type Accept, no-cors mode,
    /// and their own sec-fetch-dest. JA4H fingerprints the header
    /// set and order per request; one set for every request class
    /// is itself a tell.
    pub fn h1_headers_for_class(
        &self,
        host: &str,
        path: &str,
        class: RequestClass,
    ) -> Vec<(String, String)> {
        if class != RequestClass::Navigation {
            return self.h1_subresource_headers(host, path, class);
        }
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
        ]
    }

    /// Subresource header set in Chrome's subresource order:
    /// host, connection, the sec-ch-ua cluster, user-agent, the
    /// per-type Accept, the sec-fetch trio (site/mode/dest), then
    /// accept-encoding and accept-language. No
    /// upgrade-insecure-requests and no sec-fetch-user: Chrome does
    /// not send those on subresources. Referer is added by the
    /// caller (the page URL); sec-fetch-site is recomputed against
    /// the page origin by the client like any other hop.
    fn h1_subresource_headers(
        &self,
        host: &str,
        path: &str,
        class: RequestClass,
    ) -> Vec<(String, String)> {
        let (accept, dest) = class.accept_and_dest();
        let _ = path;
        vec![
            ("host".into(), host.into()),
            ("connection".into(), "keep-alive".into()),
            ("sec-ch-ua".into(), self.sec_ch_ua.clone()),
            ("sec-ch-ua-mobile".into(), "?0".into()),
            (
                "sec-ch-ua-platform".into(),
                format!("\"{}\"", self.platform.ch_platform()),
            ),
            ("user-agent".into(), self.user_agent.clone()),
            ("accept".into(), accept),
            ("sec-fetch-site".into(), "same-origin".into()),
            ("sec-fetch-mode".into(), "no-cors".into()),
            ("sec-fetch-dest".into(), dest),
            ("accept-encoding".into(), "gzip, deflate, br, zstd".into()),
            (
                "accept-language".into(),
                accept_language_for(host, path).to_string(),
            ),
        ]
    }
}

/// v3 F5: Accept-Language coherent with the target's locale.
/// Many sites gate localized content on this header : an en-US
/// header on a .ru page is served the English stub (and is a mild
/// incoherence signal). Derived from the host TLD and non-Latin
/// script in the path; everything else stays Chrome-default en-US.
/// What kind of request this is (v4 phase 1.2): header sets and
/// JA4H profiles differ per class, so the wire must too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Top-level document load.
    Navigation,
    /// Stylesheet subresource.
    Style,
    /// Script subresource.
    Script,
    /// Font subresource.
    Font,
    /// Image subresource.
    Image,
    /// favicon.ico (image class, dest image).
    Favicon,
}

impl RequestClass {
    /// The per-class Accept value and sec-fetch-dest, matching what
    /// Chrome sends for the same subresource type.
    fn accept_and_dest(self) -> (String, String) {
        match self {
            RequestClass::Navigation => (
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"
                    .to_string(),
                "document".to_string(),
            ),
            RequestClass::Style => ("text/css,*/*;q=0.1".to_string(), "style".to_string()),
            RequestClass::Script => ("*/*".to_string(), "script".to_string()),
            RequestClass::Font => ("*/*".to_string(), "font".to_string()),
            RequestClass::Image | RequestClass::Favicon => (
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8".to_string(),
                "image".to_string(),
            ),
        }
    }
}

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
///    it costs zero browser launches : no window, no hang, no orphaned
///    processes. This is the tier-1-friendly path: tier 1 never needs
///    to spawn a real browser just to know what version to claim.
/// 2. **Spawned `--version` probe, hard timeboxed.** Only reached
///    when the registry has nothing (custom forks with no BLBeacon
///    entry). The child is killed : whole tree on Windows : if it
///    does not answer within `PROBE_SPAWN_TIMEOUT`, so a wedged
///    browser can never block tier 1 startup or leave processes behind.
///
/// The result is cached in a `OnceLock` so the probe runs at most
/// once per process.
static PROBED: std::sync::OnceLock<Option<(u32, bool)>> = std::sync::OnceLock::new();

/// Hard cap on the spawned `--version` probe. Chrome 129 on Windows
/// is known to hang (or crash-loop its network/GPU services) under
/// `--headless=new`; without this cap tier 1 blocks forever at boot.
const PROBE_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// (major, google-chrome-branded).
fn probe_installed() -> Option<(u32, bool)> {
    *PROBED.get_or_init(|| {
        if let Some((major, branded)) = probe_registry() {
            // The key family tells the brand truth directly.
            return Some((major, branded));
        }
        // Fallback: ask the binary, but never let it hang or orphan.
        let browser = crate::ghost::resolve_browser_without_download().ok()?;
        let path = browser.path.to_string_lossy().to_string();
        let banner = probe_version_string_at_path(&path)?;
        let branded = banner.contains("Google Chrome") || banner.contains("Google Chrome Canary");
        probe_version_at_path(&path).map(|m| (m, branded))
    })
}

/// Windows: read the major version + brand truth from the browser's
/// own `BLBeacon\version` registry value. The key family decides the
/// brand: `Software\Google\Chrome` = Google-branded; Chromium/Edge/
/// Thorium report their own (non-Google) brands. Honours `DONGHOST_CHROME`
/// by probing the browser family it names first.
#[cfg(windows)]
fn probe_registry() -> Option<(u32, bool)> {
    use windows_sys::Win32::System::Registry as reg;

    // (key path, google-branded)
    let mut keys: Vec<(&str, bool)> = vec![
        ("Software\\Google\\Chrome\\BLBeacon", true),
        ("Software\\Chromium\\BLBeacon", false),
        ("Software\\Microsoft\\Edge\\BLBeacon", false),
        ("Software\\Thorium\\BLBeacon", false),
    ];
    // Sort DONGHOST_CHROME's family to the front if it doesn't already
    // lead : cheap, and makes the explicit choice authoritative.
    if let Some(p) = std::env::var_os("DONGHOST_CHROME") {
        let p = p.to_string_lossy().to_lowercase();
        for (family, key, branded) in [
            ("thorium", "Software\\Thorium\\BLBeacon", false),
            ("chrome", "Software\\Google\\Chrome\\BLBeacon", true),
            ("chromium", "Software\\Chromium\\BLBeacon", false),
            ("edge", "Software\\Microsoft\\Edge\\BLBeacon", false),
        ] {
            if p.contains(family) {
                if let Some(pos) = keys.iter().position(|(k, _)| *k == key) {
                    keys.remove(pos);
                    keys.insert(0, (key, branded));
                }
                break;
            }
        }
    }

    for (key, branded) in keys {
        if let Some(v) = registry_string(reg::HKEY_CURRENT_USER, key, "version")
            && let Some(major) = parse_version_major(&v)
        {
            return Some((major, branded));
        }
    }
    None
}

/// Non-Windows: there is no registry to consult : fall through to
/// the spawned probe directly. (Stub keeps `probe_installed`
/// platform-symmetric.)
#[cfg(not(windows))]
fn probe_registry() -> Option<(u32, bool)> {
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
    // valid out-param. KEY_READ only : no write access requested.
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
    // char). Treat the bytes as UTF-16, not UTF-8 : reading them raw
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

/// Probe a specific Chromium-family executable without changing the selected
/// backend. Used by the browser resolver for Chromium and CloakBrowser alike.
pub(crate) fn probe_version_at_path(path: &str) -> Option<u32> {
    probe_version_string_at_path(path).and_then(|v| parse_version_major(&v))
}

/// Probe a specific executable and return its full dotted Chromium version.
pub(crate) fn probe_version_string_at_path(path: &str) -> Option<String> {
    probe_version_string_at_path_result(path).ok()
}

pub(crate) fn probe_version_string_at_path_result(path: &str) -> Result<String, String> {
    // One spawn per binary per process. Ghost launches, doctor,
    // status, and cloak resolution all walk through here, and each
    // call used to pay a chrome --version spawn (~100-200ms plus a
    // fork on some platforms). The daemon launches ghosts often
    // enough that caching is a real win; Ok results never change
    // for a given on-disk path, and Err results stay recalculated
    // so a transient startup hiccup is not sticky.
    static PROBE_CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    let cache = PROBE_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.get(path) {
            return entry
                .clone()
                .ok_or_else(|| format!("browser version probe failed for {path}"));
        }
    }
    let result = probe_version_string_at_path_uncached(path);
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if result.is_ok() {
        guard.insert(path.to_string(), result.clone().ok());
        // Small cache: this map holds one entry per distinct binary
        // path (system chromium, a playwright build, a cloak binary,
        // an edge install). Never grows unbounded in practice; the
        // paths are bounded by discoverable binaries on the host.
        if guard.len() > 16 {
            guard.clear();
        }
    }
    result
}

fn probe_version_string_at_path_uncached(path: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("--version");
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

/// Run the built `--version` command, read its stdout, and return it.
/// Never runs longer than `PROBE_SPAWN_TIMEOUT`; on timeout, kills the
/// whole process tree.
fn spawn_probe_with_timeout(mut cmd: std::process::Command) -> Result<String, String> {
    use std::io::Read;

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn browser version probe: {e}"))?;

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
            return Err("browser version probe had no stdout pipe".into());
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
            // Wedged browser : kill the whole tree, not just the parent.
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
    match reaped {
        Some(status) if status.success() => {
            // "Chromium 151.0.7922.108\n" -> "151.0.7922.108".
            // The banner word varies per build (Chromium, Chrome,
            // Edge, CloakBrowser); the dotted token is the truth.
            parse_version_string(&out)
                .ok_or_else(|| format!("browser version probe returned no version token: {out:?}"))
        }
        Some(status) => Err(format!("browser version probe exited with {status}")),
        None => Err("browser version probe did not exit".into()),
    }
}

/// Parse the first full dotted Chromium version from a version banner.
pub(crate) fn parse_version_string(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|token| {
        let parts: Vec<_> = token.split('.').collect();
        if parts.len() >= 4
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
            && parts[0]
                .parse::<u32>()
                .is_ok_and(|major| (20..=400).contains(&major))
        {
            Some(token.to_string())
        } else {
            None
        }
    })
}
/// Kill the probe process and its children (Windows: taskkill /T so
/// the whole tree dies; Unix: kill the process group-less child : its
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
    use super::{BrowserProfile, Platform, parse_version_major, parse_version_string};

    #[test]
    fn brand_lists_match_chrome_151_capture() {
        // Distro Chromium (this host's capture): two brands, greased v=99.
        let p = BrowserProfile::chrome(151, Platform::Linux, false);
        assert_eq!(
            p.sec_ch_ua,
            "\"Chromium\";v=\"151\", \"Not=A?Brand\";v=\"99\""
        );
        // Google-Chrome-branded: third brand appended, same greased version.
        let b = BrowserProfile::chrome(151, Platform::Linux, true);
        assert_eq!(
            b.sec_ch_ua,
            "\"Chromium\";v=\"151\", \"Not=A?Brand\";v=\"99\", \"Google Chrome\";v=\"151\""
        );
    }

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
    fn parses_full_version() {
        assert_eq!(
            parse_version_string("Chromium 146.0.7680.177.5 Arch Linux"),
            Some("146.0.7680.177.5".into())
        );
        assert_eq!(parse_version_string("not a version"), None);
    }

    #[test]
    fn rejects_non_versions() {
        assert_eq!(parse_version_major(""), None);
        assert_eq!(parse_version_major("no numbers here"), None);
        // Plausibility band: 20..=400 : rejects years, build ids, ports.
        assert_eq!(parse_version_major("Chrome 1985.1"), None);
        assert_eq!(parse_version_major("Chrome 100000"), None);
    }
}

pub mod scorecard;

#[cfg(test)]
mod class_tests {
    use super::*;

    fn profile() -> BrowserProfile {
        BrowserProfile::chrome(151, Platform::Linux, true)
    }

    #[test]
    fn navigation_class_is_the_exact_historical_set() {
        // Byte-compat guard: phase 1.2 must not perturb the
        // navigation emission at all.
        let p = profile();
        let h = p.h1_headers_for_class("example.com", "/", RequestClass::Navigation);
        let names: Vec<&str> = h.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "host",
                "connection",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "upgrade-insecure-requests",
                "user-agent",
                "accept",
                "sec-fetch-site",
                "sec-fetch-mode",
                "sec-fetch-user",
                "sec-fetch-dest",
                "accept-encoding",
                "accept-language"
            ]
        );
        let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("sec-fetch-dest"), Some("document"));
        assert_eq!(get("sec-fetch-mode"), Some("navigate"));
        assert_eq!(get("sec-fetch-user"), Some("?1"));
        assert_eq!(get("upgrade-insecure-requests"), Some("1"));
    }

    #[test]
    fn subresource_classes_carry_their_own_sets() {
        let p = profile();
        for (class, dest, accept_prefix) in [
            (RequestClass::Style, "style", "text/css"),
            (RequestClass::Script, "script", "*/*"),
            (RequestClass::Font, "font", "*/*"),
            (RequestClass::Image, "image", "image/avif"),
            (RequestClass::Favicon, "image", "image/avif"),
        ] {
            let h = p.h1_headers_for_class("example.com", "/a", class);
            let names: Vec<&str> = h.iter().map(|(n, _)| n.as_str()).collect();
            let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
            assert_eq!(get("sec-fetch-dest"), Some(dest), "{class:?}");
            assert_eq!(get("sec-fetch-mode"), Some("no-cors"), "{class:?}");
            assert!(
                get("accept").unwrap().starts_with(accept_prefix),
                "{class:?} accept {:?}",
                get("accept")
            );
            // Subresources never carry navigation-only headers.
            assert!(!names.contains(&"upgrade-insecure-requests"), "{class:?}");
            assert!(!names.contains(&"sec-fetch-user"), "{class:?}");
            // The identity layers stay identical to navigation.
            assert_eq!(get("user-agent"), Some(p.user_agent.as_str()), "{class:?}");
            assert_eq!(get("sec-ch-ua"), Some(p.sec_ch_ua.as_str()), "{class:?}");
        }
    }
}

#[cfg(test)]
mod ua_tests {
    #[test]
    fn user_agent_carries_real_mozilla_token() {
        for platform in [
            super::Platform::Linux,
            super::Platform::Windows,
            super::Platform::MacOs,
        ] {
            let p = super::BrowserProfile::chrome(151, platform, true);
            assert!(
                p.user_agent.starts_with("Mozilla/5.0 ("),
                "UA must carry the real Mozilla/5.0 token: {}",
                p.user_agent
            );
        }
    }
}
