//! Test-only process-wide sandboxing (`cfg(test)` builds only).
//!
//! `Fetcher::new` points the engine's state plane (jar / cache / learned
//! hosts) at a per-process temp dir, so unit tests never read or write the
//! operator's real `~/.cache/doncore` state. Integration tests have their
//! own sandbox (`tests/it`); this covers the lib's own test binary, where
//! nextest runs one test per process.
//!
//! The override is `DONCORE_STATE_DIR` — the engine's own knob — NOT
//! `DONSETCH_CACHE_DIR`: the app re-reads the latter on every call
//! (`paths::cache_dir`, "tests set it late"), so setting it here would
//! hijack the app's whole cache root mid-test and break tests that manage
//! their own root (resume tokens, vault). An explicit override set by the
//! test itself always wins.

use std::sync::OnceLock;

static ONCE: OnceLock<()> = OnceLock::new();

/// The engine's state root for this test process. Returns the chosen dir
/// (also useful for assertions/cleanup in a single test).
pub(crate) fn sandbox_state_dir() -> &'static std::path::Path {
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let explicit = ["DONCORE_STATE_DIR", "DONSETCH_CACHE_DIR"]
            .into_iter()
            .find_map(|var| std::env::var_os(var).filter(|v| !v.is_empty()));
        let dir = match explicit {
            Some(d) => std::path::PathBuf::from(d),
            None => {
                let dir = std::env::temp_dir()
                    .join(format!("donsetch-test-state-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&dir);
                // SAFETY: test-only process-wide env mutation; nextest gives
                // each test its own process, and only the engine reads this
                // variable.
                unsafe { std::env::set_var("DONCORE_STATE_DIR", &dir) };
                dir
            }
        };
        let _ = DIR.set(dir);
    });
    DIR.get().expect("sandbox dir set")
}
