//! OCR tier (Tier B) : text where the glyph stream fails.
//!
//! Triggers (fusion trust audit):
//!   - page has ink but zero glyph chars (scans)
//!   - page glyph stream is garbage (PUA ratio high → broken encodings)
//!
//! Pipeline: PP-OCRv5 detection + recognition through `oar-ocr`
//! (Apache-2.0, ONNX Runtime CPU). Models are the oar-ocr REGISTRY set :
//! the exact checkpoints the crate is tested against : sha256-pinned in
//! this file and verified after download. Non-registry mirrors
//! (monkt/paddleocr-onnx) have incompatible preprocessing; we do not use
//! them. First use downloads ~12MB (EN) / ~21MB (CJK) to the lazy cache.
//!
//! DONSHEET_OCR=off disables the tier.
//! DONSHEET_OCR_MAX_PAGES caps per-document OCR cost (default 25).

#[cfg(feature = "ocr")]
use std::path::PathBuf;

use super::engine::{PageChars, PdfChar};
use super::pixels::PageBitmap;

#[cfg(feature = "ocr")]
const BASE: &str = "https://www.modelscope.cn/models/greatv/oar-ocr/resolve/master";

/// sha256-pinned registry models.
#[cfg(feature = "ocr")]
struct Model {
    name: &'static str,
    sha256: &'static str,
    min_bytes: u64,
}

#[cfg(feature = "ocr")]
const DET: Model = Model {
    name: "pp-ocrv5_mobile_det.onnx",
    sha256: "1eb7b4f7ab657ebd1c66d5f79bca7497f29768a2e3c15e52daecbba1a8e4a039",
    min_bytes: 4 * 1024 * 1024,
};
#[cfg(feature = "ocr")]
const REC_EN: Model = Model {
    name: "en_pp-ocrv5_mobile_rec.onnx",
    sha256: "8307465d3c9ef2ba4055c3bd0be55aafe11f518630212b7598b70ccb376028ac",
    min_bytes: 5 * 1024 * 1024,
};
#[cfg(feature = "ocr")]
const DICT_EN: Model = Model {
    name: "ppocrv5_en_dict.txt",
    sha256: "e025a66d31f327ba0c232e03f407ae8d105e1e709e7ccb3f408aa778c24e70d6",
    min_bytes: 512,
};
#[cfg(feature = "ocr")]
const REC_ZH: Model = Model {
    name: "pp-ocrv5_mobile_rec.onnx",
    sha256: "243a0f06d826761323e9045e9b113ab2c191c3aa50565585e628300b8eda0224",
    min_bytes: 12 * 1024 * 1024,
};
#[cfg(feature = "ocr")]
const DICT_ZH: Model = Model {
    name: "ppocrv5_dict.txt",
    sha256: "d1979e9f794c464c0d2e0b70a7fe14dd978e9dc644c0e71f14158cdf8342af1b",
    min_bytes: 50 * 1024,
};
#[cfg(feature = "ocr")]
const REC_DEVA: Model = Model {
    name: "devanagari_pp-ocrv5_mobile_rec.onnx",
    sha256: "b3d50774dfbec6ae02249ff79a925431a4381c8c6f86d342ff6e7b63e5fefa77",
    min_bytes: 5 * 1024 * 1024,
};
#[cfg(feature = "ocr")]
const DICT_DEVA: Model = Model {
    name: "ppocrv5_devanagari_dict.txt",
    sha256: "09c7440bfc5477e5c41052304b6b185aff8c4a5e8b2b4c23c1c706f6fe1ee9fc",
    min_bytes: 512,
};

/// OCR cache dir for the model zoo.
#[cfg(feature = "ocr")]
pub fn ocr_cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("ocr")
}

/// Master switch. Default ON (lazy); [fetch] ocr = false (legacy
/// DONSHEET_OCR=off) kills the tier.
pub fn enabled() -> bool {
    crate::config::cfg().fetch.ocr
}

/// Max OCR pages per document (giant scans cost; the governor is budget).
pub fn max_pages() -> usize {
    let n = crate::config::cfg().fetch.ocr_max_pages;
    if n == 0 { 25 } else { n as usize }
}

#[cfg(feature = "ocr")]
mod imp {
    use super::*;
    use sha2::Digest;
    use std::sync::{Mutex, OnceLock};

    #[allow(clippy::upper_case_acronyms)]
    type OAROCR = oar_ocr::oarocr::OAROCR;

    static ENGINE_EN: OnceLock<Result<OAROCR, String>> = OnceLock::new();
    static ENGINE_ZH: OnceLock<Result<OAROCR, String>> = OnceLock::new();
    static ENGINE_DEVA: OnceLock<Result<OAROCR, String>> = OnceLock::new();
    static DL_LOCK: Mutex<()> = Mutex::new(());
    static ENGINE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

    /// Recognition space: Latin-dominant docs use the EN recognizer,
    /// CJK-heavy pages the universal recognizer.
    pub enum RecKind {
        En,
        Zh,
        Deva,
    }

    fn sha256_file(path: &std::path::Path) -> Result<String, String> {
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        let mut h = sha2::Sha256::new();
        h.update(&data);
        let digest = h.finalize();
        Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Download `m` (atomic tmp+rename) and verify the pinned sha256.
    /// Fails closed on mismatch : a poisoned model is worse than no OCR.
    fn fetch_model(m: &Model, dir: &std::path::Path) -> Result<PathBuf, String> {
        fn inner(m: &Model, dir: &std::path::Path) -> Result<PathBuf, String> {
            let dst = dir.join(m.name);
            if dst.exists() {
                if let Ok(h) = sha256_file(&dst)
                    && h == m.sha256
                {
                    return Ok(dst);
                }
                let _ = std::fs::remove_file(&dst);
            }
            let tmp = dst.with_extension("tmp");
            let url = format!("{BASE}/{}", m.name);
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .user_agent("donsetch/0.1 (+https://github.com/dondai44423/donsetch)")
                .build()
                .map_err(|e| e.to_string())?;
            let mut r = client
                .get(&url)
                .send()
                .map_err(|e| format!("model download failed: {e}"))?;
            if !r.status().is_success() {
                return Err(format!("model download failed: HTTP {}", r.status()));
            }
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            std::io::copy(&mut r, &mut f).map_err(|e| e.to_string())?;
            let sz = std::fs::metadata(&tmp).map(|m2| m2.len()).unwrap_or(0);
            if sz < m.min_bytes {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("model {} truncated at {} bytes", m.name, sz));
            }
            let h = sha256_file(&tmp)?;
            if h != m.sha256 {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "model {} failed sha256 verification (got {}, pinned {})",
                    m.name, h, m.sha256
                ));
            }
            std::fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
            Ok(dst)
        }

        // Dedicated plain thread: `reqwest::blocking` panics when used on
        // a tokio runtime thread : and `panic = "abort"` turns that into a
        // process abort : and first-use downloads are triggered from async
        // fetch paths.
        let m = Model {
            name: m.name,
            sha256: m.sha256,
            min_bytes: m.min_bytes,
        };
        let dir = dir.to_path_buf();
        std::thread::Builder::new()
            .name("ocr-model-download".into())
            .spawn(move || inner(&m, &dir))
            .map_err(|e| format!("download thread spawn: {e}"))?
            .join()
            .map_err(|_| "download thread panicked".to_string())?
    }

    fn ensure_models(kind: &RecKind) -> Result<(PathBuf, PathBuf, PathBuf), String> {
        let dir = super::ocr_cache_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let _dl = DL_LOCK.lock().map_err(|e| e.to_string())?;
        let det = fetch_model(&DET, &dir)?;
        let (rec, dict) = match kind {
            RecKind::En => (fetch_model(&REC_EN, &dir)?, fetch_model(&DICT_EN, &dir)?),
            RecKind::Zh => (fetch_model(&REC_ZH, &dir)?, fetch_model(&DICT_ZH, &dir)?),
            RecKind::Deva => (
                fetch_model(&REC_DEVA, &dir)?,
                fetch_model(&DICT_DEVA, &dir)?,
            ),
        };
        Ok((det, rec, dict))
    }

    fn engine(kind: &RecKind) -> Result<&'static OAROCR, String> {
        let cell = match kind {
            RecKind::En => &ENGINE_EN,
            RecKind::Zh => &ENGINE_ZH,
            RecKind::Deva => &ENGINE_DEVA,
        };
        cell.get_or_init(|| {
            // Gate: ensure ONNX Runtime is loaded (AVX check + dlopen).
            // If the CPU lacks AVX or the .so is missing, return an error;
            // OCR falls back to the glyph stream.
            crate::onnx::ensure_loaded()?;
            let (det, rec, dict) = ensure_models(kind)?;
            // Run ONNX engine init in a separate thread with a timeout.
            // ONNX Runtime's C++ global constructors can deadlock on
            // some platforms (see pykeio/ort#579); the timeout prevents
            // an infinite hang : if ONNX doesn't respond in 30s, OCR
            // is disabled and pages fall back to the glyph stream.
            let det_s = det.to_string_lossy().to_string();
            let rec_s = rec.to_string_lossy().to_string();
            let dict_s = dict.to_string_lossy().to_string();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = oar_ocr::oarocr::OAROCRBuilder::new(det_s, rec_s, dict_s)
                    .build()
                    .map_err(|e| format!("ocr engine init failed: {e}"));
                let _ = tx.send(result);
            });
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(result) => result,
                Err(_) => Err(
                    "ONNX Runtime init timed out (30s) : OCR disabled, falling back to glyph stream"
                        .to_string(),
                ),
            }
        })
        .as_ref()
        .map_err(|e| e.clone())
    }

    pub fn run_ocr(bitmap: &PageBitmap, kind: &RecKind) -> Result<Vec<super::OcrLine>, String> {
        let mut rgb = Vec::with_capacity(bitmap.w * bitmap.h * 3);
        for i in 0..bitmap.buf.len() / 4 {
            let px = &bitmap.buf[i * 4..i * 4 + 4];
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        let img = image::RgbImage::from_raw(bitmap.w as u32, bitmap.h as u32, rgb)
            .ok_or("invalid bitmap dims")?;
        let dynimg = image::DynamicImage::ImageRgb8(img).into_rgb8();

        let eng = engine(kind)?;
        let _g = ENGINE_MUTEX
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|e| e.to_string())?;
        let mut results = eng
            .predict(vec![dynimg])
            .map_err(|e| format!("ocr predict failed: {e}"))?;
        let Some(result) = results.pop() else {
            return Ok(Vec::new());
        };
        let sx = bitmap.w as f32 / bitmap.page_w_pt.max(1.0);
        let sy = bitmap.h as f32 / bitmap.page_h_pt.max(1.0);
        let mut lines = Vec::new();
        for region in &result.text_regions {
            let b = &region.bounding_box;
            let Some(text) = &region.text else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() || b.points.len() < 2 {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in &b.points {
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
            lines.push(super::OcrLine {
                x0: x0 / sx,
                y0: y0 / sy,
                x1: x1 / sx,
                y1: y1 / sy,
                text: text.to_string(),
                confidence: region.confidence.unwrap_or(0.0),
            });
        }
        Ok(lines)
    }
}

/// An OCR'd line in PDF point space (y-down).
#[derive(Clone, Debug)]
pub struct OcrLine {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub text: String,
    pub confidence: f32,
}

/// Kind selection: explicit hint wins; "auto" runs the confidence
/// escalation cascade (En → Zh → Deva) on this page and picks the
/// recognizer with the best mean confidence. The cascade mirrors
/// DonShadow→DonGhost: cheap answer first, escalate on signal.
/// Returns (lines, winning kind as string) when cascade used.
#[cfg(feature = "ocr")]
pub fn ocr_page(
    bitmap: &PageBitmap,
    script_hint: &str,
) -> Result<(Vec<OcrLine>, &'static str), String> {
    if !enabled() {
        return Err("ocr disabled".to_string());
    }
    match script_hint {
        "en" => Ok((imp::run_ocr(bitmap, &imp::RecKind::En)?, "en")),
        "zh" => Ok((imp::run_ocr(bitmap, &imp::RecKind::Zh)?, "zh")),
        "deva" => Ok((imp::run_ocr(bitmap, &imp::RecKind::Deva)?, "deva")),
        _ => {
            // Cascade: each candidate must beat a confidence floor to hold.
            let en = imp::run_ocr(bitmap, &imp::RecKind::En)?;
            if mean_confidence(&en) >= 0.55 && !en.is_empty() {
                return Ok((en, "en"));
            }
            let zh = imp::run_ocr(bitmap, &imp::RecKind::Zh)?;
            if mean_confidence(&zh) >= 0.55 && !zh.is_empty() {
                return Ok((zh, "zh"));
            }
            let deva = imp::run_ocr(bitmap, &imp::RecKind::Deva)?;
            // Deva last resort: whatever it produces is the best we have.
            if !deva.is_empty() {
                return Ok((deva, "deva"));
            }
            // Nothing recognized at all: return the non-empty candidate if any.
            if !zh.is_empty() {
                return Ok((zh, "zh"));
            }
            Ok((en, "en"))
        }
    }
}

#[cfg(not(feature = "ocr"))]
pub fn ocr_page(
    _bitmap: &PageBitmap,
    _script_hint: &str,
) -> Result<(Vec<OcrLine>, &'static str), String> {
    Err("ocr feature not compiled".to_string())
}

/// OCR'd lines → synthetic PageChars for the unified pipeline. Each line
/// becomes equally-spaced chars across its bbox (recognition is
/// line-level; assembly does not need per-glyph boxes).
pub fn lines_to_chars(
    ocr_lines: &[OcrLine],
    page_idx: usize,
    page_w: f32,
    page_h: f32,
) -> PageChars {
    let mut chars = Vec::new();
    let mut order = 0u32;
    for l in ocr_lines {
        let n = l.text.chars().count().max(1) as f32;
        let advance = (l.x1 - l.x0) / n;
        let size = (l.y1 - l.y0).max(1.0) * 0.9;
        for (i, ch) in l.text.chars().enumerate() {
            chars.push(PdfChar {
                cp: ch,
                x0: l.x0 + advance * i as f32,
                y0: l.y0,
                x1: l.x0 + advance * (i as f32 + 1.0),
                y1: l.y1,
                size,
                weight: 400,
                flags: 0,
                font: 0,
                angle: 0.0,
                order,
                dingbat: false,
                rt: false,
                ocr: true,
            });
            order += 1;
        }
    }
    PageChars {
        index: page_idx,
        width: page_w,
        height: page_h,
        chars,
        images: 0,
    }
}

/// Mean recognition confidence across lines.
pub fn mean_confidence(ocr_lines: &[OcrLine]) -> f32 {
    if ocr_lines.is_empty() {
        return 0.0;
    }
    let s: f32 = ocr_lines.iter().map(|l| l.confidence).sum();
    s / ocr_lines.len() as f32
}
