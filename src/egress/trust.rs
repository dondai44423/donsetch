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

/// `builder` trusting what the fetch engine trusts on top of reqwest's bundled
/// roots: the OS store (`SSL_CERT_FILE` / `SSL_CERT_DIR` included) and the
/// owner's `[tls] cert_file` / `cert_dir`, so an organization's TLS-inspection
/// root works for the search engines' own clients as it does for fetch. Read
/// once per process, on the first call, after the config is installed. A root
/// that does not parse is left out, never the client.
pub fn with_trusted_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    static ROOTS: LazyLock<Vec<reqwest::Certificate>> = LazyLock::new(|| {
        let tls = &crate::config::cfg().tls;
        let native = rustls_native_certs::load_native_certs().certs;
        usable_roots(
            native.iter().map(|cert| cert.as_ref()),
            &crate::fetch::layer::configured_roots(&tls.cert_file, &tls.cert_dir),
        )
    });
    ROOTS
        .iter()
        .cloned()
        .fold(builder, reqwest::ClientBuilder::add_root_certificate)
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
    // reqwest refuses to build a client over one root rustls cannot read, so
    // each is vetted here once; one probe covers the usual all-good set.
    if builds_with(&roots) {
        return roots;
    }
    roots
        .into_iter()
        .filter(|root| builds_with(std::slice::from_ref(root)))
        .collect()
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
}
