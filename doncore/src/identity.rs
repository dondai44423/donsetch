//! The identity record: ONE owner for every layer the network can see.
//!
//! v0.1: derived from the wire preset (UA/version strings) + local system
//! facts (timezone, cores, memory). Phase 1 adds the Chromium oracle that
//! measures the real browser on this machine and aligns every field.

use std::process::Command;

#[derive(Clone, Debug)]
pub struct Identity {
    pub ua: String,
    pub chrome_major: String,
    pub platform: String,
    pub platform_version: String,
    pub locale: String,
    pub languages: Vec<String>,
    pub tz: String,
    pub cores: u32,
    pub memory_gb: u32,
    pub screen_w: u32,
    pub screen_h: u32,
    pub avail_w: u32,
    pub avail_h: u32,
    pub outer_h: u32,
    pub dpr: f64,
    pub gpu_vendor: String,
    pub gpu_renderer: String,
    pub battery_level: Option<f64>,
    pub battery_charging: Option<bool>,
}

impl Identity {
    pub fn detect(ua: &str, sec_ch_ua_platform_version: &str) -> Self {
        let chrome_major = ua
            .split("Chrome/")
            .nth(1)
            .and_then(|r| r.split('.').next())
            .unwrap_or("154")
            .to_string();
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(8);
        let memory_gb = read_mem_gb();
        let tz = read_tz();
        let locale = read_locale();
        let screen_w = 1920;
        let screen_h = 1080;
        let (battery_level, battery_charging) = match read_battery() {
            Some((l, c)) => (Some(l), Some(c)),
            None => (None, None),
        };
        Self {
            ua: ua.to_string(),
            chrome_major,
            platform: platform_name().to_string(),
            platform_version: sec_ch_ua_platform_version.to_string(),
            locale,
            languages: vec!["en-US".into(), "en".into()],
            tz,
            cores,
            memory_gb,
            screen_w,
            screen_h,
            avail_w: screen_w,
            avail_h: screen_h - 60,
            outer_h: screen_h + 87,
            dpr: 1.0,
            // Measured machine truth on THIS box (the differential-oracle
            // value). Per-OS derivation is the oracle's job — macOS/Windows
            // must measure their host, never inherit these strings
            // (DECIDED cross-platform, 2026-10-18).
            gpu_vendor: "Google Inc. (Intel)".into(),
            gpu_renderer: "ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)".into(),
            battery_level,
            battery_charging,
        }
    }

    pub fn json(&self) -> String {
        serde_json::json!({
            "ua": self.ua,
            "chromeMajor": self.chrome_major,
            "platform": self.platform,
            "platformVersion": self.platform_version,
            "locale": self.locale,
            "languages": self.languages,
            "tz": self.tz,
            "cores": self.cores,
            "memoryGb": self.memory_gb,
            "screenW": self.screen_w,
            "screenH": self.screen_h,
            "availW": self.avail_w,
            "availH": self.avail_h,
            "outerH": self.outer_h,
            "dpr": self.dpr,
            "gpuVendor": self.gpu_vendor,
            "gpuRenderer": self.gpu_renderer,
            "batteryLevel": self.battery_level,
            "batteryCharging": self.battery_charging,
        })
        .to_string()
    }
}

// ── host facts, one contract per OS ───────────────────────────────────────
// Linux reads /proc + /sys; macOS shells the stable CLIs (sysctl/pmset) and
// uses /etc/localtime (a zoneinfo symlink there too); Windows calls the OS
// APIs (windows-sys) and PowerShell for the timezone/locale tags. Per-OS
// branches are compile-checked by construction; runtime qualification on
// macOS/Windows is OWED (no host) — DECIDED 2026-10-18.

/// The platform name exactly as sec-ch-ua-platform / the JS layer reports it.
pub fn platform_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "Linux"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
}

// ICU (what Chrome reports via Intl) keeps some legacy tz spellings that
// differ from the tzdata names. The persona must match the installed browser
// byte-for-byte, so alias on read.
fn icu_alias(z: String) -> String {
    match z.as_str() {
        "Asia/Kathmandu" => "Asia/Katmandu".to_string(),
        "Asia/Kolkata" => "Asia/Calcutta".to_string(),
        "Asia/Yangon" => "Asia/Rangoon".to_string(),
        "Asia/Ho_Chi_Minh" => "Asia/Saigon".to_string(),
        _ => z,
    }
}

fn read_mem_gb() -> u32 {
    read_mem_bytes()
        .map(|b| {
            // Chrome quantizes deviceMemory to 2^round(log2(GiB)), clamped
            // to [1, 8] — mirror the installed browser's answer.
            let gb = (b as f64) / 1024.0 / 1024.0 / 1024.0;
            let q = 2f64.powf(gb.log2().round());
            q.clamp(1.0, 8.0) as u32
        })
        .unwrap_or(8)
}

#[cfg(target_os = "linux")]
fn read_mem_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        })
}

#[cfg(target_os = "macos")]
fn read_mem_bytes() -> Option<u64> {
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
}

#[cfg(target_os = "windows")]
fn read_mem_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut st: MEMORYSTATUSEX = std::mem::zeroed();
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut st) != 0 {
            Some(st.ullTotalPhys)
        } else {
            None
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_tz() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            s.split("zoneinfo/").nth(1).map(|z| z.to_string())
        })
        .or_else(|| {
            Command::new("date")
                .arg("+%Z")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .map(icu_alias)
        .unwrap_or_else(|| "UTC".to_string())
}

#[cfg(target_os = "windows")]
fn read_tz() -> String {
    // Windows has no zoneinfo: TZ env (dev shells) → .NET's Windows-ID→IANA
    // conversion (PowerShell 7) → raw Windows ID (full mapping owed with a
    // host). ICU aliasing applied last, same as the other platforms.
    if let Ok(tz) = std::env::var("TZ") {
        if tz.contains('/') {
            return icu_alias(tz);
        }
    }
    let ps = |script: &str| -> Option<String> {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(z) = ps("$i='';if([TimeZoneInfo]::TryConvertWindowsIdToIanaId([TimeZoneInfo]::Local.Id,[ref]$i)){$i}") {
        if z.contains('/') {
            return icu_alias(z);
        }
    }
    ps("(Get-TimeZone).Id")
        .map(icu_alias)
        .unwrap_or_else(|| "UTC".to_string())
}

#[cfg(target_os = "linux")]
fn read_battery() -> Option<(f64, bool)> {
    // Chrome's Battery Status API reads the real system battery; the persona
    // must not invent numbers the machine contradicts. First BAT* wins; None
    // on desktops (the prelude keeps Chrome's desktop fallback: level 1,
    // charging true).
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()? {
        let p = entry.ok()?.path();
        let name = p.file_name()?.to_string_lossy().to_string();
        if !name.starts_with("BAT") {
            continue;
        }
        let cap = std::fs::read_to_string(p.join("capacity"))
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()?;
        let status = std::fs::read_to_string(p.join("status")).ok()?;
        return Some((cap / 100.0, status.trim() != "Discharging"));
    }
    None
}

#[cfg(target_os = "macos")]
fn read_battery() -> Option<(f64, bool)> {
    // pmset -g batt: "-InternalBattery-0 (id=…)\t87%; discharging; …".
    // AC-only Macs report no battery line → None (desktop fallback).
    let out = Command::new("pmset").args(["-g", "batt"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().find(|l| l.contains('%'))?;
    let (head, rest) = line.split_once('%')?;
    let pct: f64 = head
        .rsplit(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()?
        .parse()
        .ok()?;
    let status = rest.split(';').next().unwrap_or("").trim();
    let charging = status == "charging" || status == "charged" || status == "AC attached";
    Some((pct / 100.0, charging))
}

#[cfg(target_os = "windows")]
fn read_battery() -> Option<(f64, bool)> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    unsafe {
        let mut st: SYSTEM_POWER_STATUS = std::mem::zeroed();
        if GetSystemPowerStatus(&mut st) == 0 {
            return None;
        }
        if st.BatteryFlag == 128 || st.BatteryLifePercent == 255 {
            return None; // no system battery (desktop) / unknown
        }
        let charging = st.ACLineStatus == 1 || (st.BatteryFlag & 8) != 0;
        Some((st.BatteryLifePercent as f64 / 100.0, charging))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|l| {
            let l = l.split('.').next().unwrap_or("en_US").replace('_', "-");
            Some(l)
        })
        .unwrap_or_else(|| "en-US".to_string())
}

#[cfg(target_os = "windows")]
fn read_locale() -> String {
    // No LANG outside dev shells: the OS user locale directly (BCP-47 tag).
    if let Ok(l) = std::env::var("LANG") {
        if !l.is_empty() {
            return l.split('.').next().unwrap_or("en_US").replace('_', "-");
        }
    }
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", "(Get-Culture).Name"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "en-US".to_string())
}
