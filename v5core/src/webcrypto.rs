//! WebCrypto's primitives for the challenge engine, on BoringSSL.
//!
//! A challenge encrypts its signal collection with `crypto.subtle` before sending
//! it; a host that answers `encrypt` with an empty buffer sends an empty signal,
//! and the vendor refuses the token it mints. The primitives here are the same
//! BoringSSL the TLS stack already links (through `btls`), so the engine gains a
//! real WebCrypto without a new dependency.
//!
//! One op, `op_crypto(operation, params, key, data)`, covers the algorithms a
//! measured challenge uses and the ones a page commonly pairs with them:
//! SHA-1/256/384/512 digests, HMAC, AES-GCM/CBC/CTR/KW, PBKDF2 and HKDF. Anything else is
//! refused with the name a browser gives it (`NotSupportedError`), and a failed
//! authentication is an `OperationError`, as in the Web Crypto spec.

use btls::hash::MessageDigest;
use btls::symm::{self, Cipher};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Params {
    /// Algorithm name as the page wrote it (`AES-GCM`, `HMAC`, ...).
    pub name: String,
    /// Hash name for digests, HMAC and PBKDF2.
    pub hash: String,
    /// Hex: the IV (GCM, CBC) or the counter block (CTR).
    pub iv: String,
    /// Hex: GCM additional authenticated data.
    pub additional_data: String,
    /// GCM tag length in bits (default 128).
    pub tag_length: Option<usize>,
    /// Hex: PBKDF2 or HKDF salt.
    pub salt: String,
    /// Hex: HKDF info.
    pub info: String,
    pub iterations: usize,
    /// Bits to derive (PBKDF2).
    pub length: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CryptoError {
    /// The algorithm or parameter set is not one this host implements.
    NotSupported(String),
    /// The operation itself failed (bad key size, failed authentication).
    Operation(String),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSupported(what) => write!(f, "NotSupportedError: {what}"),
            Self::Operation(what) => write!(f, "OperationError: {what}"),
        }
    }
}

fn digest_of(name: &str) -> Result<MessageDigest, CryptoError> {
    match name.to_ascii_uppercase().replace('_', "-").as_str() {
        "SHA-1" => Ok(MessageDigest::sha1()),
        "SHA-256" => Ok(MessageDigest::sha256()),
        "SHA-384" => Ok(MessageDigest::sha384()),
        "SHA-512" => Ok(MessageDigest::sha512()),
        other => Err(CryptoError::NotSupported(format!("hash {other}"))),
    }
}

fn hex(raw: &str) -> Result<Vec<u8>, CryptoError> {
    if !raw.len().is_multiple_of(2) {
        return Err(CryptoError::Operation("odd hex length".into()));
    }
    (0..raw.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(&raw[at..at + 2], 16)
                .map_err(|_| CryptoError::Operation("bad hex".into()))
        })
        .collect()
}

fn aes(name: &str, key: &[u8]) -> Result<Cipher, CryptoError> {
    let cipher = match (name, key.len()) {
        ("AES-GCM", 16) => Cipher::aes_128_gcm(),
        ("AES-GCM", 24) => Cipher::aes_192_gcm(),
        ("AES-GCM", 32) => Cipher::aes_256_gcm(),
        ("AES-CBC", 16) => Cipher::aes_128_cbc(),
        ("AES-CBC", 24) => Cipher::aes_192_cbc(),
        ("AES-CBC", 32) => Cipher::aes_256_cbc(),
        ("AES-CTR", 16) => Cipher::aes_128_ctr(),
        ("AES-CTR", 24) => Cipher::aes_192_ctr(),
        ("AES-CTR", 32) => Cipher::aes_256_ctr(),
        ("AES-GCM" | "AES-CBC" | "AES-CTR" | "AES-KW", length) => {
            return Err(CryptoError::Operation(format!("AES key of {length} bytes")))
        }
        (other, _) => return Err(CryptoError::NotSupported(format!("algorithm {other}"))),
    };
    Ok(cipher)
}

fn failed(error: btls::error::ErrorStack) -> CryptoError {
    CryptoError::Operation(error.to_string())
}

/// Run one WebCrypto operation. `key` is the raw key material; `data` the input.
pub fn run(
    operation: &str,
    params: &Params,
    key: &[u8],
    data: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let name = params.name.to_ascii_uppercase();
    match operation {
        "digest" => {
            let digest = digest_of(&name)?;
            btls::hash::hash(digest, data)
                .map(|bytes| bytes.to_vec())
                .map_err(failed)
        }
        "sign" => {
            if name != "HMAC" {
                return Err(CryptoError::NotSupported(format!("sign with {name}")));
            }
            hmac(&digest_of(&params.hash)?, key, data)
        }
        "encrypt" | "decrypt" if name == "AES-KW" => key_wrap(operation == "encrypt", key, data),
        "encrypt" | "decrypt" => {
            let cipher = aes(&name, key)?;
            let iv = hex(&params.iv)?;
            let encrypting = operation == "encrypt";
            match name.as_str() {
                "AES-GCM" => {
                    let tag_bytes = params.tag_length.unwrap_or(128) / 8;
                    if !matches!(tag_bytes, 4 | 8 | 12 | 13 | 14 | 15 | 16) {
                        return Err(CryptoError::Operation(format!(
                            "tag length {}",
                            tag_bytes * 8
                        )));
                    }
                    if iv.is_empty() {
                        return Err(CryptoError::Operation("empty iv".into()));
                    }
                    let aad = hex(&params.additional_data)?;
                    if encrypting {
                        let mut tag = vec![0u8; tag_bytes];
                        let mut out =
                            symm::encrypt_aead(cipher, key, Some(&iv), &aad, data, &mut tag)
                                .map_err(failed)?;
                        out.extend_from_slice(&tag);
                        Ok(out)
                    } else {
                        if data.len() < tag_bytes {
                            return Err(CryptoError::Operation(
                                "ciphertext shorter than its tag".into(),
                            ));
                        }
                        let (body, tag) = data.split_at(data.len() - tag_bytes);
                        symm::decrypt_aead(cipher, key, Some(&iv), &aad, body, tag).map_err(failed)
                    }
                }
                "AES-CBC" | "AES-CTR" => {
                    if iv.len() != 16 {
                        return Err(CryptoError::Operation(format!(
                            "{} needs a 16-byte iv",
                            name
                        )));
                    }
                    if encrypting {
                        symm::encrypt(cipher, key, Some(&iv), data).map_err(failed)
                    } else {
                        symm::decrypt(cipher, key, Some(&iv), data).map_err(failed)
                    }
                }
                other => Err(CryptoError::NotSupported(format!("algorithm {other}"))),
            }
        }
        "derive-bits" => {
            if params.length == 0 || !params.length.is_multiple_of(8) || params.length > 8 * 4096 {
                return Err(CryptoError::Operation(format!("length {}", params.length)));
            }
            let digest = digest_of(&params.hash)?;
            let salt = hex(&params.salt)?;
            match name.as_str() {
                "PBKDF2" => {
                    if params.iterations == 0 {
                        return Err(CryptoError::Operation("zero iterations".into()));
                    }
                    // A page could ask for a derivation that runs for minutes; the
                    // engine's wall clock stops V8, not a native loop. Bound it here.
                    if params.iterations > 5_000_000 {
                        return Err(CryptoError::Operation("derivation too large".into()));
                    }
                    let mut out = vec![0u8; params.length / 8];
                    btls::pkcs5::pbkdf2_hmac(key, &salt, params.iterations, digest, &mut out)
                        .map_err(failed)?;
                    Ok(out)
                }
                "HKDF" => hkdf(&digest, key, &salt, &hex(&params.info)?, params.length / 8),
                other => Err(CryptoError::NotSupported(format!("derive with {other}"))),
            }
        }
        other => Err(CryptoError::NotSupported(format!("operation {other}"))),
    }
}

fn hmac(digest: &MessageDigest, key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut mac = btls::hmac::Hmac::init(key, digest).map_err(failed)?;
    mac.update(data).map_err(failed)?;
    mac.finalize().map_err(failed)
}

/// RFC 5869: extract, then expand to `length` bytes.
fn hkdf(
    digest: &MessageDigest,
    key: &[u8],
    salt: &[u8],
    info: &[u8],
    length: usize,
) -> Result<Vec<u8>, CryptoError> {
    let size = digest.size();
    if length > 255 * size {
        return Err(CryptoError::Operation(format!("HKDF length {length}")));
    }
    let zeros = vec![0u8; size];
    let prk = hmac(digest, if salt.is_empty() { &zeros } else { salt }, key)?;
    let mut out = Vec::with_capacity(length);
    let mut block: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while out.len() < length {
        let mut input = block.clone();
        input.extend_from_slice(info);
        input.push(counter);
        block = hmac(digest, &prk, &input)?;
        out.extend_from_slice(&block);
        counter = counter.wrapping_add(1);
    }
    out.truncate(length);
    Ok(out)
}

/// RFC 3394 AES key wrap with the default IV, as WebCrypto's AES-KW.
fn key_wrap(wrapping: bool, key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    use btls::aes::{unwrap_key, wrap_key, AesKey};
    if !matches!(key.len(), 16 | 24 | 32) {
        return Err(CryptoError::Operation(format!(
            "AES key of {} bytes",
            key.len()
        )));
    }
    let refused = |_| CryptoError::Operation("AES-KW failed".into());
    if wrapping {
        if data.len() < 16 || !data.len().is_multiple_of(8) {
            return Err(CryptoError::Operation(
                "AES-KW input must be 8-byte blocks, at least two".into(),
            ));
        }
        let aes = AesKey::new_encrypt(key).map_err(refused)?;
        let mut out = vec![0u8; data.len() + 8];
        let written = wrap_key(&aes, None, &mut out, data).map_err(refused)?;
        out.truncate(written);
        Ok(out)
    } else {
        if data.len() < 24 || !data.len().is_multiple_of(8) {
            return Err(CryptoError::Operation(
                "AES-KW input must be 8-byte blocks, at least three".into(),
            ));
        }
        let aes = AesKey::new_decrypt(key).map_err(refused)?;
        let mut out = vec![0u8; data.len() - 8];
        let written = unwrap_key(&aes, None, &mut out, data).map_err(refused)?;
        out.truncate(written);
        Ok(out)
    }
}

/// The op's wire form: one status byte (0 ok, 1 NotSupportedError, 2
/// OperationError), then the output or the reason.
pub fn framed(operation: &str, params: &str, key: &[u8], data: &[u8]) -> Vec<u8> {
    let result = serde_json::from_str::<Params>(params)
        .map_err(|error| CryptoError::Operation(format!("params: {error}")))
        .and_then(|params| run(operation, &params, key, data));
    match result {
        Ok(bytes) => {
            let mut out = Vec::with_capacity(bytes.len() + 1);
            out.push(0);
            out.extend_from_slice(&bytes);
            out
        }
        Err(CryptoError::NotSupported(why)) => [&[1u8][..], why.as_bytes()].concat(),
        Err(CryptoError::Operation(why)) => [&[2u8][..], why.as_bytes()].concat(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn params(json: &str) -> Params {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn digests_match_the_published_vectors() {
        let sha256 = run("digest", &params(r#"{"name":"SHA-256"}"#), &[], b"abc").unwrap();
        assert_eq!(
            to_hex(&sha256),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let sha1 = run("digest", &params(r#"{"name":"sha-1"}"#), &[], b"abc").unwrap();
        assert_eq!(to_hex(&sha1), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        let mac = run(
            "sign",
            &params(r#"{"name":"HMAC","hash":"SHA-256"}"#),
            b"Jefe",
            b"what do ya want for nothing?",
        )
        .unwrap();
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn aes_gcm_round_trips_and_appends_its_tag() {
        // NIST GCM test case 3 (AES-128, 96-bit IV).
        let key = hex("feffe9928665731c6d6a8f9467308308").unwrap();
        let plain = hex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255").unwrap();
        let p = params(r#"{"name":"AES-GCM","iv":"cafebabefacedbaddecaf888"}"#);
        let sealed = run("encrypt", &p, &key, &plain).unwrap();
        assert_eq!(sealed.len(), plain.len() + 16);
        assert_eq!(
            to_hex(&sealed[sealed.len() - 16..]),
            "4d5c2af327cd64a62cf35abd2ba6fab4"
        );
        assert_eq!(run("decrypt", &p, &key, &sealed).unwrap(), plain);
        // A flipped bit is an authentication failure, not garbage plaintext.
        let mut tampered = sealed.clone();
        tampered[0] ^= 1;
        assert!(matches!(
            run("decrypt", &p, &key, &tampered),
            Err(CryptoError::Operation(_))
        ));
    }

    #[test]
    fn aes_cbc_pads_like_webcrypto() {
        let key = [7u8; 16];
        let p = params(r#"{"name":"AES-CBC","iv":"000102030405060708090a0b0c0d0e0f"}"#);
        let sealed = run("encrypt", &p, &key, b"sixteen byte msg").unwrap();
        assert_eq!(
            sealed.len(),
            32,
            "PKCS#7 adds a full block to an aligned input"
        );
        assert_eq!(
            run("decrypt", &p, &key, &sealed).unwrap(),
            b"sixteen byte msg"
        );
    }

    #[test]
    fn pbkdf2_matches_rfc_6070() {
        let out = run(
            "derive-bits",
            &params(
                r#"{"name":"PBKDF2","hash":"SHA-1","salt":"73616c74","iterations":2,"length":160}"#,
            ),
            b"password",
            &[],
        )
        .unwrap();
        assert_eq!(to_hex(&out), "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957");
    }

    #[test]
    fn hkdf_matches_rfc_5869_case_1() {
        let out = run(
            "derive-bits",
            &params(r#"{"name":"HKDF","hash":"SHA-256","salt":"000102030405060708090a0b0c","info":"f0f1f2f3f4f5f6f7f8f9","length":336}"#),
            &[0x0b; 22],
            &[],
        )
        .unwrap();
        assert_eq!(
            to_hex(&out),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn aes_kw_matches_rfc_3394_4_1() {
        let key = hex("000102030405060708090a0b0c0d0e0f").unwrap();
        let data = hex("00112233445566778899aabbccddeeff").unwrap();
        let p = params(r#"{"name":"AES-KW"}"#);
        let wrapped = run("encrypt", &p, &key, &data).unwrap();
        assert_eq!(
            to_hex(&wrapped),
            "1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5"
        );
        assert_eq!(run("decrypt", &p, &key, &wrapped).unwrap(), data);
    }

    #[test]
    fn the_frame_carries_the_error_class() {
        assert_eq!(framed("digest", r#"{"name":"SHA-1"}"#, &[], b"abc")[0], 0);
        assert_eq!(framed("digest", r#"{"name":"MD5"}"#, &[], b"abc")[0], 1);
        assert_eq!(
            framed("encrypt", r#"{"name":"AES-GCM","iv":""}"#, &[0; 16], b"x")[0],
            2
        );
    }

    #[test]
    fn unsupported_algorithms_are_named_not_faked() {
        assert!(matches!(
            run("encrypt", &params(r#"{"name":"RSA-OAEP"}"#), &[0; 16], b"x"),
            Err(CryptoError::NotSupported(_))
        ));
        assert!(matches!(
            run("digest", &params(r#"{"name":"MD5"}"#), &[], b"x"),
            Err(CryptoError::NotSupported(_))
        ));
    }
}
