//! BoringSSL transport configured from a BrowserProfile.
//!
//! Chrome-true, not Chrome-like: we drive Chrome's own TLS library with its
//! native Chrome behaviors on (GREASE, extension permutation, ECH-GREASE,
//! ALPS, brotli cert compression), configured from live-captured Chrome data.

use boring::ssl::{Ssl, SslConnector, SslMethod, SslSession, SslVersion};
use boring::x509::X509;
use foreign_types::ForeignType;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;
use tokio_boring::{SslStream, SslStreamBuilder};

use crate::error::FetchError;
use crate::profile::BrowserProfile;

/// Per-origin TLS session-ticket store (Chrome resumes sessions; so do we).
pub type SessionStore = Arc<Mutex<HashMap<String, SslSession>>>;

pub fn new_session_store() -> SessionStore {
    Arc::new(Mutex::new(HashMap::new()))
}

fn tls_err<E: std::fmt::Display>(e: E) -> FetchError {
    FetchError::Tls(e.to_string())
}

/// Brotli certificate decompression (TLS compress_certificate, alg id 2).
/// Client-side: servers may send brotli-compressed certificates.
unsafe extern "C" fn cert_decompress_brotli(
    _ssl: *mut boring_sys::SSL,
    out: *mut *mut boring_sys::CRYPTO_BUFFER,
    uncompressed_len: usize,
    input: *const u8,
    in_len: usize,
) -> std::os::raw::c_int {
    unsafe {
        // The length is server-declared: a hostile origin can ask for
        // gigabytes and this callback would reserve them upfront. Real
        // chains never come close to 16 MiB (Chrome bounds the same
        // way); refuse anything larger.
        if uncompressed_len > 16 << 20 {
            return 0;
        }
        let compressed = std::slice::from_raw_parts(input, in_len);
        let mut decompressed = Vec::with_capacity(uncompressed_len);
        if std::io::Read::read_to_end(
            &mut brotli::Decompressor::new(compressed, 1 << 20),
            &mut decompressed,
        )
        .is_err()
        {
            return 0;
        }
        if decompressed.len() != uncompressed_len {
            return 0;
        }
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let buf = boring_sys::CRYPTO_BUFFER_alloc(&mut data_ptr, uncompressed_len);
        if buf.is_null() || data_ptr.is_null() {
            return 0;
        }
        std::ptr::copy_nonoverlapping(decompressed.as_ptr(), data_ptr, uncompressed_len);
        *out = buf;
        1
    }
}

/// Chrome's ALPS payload for h2: the same SETTINGS it sends on the wire.
fn alps_h2_payload(profile: &BrowserProfile) -> Vec<u8> {
    let h2 = &profile.h2;
    let mut v = Vec::with_capacity(24);
    for (id, val) in [
        (0x1u16, h2.header_table_size),
        (0x2, h2.enable_push),
        (0x4, h2.initial_window_size),
        (0x6, h2.max_header_list_size),
    ] {
        v.extend_from_slice(&id.to_be_bytes());
        v.extend_from_slice(&val.to_be_bytes());
    }
    v
}

/// Which Chrome behaviors a connector puts on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandshakeProfile {
    /// Chrome-true: GREASE, extension permutation, ECH-GREASE, ALPS,
    /// brotli cert compression, OCSP/SCT requests. Matches a live
    /// Chrome 151 byte-for-byte on the extension record.
    ChromeTrue,
    /// Interception-compatible: same cipher/curve/sigalg/ALPN core but
    /// none of the exotic extensions. TLS-intercepting egress proxies
    /// (corporate MITM, cloud sandboxes) re-terminate the handshake
    /// with a second, often less lenient stack, and some of them reset
    /// connections whose ClientHello carries GREASE/ALPS/compress_cert.
    /// Stealth is moot behind a MITM anyway: the proxy sees plaintext.
    InterceptionSafe,
}

pub fn build_connector(profile: &BrowserProfile) -> Result<SslConnector, FetchError> {
    build_connector_with(profile, HandshakeProfile::ChromeTrue)
}

/// Same as `build_connector` but with the interception-safe wire profile.
pub fn build_connector_compat(profile: &BrowserProfile) -> Result<SslConnector, FetchError> {
    build_connector_with(profile, HandshakeProfile::InterceptionSafe)
}

fn build_connector_with(
    profile: &BrowserProfile,
    handshake: HandshakeProfile,
) -> Result<SslConnector, FetchError> {
    let mut b = SslConnector::builder(SslMethod::tls()).map_err(tls_err)?;
    b.set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(tls_err)?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))
        .map_err(tls_err)?;
    b.set_cipher_list(profile.tls.ciphers_12).map_err(tls_err)?;
    b.set_curves_list(profile.tls.groups).map_err(tls_err)?;
    b.set_sigalgs_list(profile.tls.sigalgs).map_err(tls_err)?;
    b.set_alpn_protos(profile.tls.alpn).map_err(tls_err)?;
    if handshake == HandshakeProfile::ChromeTrue {
        b.set_grease_enabled(true);
        b.set_permute_extensions(true);
    } else {
        b.set_grease_enabled(false);
        b.set_permute_extensions(false);
    }

    // Session storage lives in connect(): tickets are
    // egress-scoped there (a proxy's ticket must never
    // resume from the direct IP or another proxy : that
    // would link the lanes at the edge).

    if handshake == HandshakeProfile::ChromeTrue {
        // OCSP stapling request (status_request extension), like Chrome.
        unsafe { boring_sys::SSL_CTX_enable_ocsp_stapling(b.as_ptr()) };
        // SCT requests (signed_certificate_timestamp extension), like Chrome.
        unsafe { boring_sys::SSL_CTX_enable_signed_cert_timestamps(b.as_ptr()) };

        // Brotli certificate compression (compress_certificate ext, alg 2).
        // Client direction: compress = NULL (never used), real brotli decompress.
        let rc = unsafe {
            boring_sys::SSL_CTX_add_cert_compression_alg(
                b.as_ptr(),
                2,
                None,
                Some(cert_decompress_brotli),
            )
        };
        if rc != 1 {
            return Err(FetchError::Tls(
                "cert compression registration failed".into(),
            ));
        }
    }

    // Platform-native root store (Chrome uses the OS trust store; so do we).
    let roots = rustls_native_certs::load_native_certs();
    let mut loaded = 0usize;
    for cert in roots.certs {
        if let Ok(x) = X509::from_der(cert.as_ref())
            && b.cert_store_mut().add_cert(x).is_ok()
        {
            loaded += 1;
        }
    }
    // Plus the environment bundle: SSL_CERT_FILE/SSL_CERT_DIR.
    // Some rustls-native-certs builds honor it, some platforms do
    // not; loading it explicitly is deterministic everywhere. In a
    // TLS-interception network this bundle is the ONLY thing that
    // makes re-signed certificates verifiable.
    for cert in load_env_roots() {
        if b.cert_store_mut().add_cert(cert).is_ok() {
            loaded += 1;
        }
    }
    if loaded == 0 {
        return Err(FetchError::Tls("no root certs loaded".into()));
    }

    Ok(b.build())
}

/// Same wire behaviors as `build_connector_with(ChromeTrue)` but surfaced as
/// a raw `SslContextBuilder` for transports that own their own handshake glue
/// (the h3 stack runs quiche over this Chrome-true builder). Differences:
/// TLS 1.3 only (the QUIC spec allows only TLS 1.3), ALPN `h3` only, and no
/// in-boring session callbacks: the QUIC session store is persistence-managed
/// at the h3 layer via quiche's serialized SSL_SESSION bytes.
pub fn build_quic_ctx_builder(
    profile: &BrowserProfile,
) -> Result<boring::ssl::SslContextBuilder, FetchError> {
    use boring::ssl::SslContext;
    let mut b = SslContext::builder(SslMethod::tls()).map_err(tls_err)?;
    b.set_min_proto_version(Some(SslVersion::TLS1_3))
        .map_err(tls_err)?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))
        .map_err(tls_err)?;
    // TLS 1.2 cipher list is inert on a 1.3-only handshake but as long as
    // the profile stays the single source of truth it remains harmless.
    b.set_cipher_list(profile.tls.ciphers_12).map_err(tls_err)?;
    b.set_curves_list(profile.tls.groups).map_err(tls_err)?;
    b.set_sigalgs_list(profile.tls.sigalgs).map_err(tls_err)?;
    // The ClientHello with one QUIC protocol carries exactly one ALPN: h3.
    b.set_alpn_protos(b"\x02h3").map_err(tls_err)?;
    b.set_grease_enabled(true);
    b.set_permute_extensions(true);

    // OCSP stapling + SCT + brotli cert compression: Chrome sends all of
    // these extensions on QUIC too, so the QUIC ClientHello uses the same
    // request set as our h2 handshake.
    unsafe { boring_sys::SSL_CTX_enable_ocsp_stapling(b.as_ptr()) };
    unsafe { boring_sys::SSL_CTX_enable_signed_cert_timestamps(b.as_ptr()) };
    let rc = unsafe {
        boring_sys::SSL_CTX_add_cert_compression_alg(
            b.as_ptr(),
            2,
            None,
            Some(cert_decompress_brotli),
        )
    };
    if rc != 1 {
        return Err(FetchError::Tls(
            "cert compression registration failed".into(),
        ));
    }

    // Platform-native root store (same trust semantics as h1/h2).
    let roots = rustls_native_certs::load_native_certs();
    let mut loaded = 0usize;
    for cert in roots.certs {
        if let Ok(x) = X509::from_der(cert.as_ref())
            && b.cert_store_mut().add_cert(x).is_ok()
        {
            loaded += 1;
        }
    }
    for cert in load_env_roots() {
        if b.cert_store_mut().add_cert(cert).is_ok() {
            loaded += 1;
        }
    }
    if loaded == 0 {
        return Err(FetchError::Tls("no root certs loaded".into()));
    }
    Ok(b)
}

/// Roots loaded from the SSL_CERT_FILE / SSL_CERT_DIR environment.
/// Read in DER or PEM (single cert or bundle); unreadable entries
/// are skipped silently, matching libcurl. In a TLS-intercepting
/// network this is the bundle that makes re-signed server certs
/// verifiable, so it is appended to the platform store on every
/// connector build.
fn load_env_roots() -> Vec<X509> {
    // The bundle is re-read on every connector build (every fetch);
    // cache it keyed on the observed path identity (size + mtime for
    // files, mtime for dirs). A changed or removed bundle invalidates
    // the cache; a fresh process sees env changes immediately.
    static CACHE: std::sync::OnceLock<std::sync::Mutex<EnvRootsCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(EnvRootsCache::default()));
    let Ok(mut cache) = cache.lock() else {
        return Vec::new();
    };
    let key = env_roots_fingerprint();
    if cache.key == Some(key.clone()) {
        return cache.roots.clone();
    }
    let roots = read_env_roots();
    cache.key = Some(key);
    cache.roots = roots.clone();
    roots
}

#[derive(Default)]
struct EnvRootsCache {
    key: Option<Vec<(std::path::PathBuf, u64, u64)>>,
    roots: Vec<X509>,
}

/// Identity of the env bundle: (path, len, mtime) per entry; dirs are
/// keyed by (path, 0, mtime-of-dir). Parse errors keep the old parse:
/// content changes that keep identity are not worth rescanning for.
fn env_roots_fingerprint() -> Vec<(std::path::PathBuf, u64, u64)> {
    let mut fp = Vec::new();
    for path in env_root_paths() {
        match std::fs::metadata(&path) {
            Ok(md) => {
                let len = if path.is_dir() { 0 } else { md.len() };
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                fp.push((path, len, mtime));
            }
            Err(_) => fp.push((path, u64::MAX, u64::MAX)),
        }
    }
    fp
}

fn env_root_paths() -> Vec<std::path::PathBuf> {
    std::env::vars_os()
        .filter_map(|(k, v)| (k == "SSL_CERT_FILE" || k == "SSL_CERT_DIR").then_some(v))
        .map(std::path::PathBuf::from)
        .collect()
}

fn read_env_roots() -> Vec<X509> {
    let mut out = Vec::new();
    for path in env_root_paths() {
        let entries: Vec<std::path::PathBuf> = if path.is_dir() {
            let Ok(rd) = std::fs::read_dir(&path) else {
                continue;
            };
            rd.filter_map(|e| e.ok().map(|e| e.path())).collect()
        } else {
            vec![path]
        };
        for file in entries {
            if let Ok(bytes) = std::fs::read(&file) {
                if let Ok(x) = X509::from_der(&bytes) {
                    out.push(x);
                    continue;
                }
                for pem in pem_certs(&bytes) {
                    if let Ok(x) = X509::from_pem(pem) {
                        out.push(x);
                    }
                }
            }
        }
    }
    out
}

/// Split a byte buffer into PEM cert block text (including the
/// BEGIN/END armor) so bundles with many certs all load.
fn pem_certs(bytes: &[u8]) -> Vec<&[u8]> {
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    const END: &[u8] = b"-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(start) = bytes[search_from..]
        .windows(BEGIN.len())
        .position(|w| w == BEGIN)
        .map(|p| p + search_from)
    {
        let after_start = start + BEGIN.len();
        let Some(rel_end) = bytes[after_start..]
            .windows(END.len())
            .position(|w| w == END)
        else {
            break;
        };
        let end = after_start + rel_end + END.len();
        out.push(&bytes[start..end]);
        search_from = end;
    }
    out
}

/// Handshake. Applies per-connection profile bits (ECH-GREASE, ALPS),
/// resumes a cached session when the origin gave us a ticket, then connects.
/// `handshake` controls whether the exotic Chrome extensions ride along
/// (they must stay off for MITM interception compat; see build_connector).
pub async fn connect(
    profile: &BrowserProfile,
    connector: &SslConnector,
    domain: &str,
    tcp: TcpStream,
    sessions: &SessionStore,
    session_key: &str,
    handshake: HandshakeProfile,
) -> Result<SslStream<TcpStream>, FetchError> {
    let mut ssl: Ssl = connector
        .configure()
        .map_err(tls_err)?
        .into_ssl(domain)
        .map_err(tls_err)?;

    // Session resumption (ticket from a previous visit to this origin).
    if let Ok(guard) = sessions.lock()
        && let Some(session) = guard.get(session_key)
    {
        // Safe: session belongs to this client ctx; stale ticket just
        // falls back to a full handshake.
        let _ = unsafe { ssl.set_session(session) };
    }

    if handshake == HandshakeProfile::ChromeTrue {
        // ECH-GREASE (encrypted_client_hello extension), like Chrome.
        ssl.set_enable_ech_grease(true);

        // ALPS (application_settings extension) with Chrome's h2 settings payload.
        let alps = alps_h2_payload(profile);
        let rc = unsafe {
            boring_sys::SSL_add_application_settings(
                ssl.as_ptr(),
                b"h2".as_ptr(),
                2,
                alps.as_ptr(),
                alps.len(),
            )
        };
        if rc != 1 {
            return Err(FetchError::Tls("ALPS registration failed".into()));
        }
    }

    let stream = SslStreamBuilder::new(ssl, tcp)
        .connect()
        .await
        .map_err(|e| FetchError::Tls(classify_handshake_error(&e)))?;

    // Chrome caches session tickets aggressively : so do
    // we, but EGRESS-SCOPED (session_key carries the
    // proxy id when proxied; see fetch/client.rs).
    if let Some(sess) = stream.ssl().session() {
        let mut store = sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if store.len() >= 512 {
            store.clear(); // sessions are short-lived; wipe + refill
        }
        store.insert(session_key.to_string(), sess.to_owned());
    }
    Ok(stream)
}

/// Turn a handshake failure into a short, actionable message. The
/// raw Debug dump of a MidHandshakeSslStream terrifies users and
/// hides the actual cause behind struct fields; boring's Display
/// error stack is short and greppable, so classification runs on
/// that plus the io error chain.
pub fn classify_handshake_error<E: std::error::Error>(e: &E) -> String {
    const MANY_IO: [&str; 6] = [
        "ConnectionReset",
        "connection reset by peer",
        "Connection reset",
        "ConnectionRefused",
        "connection refused",
        "ECONNRESET",
    ];
    const MANY_EOF: [&str; 5] = [
        "stream closed",
        "UnexpectedEof",
        "unexpected EOF",
        "SYSCALL",
        "ECONNRESET",
    ];
    let text = e.to_string();
    let low = text.to_lowercase();

    if MANY_IO.iter().any(|m| text.contains(m)) || low.contains("os error 104") {
        return format!(
            "TLS handshake aborted (connection reset or cut mid-negotiation). {}",
            EGRESS_HINT
        );
    }
    if MANY_EOF.iter().any(|m| text.contains(m)) || low.contains("os error 54") {
        return format!(
            "TLS handshake cut short (peer closed the connection). {}",
            EGRESS_HINT
        );
    }
    if low.contains("certificate verify failed")
        || low.contains("verification failed")
        || low.contains("hostname mismatch")
        || low.contains("unknown ca")
        || low.contains("unknown issuer")
        || low.contains("unable to get local issuer")
        || low.contains("self-signed")
        || low.contains("invalid certificate verification")
    {
        return format!("TLS certificate verification failed. {}", CERT_HINT);
    }
    if low.contains("certificate_unknown") || low.contains("tlsv1") || low.contains("no protocols")
    {
        return "TLS handshake failed (protocol/cipher negotiation rejected by the peer)".into();
    }
    text
}

/// What to tell a user when the transport dies mid-handshake: the
/// two situations that produce this are an egress filter killing
/// direct HTTPS (use the env proxy convention, opt out with the
/// kill switch) or an interception proxy that dislikes the exotic
/// Chrome ClientHello (handled automatically when proxied).
const EGRESS_HINT: &str = "The egress path is killing HTTPS out of band: if this machine or sandbox routes egress through an HTTP(S) proxy, export HTTPS_PROXY/HTTP_PROXY (donsetch honors them, NO_PROXY accepted, DONSETCH_NO_ENV_PROXY=1 disables) and point SSL_CERT_FILE at the interception CA bundle. Run `donsetch doctor` for a live egress diagnosis.";

/// Trust-store failure: the interception proxy re-signs every cert,
/// which verifies only against the bundle the network operator gave
/// out (SSL_CERT_FILE). Direct connections to real hosts fail when
/// the CA that signed them isn't in the platform store either.
const CERT_HINT: &str = "The presentation cert chain was not issued by any trusted root. In a TLS-intercepting network the proxy re-signs certificates with its own CA: export SSL_CERT_FILE (or SSL_CERT_DIR) pointing at that bundle so donsetch trusts it, exactly like curl/openssl do. `donsetch doctor` reports both stores.";

/// Trust-store inventory for diagnostics: (system roots, env-bundle
/// roots). Cheap: no connector build, no network.
pub fn trust_store_report() -> (usize, usize) {
    let sys = rustls_native_certs::load_native_certs().certs.len();
    let env = load_env_roots().len();
    (sys, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BogusErr(String);
    impl std::fmt::Display for BogusErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::fmt::Debug for BogusErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for BogusErr {}

    #[test]
    fn classify_reset_errors_get_the_egress_hint() {
        for input in [
            "SYSCALL (5), cause: ConnectionReset",
            "os error 104: connection reset by peer",
            "The server's certificate or handshake failed: ConnectionReset",
        ] {
            let msg = classify_handshake_error(&BogusErr(input.into()));
            assert!(
                msg.contains("connection reset") || msg.contains("cut"),
                "{input} => {msg}"
            );
            assert!(msg.contains("HTTPS_PROXY"), "{input} => {msg}");
        }
    }

    #[test]
    fn classify_verify_errors_get_the_cert_hint() {
        for input in [
            "error:0A000086:SSL routines:tls_process_server_certificate:certificate verify failed",
            "X509VerifyError: unable to get local issuer certificate",
            "Invalid certificate verification context",
            "self-signed certificate chain",
        ] {
            let msg = classify_handshake_error(&BogusErr(input.into()));
            assert!(
                msg.contains("certificate verification failed") || msg.contains("trusted root"),
                "{input} => {msg}"
            );
            assert!(msg.contains("SSL_CERT_FILE"), "{input} => {msg}");
        }
    }

    #[test]
    fn unrelated_errors_pass_through_untouched() {
        let input = "an opaque handshake failure";
        assert_eq!(classify_handshake_error(&BogusErr(input.into())), input);
    }

    #[test]
    fn pem_bundle_splits_multiple_certs() {
        let two = [
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----",
            "-----BEGIN CERTIFICATE-----\nMIIB2\n-----END CERTIFICATE-----",
        ]
        .join("\n");
        let blocks = pem_certs(two.as_bytes());
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with(b"-----BEGIN"));
        assert!(blocks[1].starts_with(b"-----BEGIN"));
        // CRLF armor splits too.
        let crlf = two.replace('\n', "\r\n");
        assert_eq!(pem_certs(crlf.as_bytes()).len(), 2);
        // Junk without armor yields nothing.
        assert!(pem_certs(b"nothing here").is_empty());
    }

    #[test]
    fn env_roots_loads_ssl_cert_file_pem_bundle() {
        let dir = std::env::temp_dir().join(format!("donsetch-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bundle = [
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----",
            "-----BEGIN CERTIFICATE-----\nMIIB2\n-----END CERTIFICATE-----",
        ]
        .join("\n");
        let path = dir.join("ca.pem");
        std::fs::write(&path, &bundle).unwrap();
        // SAFETY: nextest isolates each test in its own process.
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &path);
            std::env::remove_var("SSL_CERT_DIR");
        }
        // The bogus base64 will not parse as certs; what matters is
        // that the loader found and split the bundle (no panic, no
        // file-not-found emptyness confusion). Empty parse output is
        // the expected result for synthetic PEM, so flip to a real
        // assertion: the file is read, blocks are split.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(pem_certs(&bytes).len(), 2);
        let roots = load_env_roots(); // exercises the full env path
        // S1 companion (L1): the bundle parse is cached on (path,
        // len, mtime). A second call hits the cache and returns the
        // same roots without re-reading.
        let roots2 = load_env_roots();
        assert_eq!(roots.len(), roots2.len());
    }

    #[test]
    fn env_roots_cache_key_tracks_path_identity() {
        let dir = std::env::temp_dir().join(format!("dscache{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binding = rcgen::generate_simple_self_signed(vec!["example.org".to_string()]).unwrap();
        let ca = binding.cert.der();
        let path = dir.join("ca.der");
        std::fs::write(&path, ca).unwrap();
        // SAFETY: nextest isolates each test in its own process.
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &path);
            std::env::remove_var("SSL_CERT_DIR");
        }
        let first = load_env_roots();
        assert_eq!(first.len(), 1, "der bundle parses to one cert");
        // Same identity -> cache hit, same result.
        assert_eq!(load_env_roots().len(), 1);
        // New path with different content -> new identity, re-read.
        let path2 = dir.join("ca2.der");
        std::fs::write(&path2, [0u8; 64]).unwrap();
        unsafe { std::env::set_var("SSL_CERT_FILE", &path2) };
        assert!(load_env_roots().is_empty(), "garbage file parses to zero");
    }

    #[test]
    fn decompress_rejects_hostile_declared_size() {
        // S1: the server-declared uncompressed length used to reserve
        // memory upfront. A hostile 4 GiB declaration must bail with
        // 0 BEFORE any allocation (pre-fix: with_capacity(4 GiB) ->
        // abort/OOM right here).
        let mut out: *mut boring_sys::CRYPTO_BUFFER = std::ptr::null_mut();
        let rc = unsafe {
            cert_decompress_brotli(
                std::ptr::null_mut(),
                &mut out,
                u32::MAX as usize,
                [].as_ptr(),
                0,
            )
        };
        assert_eq!(rc, 0, "oversized declaration refused, nothing allocated");
    }

    #[test]
    fn decompress_roundtrip_small_payload() {
        // Positive path still works: a real brotli blob decompresses
        // into a CRYPTO_BUFFER of the declared length.
        let payload = b"MIIB2 certification payload for the roundtrip";
        let mut compressed = Vec::new();
        {
            use std::io::Write;
            let mut enc = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            enc.write_all(payload).unwrap();
        }
        let mut out: *mut boring_sys::CRYPTO_BUFFER = std::ptr::null_mut();
        let rc = unsafe {
            cert_decompress_brotli(
                std::ptr::null_mut(),
                &mut out,
                payload.len(),
                compressed.as_ptr(),
                compressed.len(),
            )
        };
        assert_eq!(rc, 1, "decompressed into the allocated buffer");
        assert!(!out.is_null());
        unsafe {
            let len = boring_sys::CRYPTO_BUFFER_len(out);
            assert_eq!(len, payload.len());
            let data = std::slice::from_raw_parts(boring_sys::CRYPTO_BUFFER_data(out), len);
            assert_eq!(data, payload);
            boring_sys::CRYPTO_BUFFER_free(out);
        }
    }
}
