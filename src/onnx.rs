//! Runtime ONNX Runtime initialization.
//!
//! ONNX Runtime backs OCR and the semantic reranker. It is acquired,
//! linked and initialized differently on every target:
//!
//! - **Linux x86_64** : dlopen'd at runtime; Microsoft's own release
//!   build of the shared library, shipped beside the binary
//! - **Linux aarch64** : no ONNX; released without `ocr,rerank`
//! - **macOS arm64** : statically linked
//! - **macOS x86_64** : no ONNX; released without `ocr,rerank`
//! - **Windows x64** : dlopen'd at runtime; Microsoft's own release
//!   build of the DLL, shipped beside the exe
//!
//! Neither dlopen target gates the load on AVX any more; see "Why
//! there is no AVX gate" below.
//!
//! Which targets get OCR/rerank at all is decided in the release matrix
//! (`.github/workflows/release.yml`).
//!
//! **The one rule that has already broken a release:** declare `ort` only
//! in the two mutually exclusive `[target.'cfg(...)'.dependencies]`
//! sections of `Cargo.toml`, never in shared `[dependencies]`. Cargo
//! **unions** features across every target section whose cfg matches : it
//! does not pick one : so a shared entry leaks `load-dynamic` onto macOS,
//! where it wins over static linking and ships a binary with no ONNX in
//! it, no dylib beside it, and no error anywhere. That is exactly how
//! v3.3.0 went out with OCR and rerank dead on win32-x64 and
//! darwin-arm64.
//!
//! Everything below is reference detail: per-platform rationale, a
//! postmortem of the v3.3.0 wiring bug, and the history of the Windows
//! static link. Read the section for the platform or failure you are
//! actually touching.
//!
//! ## Linux x86_64 : dlopen
//!
//! Dynamically loaded to avoid SIGILL on non-AVX CPUs. pyke's prebuilt
//! ONNX *static* archive contains unguarded AVX instructions in C++
//! global constructors that run before `main()`, so statically linking
//! it kills the process at startup on any CPU without AVX (#57). With
//! `ort`'s `load-dynamic` feature ONNX is NOT statically linked: the
//! shared library is Microsoft's own release build, fetched by
//! `build.rs`, shipped beside the binary and dlopen'd at runtime.
//!
//! ## Why there is no AVX gate
//!
//! Until 4.3.x the dlopen was additionally gated on `cpu::has_avx()`,
//! on the assumption that the shared library had the same constructor
//! problem as the static archive. It does not: Microsoft's build
//! selects its kernels at runtime (MLAS dispatch), and the shipped
//! `libonnxruntime.so` both loads and runs OCR on a CPU with SSE4.2
//! and no AVX (verified under `qemu-x86_64 -cpu Nehalem`: a scanned
//! PDF OCR'd at 99% confidence; #277's Celeron J1900 is that class of
//! CPU). The Windows DLL from the same release behaves the same: under
//! Intel SDE `-nhm` (emulated Nehalem, no AVX) it loads and runs both
//! OCR and rerank with no invalid-instruction report. The gate was
//! therefore the only thing disabling OCR and rerank on such
//! machines. `cpu::has_avx()` stays as a doctor diagnostic; nothing is
//! gated on it.
//!
//! ## Linux aarch64 : no ONNX
//!
//! Released without `ocr,rerank`. ONNX Runtime's static global
//! constructors can deadlock there (issue #9), so the features are simply
//! not built rather than shipped broken.
//!
//! ## macOS : static link (arm64 only)
//!
//! arm64 links statically via `download-binaries`; there is no AVX concept
//! on ARM (NEON), so no gate is needed. x86_64-apple-darwin is released
//! without `ocr,rerank` because `ort-sys` publishes no prebuilt for that
//! target.
//!
//! ## Windows x64 : dlopen at runtime
//!
//! Until 4.3.x Windows linked ONNX statically, on the reasoning that AVX
//! issues are rare and that pyke ships no `onnxruntime.dll` (its Windows
//! artifact is a ~305MB `onnxruntime.lib` plus `DirectML.dll`, and the
//! MSVC linker cannot make a DLL from that archive: duplicate protobuf
//! symbols). Both halves were true and the conclusion was still wrong:
//! the static archive's global constructors run AVX before `main()`, so
//! on a CPU without AVX (#277: a first-generation Core i7) `donsetch.exe`
//! died at process start with no output, every feature included (Intel
//! SDE reproduces it: `vpxor` under `-nhm`, and even under `-snb`,
//! Sandy Bridge *with* AVX, `shlx` (BMI2) in the archive's MSVC STL
//! code, so the static build in fact needed a Haswell-class CPU); and
//! pyke is not the only source of the DLL. Microsoft publishes
//! `onnxruntime-win-x64-<version>.zip` with `lib/onnxruntime.dll` (MIT)
//! for every release, built without the DirectML provider.
//!
//! So Windows now does what Linux does: `load-dynamic`, the DLL beside
//! the exe, `find_shared_lib` then `init_from` (no gate: the runtime
//! dispatches its kernels, see the section above).
//! `build.rs` fetches the pinned Microsoft zip, verifies its sha256 and
//! places `onnxruntime.dll` beside every build's exe (dev builds too, the
//! same `fetch_onnx_prebuilt` path as the Linux `.so`); `release.yml`
//! packages it next to `donsetch.exe` and `pdfium.dll`, and the
//! self-updater carries it across updates like `pdfium.dll`. Any ORT
//! release at or above the `api-N` feature's version satisfies
//! `GetApi`, so the pin can move forward without touching the crate.
//!
//! What this removed: the hard `DirectML.dll` import (the static archive
//! was built with the DirectML provider, so `ort-sys` emitted the link
//! directive and the exe would not start on Server Core / Nano / pre-1903
//! Windows 10 without a copy of that DLL), and the `copy-dylibs`
//! dev-build shim that went with it. Microsoft's CPU build imports the
//! VC++ runtime (`MSVCP140`, `VCRUNTIME140` and their `_1` variants),
//! which the MSVC target already requires, plus `dxgi`, `dbghelp` and
//! `SETUPAPI`, and loads `dxcore.dll` at runtime for device discovery.
//! Where `dxgi.dll` is missing (Nano Server) the DLL fails to load and
//! only OCR/rerank are disabled; the exe itself still starts.
//!
//! ## Postmortem : how the v3.3.0 feature leak stayed silent
//!
//! The rule itself is at the top of this comment; this is why nothing
//! caught the violation. `load-dynamic` implies `ort-sys/disable-linking`,
//! and `ort-sys`'s build script early-returns on that flag : before
//! downloading anything and before `copy-dylibs` runs : so there is no
//! build-time error, only a runtime dlopen that finds nothing. At runtime,
//! `load_and_init()` below still discards `commit()`'s `Result`, and the
//! doctor's "static link, compiled in" line is a `cfg` constant rather
//! than a probe, so neither surfaced it either. Worth fixing if you touch
//! this again. The tell in the shipped artifacts was the Windows exe
//! dropping 35.6MB -> 16.3MB : the missing ONNX static archive.
//!
//! ## History : the Windows `DirectML.dll` import (removed in #298)
//!
//! pyke's static Windows archive was built with the DirectML provider,
//! so `ort-sys` emitted `DXGI`/`D3D12`/`DirectML` link directives and
//! `donsetch.exe` hard-imported `DirectML.dll` without ever calling it.
//! The exe could not start where that DLL was missing (Server Core,
//! Nano Server, Windows 10 before 1903), and a `System32` copy taken
//! from another Windows version failed to load with `0xC0000142`.
//! `copy-dylibs` put the redist beside dev builds for that reason. The
//! dlopen switch removed all of it: neither the exe nor Microsoft's
//! CPU-only `onnxruntime.dll` references DirectML.

#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    any(feature = "ocr", feature = "rerank")
))]
use std::path::PathBuf;
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::sync::OnceLock;
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::time::Duration;
/// Message when an init attempt deadlocked inside the dynamic
/// loader (pykeio/ort #579/#560 class). Kept as a stable, actionable
/// line: the user can still run without OCR/rerank.
pub const ONNX_HUNG_MSG: &str = "ONNX Runtime initialization hung (known upstream loader deadlock); OCR and rerank are disabled for this run. Fetch, PDF, crawl and search all continue to work normally.";

/// How long the dedicated ONNX init thread may take before we
/// declare the loader deadlocked and fail fast.
pub const ONNX_INIT_TIMEOUT_SECS: u64 = 15;

/// Ensure ONNX Runtime is loaded and initialized.
///
/// Returns `Ok(())` if ONNX is ready for use, or an `Err` with a
/// human-readable message explaining why OCR/rerank is unavailable.
///
/// Safe to call multiple times: the first call loads+inits, all
/// subsequent calls return immediately.
pub fn ensure_loaded() -> Result<(), String> {
    #[cfg(not(any(feature = "ocr", feature = "rerank")))]
    {
        Err("not compiled with OCR/rerank support".to_string())
    }
    #[cfg(any(feature = "ocr", feature = "rerank"))]
    {
        static STATE: OnceLock<Result<(), String>> = OnceLock::new();
        static HUNG: AtomicBool = AtomicBool::new(false);

        if let Some(r) = STATE.get() {
            return r.clone();
        }
        // Once an init attempt has hung, fail fast forever: do not
        // re-spawn a thread that will also hang (each hung attempt
        // leaks that thread; a retry-happy daemon would stack them).
        if HUNG.load(Ordering::Acquire) {
            return Err(ONNX_HUNG_MSG.to_string());
        }

        // ort's init path (dlopen on Linux/Windows, env construction on
        // macOS) can deadlock inside the dynamic loader in
        // complex binaries (pykeio/ort #579, #560) instead of
        // returning an error. Run it on a dedicated thread with a
        // bounded wait so a hung loader can never hang the MCP
        // server; the stuck thread leaks (it cannot be killed) but
        // the daemon keeps working and every later call fails fast.
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("onnx-init".into())
            .spawn(move || {
                // Receiver may already be gone on timeout: ignore.
                let _ = tx.send(load_and_init());
            });
        let outcome = match spawned {
            Ok(_) => match rx.recv_timeout(Duration::from_secs(ONNX_INIT_TIMEOUT_SECS)) {
                Ok(r) => r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    HUNG.store(true, Ordering::Release);
                    Err(ONNX_HUNG_MSG.to_string())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    Err("ONNX init thread panicked".to_string())
                }
            },
            Err(e) => Err(format!("cannot spawn ONNX init thread: {e}")),
        };
        STATE.get_or_init(|| outcome).clone()
    }
}

// ── Linux / Windows: dynamic loading ───────────────────────────

#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    any(feature = "ocr", feature = "rerank")
))]
fn load_and_init() -> Result<(), String> {
    // No AVX gate here: see "Why there is no AVX gate" at the top.

    // 1. Find the shared library.
    let lib_path = find_shared_lib().ok_or_else(|| {
        "ONNX Runtime shared library not found. \
            OCR and rerank are disabled."
            .to_string()
    })?;

    // 2. dlopen and init.
    //    ort::init_from loads the .so via libloading.
    //    builder.commit() initializes the ONNX environment.
    let builder = ort::init_from(&lib_path).map_err(|e| {
        format!(
            "Failed to load ONNX Runtime from {}: {e}",
            lib_path.display()
        )
    })?;

    // Surface commit() failures: a dylib that loads but cannot
    // initialize must fail OCR/rerank loudly, not silently degrade
    // (that exact silence hid the v3.3.0 feature leak).
    // NOTE: on the load-dynamic path commit() reports bool.
    if !builder.commit() {
        return Err("ONNX Runtime init failed (dynamic load)".to_string());
    }

    eprintln!("[onnx] Runtime loaded from {}", lib_path.display());
    Ok(())
}

/// Find the ONNX Runtime shared library.
///
/// Searches:
/// 1. Next to the current executable (primary).
/// 2. `cache_dir()/onnx/` (fallback for relocatable installs).
#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    any(feature = "ocr", feature = "rerank")
))]
fn find_shared_lib() -> Option<PathBuf> {
    let lib_name = shared_lib_name();

    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join(lib_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    let cache = crate::paths::cache_dir().join("onnx").join(lib_name);
    if cache.exists() {
        return Some(cache);
    }

    None
}

/// The runtime's file name beside the binary on the dlopen targets.
#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    any(feature = "ocr", feature = "rerank")
))]
pub(crate) fn shared_lib_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else {
        "libonnxruntime.so"
    }
}

// ── macOS: static linking ──────────────────────────────────────

#[cfg(all(
    not(any(target_os = "linux", target_os = "windows")),
    any(feature = "ocr", feature = "rerank")
))]
fn load_and_init() -> Result<(), String> {
    // macOS ARM64: no AVX concept (ARM NEON). Always works.
    // Just initialize the ONNX environment (static link).
    // Surface commit() failures: the 3.3.0 leak shipped binaries
    // where the static archive was never linked in and this call
    // failed silently : treat it as an error instead.
    // NOTE: commit() reports bool on this path too.
    if !ort::init().commit() {
        return Err("ONNX Runtime init failed (static)".to_string());
    }
    eprintln!("[onnx] Runtime initialized (static link)");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(all(target_os = "linux", any(feature = "ocr", feature = "rerank")))]
    #[test]
    fn shared_lib_name_is_so_on_linux() {
        assert_eq!(super::shared_lib_name(), "libonnxruntime.so");
    }

    #[cfg(all(target_os = "windows", any(feature = "ocr", feature = "rerank")))]
    #[test]
    fn shared_lib_name_is_dll_on_windows() {
        assert_eq!(super::shared_lib_name(), "onnxruntime.dll");
    }

    /// Payload probe: the ONNX environment must actually initialize
    /// in this binary. On static-link targets this is the only thing
    /// that catches a build where the archive was never linked in
    /// (the v3.3.0 leak); on Linux it proves the dylib loads and
    /// commits. Runs in every features-enabled CI job, so a dead
    /// payload fails at merge time, not at release time. Non-AVX
    /// Linux hosts skip it: they can't run ONNX by design (their
    /// builds must still pass).
    #[cfg(any(feature = "ocr", feature = "rerank"))]
    #[test]
    fn onnx_payload_probe_initializes() {
        super::ensure_loaded().expect("ONNX Runtime failed to initialize");
        // A second call must reuse the memoized state, not re-init.
        super::ensure_loaded().expect("ONNX Runtime failed to initialize (recheck)");
    }
}
