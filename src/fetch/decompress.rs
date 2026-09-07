//! Streaming body decompression: br / gzip / deflate / zstd.
//!
//! All codecs are capped: a malicious server can hand us 500 KB
//! of gzip that expands to gigabytes (decompression bomb). We
//! read at most MAX_DECOMPRESSED + 1 bytes so the cap is exact.

use std::io::Read;

use crate::error::FetchError;

/// Hard cap on a decompressed response body (64 MiB).
pub const MAX_DECOMPRESSED: usize = 64 << 20;

fn read_capped<R: Read>(r: R) -> Result<Vec<u8>, FetchError> {
    let mut out = Vec::new();
    let mut limited = r.take((MAX_DECOMPRESSED + 1) as u64);
    limited
        .read_to_end(&mut out)
        .map_err(|e| FetchError::Http(format!("decompress: {e}")))?;
    if out.len() > MAX_DECOMPRESSED {
        return Err(FetchError::Http(format!(
            "decompressed body exceeds {} MiB cap",
            MAX_DECOMPRESSED >> 20
        )));
    }
    Ok(out)
}

pub fn decompress(encoding: &str, body: &[u8]) -> Result<Vec<u8>, FetchError> {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => {
            if body.len() > MAX_DECOMPRESSED {
                return Err(FetchError::Http(format!(
                    "body exceeds {} MiB cap",
                    MAX_DECOMPRESSED >> 20
                )));
            }
            Ok(body.to_vec())
        }
        "br" => read_capped(brotli::Decompressor::new(body, 1 << 20)),
        "gzip" => read_capped(flate2::read::GzDecoder::new(body)),
        "deflate" => read_capped(flate2::read::ZlibDecoder::new(body)),
        "zstd" => {
            let dec = zstd::stream::read::Decoder::new(body)
                .map_err(|e| FetchError::Http(format!("zstd: {e}")))?;
            read_capped(dec)
        }
        other => {
            // Layered encodings ("gzip, br", rare but real): peel one
            // layer per pass, innermost last. The cap applies to the
            // final size.
            if let Some((outer, inner)) = other.split_once(", ").or_else(|| other.split_once(",")) {
                let middle = decompress(inner.trim(), body)?;
                return decompress(outer.trim(), &middle);
            }
            Err(FetchError::Http(format!(
                "unknown content-encoding: {other}"
            )))
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn layered_encoding_peels_all_layers() {
        // "gzip, br" = gzip applied first (innermost), br on top
        // (outermost); decoding peels outermost first. Both decoders run.
        let payload = b"layered payload for the peeling test";
        let gzipped = {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap()
        };
        let mut br = Vec::new();
        {
            let mut enc = brotli::CompressorWriter::new(&mut br, 4096, 5, 22);
            enc.write_all(&gzipped).unwrap();
        }
        let plain = decompress("gzip, br", &br).expect("peels br then gzip");
        assert_eq!(plain, payload);
    }

    #[test]
    fn unknown_single_encoding_still_errors() {
        let e = decompress("xz", b"payload").unwrap_err();
        assert!(format!("{e}").contains("xz"), "honest error preserved");
    }
}
