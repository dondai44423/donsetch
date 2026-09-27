//! The contract of `crate::sandbox()` (#315): once an integration
//! test has called it, the layered config has no file layer and every
//! stateful path hangs off a private cache root, so the developer's
//! own `donsetch.toml` and cache stay out of the test.

use std::path::PathBuf;

// Plants a `donsetch.toml` at the default location under a private
// XDG_CONFIG_HOME and checks it never reaches the layered config. On
// Linux `dirs::config_dir()` honours the variable, so the first
// assertion is a real regression check on a clean box; on Windows and
// macOS the platform dir is fixed, so there the planted file is simply
// ignored and the first half proves nothing unless the developer's
// real file is present (then it is the real check). Depends on
// nextest's one process per test: `config::cfg()` is a process-wide
// OnceLock and the sandbox has to run before its first call.
#[test]
fn the_sandbox_keeps_the_default_config_location_out_of_the_layered_config() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let home = std::env::temp_dir().join(format!("donsetch-it-cfg-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(home.join("donsetch")).unwrap();
    std::fs::write(
        home.join("donsetch").join("donsetch.toml"),
        "[fetch]\ndns_cache_ttl_secs = 4242\n",
    )
    .unwrap();
    let saved_xdg = std::env::var_os("XDG_CONFIG_HOME");
    // SAFETY: test-only env mutation; nextest runs this test alone in
    // its own process, before any thread exists.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &home);
        std::env::remove_var("DONSETCH_NO_CONFIG_FILE");
    }

    let root = crate::sandbox();
    let cfg = donsetch::config::cfg();
    let seen = cfg.fetch.dns_cache_ttl_secs;

    unsafe {
        match saved_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&home);

    assert_ne!(
        seen, 4242,
        "the planted default-location donsetch.toml reached the layered config"
    );
    assert_eq!(
        std::env::var("DONSETCH_NO_CONFIG_FILE").as_deref(),
        Ok("1"),
        "the sandbox drops the config file layer"
    );
    assert!(
        std::env::var_os("DONSETCH_CONFIG").is_none(),
        "NO_CONFIG_FILE together with DONSETCH_CONFIG is a hard error; the sandbox clears the path"
    );
    assert_eq!(
        std::env::var_os("DONSETCH_CACHE_DIR")
            .map(PathBuf::from)
            .as_ref(),
        Some(&root),
        "every stateful path hangs off the sandbox root"
    );
    assert_eq!(donsetch::paths::cache_dir(), root);
    assert!(
        root.is_dir(),
        "the root exists before the test touches state"
    );
}
