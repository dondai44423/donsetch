//! CPU feature detection with disk-cached persistence.
//!
//! AVX support is checked once via CPUID and cached permanently if
//! present. If absent, re-checked on each process start (the user
//! may upgrade their CPU). Once AVX is confirmed, the cache file
//! records `"avx":true` and no CPUID check is ever performed again.
//!
//! This used to gate ONNX Runtime loading: pyke's prebuilt ONNX
//! static archive contains unguarded AVX instructions in its C++
//! global constructors, and statically linking it causes SIGILL on
//! non-AVX CPUs at process start. The dlopen targets ship Microsoft's
//! own build instead, which dispatches at runtime and runs without
//! AVX (see src/onnx.rs, "Why there is no AVX gate"), so the result
//! is a doctor diagnostic now and gates nothing.

#[cfg(target_arch = "x86_64")]
use std::path::PathBuf;
#[cfg(target_arch = "x86_64")]
use std::sync::OnceLock;

/// Returns true if the CPU supports AVX (or is non-x86, which
/// always passes since the AVX problem only exists on x86_64).
///
/// Persistence model:
/// - AVX=yes: cached permanently in `cache_dir()/avx.json`,
///   never re-checked.
/// - AVX=no: NOT cached permanently. Re-checked via CPUID on
///   every process start, in case the user upgrades to an AVX
///   CPU. Once found, it becomes permanent.
pub fn has_avx() -> bool {
    // Non-x86_64: always available (ARM, etc. have no AVX concept).
    #[cfg(not(target_arch = "x86_64"))]
    {
        true
    }

    #[cfg(target_arch = "x86_64")]
    {
        static CACHED: OnceLock<bool> = OnceLock::new();
        *CACHED.get_or_init(|| {
            // 1. Check disk cache for permanent "yes".
            if let Some(true) = read_cache() {
                return true;
            }

            // 2. CPUID check (sub-microsecond).
            let has = std::is_x86_feature_detected!("avx");

            // 3. Cache permanently only if AVX is present.
            // If absent, don't write permanent cache; the next
            // process start will re-check via CPUID.
            if has {
                write_cache(true);
            }

            has
        })
    }
}

#[cfg(target_arch = "x86_64")]
fn cache_path() -> PathBuf {
    crate::paths::cache_dir().join("avx.json")
}

#[cfg(target_arch = "x86_64")]
fn read_cache() -> Option<bool> {
    let data = std::fs::read_to_string(cache_path()).ok()?;
    // Simple JSON-ish parse: look for "avx":true
    if data.contains("\"avx\":true") || data.contains("\"avx\": true") {
        Some(true)
    } else {
        None // "avx":false or corrupt -> treat as no cache
    }
}

#[cfg(target_arch = "x86_64")]
fn write_cache(avx: bool) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = format!("{{\"avx\":{avx}}}");
    let _ = std::fs::write(path, data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_avx_returns_bool() {
        let _ = has_avx();
    }
}
