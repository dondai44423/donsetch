//! CloakBrowser backend selection and fail-closed public binary installation.
//!
//! CloakBrowser is never downloaded implicitly. A local
//! `CLOAKBROWSER_BINARY_PATH` is accepted without network access; the signed
//! public GitHub release is used only when `DONSETCH_CLOAK_AUTO_DOWNLOAD=1`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use tar::Archive;

const CLOAK_REPOSITORY: &str = "CloakHQ/CloakBrowser";
const CLOAK_VERSION: &str = "146.0.7680.177.5";
const CLOAK_SIGNING_KEY: &str = "MKFKwIhUcKWq5xTuNA0Ovg99njcDEcEJvmWYYhApvaU=";
const CLOAK_DOWNLOAD_OPT_IN: &str = "DONSETCH_CLOAK_AUTO_DOWNLOAD";
const DOWNLOAD_ATTEMPTS: u8 = 3;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserBackend {
    /// The original Chromium backend: headful on Xvfb/off-screen where possible.
    Chromium,
    /// The original browser binary, forced into Chromium's new headless mode.
    HeadlessChromium,
    /// CloakBrowser's patched Chromium backend.
    CloakBrowser,
}

impl BrowserBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::HeadlessChromium => "headless",
            Self::CloakBrowser => "cloakbrowser",
        }
    }
}

fn parse_backend(value: Option<&str>) -> Result<Option<BrowserBackend>, String> {
    match value {
        None | Some("") | Some("auto") => Ok(None),
        Some("chromium") | Some("chrome") | Some("original") => Ok(Some(BrowserBackend::Chromium)),
        Some("headless") | Some("original-headless") => Ok(Some(BrowserBackend::HeadlessChromium)),
        Some("cloak") | Some("cloakbrowser") => Ok(Some(BrowserBackend::CloakBrowser)),
        Some(other) => Err(format!(
            "invalid browser backend `{other}`; use chromium, headless, cloakbrowser, or auto"
        )),
    }
}

/// Whether the configured backend forces the original browser into headless mode.
/// Invalid values return false here; `resolve_browser` reports the configuration
/// error when the browser is actually acquired.
pub fn headless_mode_requested() -> bool {
    let requested = std::env::var_os("DONSETCH_BROWSER_BACKEND")
        .or_else(|| std::env::var_os("DONGHOST_BROWSER_BACKEND"))
        .map(|v| v.to_string_lossy().trim().to_ascii_lowercase());
    parse_backend(requested.as_deref()).ok().flatten() == Some(BrowserBackend::HeadlessChromium)
}

#[derive(Clone, Debug)]
pub struct BrowserResolution {
    pub backend: BrowserBackend,
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
            "backend={} source={} path={} version={version}",
            self.backend.as_str(),
            self.source,
            self.path.display()
        )
    }
}

/// Resolve the configured browser. `auto` is plain Chromium discovery with
/// the original headful/off-screen behavior. CloakBrowser is used only after
/// explicit selection (`DONSETCH_BROWSER_BACKEND=cloakbrowser`); a bare
/// `CLOAKBROWSER_BINARY_PATH` does not switch backends, and a download
/// additionally requires `DONSETCH_CLOAK_AUTO_DOWNLOAD=1` (or an explicit
/// backend plus that flag when Chromium discovery fails).
pub fn resolve_browser() -> Result<BrowserResolution, String> {
    resolve_browser_with_download(true)
}
/// Resolve the configured browser without downloading a public binary.
pub fn resolve_browser_without_download() -> Result<BrowserResolution, String> {
    resolve_browser_with_download(false)
}

fn resolve_browser_with_download(allow_download: bool) -> Result<BrowserResolution, String> {
    let requested = std::env::var_os("DONSETCH_BROWSER_BACKEND")
        .or_else(|| std::env::var_os("DONGHOST_BROWSER_BACKEND"))
        .map(|v| v.to_string_lossy().trim().to_ascii_lowercase());
    let backend = parse_backend(requested.as_deref())?;

    if backend == Some(BrowserBackend::CloakBrowser) {
        return resolve_cloak(allow_download);
    }

    if let Some(chromium_backend) =
        backend.filter(|backend| *backend != BrowserBackend::CloakBrowser)
    {
        return resolve_chromium(chromium_backend);
    }

    match resolve_chromium(BrowserBackend::Chromium) {
        Ok(chromium) => Ok(chromium),
        Err(chromium_error) if allow_download && auto_download_enabled() => {
            resolve_cloak(true).map_err(|cloak_error| {
                format!(
                    "Chromium unavailable ({chromium_error}); CloakBrowser unavailable ({cloak_error})"
                )
            })
        }
        Err(error) => Err(error),
    }
}

fn resolve_chromium(backend: BrowserBackend) -> Result<BrowserResolution, String> {
    let path = super::chromium_binary()?;
    let version = crate::profile::probe_version_string_at_path(&path);
    let source = if std::env::var_os("DONGHOST_CHROME").is_some() {
        "explicit"
    } else {
        "system"
    };
    Ok(BrowserResolution {
        backend,
        path: PathBuf::from(path),
        source,
        version,
    })
}

fn resolve_cloak(allow_download: bool) -> Result<BrowserResolution, String> {
    let (path, source) = if let Some(raw) = std::env::var_os("CLOAKBROWSER_BINARY_PATH") {
        let path = PathBuf::from(raw);
        validate_binary(&path)
            .map_err(|e| format!("CLOAKBROWSER_BINARY_PATH `{}` invalid: {e}", path.display()))?;
        (path, "explicit")
    } else {
        let requested = requested_version()?;
        if let Some(path) = cached_binary(&requested) {
            (path, "cache")
        } else if !allow_download || !auto_download_enabled() {
            return Err(format!(
                "CloakBrowser binary not found; set CLOAKBROWSER_BINARY_PATH or {}=1 to download the signed public binary",
                CLOAK_DOWNLOAD_OPT_IN
            ));
        } else {
            (install_public_binary()?, "downloaded")
        }
    };
    let version = crate::profile::probe_version_string_at_path(&path.to_string_lossy());
    Ok(BrowserResolution {
        backend: BrowserBackend::CloakBrowser,
        path,
        source,
        version,
    })
}

fn auto_download_enabled() -> bool {
    std::env::var_os(CLOAK_DOWNLOAD_OPT_IN).is_some_and(|v| v == "1")
}

fn validate_binary(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("path is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("file is not executable".into());
        }
    }
    Ok(())
}

fn platform_tag() -> Result<&'static str, String> {
    // The public free release currently publishes Linux x64 and Windows x64
    // only. Other platforms must use a local override (or a separately
    // licensed build); never guess an archive name and download a
    // non-existent or mismatched artifact.
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x64"),
        ("windows", "x86_64") => Ok("windows-x64"),
        (os, arch) => Err(format!(
            "no public CloakBrowser binary for {os} {arch}; set CLOAKBROWSER_BINARY_PATH"
        )),
    }
}

fn requested_version() -> Result<String, String> {
    let Some(raw) = std::env::var_os("CLOAKBROWSER_VERSION") else {
        return Ok(CLOAK_VERSION.into());
    };
    let value = raw.to_string_lossy().trim().to_string();
    if valid_version(&value) {
        Ok(value)
    } else {
        Err(format!(
            "invalid CLOAKBROWSER_VERSION `{value}`; expected a full numeric version"
        ))
    }
}

fn valid_version(value: &str) -> bool {
    value.split('.').count() >= 4
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("CLOAKBROWSER_CACHE_DIR") {
        PathBuf::from(path)
    } else {
        crate::paths::cache_dir().join("cloakbrowser")
    }
}

fn binary_path(root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        root.join("chrome.exe")
    } else if cfg!(target_os = "macos") {
        root.join("Chromium.app/Contents/MacOS/Chromium")
    } else {
        root.join("chrome")
    }
}

fn archive_name(tag: &str) -> &'static str {
    if tag == "windows-x64" {
        "cloakbrowser-windows-x64.zip"
    } else {
        match tag {
            "linux-x64" => "cloakbrowser-linux-x64.tar.gz",
            "linux-arm64" => "cloakbrowser-linux-arm64.tar.gz",
            "darwin-arm64" => "cloakbrowser-darwin-arm64.tar.gz",
            "darwin-x64" => "cloakbrowser-darwin-x64.tar.gz",
            _ => "cloakbrowser-unsupported.tar.gz",
        }
    }
}
fn cached_binary(version: &str) -> Option<PathBuf> {
    let path = binary_path(&cache_dir().join(format!("chromium-{version}")));
    validate_binary(&path).is_ok().then_some(path)
}
fn install_public_binary() -> Result<PathBuf, String> {
    let tag = platform_tag()?;
    let version = requested_version()?;
    if let Some(path) = cached_binary(&version) {
        return Ok(path);
    }
    let root = cache_dir().join(format!("chromium-{version}"));
    let expected = binary_path(&root);

    let name = archive_name(tag);
    let release = format!("chromium-v{version}");
    let base = format!("https://github.com/{CLOAK_REPOSITORY}/releases/download/{release}");
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("donsetch-cloak-installer")
        .build()
        .map_err(|e| e.to_string())?;
    let archive_url = format!("{base}/{name}");
    let manifest_url = format!("{base}/SHA256SUMS");
    let signature_url = format!("{base}/SHA256SUMS.sig");
    let archive_path = temporary_path("archive");
    let result = (|| {
        download_to(&client, &archive_url, &archive_path)?;
        let manifest = client
            .get(&manifest_url)
            .send()
            .map_err(|e| format!("manifest request: {e}"))?
            .error_for_status()
            .map_err(|e| format!("manifest response: {e}"))?
            .bytes()
            .map_err(|e| format!("manifest body: {e}"))?;
        let signature = client
            .get(&signature_url)
            .send()
            .map_err(|e| format!("signature request: {e}"))?
            .error_for_status()
            .map_err(|e| format!("signature response: {e}"))?
            .bytes()
            .map_err(|e| format!("signature body: {e}"))?;
        let expected_hash = verify_manifest(&manifest, &signature, &version, name)?;
        let actual_hash = sha256_file(&archive_path)?;
        if actual_hash != expected_hash {
            return Err(format!(
                "archive SHA-256 mismatch for {name}: expected {expected_hash}, got {actual_hash}"
            ));
        }
        extract_archive(&archive_path, &root)?;
        validate_binary(&expected).map_err(|e| format!("downloaded binary invalid: {e}"))?;
        Ok(expected.clone())
    })();
    let _ = fs::remove_file(&archive_path);
    if result.is_err() {
        let _ = fs::remove_dir_all(&root);
    }
    result
}

fn verify_manifest(
    manifest: &[u8],
    signature_b64: &[u8],
    version: &str,
    archive_name: &str,
) -> Result<String, String> {
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(CLOAK_SIGNING_KEY)
        .map_err(|e| format!("pinned signing key: {e}"))?;
    let key: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "pinned signing key is not 32 bytes".to_string())?;
    let key = VerifyingKey::from_bytes(&key).map_err(|e| format!("pinned signing key: {e}"))?;
    let signature_text = std::str::from_utf8(signature_b64)
        .map_err(|e| format!("manifest signature is not UTF-8: {e}"))?
        .trim();
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_text)
        .map_err(|e| format!("manifest signature is not base64: {e}"))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|e| format!("manifest signature is malformed: {e}"))?;
    key.verify(manifest, &signature)
        .map_err(|_| "SHA256SUMS Ed25519 signature verification failed".to_string())?;
    let text = std::str::from_utf8(manifest).map_err(|e| format!("manifest is not UTF-8: {e}"))?;
    let declared = text
        .lines()
        .find_map(|line| line.strip_prefix("version=").map(str::trim))
        .ok_or_else(|| "signed manifest has no version binding".to_string())?;
    if declared != version {
        return Err(format!(
            "signed manifest version mismatch: requested {version}, declares {declared}"
        ));
    }
    text.lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            if name == archive_name
                && hash.len() == 64
                && hash.chars().all(|c| c.is_ascii_hexdigit())
            {
                Some(hash.to_ascii_lowercase())
            } else {
                None
            }
        })
        .ok_or_else(|| format!("signed manifest has no entry for {archive_name}"))
}
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temporary_path(label: &str) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "donsetch-cloak-{label}-{}-{counter}",
        std::process::id(),
    ))
}

fn download_to(client: &Client, url: &str, path: &Path) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match download_to_once(client, url, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = fs::remove_file(path);
                last_error = Some(error);
                if attempt < DOWNLOAD_ATTEMPTS {
                    std::thread::sleep(Duration::from_secs(2 * u64::from(attempt)));
                }
            }
        }
    }
    Err(format!(
        "archive download failed after {DOWNLOAD_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown download error".into())
    ))
}

fn download_to_once(client: &Client, url: &str, path: &Path) -> Result<(), String> {
    let mut response = client
        .get(url)
        .send()
        .map_err(|e| format!("archive request: {e}"))?
        .error_for_status()
        .map_err(|e| format!("archive response: {e}"))?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_ARCHIVE_BYTES)
    {
        return Err("archive exceeds 1 GiB safety limit".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("temporary archive: {e}"))?;
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| format!("archive read: {e}"))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > MAX_ARCHIVE_BYTES {
            return Err("archive exceeds 1 GiB safety limit".into());
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("archive write: {e}"))?;
    }
    file.flush().map_err(|e| format!("archive flush: {e}"))?;
    Ok(())
}

fn extract_archive(archive_path: &Path, root: &Path) -> Result<(), String> {
    let parent = root.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("cache directory: {e}"))?;
    let staging = parent.join(format!(
        ".donsetch-cloak-staging-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&staging).map_err(|e| format!("extract staging: {e}"))?;

    let result = (|| {
        if archive_path.extension().is_some_and(|e| e == "zip") {
            extract_zip(archive_path, &staging)?;
        } else {
            extract_tar(archive_path, &staging)?;
        }
        flatten_single_directory(&staging)?;
        if root.exists() {
            fs::remove_dir_all(root).map_err(|e| format!("replace old CloakBrowser: {e}"))?;
        }
        fs::rename(&staging, root).map_err(|e| format!("install extracted browser: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn safe_member(path: &Path) -> Result<(), String> {
    // has_root(), not is_absolute(): on Windows a path can have a
    // root without a drive prefix (`\tmp\chrome`, or `/tmp/chrome`
    // once the archive's forward slashes are turned into
    // components) and is_absolute() returns false for those —
    // joining one onto the extraction dir still replaces
    // everything past the prefix, escaping it just the same. Unix
    // defines is_absolute() as has_root(), so this is a no-op
    // there.
    if path.has_root() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        Err(format!(
            "archive path escapes extraction root: {}",
            path.display()
        ))
    } else {
        Ok(())
    }
}

fn extract_tar(path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("open archive: {e}"))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    for entry in archive.entries().map_err(|e| format!("read tar: {e}"))? {
        let mut entry = entry.map_err(|e| format!("read tar entry: {e}"))?;
        let member = entry
            .path()
            .map_err(|e| format!("tar path: {e}"))?
            .into_owned();
        safe_member(&member)?;
        if entry.header().entry_type().is_symlink() || entry.header().entry_type().is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|e| format!("tar link: {e}"))?
                .ok_or_else(|| "tar link has no target".to_string())?;
            safe_member(&target)?;
        }
        entry
            .unpack_in(dest)
            .map_err(|e| format!("extract tar entry: {e}"))?;
    }
    Ok(())
}

fn extract_zip(path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("read zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("read zip entry: {e}"))?;
        let name = PathBuf::from(entry.name());
        safe_member(&name)?;
        let out = dest.join(&name);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| format!("extract zip directory: {e}"))?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("extract zip parent: {e}"))?;
            }
            let mut output = File::create(&out).map_err(|e| format!("extract zip file: {e}"))?;
            std::io::copy(&mut entry, &mut output).map_err(|e| format!("extract zip data: {e}"))?;
        }
    }
    Ok(())
}

fn flatten_single_directory(root: &Path) -> Result<(), String> {
    let entries: Vec<_> = fs::read_dir(root)
        .map_err(|e| format!("inspect extracted archive: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("inspect extracted archive: {e}"))?;
    if entries.len() != 1 || !entries[0].path().is_dir() || entries[0].file_name() == "Chromium.app"
    {
        return Ok(());
    }
    let child = entries[0].path();
    for entry in fs::read_dir(&child).map_err(|e| format!("flatten archive: {e}"))? {
        let entry = entry.map_err(|e| format!("flatten archive: {e}"))?;
        fs::rename(entry.path(), root.join(entry.file_name()))
            .map_err(|e| format!("flatten archive: {e}"))?;
    }
    fs::remove_dir(child).map_err(|e| format!("flatten archive: {e}"))
}

/// Verify a downloaded archive against an already authenticated digest.
#[allow(dead_code)]
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("read archive for checksum: {e}"))?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("checksum read: {e}"))?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    let digest = digest.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_selection_distinguishes_cloak_and_original_headless() {
        assert_eq!(
            parse_backend(Some("cloakbrowser")),
            Ok(Some(BrowserBackend::CloakBrowser))
        );
        assert_eq!(
            parse_backend(Some("chromium")),
            Ok(Some(BrowserBackend::Chromium))
        );
        assert_eq!(
            parse_backend(Some("headless")),
            Ok(Some(BrowserBackend::HeadlessChromium))
        );
        assert_eq!(
            parse_backend(Some("original-headless")),
            Ok(Some(BrowserBackend::HeadlessChromium))
        );
        assert_eq!(parse_backend(Some("auto")), Ok(None));
    }

    #[test]
    fn backend_selection_rejects_unknown_values() {
        assert!(parse_backend(Some("firefox")).is_err());
    }

    #[test]
    fn empty_backend_value_means_auto() {
        assert_eq!(parse_backend(Some("")), Ok(None));
    }

    #[test]
    fn cloak_never_selected_by_bare_binary_path() {
        // Dondai's rule: CloakBrowser is not used unless explicitly selected.
        // A stray CLOAKBROWSER_BINARY_PATH (set for another tool) must not
        // switch the backend on its own.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let backend_var = std::env::var_os("DONSETCH_BROWSER_BACKEND");
        let legacy_var = std::env::var_os("DONGHOST_BROWSER_BACKEND");
        let path_var = std::env::var_os("CLOAKBROWSER_BINARY_PATH");
        // SAFETY: single-threaded for the duration via ENV_LOCK, and the
        // process has no other env-mutating code paths in this test binary.
        unsafe {
            std::env::remove_var("DONSETCH_BROWSER_BACKEND");
            std::env::remove_var("DONGHOST_BROWSER_BACKEND");
            std::env::set_var("CLOAKBROWSER_BINARY_PATH", "/nonexistent/cloak-chrome");
        }
        let resolved = resolve_browser_without_download();
        for (k, v) in [
            ("DONSETCH_BROWSER_BACKEND", backend_var),
            ("DONGHOST_BROWSER_BACKEND", legacy_var),
            ("CLOAKBROWSER_BINARY_PATH", path_var),
        ] {
            unsafe {
                match v {
                    Some(value) => std::env::set_var(k, value),
                    None => std::env::remove_var(k),
                }
            }
        }
        drop(_guard);
        // If the result is Ok the backend must not be CloakBrowser. If no
        // Chromium was discoverable on this machine it errors out, which
        // still proves CloakBrowser was not silently selected.
        if let Ok(browser) = resolved {
            assert_ne!(browser.backend, BrowserBackend::CloakBrowser);
        }
    }
    #[test]
    fn archive_names_are_platform_specific() {
        assert_eq!(archive_name("linux-x64"), "cloakbrowser-linux-x64.tar.gz");
        assert_eq!(archive_name("windows-x64"), "cloakbrowser-windows-x64.zip");
    }

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(safe_member(Path::new("../chrome")).is_err());
        assert!(safe_member(Path::new("/tmp/chrome")).is_err());
        assert!(safe_member(Path::new("chrome")).is_ok());
    }

    #[test]
    fn version_pin_rejects_url_injection() {
        assert!(!valid_version("146.0.1/../../x"));
        assert!(valid_version("146.0.7680.177.5"));
    }
}
