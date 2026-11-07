//! Runtime ONNX Runtime initialization.
//!
//! ONNX Runtime backs OCR and the semantic reranker. It is acquired,
//! linked and initialized differently on every target:
//!
//! - **Linux x86_64** : dlopen'd at runtime; Microsoft's own release
//!   build of the shared library, shipped beside the binary
//! - **Linux aarch64** : no ONNX; released without `ocr,rerank`
//! - **macOS arm64 / x86_64** : dlopen'd at runtime; Microsoft's
//!   signed 1.23.2 dylib, shipped beside the binary
//! - **Windows x64** : dlopen'd at runtime; Microsoft's own release
//!   build of the DLL, shipped beside the exe
//!
//! No ONNX target gates the load on AVX any more; see "Why
//! there is no AVX gate" below.
//!
//! Which targets get OCR/rerank at all is decided in the release matrix
//! (`.github/workflows/release.yml`).
//!
//! **The rule that has already broken a release:** declare `ort` in
//! exactly ONE `[target.'cfg(...)'.dependencies]` section of
//! `Cargo.toml`, with one feature set. Cargo **unions** features across
//! every target section whose cfg matches : it does not pick one : so a
//! second, differently configured section (or a shared entry) leaks
//! `load-dynamic` onto targets it was not meant for and ships a binary
//! with no ONNX in it, no dylib beside it, and no error anywhere. That is
//! exactly how v3.3.0 went out with OCR and rerank dead on win32-x64 and
//! darwin-arm64. Since #316 every target shares the one section, so the
//! hazard is gone by construction.
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
//! ## macOS : dlopen, both arches (since #316)
//!
//! arm64 used to link statically via `download-binaries`, and
//! x86_64-apple-darwin was released without `ocr,rerank` (`ort-sys`
//! publishes no prebuilt for it; Intel Macs lost OCR and rerank). Both now
//! take the dlopen path with Microsoft's macOS shared library: the pinned
//! 1.23.2 build (the last Developer-ID-signed macOS release; see
//! `build.rs` for why not 1.24.2), loaded from beside the binary like
//! Linux and Windows. The `api-23` floor serves it. On ARM there is no AVX
//! concept (NEON); on Intel Macs the runtime dispatches its kernels like
//! everywhere else.
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
//! build-time error, only a runtime dlopen that finds nothing. At the
//! time, `load_and_init()` discarded `commit()`'s `Result` and the
//! doctor's "static link, compiled in" line was a `cfg` constant rather
//! than a probe, so neither surfaced it either; both have since been fixed
//! (commit() failures error, and the doctor probe is real). The tell in
//! the shipped artifacts was the Windows exe
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
    any(target_os = "linux", target_os = "windows", target_os = "macos"),
    any(feature = "ocr", feature = "rerank")
))]
use std::path::{Path, PathBuf};
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::sync::OnceLock;
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::time::Duration;
/// Message when an init attempt did not finish within
/// [`ONNX_INIT_TIMEOUT_SECS`]. Kept as a stable, actionable line: the
/// user can still run without OCR/rerank.
pub const ONNX_HUNG_MSG: &str = "ONNX Runtime initialization hung (known upstream loader deadlock); OCR and rerank are disabled for this run. Fetch, PDF, crawl and search all continue to work normally.";

/// How long the dedicated ONNX init thread may take before we
/// declare the loader deadlocked and fail fast.
pub const ONNX_INIT_TIMEOUT_SECS: u64 = 15;

/// Error when no ONNX Runtime library is found beside the binary or in
/// the cache fallback.
pub const ONNX_NOT_FOUND_MSG: &str =
    "ONNX Runtime shared library not found. OCR and rerank are disabled.";

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

        // A load can block instead of returning an error: ort
        // 2.0.0-rc.12 builds its own errors through the runtime's API,
        // so a library that loads but that ort then rejects re-enters
        // ort's init lock on the same thread and never returns
        // (pykeio/ort #560), and a library constructor can block inside
        // the loader. Run it on a dedicated thread with a bounded wait
        // so a stuck init can never hang the MCP server; the stuck
        // thread leaks (it cannot be killed) but the daemon keeps
        // working and every later call fails fast.
        let outcome = match init_within(load_and_init, Duration::from_secs(ONNX_INIT_TIMEOUT_SECS))
        {
            Some(r) => r,
            None => {
                HUNG.store(true, Ordering::Release);
                Err(ONNX_HUNG_MSG.to_string())
            }
        };
        STATE.get_or_init(|| outcome).clone()
    }
}

/// Run `init` on a dedicated `onnx-init` thread and wait at most
/// `timeout` for its result.
///
/// `None` means the wait expired: the thread is left running, since a
/// thread blocked inside the loader cannot be stopped. A thread that
/// panics or cannot be spawned yields `Some(Err(..))`.
#[cfg(any(feature = "ocr", feature = "rerank"))]
fn init_within<F>(init: F, timeout: Duration) -> Option<Result<(), String>>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("onnx-init".into())
        .spawn(move || {
            // Receiver may already be gone on timeout: ignore.
            let _ = tx.send(init());
        });
    match spawned {
        Ok(_) => match rx.recv_timeout(timeout) {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Some(Err("ONNX init thread panicked".to_string()))
            }
        },
        Err(e) => Some(Err(format!("cannot spawn ONNX init thread: {e}"))),
    }
}

// ── Linux / Windows: dynamic loading ───────────────────────────

#[cfg(all(
    any(target_os = "linux", target_os = "windows", target_os = "macos"),
    any(feature = "ocr", feature = "rerank")
))]
fn load_and_init() -> Result<(), String> {
    // No AVX gate here: see "Why there is no AVX gate" at the top.

    // 1. Find the shared library.
    let lib_path = find_shared_lib().ok_or_else(|| ONNX_NOT_FOUND_MSG.to_string())?;

    // 2. Make libstdc++ available to the runtime. libonnxruntime.so
    //    needs libstdc++.so.6, and the loader looks for a dependency of
    //    a library on LD_LIBRARY_PATH, that library's own RUNPATH
    //    ($ORIGIN here) and the system paths, but not on the binary's
    //    RUNPATH. dlopen of a bare name does search the calling object's
    //    RUNPATH, and the caller here is this binary, so opening
    //    libstdc++ first finds it wherever the binary's RUNPATH holds it,
    //    as a binary that links libstdc++ itself would. A Nix-built
    //    binary's RUNPATH names the toolchain's lib directory, where
    //    libstdc++.so.6 sits beside the libgcc_s.so.1 the binary links;
    //    the runtime alone finds no libstdc++ there. The runtime's load
    //    below then reuses this copy by its soname. A binary with no
    //    RUNPATH gets the copy the runtime's own lookup would find (none
    //    is shipped beside the runtime). Best effort: if this fails, the
    //    runtime's own load reports the loader's error.
    #[cfg(target_os = "linux")]
    let _ = ort::util::preload_dylib("libstdc++.so.6");

    load_and_init_from(&lib_path)
}

/// Load the ONNX Runtime library at `lib_path` and initialize its
/// environment. A library that cannot be loaded returns the loader's
/// own error.
///
/// A library that loads but that ort then rejects, or whose
/// constructor blocks, may never return: call it through
/// [`init_within`].
#[cfg(all(
    any(target_os = "linux", target_os = "windows", target_os = "macos"),
    any(feature = "ocr", feature = "rerank")
))]
fn load_and_init_from(lib_path: &Path) -> Result<(), String> {
    // 3. Open the library ourselves first, so a library that cannot be
    //    loaded fails here with the loader's own message (on Linux the
    //    dlerror text, e.g. "libstdc++.so.6: cannot open shared object
    //    file"). ort 2.0.0-rc.12 builds its load error through the
    //    runtime's own API, which is not loaded yet, so a failed load
    //    inside ort::init_from re-enters ort's init lock on the same
    //    thread and never returns (pykeio/ort #560). preload_dylib
    //    reports a plain libloading error and keeps the library loaded,
    //    so init_from below only ever opens a library that already
    //    loaded once.
    if let Err(e) = ort::util::preload_dylib(lib_path) {
        let mut msg = format!(
            "Failed to load ONNX Runtime from {}: {e}",
            lib_path.display()
        );
        // libloading's own text is just "dlopen failed"; the loader's
        // message is in the source chain.
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            msg.push_str(": ");
            msg.push_str(&s.to_string());
            source = s.source();
        }
        return Err(msg);
    }

    // 4. dlopen and init.
    //    ort::init_from loads the .so via libloading.
    //    builder.commit() initializes the ONNX environment.
    let builder = ort::init_from(lib_path).map_err(|e| {
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

    crate::diag!("[onnx] Runtime loaded from {}", lib_path.display());
    Ok(())
}

/// Find the ONNX Runtime shared library.
///
/// Searches:
/// 1. Next to the current executable (primary).
/// 2. `cache_dir()/onnx/` (fallback for relocatable installs).
#[cfg(all(
    any(target_os = "linux", target_os = "windows", target_os = "macos"),
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
    any(target_os = "linux", target_os = "windows", target_os = "macos"),
    any(feature = "ocr", feature = "rerank")
))]
pub(crate) fn shared_lib_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
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

    #[cfg(all(target_os = "macos", any(feature = "ocr", feature = "rerank")))]
    #[test]
    fn shared_lib_name_is_dylib_on_macos() {
        assert_eq!(super::shared_lib_name(), "libonnxruntime.dylib");
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

    /// The shipped runtime, when it cannot be loaded, is reported at
    /// once with the loader's error, never as a hang after the init
    /// timeout. Where it loads there is nothing to check; the next two
    /// tests cover a failing load.
    #[cfg(all(
        any(target_os = "linux", target_os = "windows", target_os = "macos"),
        any(feature = "ocr", feature = "rerank")
    ))]
    #[test]
    fn a_runtime_that_cannot_load_fails_at_once_with_the_loader_error() {
        let started = std::time::Instant::now();
        let outcome = super::ensure_loaded();
        let took = started.elapsed();
        if let Err(e) = &outcome {
            assert_ne!(
                e,
                super::ONNX_HUNG_MSG,
                "a runtime that failed to load was reported as a loader hang"
            );
            assert!(
                took < std::time::Duration::from_secs(5),
                "the load error took {took:?} (timeout {}s): {e}",
                super::ONNX_INIT_TIMEOUT_SECS
            );
            assert!(
                e.starts_with("Failed to load ONNX Runtime from "),
                "not the loader's error: {e}"
            );
        }
    }

    /// `outcome` is the load error for `lib` within the wait: libloading's
    /// own text followed by the loader's message.
    #[cfg(all(
        any(target_os = "linux", target_os = "windows", target_os = "macos"),
        any(feature = "ocr", feature = "rerank")
    ))]
    fn assert_loader_error(outcome: Option<Result<(), String>>, lib: &std::path::Path) {
        let prefix = format!("Failed to load ONNX Runtime from {}: ", lib.display());
        match outcome {
            // e.g. "dlopen failed: <path>: file too short" on Linux.
            Some(Err(e)) => assert!(
                e.strip_prefix(&prefix)
                    .is_some_and(|detail| detail.contains(": ")),
                "not the loader's error: {e}"
            ),
            other => panic!("expected the loader's error within the wait, got {other:?}"),
        }
    }

    /// The shipped runtime, loaded where the loader cannot find one of
    /// its dependencies, fails at once with the loader's error. That
    /// needs a host where the runtime's own lookup misses a dependency:
    /// NixOS without `LD_LIBRARY_PATH`, where `libonnxruntime.so` finds
    /// no `libstdc++.so.6` by itself (`load_and_init` opens it from the
    /// binary's RUNPATH first; this test skips that step). Where the
    /// runtime loads there is nothing to check. Relies on nextest's
    /// process per test: this test commits ort's environment, and ort
    /// accepts one commit per process, so in a shared process this test
    /// and `ensure_loaded` cannot both succeed.
    #[cfg(all(
        any(target_os = "linux", target_os = "windows", target_os = "macos"),
        any(feature = "ocr", feature = "rerank")
    ))]
    #[test]
    fn a_runtime_whose_dependency_is_missing_fails_at_once_with_the_loader_error() {
        let lib = super::find_shared_lib().expect("the shipped runtime beside the test binary");
        let target = lib.clone();
        // Where the runtime loads, this wait also covers a successful
        // load; the first one after a build has taken over 4 s on 9040.
        let outcome = super::init_within(
            move || super::load_and_init_from(&target),
            std::time::Duration::from_secs(10),
        );
        if outcome != Some(Ok(())) {
            assert_loader_error(outcome, &lib);
        }
    }

    /// A file that is not a loadable library fails at once with the
    /// loader's error, on every host.
    #[cfg(all(
        any(target_os = "linux", target_os = "windows", target_os = "macos"),
        any(feature = "ocr", feature = "rerank")
    ))]
    #[test]
    fn a_file_that_is_not_a_library_fails_at_once_with_the_loader_error() {
        // cache_dir() is this test process's own sandbox under cfg(test).
        let dir = crate::paths::cache_dir().join("not-a-runtime");
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join(super::shared_lib_name());
        std::fs::write(&lib, b"not a shared library").unwrap();
        let target = lib.clone();
        let outcome = super::init_within(
            move || super::load_and_init_from(&target),
            std::time::Duration::from_secs(5),
        );
        assert_loader_error(outcome, &lib);
    }

    /// The timeout guard still reports an init that really blocks.
    #[cfg(any(feature = "ocr", feature = "rerank"))]
    #[test]
    fn an_init_that_blocks_is_reported_when_the_wait_expires() {
        let wait = std::time::Duration::from_millis(300);
        let started = std::time::Instant::now();
        let outcome = super::init_within(
            || -> Result<(), String> {
                loop {
                    std::thread::park();
                }
            },
            wait,
        );
        let took = started.elapsed();
        assert!(
            outcome.is_none(),
            "a blocked init must time out: {outcome:?}"
        );
        assert!(
            took >= wait && took < std::time::Duration::from_secs(5),
            "the guard returned after {took:?}, waiting {wait:?}"
        );
    }
}
