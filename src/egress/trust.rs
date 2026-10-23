//! System / env trust-store inventory for `donsetch doctor`.
//!
//! The old transport's TLS module carried this; the wire is the engine
//! now, but the doctor line stays. Cheap: no connector build, no network.

/// (system roots, env-bundle roots). The env bundle is `SSL_CERT_FILE`
/// (or `SSL_CERT_DIR`) read as PEM; roots are counted by their
/// `BEGIN CERTIFICATE` blocks, which is all the doctor line needs.
pub fn trust_store_report() -> (usize, usize) {
    let sys = rustls_native_certs::load_native_certs().certs.len();
    (sys, env_root_count())
}

fn env_root_count() -> usize {
    if let Some(file) = std::env::var_os("SSL_CERT_FILE") {
        return count_blocks(std::path::Path::new(&file));
    }
    if let Some(dir) = std::env::var_os("SSL_CERT_DIR") {
        return std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|e| count_blocks(&e.path())).sum())
            .unwrap_or(0);
    }
    0
}

fn count_blocks(path: &std::path::Path) -> usize {
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    std::fs::read(path)
        .map(|bytes| bytes.windows(BEGIN.len()).filter(|w| *w == BEGIN).count())
        .unwrap_or(0)
}
