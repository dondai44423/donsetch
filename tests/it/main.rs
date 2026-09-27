//! The integration tests, as one crate.
//!
//! Every file directly under `tests/` is its own crate, so each one
//! linked the whole library and its dependency tree into a separate
//! binary, and re-generated the generic code it used from the library.
//! As modules of this one crate they share a single link. nextest still
//! runs every test in its own process, so the per-test isolation the
//! `DONSETCH_CACHE_DIR` tests rely on is unchanged.

/// The per-test state sandbox every in-process module calls first.
///
/// Returns the private cache root. After the call the layered config
/// has no file layer and every stateful path hangs off that root, so
/// nothing a test does reads or writes the developer's own
/// `donsetch.toml` or cache (#315; the unit-test defaults from #299
/// and #312 do not reach this crate, which links the lib without
/// `cfg(test)`).
///
/// Env-based by necessity: the lib reads `DONSETCH_NO_CONFIG_FILE` and
/// `DONSETCH_CACHE_DIR`, and the config layer is a process-wide
/// `OnceLock`, so this must run before the first `config::cfg()` call.
/// That is safe only under nextest's one process per test; under
/// libtest the variables would be shared across threads.
pub(crate) fn sandbox() -> std::path::PathBuf {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        // An explicit cache override wins, as it does for the unit
        // tests: a developer who keeps all dev state under one root
        // keeps it. The config file layer is always dropped, and the
        // explicit path is cleared first: NO_CONFIG_FILE together
        // with DONSETCH_CONFIG is a hard error by design.
        // SAFETY: test-only mutation of the process env, before the
        // test spawns threads or touches the lib (see above).
        unsafe {
            std::env::remove_var("DONSETCH_CONFIG");
            std::env::set_var("DONSETCH_NO_CONFIG_FILE", "1");
        }
        if let Some(d) = std::env::var_os("DONSETCH_CACHE_DIR").filter(|v| !v.is_empty()) {
            return std::path::PathBuf::from(d);
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("donsetch-test")
            .join(format!("it-{}-{nanos}", std::process::id()))
            .join("cache");
        let _ = std::fs::create_dir_all(&dir);
        extern "C" fn sweep() {
            if let Some(root) = ROOT.get()
                && let Some(parent) = root.parent()
            {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
        // SAFETY: registering a plain extern "C" fn with no arguments.
        unsafe {
            libc::atexit(sweep);
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        dir
    })
    .clone()
}

mod auth_login;
mod bypass_live;
mod crawl_fresh_fetch;
mod daemon_boot_stays_alive;
mod egress_proxy;
mod mcp_tool_call_does_not_abort;
mod request_class;
mod revalidate_redirect;
mod secure_cookie_leak;
mod soak;
mod state_sandbox;
mod token_invariants;
