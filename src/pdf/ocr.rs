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

/// Whether [`ocr_page`] can recognize a page now: the tier is enabled,
/// the `ocr` feature is compiled, ONNX Runtime loads, and the recognizer
/// `script_hint` starts with initializes, its models fetched on first use
/// as [`ocr_page`] would. `Err` carries the reason. A recognizer keeps its
/// first outcome for the life of the process, so a repeat call is cheap.
pub fn ready(script_hint: &str) -> Result<(), String> {
    #[cfg(test)]
    if let Some(r) = test_ocr::ready() {
        return r;
    }
    if !enabled() {
        return Err("ocr disabled".to_string());
    }
    #[cfg(feature = "ocr")]
    {
        // The "auto" cascade in `ocr_page` starts with the English recognizer.
        let kind = match script_hint {
            "zh" => imp::RecKind::Zh,
            "deva" => imp::RecKind::Deva,
            _ => imp::RecKind::En,
        };
        imp::init(&kind)
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = script_hint;
        Err("ocr feature not compiled".to_string())
    }
}

/// Effective intra-op thread count per ONNX Runtime OCR session.
///
/// OCR runs serially behind `ENGINE_MUTEX` one page at a time, so letting each
/// ONNX model spin up a full-core pool (e.g. 24 threads × 2 models = 48+ idle
/// threads) bloats long-lived MCP daemons without improving throughput (#475).
#[cfg(any(feature = "ocr", test))]
fn ocr_intra_threads_for(configured: u32, available_cores: usize) -> usize {
    if configured > 0 {
        return (configured as usize).min(64);
    }
    available_cores.clamp(1, 4)
}

#[cfg(any(feature = "ocr", test))]
mod cache {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Cooldown before retrying a transient OCR model download failure.
    const TRANSIENT_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

    struct CachedSlot<T> {
        engine: Arc<T>,
        last_used: Instant,
    }

    struct FatalEntry {
        message: String,
        /// `Some(deadline)` for transient network/download failures (retriable
        /// after cooldown); `None` for permanent runtime/load failures.
        retry_after: Option<Instant>,
    }

    struct Inner<T> {
        slots: [Option<CachedSlot<T>>; 3],
        fatal: [Option<FatalEntry>; 3],
        reaper_active: bool,
    }

    fn is_transient_ocr_error(err: &str) -> bool {
        err.starts_with("model download failed")
            || (err.starts_with("model ") && err.contains("truncated"))
    }

    /// Idle-evicting cache for the three OCR recognizers (En, Zh, Deva).
    ///
    /// Keeps an active engine warm across pages and back-to-back PDFs, then
    /// drops its ONNX Runtime sessions once idle for `ttl` so CPU memory arenas
    /// and worker threads are returned to the OS (#475). The background reaper
    /// thread spawns lazily and exits as soon as all slots are empty.
    pub(super) struct IdleEngineCache<T: Send + Sync + 'static> {
        inner: Mutex<Inner<T>>,
        cvar: Condvar,
        init_lock: Mutex<()>,
    }

    impl<T: Send + Sync + 'static> IdleEngineCache<T> {
        pub(super) const fn new() -> Self {
            Self {
                inner: Mutex::new(Inner {
                    slots: [None, None, None],
                    fatal: [None, None, None],
                    reaper_active: false,
                }),
                cvar: Condvar::new(),
                init_lock: Mutex::new(()),
            }
        }

        pub(super) fn get_or_init<F>(
            &'static self,
            idx: usize,
            ttl: Duration,
            build: F,
        ) -> Result<Arc<T>, String>
        where
            F: FnOnce() -> Result<T, String>,
        {
            {
                let mut g = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(slot) = g.slots[idx].as_mut() {
                    slot.last_used = Instant::now();
                    return Ok(Arc::clone(&slot.engine));
                }
                if let Some(entry) = g.fatal[idx].as_ref() {
                    if entry.retry_after.is_none_or(|until| Instant::now() < until) {
                        return Err(entry.message.clone());
                    }
                    g.fatal[idx] = None;
                }
            }

            // Serialize cold engine builds without holding the cache lock
            // across model downloads or ONNX session creation.
            let _init = self
                .init_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            {
                let mut g = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(slot) = g.slots[idx].as_mut() {
                    slot.last_used = Instant::now();
                    return Ok(Arc::clone(&slot.engine));
                }
                if let Some(entry) = g.fatal[idx].as_ref() {
                    if entry.retry_after.is_none_or(|until| Instant::now() < until) {
                        return Err(entry.message.clone());
                    }
                    g.fatal[idx] = None;
                }
            }

            match build() {
                Ok(engine) => {
                    let arc = Arc::new(engine);
                    let mut g = self
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    g.slots[idx] = Some(CachedSlot {
                        engine: Arc::clone(&arc),
                        last_used: Instant::now(),
                    });
                    if !ttl.is_zero() && !g.reaper_active {
                        g.reaper_active = true;
                        let spawned = std::thread::Builder::new()
                            .name("ocr-idle-reaper".into())
                            .spawn(move || self.reap_loop(ttl));
                        if spawned.is_err() {
                            g.reaper_active = false;
                        }
                    } else if !ttl.is_zero() {
                        self.cvar.notify_one();
                    }
                    Ok(arc)
                }
                Err(err) => {
                    let retry_after = is_transient_ocr_error(&err)
                        .then(|| Instant::now() + TRANSIENT_RETRY_COOLDOWN);
                    let mut g = self
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    g.fatal[idx] = Some(FatalEntry {
                        message: err.clone(),
                        retry_after,
                    });
                    Err(err)
                }
            }
        }

        #[cfg(feature = "ocr")]
        pub(super) fn touch(&self, idx: usize) {
            let mut g = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(slot) = g.slots[idx].as_mut() {
                slot.last_used = Instant::now();
            }
        }

        #[cfg(test)]
        pub(super) fn loaded_count(&self) -> usize {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .slots
                .iter()
                .filter(|s| s.is_some())
                .count()
        }

        #[cfg(test)]
        pub(super) fn reaper_active(&self) -> bool {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reaper_active
        }

        fn reap_loop(&self, ttl: Duration) {
            loop {
                let mut evicted: Vec<Arc<T>> = Vec::new();
                let wait = {
                    let mut g = self
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let now = Instant::now();
                    let mut next_wait: Option<Duration> = None;
                    for slot in &mut g.slots {
                        if let Some(cached) = slot {
                            let elapsed = now.saturating_duration_since(cached.last_used);
                            if elapsed >= ttl {
                                if let Some(removed) = slot.take() {
                                    evicted.push(removed.engine);
                                }
                            } else {
                                let rem = ttl.saturating_sub(elapsed);
                                next_wait = Some(match next_wait {
                                    Some(cur) => cur.min(rem),
                                    None => rem,
                                });
                            }
                        }
                    }
                    match next_wait {
                        Some(w) => w,
                        None => {
                            g.reaper_active = false;
                            // Drop evicted engines outside the lock before exiting.
                            drop(g);
                            if !evicted.is_empty() {
                                drop(evicted);
                                trim_allocator_after_evict();
                            }
                            return;
                        }
                    }
                };
                if !evicted.is_empty() {
                    drop(evicted);
                    trim_allocator_after_evict();
                }
                let g = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = self
                    .cvar
                    .wait_timeout(g, wait.max(Duration::from_millis(10)));
            }
        }
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn trim_allocator_after_evict() {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: glibc `malloc_trim(0)` is thread-safe and asks the allocator
        // to return freed ONNX Runtime arena heap pages back to the kernel.
        unsafe {
            let _ = malloc_trim(0);
        }
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    fn trim_allocator_after_evict() {}
}

#[cfg(feature = "ocr")]
mod imp {
    use super::*;
    use sha2::Digest;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    #[allow(clippy::upper_case_acronyms)]
    type OAROCR = oar_ocr::oarocr::OAROCR;

    static ENGINE_CACHE: cache::IdleEngineCache<OAROCR> = cache::IdleEngineCache::new();
    static DL_LOCK: Mutex<()> = Mutex::new(());
    static ENGINE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

    /// Recognition space: Latin-dominant docs use the EN recognizer,
    /// CJK-heavy pages the universal recognizer.
    #[derive(Clone, Copy)]
    pub enum RecKind {
        En,
        Zh,
        Deva,
    }

    impl RecKind {
        fn index(self) -> usize {
            match self {
                RecKind::En => 0,
                RecKind::Zh => 1,
                RecKind::Deva => 2,
            }
        }
    }

    fn ort_session_config() -> oar_ocr::core::config::OrtSessionConfig {
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(2);
        let threads = ocr_intra_threads_for(crate::config::cfg().fetch.ocr_threads, cores);
        // Bound thread pools and disable memory-pattern scratchpad retention
        // and busy-spinning so OCR sessions stay lean inside a long-lived
        // MCP daemon (#475).
        oar_ocr::core::config::OrtSessionConfig::new()
            .with_intra_threads(threads)
            .with_inter_threads(1)
            .with_parallel_execution(false)
            .with_memory_pattern(false)
            .add_config_entry("session.intra_op.allow_spinning", "0")
            .add_config_entry("session.inter_op.allow_spinning", "0")
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
            static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dst.with_extension(format!("{}.{seq}.tmp", std::process::id()));
            let url = format!("{BASE}/{}", m.name);
            let mut builder = crate::egress::trust::with_trusted_roots_blocking(
                reqwest::blocking::Client::builder(),
            )
            .timeout(std::time::Duration::from_secs(600))
            .user_agent("donsetch/0.1 (+https://github.com/dondai44423/donsetch)")
            .no_proxy();
            if let Some(proxy) = crate::egress::proxy::from_env_for(&url)
                .map_err(|e| format!("model download proxy failed: {e}"))?
            {
                let p = reqwest::Proxy::all(proxy.to_url())
                    .map_err(|e| format!("model download proxy failed: {e}"))?;
                builder = builder.proxy(p);
            }
            let client = builder.build().map_err(|e| e.to_string())?;
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

    fn engine(kind: &RecKind) -> Result<Arc<OAROCR>, String> {
        let ttl = Duration::from_secs(crate::config::cfg().fetch.ocr_idle_secs);
        ENGINE_CACHE.get_or_init(kind.index(), ttl, || {
            // Gate: ensure ONNX Runtime is loaded (AVX check + dlopen).
            // If the runtime library is missing or fails to init, return an error;
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
            let session_cfg = ort_session_config();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = oar_ocr::oarocr::OAROCRBuilder::new(det_s, rec_s, dict_s)
                    .ort_session(session_cfg)
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
        // Refresh the idle clock after inference completes so a long scan
        // never triggers eviction mid-document.
        ENGINE_CACHE.touch(kind.index());
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

    /// Initialize the recognizer for `kind` without running it.
    pub fn init(kind: &RecKind) -> Result<(), String> {
        engine(kind).map(|_| ())
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
/// recognizer with the best mean confidence: cheap answer first,
/// escalate on signal.
/// Returns (lines, winning kind as string) when cascade used.
#[cfg(feature = "ocr")]
pub fn ocr_page(
    bitmap: &PageBitmap,
    script_hint: &str,
) -> Result<(Vec<OcrLine>, &'static str), String> {
    #[cfg(test)]
    if let Some(r) = test_ocr::recognize(bitmap) {
        return r.map(|lines| (lines, "en"));
    }
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
    #[cfg(test)]
    if let Some(r) = test_ocr::recognize(_bitmap) {
        return r.map(|lines| (lines, "en"));
    }
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
                spacing_box: None,
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

/// Test seam: a stand-in for the ONNX recognizer on the current thread.
/// While one is set, [`ready`] and [`ocr_page`] answer from it instead of
/// the tier switch, the feature, ONNX Runtime and the models.
#[cfg(test)]
pub(crate) mod test_ocr {
    use super::{OcrLine, PageBitmap};
    use std::cell::RefCell;

    type Fake = Box<dyn FnMut(&PageBitmap) -> Vec<OcrLine>>;

    thread_local! {
        static SEAM: RefCell<Option<Result<Fake, String>>> = const { RefCell::new(None) };
    }

    /// OCR cannot run: both entry points fail with `why`.
    pub(crate) fn unavailable(why: &str) {
        SEAM.set(Some(Err(why.to_string())));
    }

    /// OCR runs `recognize` on each page it is handed.
    pub(crate) fn fake(recognize: impl FnMut(&PageBitmap) -> Vec<OcrLine> + 'static) {
        SEAM.set(Some(Ok(Box::new(recognize))));
    }

    pub(crate) fn clear() {
        SEAM.set(None);
    }

    pub(super) fn ready() -> Option<Result<(), String>> {
        SEAM.with_borrow(|s| {
            s.as_ref()
                .map(|r| r.as_ref().map(|_| ()).map_err(Clone::clone))
        })
    }

    pub(super) fn recognize(bitmap: &PageBitmap) -> Option<Result<Vec<OcrLine>, String>> {
        SEAM.with_borrow_mut(|s| {
            s.as_mut().map(|r| match r {
                Ok(f) => Ok(f(bitmap)),
                Err(why) => Err(why.clone()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{cache::IdleEngineCache, ocr_intra_threads_for};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn issue_475_auto_ocr_threads_are_bounded_for_daemons() {
        assert_eq!(ocr_intra_threads_for(0, 24), 4);
        assert_eq!(ocr_intra_threads_for(0, 8), 4);
        assert_eq!(ocr_intra_threads_for(0, 2), 2);
        assert_eq!(ocr_intra_threads_for(0, 0), 1);
        // Explicit override is honored up to 64.
        assert_eq!(ocr_intra_threads_for(6, 24), 6);
        assert_eq!(ocr_intra_threads_for(128, 24), 64);
    }

    #[test]
    fn issue_475_idle_engine_cache_evicts_and_stops_reaper_when_empty() {
        static CACHE: IdleEngineCache<DropProbe> = IdleEngineCache::new();
        static DROPS: AtomicUsize = AtomicUsize::new(0);

        struct DropProbe;
        impl Drop for DropProbe {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        let ttl = Duration::from_millis(60);
        let first = CACHE
            .get_or_init(0, ttl, || Ok(DropProbe))
            .expect("initial load");
        assert_eq!(CACHE.loaded_count(), 1);
        assert!(CACHE.reaper_active());
        // Repeat hit reuses the cached engine without rebuilding.
        let second = CACHE
            .get_or_init(0, ttl, || Err("should not rebuild while warm".into()))
            .expect("warm hit");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        drop(first);
        drop(second);

        // Wait for idle eviction + reaper exit.
        let deadline = Instant::now() + Duration::from_secs(2);
        while (CACHE.loaded_count() > 0 || CACHE.reaper_active()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(CACHE.loaded_count(), 0, "engine should be evicted on idle");
        assert!(
            !CACHE.reaper_active(),
            "reaper thread should exit once all slots are empty"
        );
        assert_eq!(DROPS.load(Ordering::SeqCst), 1, "engine must be dropped");

        // Reload after eviction works cleanly.
        let _reloaded = CACHE
            .get_or_init(0, Duration::ZERO, || Ok(DropProbe))
            .expect("reload after eviction");
        assert_eq!(CACHE.loaded_count(), 1);
        assert!(
            !CACHE.reaper_active(),
            "ttl=0 keeps engine without spawning a reaper"
        );
    }
}
