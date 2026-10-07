// Build script: PDFium acquisition + linking.
//
// PDFium is the one heavy vendored primitive (Chrome's own PDF engine,
// BSD-licensed).
//
// **Linux/macOS**: statically link prebuilt static archives from
// kognitos/pdfium-static (a fork of bblanchon/pdfium-binaries producing
// .a instead of shared libs). The archives bundle Chromium's
// namespace-mangled libc++, so there is no host C++ runtime conflict.
//
// **Windows**: use the shared library (pdfium.dll) from
// bblanchon/pdfium-binaries. PDFium's static .lib is built with /MT
// (static CRT), but Rust's MSVC target uses /MD (dynamic CRT). Mixing
// /MT and /MD in one binary is undefined behavior on MSVC — the linker
// emits LNK2005 multiply-defined symbols for C++ CRT internals
// (std::_Raise_handler, std::ctype<char>::id, etc.) that exist in
// libcpmt.lib but not msvcprt.lib. The DLL sidesteps this entirely:
// pdfium.dll has its own CRT baked in, no conflict with the host.
//
// If vendor/pdfium/lib does not contain the library for the target,
// we download and unpack the pinned release with curl/tar. The SHA-256
// of the downloaded tarball is verified against a pinned map and is
// required — builds refuse to download when no pinned hash exists.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned PDFium release for static archives (Linux/macOS).
/// Source: kognitos/pdfium-static (fork of bblanchon/pdfium-binaries).
const PDFIUM_STATIC_TAG: &str = "chromium/7809";

/// Pinned PDFium release for the Windows shared library (DLL).
/// Source: bblanchon/pdfium-binaries. This is the closest release to
/// 7809 that bblanchon provides. The FFI surface is identical — both
/// are Chromium 149-era PDFium builds.
const PDFIUM_SHARED_TAG: &str = "chromium/7802";

/// sha256 of the pinned tarball per platform "os-arch".
/// Verified on download and required; builds fail before any network download if no entry exists.
/// Entries are release-asset digests for the exact pinned artifacts: static
/// Linux/macOS assets from kognitos/pdfium-static chromium/7809, and shared
/// Android/Windows assets from bblanchon/pdfium-binaries chromium/7802.
/// They were cross-checked against each release's attestation metadata; a
/// sidecar checksum file from the same release is not authorization.
const KNOWN_HASHES: &[(&str, &str)] = &[
    (
        "linux-x64",
        "13908bb2d40a6e017c4c5a6a7baecc6efd7b1c30392c8a79e80072d2b48b18eb",
    ),
    (
        "linux-arm64",
        "abe1c3d5b168ec2baaafc7a8fcddfda1a09417f39199c7993fd28d34d3a7f70e",
    ),
    (
        "mac-x64",
        "c097fd17a07826bb36617dda0cd02bd7829c0f2087f33e927124df21dc5cef06",
    ),
    (
        "mac-arm64",
        "08556377b5d33b2fef7c6bfec66e01b9b23007c10533ab0404fe54538cbb2837",
    ),
    (
        "win-x64",
        "487156c28d81bd162107ca0ba85849cbfbb0127be4210a7cfec6def66802082d",
    ),
    (
        "win-arm64",
        "15d679b0baf8bb470c9fae155c0abc4ab752017b38f4cc71940314af565c53e2",
    ),
    (
        "android-x64",
        "596cbe4fd6cbb118a9f0576fa96f2c0f4476f7a85779e7f32d64888a6e4f1ddd",
    ),
    (
        "android-arm64",
        "4e510dd0757af1439107c23577fafdc854fac9c403a5a5b20f78ebf87097672c",
    ),
];

fn main() {
    // Declare the custom cfg so rustc doesn't warn about it.
    println!("cargo::rustc-check-cfg=cfg(linux_like)");

    let os = env::var("CARGO_CFG_TARGET_OS").expect("no target os");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("no target arch");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("no manifest dir"));
    let vendored = manifest.join("vendor").join("pdfium");
    let libdir = vendored.join("lib");

    let is_windows = os == "windows";
    let is_android = os == "android";
    // Windows and Android use shared libraries (DLL / .so) from
    // bblanchon/pdfium-binaries. Linux/macOS use static archives from
    // kognitos/pdfium-static. Android's bionic libc cannot link the
    // glibc-targeted static archives (issue #16).
    let is_shared = is_windows || is_android;

    // Windows: pdfium.lib is an import library for pdfium.dll.
    // Android: libpdfium.so is a shared library.
    // Linux/macOS: libpdfium.a is a full static archive.
    let pdfium_name = if is_shared {
        if is_windows {
            "pdfium.lib"
        } else {
            "libpdfium.so"
        }
    } else {
        "libpdfium.a"
    };

    if !libdir.join(pdfium_name).exists() {
        fetch_pdfium(&os, &arch, &vendored);
    }

    println!("cargo:rustc-link-search=native={}", libdir.display());

    if is_shared {
        // Link against the shared library. On Windows this links
        // against the import library; pdfium.dll is resolved at
        // runtime. On Android, libpdfium.so is linked directly.
        println!("cargo:rustc-link-lib=dylib=pdfium");
        if is_windows {
            for l in ["gdi32", "user32", "advapi32", "comdlg32", "shell32"] {
                println!("cargo:rustc-link-lib=dylib={}", l);
            }
        } else if is_android {
            // PDFium's Android .so links against libc++_shared.so
            // (NDK C++ runtime). Termux ships libc++_shared.so in
            // $PREFIX/lib; the linker finds it via the search path.
            println!("cargo:rustc-link-lib=dylib=c++_shared");
            println!("cargo:rustc-link-lib=dylib=log");
        }

        // Copy the shared library to the output directory so tests
        // and the binary can find it at runtime without requiring it
        // in PATH (Windows) or LD_LIBRARY_PATH (Android).
        let bindir = vendored.join("bin");
        let shared_lib = if is_windows {
            bindir.join("pdfium.dll")
        } else {
            bindir.join("libpdfium.so")
        };
        if shared_lib.exists() {
            let out_dir = env::var("OUT_DIR").expect("no OUT_DIR");
            let out_path = PathBuf::from(&out_dir);
            let profile_dir = out_path
                .ancestors()
                .nth(3)
                .expect("cannot find profile dir from OUT_DIR");
            let dest_name = if is_windows {
                "pdfium.dll"
            } else {
                "libpdfium.so"
            };
            for dest in [
                profile_dir.join(dest_name),
                profile_dir.join("deps").join(dest_name),
            ] {
                if !dest.exists() {
                    let _ = fs::copy(&shared_lib, &dest);
                }
            }
        }
    } else {
        println!("cargo:rustc-link-lib=static=pdfium");
        match os.as_str() {
            "linux" => {
                // Bundled namespace-mangled libc++ satisfies pdfium's internal
                // std::__Cr::* references without touching the host runtime.
                println!("cargo:rustc-link-lib=static=c++");
                println!("cargo:rustc-link-lib=static=c++abi");
                println!("cargo:rustc-link-lib=dylib=pthread");
                println!("cargo:rustc-link-lib=dylib=dl");
                println!("cargo:rustc-link-lib=dylib=m");
            }
            "macos" => {
                println!("cargo:rustc-link-lib=dylib=c++");
                for f in ["CoreGraphics", "CoreFoundation", "CoreText", "AppKit"] {
                    println!("cargo:rustc-link-lib=framework={}", f);
                }
            }
            other => panic!("pdfium: unsupported target os {other}"),
        }
    }

    // Android shares the Linux code paths (process groups, /proc,
    // prctl, known Chrome paths, headless fallback). Emit a cfg so
    // the source uses #[cfg(linux_like)] instead of repeating
    // #[cfg(any(target_os = "linux", target_os = "android"))] everywhere.
    if os == "linux" || os == "android" {
        println!("cargo:rustc-cfg=linux_like");
    }

    // Force re-run when the vendor lib dir is missing or changes.
    // On a fresh CI checkout, vendor/pdfium/lib doesn't exist, so cargo
    // always re-runs this build script and triggers the download.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/pdfium/lib");
    // The version resource is derived from these two files alone, so a version
    // bump or a rename must re-run this script or the exe keeps a stale one.
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src/display_name.rs");

    // ── LLD auto-detection (Linux) ────────────────────────────
    //
    // The PDFium static archives are produced by LLVM/Clang. GNU ld
    // on some versions/architectures can't parse the ELF machine type
    // of LLVM-generated archive members — notably aarch64 with GNU
    // ld 2.42+ reports "architecture: UNKNOWN!" and skips them as
    // incompatible. Newer GNU binutils (2.44+) on x86_64 also reject
    // LLVM CREL relocations in the same archives.
    //
    // LLD handles both correctly. If ld.lld is available, we inject
    // -fuse-ld=lld into the link step so the compiler driver (cc/gcc)
    // uses LLD instead of GNU ld. This is transparent to the user —
    // no RUSTFLAGS or .cargo/config.toml needed.
    if os == "linux" || os == "android" {
        let lld_available = Command::new("ld.lld")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if lld_available {
            println!("cargo:rustc-link-arg=-fuse-ld=lld");
        } else if arch == "aarch64" {
            eprintln!(
                "warning: donsetch: LLD not found. GNU ld on aarch64 may fail \
                 to link LLVM-produced PDFium archives. \
                 Install lld (e.g., apt install lld / pkg install lld) and rebuild."
            );
        }
    }

    // ── Windows main-thread stack size ────────────────────────
    //
    // On Windows the main thread's stack size comes from the PE header,
    // which defaults to 1MB. Linux gives 8MB. donsetch runs its whole
    // future tree on the main thread via tokio's block_on, and the
    // fetch_tool state machine does not fit in 1MB unoptimized — a debug
    // build dies with 0xc00000fd (STATUS_STACK_OVERFLOW) inside __chkstk
    // before entering the function body, with no usable backtrace since
    // a stack overflow is not a panic. Release only fits because
    // optimization shrinks the frame, which is luck, not headroom.
    //
    // Ask the linker for Linux's 8MB so the ceiling stops being
    // profile-dependent. Transparent to the user, like the LLD flag above.
    if is_windows {
        let abi = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if abi == "gnu" {
            // MinGW: GNU ld spelling.
            println!("cargo:rustc-link-arg=-Wl,--stack,8388608");
        } else {
            // MSVC link.exe.
            println!("cargo:rustc-link-arg=/STACK:8388608");
        }
    }

    // ── Windows version resource ──────────────────────────────
    // The metadata Explorer, Task Manager and Get-Command read off the exe.
    stamp_version_resource(&os);

    // ── aarch64 + ONNX note ──────────────────────────────────
    //
    // ONNX Runtime is now dynamically loaded (ort load-dynamic).
    // The C++ global constructor deadlock that affected aarch64
    // with static linking no longer occurs at process start.
    // ONNX is dlopen'd at runtime only when OCR/rerank is used.
    // The aarch64 ONNX prebuilt is not available from pyke, so
    // aarch64 builds simply lack OCR/rerank (reported at runtime).

    // ── ONNX Runtime shared library (dynamic loading) ────────
    //
    // ONNX Runtime's prebuilt static archive contains unguarded
    // AVX instructions in C++ global constructors. Statically linking
    // it causes SIGILL at process start on non-AVX CPUs (pre-2011
    // Intel, QEMU default, Docker VMs without AVX passthrough).
    //
    // With ort's `load-dynamic` feature, ONNX is NOT statically
    // linked. Instead, we build a shared library (.so/.dylib/.dll)
    // from the prebuilt static archive at build time, and dlopen it
    // at runtime after an AVX check. Non-AVX CPUs get a working binary
    // (minus OCR/rerank) instead of SIGILL.
    let has_onnx =
        env::var_os("CARGO_FEATURE_OCR").is_some() || env::var_os("CARGO_FEATURE_RERANK").is_some();
    if has_onnx {
        if let Some(info) = onnx_target_info(&os, &arch) {
            // Every ONNX target fetches Microsoft's shared library
            // for dynamic loading and places it beside the binary
            // (dev builds included, #298 review; macOS since #316).
            fetch_onnx_prebuilt(info, &manifest);
        } else {
            // Anything without a pinned prebuilt (aarch64 Linux's
            // loader deadlocks; Android is unsupported): the
            // features compile and report unavailable at runtime.
            eprintln!(
                "donsetch build: OCR/rerank enabled, no pinned ONNX prebuilt for {os}-{arch}"
            );
        }
    }

    // ── Compile-time metadata for `donsetch -v` ────────────────
    // Captured here so the binary self-reports its build identity.

    // Git resolves linked worktrees and packed refs; watching .git alone
    // misses commits on an existing branch and caches an old build identity.
    let branch = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok());
    for name in ["HEAD", "packed-refs"]
        .into_iter()
        .chain(branch.as_deref().map(str::trim))
    {
        if let Ok(out) = Command::new("git")
            .args(["rev-parse", "--git-path", name])
            .output()
            && out.status.success()
            && let Ok(path) = String::from_utf8(out.stdout)
        {
            let path = Path::new(path.trim());
            // Cargo treats a missing watched file as perpetually changed.
            // A packed branch watches its existing parent for a new loose ref.
            let watched = if name == "packed-refs" {
                path.exists().then_some(path)
            } else {
                path.ancestors().find(|parent| parent.exists())
            };
            if let Some(watched) = watched {
                println!("cargo:rerun-if-changed={}", watched.display());
            }
        }
    }
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .filter(|hash| !hash.trim().is_empty());
    println!(
        "cargo:rustc-env=DONSETCH_GIT_HASH={}",
        hash.as_deref().map(str::trim).unwrap_or("unknown")
    );

    // PDFium variant string.
    let pdfium_tag = if is_shared {
        PDFIUM_SHARED_TAG
    } else {
        PDFIUM_STATIC_TAG
    };
    let pdfium_kind = if is_shared { "shared" } else { "static" };
    println!("cargo:rustc-env=DONSETCH_PDFIUM={pdfium_kind}, {pdfium_tag}");

    // Target triple.
    let triple = match (os.as_str(), arch.as_str()) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("android", "aarch64") => "aarch64-linux-android",
        ("android", "x86_64") => "x86_64-linux-android",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => "unknown",
    };
    println!("cargo:rustc-env=DONSETCH_TARGET={triple}");

    // Enabled feature flags.
    //
    // Read from the environment rather than a hand-written list: Cargo sets
    // CARGO_FEATURE_<NAME> for every enabled feature of this package, so a
    // feature added to Cargo.toml shows up in `--version` without touching
    // this code. The old hardcoded pair is why `http` was never reported.
    //
    // The name is recovered by lowercasing, which cannot distinguish a
    // feature spelled with `-` from one spelled with `_` (Cargo maps both to
    // `_`). Every feature here is a single word; a hyphenated one would be
    // displayed with an underscore.
    //
    // Names are restricted to the characters Cargo can produce here. The value
    // of a `cargo:rustc-env=` line runs to the end of the line and has no
    // escape syntax, so a name carrying a newline would inject a second
    // directive; `vars_os` is used because `vars` panics on an environment
    // holding any non-UTF-8 entry, feature-related or not.
    let mut feats: Vec<String> = env::vars_os()
        .filter_map(|(k, _)| {
            let name = k.to_str()?.strip_prefix("CARGO_FEATURE_")?;
            // `default` is a feature like any other and Cargo exports it, but
            // it is empty here: it would only report whether
            // --no-default-features was passed. Any member it ever gains is
            // listed on its own.
            if name == "DEFAULT" {
                return None;
            }
            (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                .then(|| name.to_ascii_lowercase())
        })
        .collect();
    // Deterministic order: env iteration order is not guaranteed.
    feats.sort();
    let feats_display = if feats.is_empty() {
        "(none)".to_string()
    } else {
        feats.join(", ")
    };
    println!("cargo:rustc-env=DONSETCH_FEATURES={feats_display}");
}

fn target_pair(os: &str, arch: &str) -> &'static str {
    match (os, arch) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        // Android uses bblanchon's shared library (.so), NOT the
        // kognitos static archive (.a). The static archive is
        // glibc-targeted and cannot link on bionic (issue #16).
        ("android", "x86_64") => "android-x64",
        ("android", "aarch64") => "android-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("macos", "aarch64") => "mac-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        (o, a) => panic!("pdfium: unsupported target pair {o}-{a}"),
    }
}

/// Download the pinned PDFium release into `vendored` when missing.
///
/// Linux/macOS: static archive from kognitos/pdfium-static.
/// Windows: shared library (DLL + import lib) from bblanchon/pdfium-binaries.
///
/// Fails the build loudly rather than silently proceeding.
fn fetch_pdfium(os: &str, arch: &str, vendored: &Path) {
    let pair = target_pair(os, arch);
    let is_windows = os == "windows";
    let is_android = os == "android";
    let is_shared = is_windows || is_android;

    // Shared library (Windows DLL, Android .so): bblanchon/pdfium-binaries.
    // Static archive (Linux/macOS): kognitos/pdfium-static.
    let (url, tgz_name) = if is_shared {
        let tag = PDFIUM_SHARED_TAG;
        (
            format!(
                "https://github.com/bblanchon/pdfium-binaries/releases/download/{tag}/pdfium-{pair}.tgz"
            ),
            format!("pdfium-{pair}.tgz"),
        )
    } else {
        (
            format!(
                "https://github.com/kognitos/pdfium-static/releases/download/{PDFIUM_STATIC_TAG}/pdfium-{pair}-static.tgz"
            ),
            format!("pdfium-{pair}-static.tgz"),
        )
    };

    let pinned_hash = KNOWN_HASHES
        .iter()
        .find(|(p, _)| *p == pair)
        .map(|(_, h)| *h)
        .unwrap_or_else(|| {
            panic!(
                "pdfium: no pinned sha256 for {pair} — refusing unverified download. \
                Build from source with a vendored pdfium, or add an audited hash to KNOWN_HASHES in build.rs"
            )
        });

    let tgz = vendored.join(&tgz_name);
    let _ = fs::create_dir_all(vendored);

    eprintln!("donsetch build: fetching pdfium {pair} from {url}");
    let status = Command::new("curl")
        .args([
            "-fSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
            "-o",
        ])
        .arg(&tgz)
        .arg(&url)
        .status()
        .unwrap_or_else(|e| {
            panic!("pdfium: failed to spawn curl ({e}) — install curl: apt install curl")
        });
    if !status.success() {
        panic!("pdfium: curl download failed for {url}");
    }

    let mut f = fs::File::open(&tgz).expect("pdfium: cannot open downloaded tarball");
    let mut buf = Vec::with_capacity(8 * 1024 * 1024);
    f.read_to_end(&mut buf)
        .expect("pdfium: cannot read tarball");
    let got = sha256_hex(&buf);
    assert_eq!(
        got, pinned_hash,
        "pdfium: sha256 mismatch for {pair} (expected {pinned_hash}, got {got})"
    );

    let status = Command::new("tar")
        .arg("xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(vendored)
        .status()
        .unwrap_or_else(|e| {
            panic!("pdfium: failed to spawn tar ({e}) — install tar: apt install tar")
        });
    if !status.success() {
        panic!("pdfium: tar extraction failed for {tgz:?}");
    }
    let _ = fs::remove_file(&tgz);

    // bblanchon names the Windows import library pdfium.dll.lib, but
    // the MSVC linker (via cargo:rustc-link-lib=dylib=pdfium) looks for
    // pdfium.lib. Rename so the linker finds it.
    if is_windows {
        let old = vendored.join("lib").join("pdfium.dll.lib");
        let new = vendored.join("lib").join("pdfium.lib");
        if old.exists() && !new.exists() {
            let _ = fs::rename(&old, &new);
        }
    }
}

/// Pinned ONNX Runtime tarball info per platform.
/// Source: microsoft/onnxruntime official GitHub releases: v1.24.2 for
/// Linux/Windows (the prebuilt shared library; built against older
/// glibc than the pyke archive relink we used before, GLIBC_2.27 max
/// required vs 2.38, and free of `__isoc23_*`/2.38-only imports, so
/// OCR/rerank work on every distro from Ubuntu 18.04 onward), and
/// v1.23.2 for macOS (#316: the last Developer-ID-signed macOS build;
/// 1.24.2+ mac tarballs are ad-hoc signed and raise the minimum to
/// macOS 14, while 1.23.2 asks 13.4). SHA256 pins verified at download.
struct OnnxTarget {
    url: &'static str,
    sha256: &'static str,
    /// Path of the real shared library inside the archive.
    inner_lib: &'static str,
    /// File name we ship it as, next to the binary.
    shared_name: &'static str,
    /// How Microsoft packaged this target's release.
    archive: OnnxArchive,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OnnxArchive {
    /// `.tgz` (Linux): a gzip'd tar.
    Tgz,
    /// `.zip` (Windows).
    Zip,
}

/// Return ONNX target info if a prebuilt is available for this platform.
/// Every ONNX target uses runtime dlopen of the official prebuilt: Linux
/// x86_64 and Windows x64 since #298, both macOS arches since #316.
/// Linux aarch64 is deliberately absent (the official aarch64 prebuilt
/// deadlocks in the loader; see the NOTE in the match below).
fn onnx_target_info(os: &str, arch: &str) -> Option<OnnxTarget> {
    let (url, sha256, inner_lib, shared_name, archive) = match (os, arch) {
        ("linux", "x86_64") => (
            "https://github.com/microsoft/onnxruntime/releases/download/v1.24.2/onnxruntime-linux-x64-1.24.2.tgz",
            "43725474ba5663642e17684717946693850e2005efbd724ac72da278fead25e6",
            "onnxruntime-linux-x64-1.24.2/lib/libonnxruntime.so.1.24.2",
            "libonnxruntime.so",
            OnnxArchive::Tgz,
        ),
        // Windows x64: the same release, Microsoft's CPU-only build
        // (no DirectML import). Loaded at runtime like the Linux .so
        // (#277, #298); the exe no longer links ONNX statically.
        ("windows", "x86_64") => (
            "https://github.com/microsoft/onnxruntime/releases/download/v1.24.2/onnxruntime-win-x64-1.24.2.zip",
            "8e3e9c826375352e29cb2614fe44f3d7a4b0ff7b8028ad7a456af9d949a7e8b0",
            "onnxruntime-win-x64-1.24.2/lib/onnxruntime.dll",
            "onnxruntime.dll",
            OnnxArchive::Zip,
        ),
        // macOS (#316), both arches: Microsoft's last signed mac
        // builds. 1.24.2+ mac tarballs exist but are ad-hoc signed
        // only and raise the minimum to macOS 14; 1.23.2 is the last
        // Developer-ID-signed release and asks 13.4 (verified in
        // review: CMS blob, Microsoft Corporation UBF8T346G9). The
        // crate's api-23 floor serves it, the GetApi table is
        // append-only.
        ("macos", "aarch64") => (
            "https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-osx-arm64-1.23.2.tgz",
            "b4d513ab2b26f088c66891dbbc1408166708773d7cc4163de7bdca0e9bbb7856",
            "onnxruntime-osx-arm64-1.23.2/lib/libonnxruntime.1.23.2.dylib",
            "libonnxruntime.dylib",
            OnnxArchive::Tgz,
        ),
        ("macos", "x86_64") => (
            "https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-osx-x86_64-1.23.2.tgz",
            "d10359e16347b57d9959f7e80a225a5b4a66ed7d7e007274a15cae86836485a6",
            "onnxruntime-osx-x86_64-1.23.2/lib/libonnxruntime.1.23.2.dylib",
            "libonnxruntime.dylib",
            OnnxArchive::Tgz,
        ),
        // NOTE: onnxruntime-linux-aarch64-1.24.2 exists but dlopen'ing
        // it from this binary deadlocks inside the loader on native
        // aarch64 (proven by the CI payload probe timing out at its
        // exact 15s guard in the v3.4.2 arm64 experiment). Same
        // family as pykeio/ort #579. ARM64 therefore ships without
        // OCR/rerank (doctor reports "not compiled"; the honest
        // guard message explains it where relevant).
        _ => return None,
    };
    Some(OnnxTarget {
        url,
        sha256,
        inner_lib,
        shared_name,
        archive,
    })
}

/// Download the official ONNX Runtime prebuilt, verify its SHA256, and
/// copy it next to the binary for runtime dlopen. Used on every ONNX
/// target (Linux x86_64 and Windows x64 since #298; both macOS arches
/// since #316): the ort-sys static relink is gone, so builds no longer
/// need a working C toolchain or pay a 110MB static extraction.
fn fetch_onnx_prebuilt(info: OnnxTarget, manifest: &Path) {
    let vendored = manifest.join("vendor").join("onnx");
    let _ = fs::create_dir_all(&vendored);
    let shared_path = vendored.join(info.shared_name);

    // If the shared library already exists, skip the download. The
    // vendored dir is cleaned on `cargo clean`.
    if shared_path.exists() {
        eprintln!(
            "donsetch build: ONNX shared lib already present at {}",
            shared_path.display()
        );
        copy_onnx_shared_lib(&shared_path);
        return;
    }

    // 1. Download the archive.
    let tarball = vendored.join(match info.archive {
        OnnxArchive::Tgz => "onnx.tgz",
        OnnxArchive::Zip => "onnx.zip",
    });
    eprintln!("donsetch build: fetching ONNX Runtime from {}", info.url);
    let status = Command::new("curl")
        .args([
            "-fSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
            "-o",
        ])
        .arg(&tarball)
        .arg(info.url)
        .status()
        .unwrap_or_else(|e| panic!("ONNX: failed to spawn curl ({e})"));
    if !status.success() {
        panic!("ONNX: curl download failed for {}", info.url);
    }

    // 2. Verify SHA-256.
    let mut f = fs::File::open(&tarball).expect("ONNX: cannot open tarball");
    let mut buf = Vec::with_capacity(16 * 1024 * 1024);
    f.read_to_end(&mut buf).expect("ONNX: cannot read tarball");
    let got = sha256_hex(&buf);
    assert_eq!(
        got, info.sha256,
        "ONNX: sha256 mismatch (expected {}, got {})",
        info.sha256, got
    );

    // 3. Extract the shared library from the archive.
    let entry = match info.archive {
        OnnxArchive::Tgz => extract_tarball_entry(&buf, info.inner_lib),
        OnnxArchive::Zip => extract_zip_entry(&buf, info.inner_lib),
    }
    .unwrap_or_else(|| panic!("ONNX: {} not found in archive", info.inner_lib));
    fs::write(&shared_path, &entry).expect("ONNX: cannot write shared lib");

    // 4. Sanity: the file must be a plausible shared library of its
    // platform's format (ELF .so, PE DLL, Mach-O dylib).
    assert!(
        entry.len() > 10 * 1024 * 1024,
        "ONNX: extracted lib is implausibly small ({} bytes)",
        entry.len()
    );
    let magic_ok = match info.shared_name {
        "onnxruntime.dll" => &entry[..2] == b"MZ",
        // Little-endian 64-bit thin Mach-O in practice (`cf fa ed
        // fe`, both macOS dylibs checked); the other byte orders and
        // the fat magics match too rather than failing a valid
        // library over packaging drift.
        "libonnxruntime.dylib" => matches!(
            &entry[..4],
            [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
        ),
        _ => &entry[..4] == b"\x7fELF",
    };
    assert!(magic_ok, "ONNX: extracted file is not a shared library");

    let _ = fs::remove_file(&tarball);

    // 5. Copy to output directories.
    copy_onnx_shared_lib(&shared_path);
}

/// Extract one entry from a zip archive (Microsoft's Windows release).
fn extract_zip_entry(data: &[u8], wanted: &str) -> Option<Vec<u8>> {
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor).ok()?;
    let mut file = archive.by_name(wanted).ok()?;
    let mut out = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Extract one entry from a gzip-compressed tar archive (plain
/// gzip, unlike the removed pyke LZMA2 custom format).
fn extract_tarball_entry(data: &[u8], wanted: &str) -> Option<Vec<u8>> {
    use flate2::read::GzDecoder;
    let mut tar_bytes = Vec::new();
    GzDecoder::new(data).read_to_end(&mut tar_bytes).ok()?;
    extract_tar_entry(&tar_bytes, wanted)
}

/// Copy the ONNX shared library to all locations where the binary
/// and tests can find it at runtime.
fn copy_onnx_shared_lib(shared_path: &Path) {
    let out_dir = env::var("OUT_DIR").expect("no OUT_DIR");
    let out_path = PathBuf::from(&out_dir);
    let profile_dir = out_path
        .ancestors()
        .nth(3)
        .expect("cannot find profile dir from OUT_DIR");
    let dest_name = shared_path.file_name().unwrap();
    for dest in [
        profile_dir.join(dest_name),
        profile_dir.join("deps").join(dest_name),
        profile_dir.join("examples").join(dest_name),
        out_path.join(dest_name),
    ] {
        if !dest.exists() {
            let _ = fs::copy(shared_path, &dest);
        }
    }
}

fn extract_tar_entry(data: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut pos = 0;
    while pos + 512 <= data.len() {
        let header = &data[pos..pos + 512];
        // End-of-archive: two zero blocks.
        if header.iter().all(|&b| b == 0) {
            break;
        }
        // File name: bytes 0-99, null-terminated. Microsoft's macOS
        // tarballs store GNU-style `./`-prefixed member names; the
        // wanted paths are written without them.
        let entry_name = header.split(|&b| b == 0).next().unwrap_or(&[]);
        let entry_name = String::from_utf8_lossy(entry_name);
        let entry_name = entry_name.trim_start_matches("./");
        // File size: bytes 124-135, octal string.
        let size_str = header[124..136]
            .split(|&b| b == 0 || b == b' ')
            .next()
            .unwrap_or(&[]);
        let size_str = std::str::from_utf8(size_str).unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 8).unwrap_or(0);
        // Data starts at pos + 512.
        let data_start = pos + 512;
        let data_end = data_start + size;
        if data_end > data.len() {
            break;
        }
        if entry_name == name {
            return Some(data[data_start..data_end].to_vec());
        }
        // Next entry: data padded to 512-byte boundary.
        let padded = (size + 511) & !511;
        pos = data_start + padded;
    }
    None
}

/// Build a shared library from a static archive using the system linker.
/// Only called on Linux x86_64 (see onnx_target_info).
/// Uses --whole-archive + --allow-multiple-definition to handle
/// duplicate protobuf symbols in the ONNX archive, and -z noexecstack
/// to clear the executable stack flag (ONNX assembly objects lack
/// .note.GNU-stack, which defaults to execstack on Linux).
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

// Shared with the crate : see src/display_name.rs. Item position: include!
// cannot expand items inside a function body.
#[cfg(windows)]
include!("src/display_name.rs");

/// Publisher shown by Explorer, Task Manager and UAC elevation prompts.
///
/// The Win32 spec marks CompanyName as required, but there is no company or
/// publisher behind this project to claim, and an invented one would be worse
/// than none. Maintainer: set this to `Some("...")` -- a name or handle -- and
/// it is stamped into every Windows build; leave it `None` to omit the field.
#[cfg(windows)]
const COMPANY_NAME: Option<&str> = None;

/// Stamp a Windows version resource (`VERSIONINFO`) into the executable.
///
/// Without one the binary reports no publisher, description or version in
/// Explorer, Task Manager or `Get-Command`. Every field is derived from
/// `Cargo.toml`, so nothing has to be maintained alongside a release. Best
/// effort: a missing resource compiler downgrades to a warning rather than
/// failing the build.
///
/// `os` is the target: a Windows host can still be building for something else.
/// The `#[cfg(windows)]` gate is the host : see the `winresource` entry in
/// Cargo.toml.
#[cfg(windows)]
fn stamp_version_resource(os: &str) {
    if os != "windows" {
        return;
    }

    use winresource::{VersionInfo, WindowsResource};

    // `new()` already fills ProductName from the package name and both version
    // strings from the full package version.
    let mut res = WindowsResource::new();

    // FileDescription is a short label shown to users -- Task Manager treats it
    // as the app name -- so it takes the display title. The prose belongs in
    // Comments, which is specified as "additional information ... for
    // diagnostic purposes".
    res.set("FileDescription", DISPLAY_NAME);
    if let Some(company) = COMPANY_NAME {
        res.set("CompanyName", company);
    }
    if let Ok(description) = env::var("CARGO_PKG_DESCRIPTION")
        && !description.is_empty()
    {
        res.set("Comments", &description);
    }
    if let Ok(license) = env::var("CARGO_PKG_LICENSE")
        && !license.is_empty()
    {
        res.set("LegalCopyright", &license);
    }

    // The numeric FILEVERSION is four 16-bit words, so it carries only
    // MAJOR.MINOR.PATCH; the string field keeps the full version verbatim.
    // Anything trailing the numbers means this is not an upstream release:
    // rc/beta are pre-releases, anything else is a private build.
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let numeric = format!(
        "{}.{}.{}",
        env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_default(),
        env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_default(),
        env::var("CARGO_PKG_VERSION_PATCH").unwrap_or_default(),
    );
    let suffix = version.strip_prefix(&numeric).unwrap_or_default();
    if !suffix.is_empty() {
        let suffix = suffix.to_ascii_lowercase();
        if suffix.contains("rc") || suffix.contains("beta") {
            res.set_version_info(VersionInfo::FILEFLAGS, VersionInfo::VS_FF_PRERELEASE);
        } else {
            // VS_FF_PRIVATEBUILD requires the PrivateBuild string to be set.
            res.set("PrivateBuild", &version);
            res.set_version_info(VersionInfo::FILEFLAGS, VersionInfo::VS_FF_PRIVATEBUILD);
        }
    }

    if let Err(e) = res.compile() {
        println!("cargo:warning=skipping Windows version resource: {e}");
    }
}

/// No-op off Windows: `winresource` is a host-gated build-dependency, so it is
/// not even in the graph here.
#[cfg(not(windows))]
fn stamp_version_resource(_os: &str) {}
