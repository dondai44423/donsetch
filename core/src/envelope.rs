//! The engine's envelope: every bound the site's own program runs inside.
//!
//! The engine executes code the site chose, in our process, with an op that can
//! reach the network as our identity. Nothing a page does may hang the fetcher,
//! exhaust its memory, or turn it into a proxy into the user's network. Each bound
//! here is enforced on the Rust side of the op boundary, never in the host layer's
//! JavaScript, because a page can reach the ops directly.
//!
//! * wall clock — one budget for the whole mint; V8 is terminated at the deadline
//!   and every blocking op is clamped to what is left of it;
//! * memory — a V8 heap limit that terminates instead of aborting, and an
//!   ArrayBuffer allocator that refuses past a live-bytes cap;
//! * network — http(s) only, browser-unsafe ports refused, non-public addresses
//!   refused (at the URL, at DNS resolution and at every redirect hop) unless the
//!   host is the page's own, bounded op count, request size, response size, total
//!   bytes and redirect chain;
//! * one engine at a time per process.

use std::ffi::c_void;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Every number the envelope enforces. The defaults are what the measured
/// challenges need with generous headroom; tests shrink them.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Total wall clock for one mint, from the moment the engine slot is held.
    pub wall: Duration,
    /// V8 heap ceiling in bytes.
    pub heap_max: usize,
    /// Live ArrayBuffer backing-store bytes allowed at once.
    pub buffer_max: usize,
    /// HTTP ops one mint may issue.
    pub ops_max: u32,
    /// Largest request body a page may send.
    pub request_body_max: usize,
    /// Largest single response the page may read.
    pub response_body_max: usize,
    /// Total response bytes across the mint.
    pub total_response_max: usize,
    /// Redirect hops one op may follow.
    pub redirects_max: usize,
    /// One op's own timeout (also clamped to the wall clock left).
    pub op_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // A goku mint is 1.4–1.6 s and the legacy shapes are about the same;
            // the cap is a ceiling for hostile or broken programs, not a target.
            wall: Duration::from_millis(10_000),
            heap_max: 256 * 1024 * 1024,
            buffer_max: 128 * 1024 * 1024,
            ops_max: 96,
            request_body_max: 1024 * 1024,
            response_body_max: 8 * 1024 * 1024,
            total_response_max: 32 * 1024 * 1024,
            redirects_max: 5,
            op_timeout: Duration::from_secs(9),
        }
    }
}

impl Limits {
    /// Defaults, with the wall clock overridable by `V5_ENGINE_WALL_MS`.
    pub fn from_env() -> Self {
        let mut limits = Self::default();
        if let Some(ms) = std::env::var("V5_ENGINE_WALL_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|ms| *ms >= 500)
        {
            limits.wall = Duration::from_millis(ms);
        }
        limits
    }
}

// ----- V8 flags ---------------------------------------------------------------

/// Largest WebAssembly memory a page may create, in 64 KiB pages (128 MB). Wasm
/// memory is mapped by V8 itself, not by the ArrayBuffer allocator, so the buffer
/// cap does not see it.
const WASM_MAX_PAGES: u32 = 2048;

/// Set the process-wide V8 flags the envelope relies on. V8 reads flags once, so
/// this runs before the first isolate and the result is remembered: an engine
/// whose caps are not in force does not run.
pub fn init_v8() -> Result<(), String> {
    static FLAGS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    FLAGS
        .get_or_init(|| {
            let flag = format!("--wasm-max-mem-pages={WASM_MAX_PAGES}");
            // The first argument stands for the program name and always comes
            // back; anything else is a flag this V8 does not know.
            let unknown = deno_core::v8_set_flags(vec!["donsetch-core".to_string(), flag.clone()]);
            match unknown.get(1) {
                None => Ok(()),
                Some(bad) => Err(format!(
                    "V8 does not know {bad}: the wasm memory cap is not in force"
                )),
            }
        })
        .clone()
}

// ----- resident memory --------------------------------------------------------

/// Room above heap + buffers before the process-wide memory backstop trips: the
/// HTTP client, parser and state are in the same process.
pub const RSS_SLACK: usize = 512 * 1024 * 1024;

/// The process's resident set in KB: `VmRSS` on Linux, the working set on
/// Windows, `resident_size` on macOS. `None` where the platform gives no answer;
/// the host layer's WebAssembly budget and the V8 flags are the bounds there, and
/// this is only the net under them.
pub fn resident_kb() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }
    #[cfg(windows)]
    {
        native::windows_working_set().map(|bytes| bytes / 1024)
    }
    #[cfg(target_os = "macos")]
    {
        native::mac_resident_size().map(|bytes| bytes / 1024)
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        None
    }
}

/// The two native calls behind `resident_kb`, declared here so no extra crate is
/// needed. Layouts follow the platform headers named on each item.
#[cfg(any(windows, target_os = "macos"))]
mod native {
    /// `PROCESS_MEMORY_COUNTERS` (psapi.h).
    #[cfg(windows)]
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        // kernel32 exports the psapi call as K32GetProcessMemoryInfo since Windows 7.
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    #[cfg(windows)]
    pub fn windows_working_set() -> Option<usize> {
        let size = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        // SAFETY: an all-zero PROCESS_MEMORY_COUNTERS is valid; the call writes at
        // most `cb` bytes into it, and the pseudo-handle needs no closing.
        let mut counters: ProcessMemoryCounters = unsafe { std::mem::zeroed() };
        counters.cb = size;
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
        (ok != 0).then_some(counters.working_set_size)
    }

    /// `mach_task_basic_info` (mach/task_info.h): 48 bytes, flavor 20.
    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: [i32; 2],
        system_time: [i32; 2],
        policy: i32,
        suspend_count: i32,
    }

    #[cfg(target_os = "macos")]
    extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }

    #[cfg(target_os = "macos")]
    pub fn mac_resident_size() -> Option<usize> {
        const MACH_TASK_BASIC_INFO: i32 = 20;
        let mut count =
            (std::mem::size_of::<MachTaskBasicInfo>() / std::mem::size_of::<i32>()) as u32;
        // SAFETY: an all-zero struct is valid; task_info writes at most `count`
        // integers, and mach_task_self_ is the process's own task port.
        let mut info: MachTaskBasicInfo = unsafe { std::mem::zeroed() };
        let status = unsafe {
            task_info(
                mach_task_self_,
                MACH_TASK_BASIC_INFO,
                &mut info as *mut _ as *mut i32,
                &mut count,
            )
        };
        (status == 0).then_some(info.resident_size as usize)
    }
}

// ----- one engine at a time ---------------------------------------------------

static SLOT: Mutex<()> = Mutex::new(());

/// The engine slot. Held for the whole mint, so at most one V8 isolate lives in
/// this process at a time and its memory exists only while a mint runs.
pub fn slot() -> MutexGuard<'static, ()> {
    SLOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ----- the network guard ------------------------------------------------------

/// Ports a browser refuses to connect to (Chromium's `ERR_UNSAFE_PORT` set): the
/// engine is not a way to speak SMTP, IRC or a database wire protocol.
const UNSAFE_PORTS: &[u16] = &[
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668, 6669,
    6679, 6697, 10080,
];

/// True when an address is on the public internet. Everything else — loopback,
/// private, link-local (cloud metadata), CGNAT, documentation, multicast,
/// unspecified — is the user's own network and is never the engine's to reach.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_v4(mapped);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00 // unique local
        || (segments[0] & 0xffc0) == 0xfe80 // link local
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || (segments[0] == 0x0064 && segments[1] == 0xff9b)) // NAT64 can front a private v4
}

/// What the page's own network ops may touch.
pub struct Guard {
    pub limits: Limits,
    /// The page's own host: it was already fetched by the user's request, so it
    /// is trusted even when it is not public (a local development server).
    page_host: String,
    ops: AtomicU32,
    bytes: AtomicUsize,
}

impl Guard {
    pub fn new(limits: Limits, page_host: &str) -> Arc<Self> {
        Arc::new(Self {
            limits,
            page_host: page_host.to_ascii_lowercase(),
            ops: AtomicU32::new(0),
            bytes: AtomicUsize::new(0),
        })
    }

    /// Reserve one op. False once the page has used its budget.
    pub fn take_op(&self) -> bool {
        self.ops.fetch_add(1, Ordering::SeqCst) < self.limits.ops_max
    }

    pub fn ops_used(&self) -> u32 {
        self.ops.load(Ordering::SeqCst).min(self.limits.ops_max)
    }

    /// Response bytes the page may still read: the smaller of the per-response cap
    /// and what is left of the total.
    pub fn body_allowance(&self) -> usize {
        let left = self
            .limits
            .total_response_max
            .saturating_sub(self.bytes.load(Ordering::SeqCst));
        left.min(self.limits.response_body_max)
    }

    pub fn record_bytes(&self, len: usize) {
        self.bytes.fetch_add(len, Ordering::SeqCst);
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes.load(Ordering::SeqCst)
    }

    fn is_page_host(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(&self.page_host)
    }

    fn address_allowed(&self, host: &str, ip: IpAddr) -> bool {
        is_public(ip) || self.is_page_host(host)
    }

    /// Validate a URL before any socket exists: scheme, port and a literal
    /// address. Names are checked again where they are resolved.
    pub fn check_url(&self, raw: &str) -> Result<url::Url, String> {
        let parsed = url::Url::parse(raw).map_err(|error| format!("blocked: bad url ({error})"))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => return Err(format!("blocked: scheme {other}: is not reachable")),
        }
        if let Some(port) = parsed.port_or_known_default() {
            if UNSAFE_PORTS.contains(&port) {
                return Err(format!("blocked: port {port} is not reachable"));
            }
        }
        match parsed.host() {
            None => Err("blocked: no host".to_string()),
            Some(url::Host::Domain(_)) => Ok(parsed),
            Some(url::Host::Ipv4(ip)) => self.literal(&parsed, IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => self.literal(&parsed, IpAddr::V6(ip)),
        }
    }

    fn literal(&self, parsed: &url::Url, ip: IpAddr) -> Result<url::Url, String> {
        let host = parsed
            .host_str()
            .unwrap_or_default()
            .trim_matches(['[', ']']);
        if self.address_allowed(host, ip) {
            Ok(parsed.clone())
        } else {
            Err(format!("blocked: {ip} is not a public address"))
        }
    }

    /// DNS that answers only addresses this guard allows. Resolving through it
    /// closes the gap between "the name looked public" and "the socket went to a
    /// private address": the connection can only use what is returned here.
    pub fn resolver(self: &Arc<Self>) -> GuardedResolver {
        GuardedResolver {
            guard: Arc::clone(self),
        }
    }

    /// Every redirect hop is validated like a fresh request and the chain is
    /// bounded.
    pub fn redirect_policy(self: &Arc<Self>) -> wreq::redirect::Policy {
        let guard = Arc::clone(self);
        wreq::redirect::Policy::custom(move |attempt| {
            if attempt.previous.len() > guard.limits.redirects_max {
                return attempt.error("too many redirects");
            }
            match guard.check_url(&attempt.uri.to_string()) {
                Ok(_) => attempt.follow(),
                Err(reason) => attempt.error(reason),
            }
        })
    }
}

pub struct GuardedResolver {
    guard: Arc<Guard>,
}

impl wreq::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: wreq::dns::Name) -> wreq::dns::Resolving {
        let guard = Arc::clone(&self.guard);
        Box::pin(async move {
            let host = name.as_str().to_string();
            #[cfg(test)]
            let pinned = test_hosts::lookup(&host);
            #[cfg(not(test))]
            let pinned: Option<IpAddr> = None;
            let resolved: Vec<SocketAddr> = match pinned {
                Some(ip) => vec![SocketAddr::new(ip, 0)],
                None => tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|error| -> BoxError { Box::new(error) })?
                    .collect(),
            };
            let kept: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|addr| guard.address_allowed(&host, addr.ip()))
                .collect();
            if kept.is_empty() {
                let error = io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("blocked: {host} resolves only to non-public addresses"),
                );
                return Err(Box::new(error) as BoxError);
            }
            Ok(Box::new(kept.into_iter()) as wreq::dns::Addrs)
        })
    }
}

/// Test-only names that resolve to a local fixture server, so a fixture can run
/// under a real-looking host on every OS (`*.localhost` resolution is not portable).
#[cfg(test)]
pub mod test_hosts {
    use std::net::IpAddr;
    use std::sync::Mutex;

    static HOSTS: Mutex<Vec<(String, IpAddr)>> = Mutex::new(Vec::new());

    pub fn pin(host: &str, ip: IpAddr) {
        let mut hosts = HOSTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        hosts.retain(|(name, _)| !name.eq_ignore_ascii_case(host));
        hosts.push((host.to_ascii_lowercase(), ip));
    }

    pub fn lookup(host: &str) -> Option<IpAddr> {
        let hosts = HOSTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        hosts
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(host))
            .map(|(_, ip)| *ip)
    }
}

// ----- ArrayBuffer accounting -------------------------------------------------

/// Live and peak ArrayBuffer backing-store bytes. V8 allocates typed-array memory
/// outside its heap, so the heap limit alone would let `new Uint8Array(1 << 30)`
/// through.
pub struct Meter {
    live: AtomicUsize,
    peak: AtomicUsize,
    refusals: AtomicUsize,
    max: usize,
}

impl Meter {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            refusals: AtomicUsize::new(0),
            max,
        })
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Allocations refused for being over the cap.
    pub fn refusals(&self) -> usize {
        self.refusals.load(Ordering::SeqCst)
    }

    fn reserve(&self, len: usize) -> bool {
        let mut live = self.live.load(Ordering::SeqCst);
        loop {
            let next = match live.checked_add(len) {
                Some(next) if next <= self.max => next,
                _ => {
                    self.refusals.fetch_add(1, Ordering::SeqCst);
                    return false;
                }
            };
            match self
                .live
                .compare_exchange(live, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    self.peak.fetch_max(next, Ordering::SeqCst);
                    return true;
                }
                Err(seen) => live = seen,
            }
        }
    }

    fn release(&self, len: usize) {
        self.live.fetch_sub(len, Ordering::SeqCst);
    }
}

fn layout(len: usize) -> std::alloc::Layout {
    // V8 hands the same `len` back on free; a zero-length buffer still needs a
    // real, unique address.
    std::alloc::Layout::from_size_align(len.max(1), 16).expect("buffer layout")
}

unsafe extern "C" fn buffer_allocate(meter: &Meter, len: usize) -> *mut c_void {
    if !meter.reserve(len) {
        return std::ptr::null_mut();
    }
    let pointer = unsafe { std::alloc::alloc_zeroed(layout(len)) };
    if pointer.is_null() {
        meter.release(len);
    }
    pointer as *mut c_void
}

unsafe extern "C" fn buffer_allocate_uninitialized(meter: &Meter, len: usize) -> *mut c_void {
    // Never hand a page uninitialised memory from our process.
    unsafe { buffer_allocate(meter, len) }
}

unsafe extern "C" fn buffer_free(meter: &Meter, data: *mut c_void, len: usize) {
    unsafe { std::alloc::dealloc(data as *mut u8, layout(len)) };
    meter.release(len);
}

unsafe extern "C" fn buffer_drop(meter: *const Meter) {
    drop(unsafe { Arc::from_raw(meter) });
}

static VTABLE: deno_core::v8::RustAllocatorVtable<Meter> = deno_core::v8::RustAllocatorVtable {
    allocate: buffer_allocate,
    allocate_uninitialized: buffer_allocate_uninitialized,
    free: buffer_free,
    drop: buffer_drop,
};

/// The allocator V8 uses for ArrayBuffers, refusing past `meter`'s cap.
pub fn buffer_allocator(meter: &Arc<Meter>) -> deno_core::v8::SharedRef<deno_core::v8::Allocator> {
    let handle = Arc::into_raw(Arc::clone(meter));
    unsafe { deno_core::v8::new_rust_allocator(handle, &VTABLE) }.make_shared()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(page: &str) -> Arc<Guard> {
        Guard::new(Limits::default(), page)
    }

    #[test]
    fn only_the_public_internet_is_public() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
            "::",
        ] {
            assert!(
                !is_public(private.parse().unwrap()),
                "{private} must be refused"
            );
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
        ] {
            assert!(
                is_public(public.parse().unwrap()),
                "{public} must be allowed"
            );
        }
    }

    #[test]
    fn urls_are_checked_before_a_socket_exists() {
        let guard = guard("www.example.com");
        assert!(guard.check_url("https://www.example.com/x").is_ok());
        assert!(guard
            .check_url("https://cdn.vendor.net/challenge.js")
            .is_ok());
        for refused in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "data:text/plain,hi",
            "gopher://example.com/",
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://2130706433/", // 127.0.0.1 written as one integer
            "http://0x7f.1/",
            "https://example.com:22/",
            "http://example.com:25/",
            "http://example.com:6666/",
        ] {
            assert!(
                guard.check_url(refused).is_err(),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn the_pages_own_host_is_trusted_even_when_local() {
        let guard = guard("127.0.0.1");
        assert!(guard.check_url("http://127.0.0.1:8080/x").is_ok());
        assert!(guard.check_url("http://127.0.0.2:8080/x").is_err());
        assert!(guard.check_url("http://169.254.169.254/").is_err());
    }

    #[test]
    fn the_op_budget_is_a_hard_count() {
        let limits = Limits {
            ops_max: 3,
            ..Limits::default()
        };
        let guard = Guard::new(limits, "a.test");
        assert!(guard.take_op() && guard.take_op() && guard.take_op());
        assert!(!guard.take_op());
        assert!(!guard.take_op());
        assert_eq!(guard.ops_used(), 3);
    }

    #[test]
    fn the_body_allowance_shrinks_with_the_total() {
        let limits = Limits {
            response_body_max: 100,
            total_response_max: 150,
            ..Limits::default()
        };
        let guard = Guard::new(limits, "a.test");
        assert_eq!(guard.body_allowance(), 100);
        guard.record_bytes(100);
        assert_eq!(guard.body_allowance(), 50);
        guard.record_bytes(100);
        assert_eq!(guard.body_allowance(), 0);
    }

    #[test]
    fn the_meter_refuses_past_its_cap_and_counts_frees() {
        let meter = Meter::new(1000);
        assert!(meter.reserve(600));
        assert!(!meter.reserve(600));
        meter.release(600);
        assert!(meter.reserve(1000));
        assert_eq!(meter.peak(), 1000);
    }
}
