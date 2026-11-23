//! System / env trust-store inventory for `donsetch doctor`, and the same
//! extra roots for the product's own `reqwest` clients.
//!
//! The old transport's TLS module carried this; the wire is the engine
//! now, but the doctor line stays. Cheap: no connector build, no network.

use std::sync::LazyLock;

/// (system roots, env-bundle roots). The env bundle is `SSL_CERT_FILE`
/// (or `SSL_CERT_DIR`) read as PEM; roots are counted by their
/// `BEGIN CERTIFICATE` blocks, which is all the doctor line needs.
pub fn trust_store_report() -> (usize, usize) {
    let sys = rustls_native_certs::load_native_certs().certs.len();
    (sys, env_root_count())
}

fn env_root_count() -> usize {
    let mut total = 0usize;
    if let Some(file) = std::env::var_os("SSL_CERT_FILE").filter(|v| !v.is_empty()) {
        total += count_blocks(std::path::Path::new(&file));
    }
    if let Some(dirs) = std::env::var_os("SSL_CERT_DIR").filter(|v| !v.is_empty()) {
        for dir in std::env::split_paths(&dirs) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                total += entries
                    .flatten()
                    .map(|e| count_blocks(&e.path()))
                    .sum::<usize>();
            }
        }
    }
    total
}

/// Number of PEM certificate blocks readable from `[tls] cert_file` and
/// `[tls] cert_dir`. Cheap local read for `donsetch doctor`.
pub fn configured_root_count() -> usize {
    let tls = &crate::config::cfg().tls;
    crate::fetch::layer::configured_roots(&tls.cert_file, &tls.cert_dir)
        .iter()
        .map(|bytes| count_blocks_in_bytes(bytes))
        .sum()
}

fn count_blocks(path: &std::path::Path) -> usize {
    std::fs::read(path)
        .map(|bytes| count_blocks_in_bytes(&bytes))
        .unwrap_or(0)
}

fn count_blocks_in_bytes(bytes: &[u8]) -> usize {
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    bytes.windows(BEGIN.len()).filter(|w| *w == BEGIN).count()
}

fn trusted_roots() -> &'static [reqwest::Certificate] {
    static ROOTS: LazyLock<Vec<reqwest::Certificate>> = LazyLock::new(|| {
        let tls = &crate::config::cfg().tls;
        let native = rustls_native_certs::load_native_certs().certs;
        usable_roots(
            native.iter().map(|cert| cert.as_ref()),
            &crate::fetch::layer::configured_roots(&tls.cert_file, &tls.cert_dir),
        )
    });
    &ROOTS
}

/// `builder` trusting what the fetch engine trusts on top of reqwest's bundled
/// roots: the OS store (`SSL_CERT_FILE` / `SSL_CERT_DIR` included) and the
/// owner's `[tls] cert_file` / `cert_dir`, so an organization's TLS-inspection
/// root works for every outbound `reqwest` client as it does for fetch. Read
/// once per process, on the first call, after the config is installed. A root
/// that does not parse is left out, never the client.
pub fn with_trusted_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    trusted_roots()
        .iter()
        .cloned()
        .fold(builder, reqwest::ClientBuilder::add_root_certificate)
}

/// Blocking-client counterpart to [`with_trusted_roots`] (used by OCR model
/// downloads on a dedicated thread).
pub fn with_trusted_roots_blocking(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    trusted_roots().iter().cloned().fold(
        builder,
        reqwest::blocking::ClientBuilder::add_root_certificate,
    )
}

fn usable_roots<'a>(
    der: impl Iterator<Item = &'a [u8]>,
    pem_bundles: &[Vec<u8>],
) -> Vec<reqwest::Certificate> {
    let mut roots: Vec<reqwest::Certificate> = der
        .filter_map(|der| reqwest::Certificate::from_der(der).ok())
        .collect();
    for pem in pem_bundles {
        match reqwest::Certificate::from_pem_bundle(pem) {
            Ok(certs) if !certs.is_empty() => roots.extend(certs),
            _ => crate::diag!("[tls] a configured CA bundle is not PEM certificates; left out"),
        }
    }
    vet_roots(roots)
}

// reqwest refuses to build a client over one root rustls cannot read. One probe
// covers the usual all-good set; divide-and-conquer isolates bad OS/bundle
// roots in O(k log n) probes instead of O(n).
fn vet_roots(roots: Vec<reqwest::Certificate>) -> Vec<reqwest::Certificate> {
    if roots.is_empty() || builds_with(&roots) {
        return roots;
    }
    if roots.len() == 1 {
        return Vec::new();
    }
    let mid = roots.len() / 2;
    let mut left = roots;
    let right = left.split_off(mid);
    let mut kept = vet_roots(left);
    kept.extend(vet_roots(right));
    kept
}

fn builds_with(roots: &[reqwest::Certificate]) -> bool {
    let probe = reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .no_proxy();
    roots
        .iter()
        .cloned()
        .fold(probe, reqwest::ClientBuilder::add_root_certificate)
        .build()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_roots_keep_good_certificates_and_skip_the_rest() {
        let good = rcgen::generate_simple_self_signed(vec!["corp.example".into()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["other.example".into()]).unwrap();
        // Valid PEM and base64, but the DER inside is not a certificate.
        let not_a_cert =
            b"-----BEGIN CERTIFICATE-----\nAAECAwQFBgcICQ==\n-----END CERTIFICATE-----\n".to_vec();
        let roots = usable_roots(
            [other.cert.der().as_ref(), b"garbage DER".as_slice()].into_iter(),
            &[
                good.cert.pem().into_bytes(),
                b"not a PEM file".to_vec(),
                not_a_cert,
            ],
        );
        assert_eq!(
            roots.len(),
            2,
            "the DER root and the PEM root, nothing else"
        );
        assert!(builds_with(&roots));
        assert!(usable_roots(std::iter::empty(), &[]).is_empty());
    }

    #[test]
    fn vet_roots_binary_split_isolates_multiple_bad_roots() {
        let g1 = rcgen::generate_simple_self_signed(vec!["a.example".into()]).unwrap();
        let g2 = rcgen::generate_simple_self_signed(vec!["b.example".into()]).unwrap();
        let g3 = rcgen::generate_simple_self_signed(vec!["c.example".into()]).unwrap();
        let bad =
            b"-----BEGIN CERTIFICATE-----\nAAECAwQFBgcICQ==\n-----END CERTIFICATE-----\n".to_vec();
        let roots = usable_roots(
            [g1.cert.der().as_ref(), g2.cert.der().as_ref()].into_iter(),
            &[bad.clone(), g3.cert.pem().into_bytes(), bad],
        );
        assert_eq!(roots.len(), 3);
        assert!(builds_with(&roots));
    }

    #[test]
    fn env_root_count_splits_multi_dir_and_ignores_empty_file() {
        let base = crate::paths::cache_dir().join("trust-env-multi");
        let dir_a = base.join("a");
        let dir_b = base.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let c1 = rcgen::generate_simple_self_signed(vec!["a.example".into()]).unwrap();
        let c2 = rcgen::generate_simple_self_signed(vec!["b.example".into()]).unwrap();
        std::fs::write(dir_a.join("a.pem"), c1.cert.pem()).unwrap();
        std::fs::write(dir_b.join("b.pem"), c2.cert.pem()).unwrap();
        let joined = std::env::join_paths([&dir_a, &dir_b]).unwrap();
        unsafe {
            std::env::set_var("SSL_CERT_FILE", "");
            std::env::set_var("SSL_CERT_DIR", &joined);
        }
        assert_eq!(env_root_count(), 2);
    }
}
