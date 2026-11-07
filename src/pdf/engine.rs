//! Safe wrapper over the PDFium C API.
//!
//! This module is the ONLY place `unsafe` PDFium calls live. Every call
//! is serialized behind a single global lock (`core()`) because PDFium
//! keeps process-global state and is not thread-safe. Handles never
//! escape: a document is loaded, walked, and closed entirely inside
//! `load_document`, which hands plain Rust data to the caller.
//!
//! Coordinates are normalized to screen space (origin top-left, y grows
//! downward, points) using the page height at load time : the rest of
//! the pipeline never has to think about PDF's bottom-left origin.

#![allow(dead_code)]
use std::ffi::{CString, c_char, c_void};
use std::os::raw::c_ulong;
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::sys::*;

/// One extracted character, normalized to screen-space points.
#[derive(Clone, Debug)]
pub struct PdfChar {
    /// The unicode scalar (private-use / unmapped glyphs become '\0').
    pub cp: char,
    /// Normalized bbox: x0 left, y0 top, x1 right, y1 bottom (screen space).
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    /// Glyph bounds including side bearings, in the same normalized frame.
    /// Used only for spacing; ink boxes retain layout/table geometry.
    pub spacing_box: Option<[f32; 4]>,
    /// Font size in points.
    pub size: f32,
    /// Font weight 100..900 (-1 when unknown).
    pub weight: i32,
    /// PDF descriptor flags (sys::FONT_*).
    pub flags: u32,
    /// Interned font family name index (into RawDoc.fonts).
    pub font: u16,
    /// Text rotation in degrees (0 = normal horizontal).
    pub angle: f32,
    /// Character index order in the content stream.
    pub order: u32,
    /// Dingbat-font glyph (checkbox/seal art : picture, not text).
    pub dingbat: bool,
    /// Canonicalized from a rotated/vertical run (rotate.rs set it).
    pub rt: bool,
    /// Text came from the OCR tier, not the glyph stream.
    pub ocr: bool,
}

impl PdfChar {
    pub fn spacing_x(&self) -> (f32, f32) {
        self.spacing_box
            .map(|b| (b[0], b[2]))
            .unwrap_or((self.x0, self.x1))
    }
}

pub struct PageChars {
    pub index: usize,
    pub width: f32,
    pub height: f32,
    pub chars: Vec<PdfChar>,
    /// Count of image page-objects (for scanned-PDF detection).
    pub images: usize,
}

#[derive(Clone, Debug, Default)]
pub struct RawMeta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
}

#[allow(dead_code)] // consumed when outline wires into MCP meta
#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub level: usize,
    pub page: Option<usize>,
}

#[derive(Default)]
pub struct RawDoc {
    pub fonts: Vec<String>,
    pub meta: RawMeta,
    pub outline: Vec<OutlineItem>,
    pub page_count: usize,
    /// Pages actually read: `page_count` capped at `MAX_PAGES`.
    pub pages_read: usize,
}

/// Pages parsed per document. Each page is a rasterization (up to
/// 80 M px) plus the text, layout and table passes; the size gate
/// (`pdf_max_mb`, 100 MB) admits a document of a few hundred
/// thousand near-empty pages, which is hours of work for a note that
/// says "scanned". The remainder is reported, not read.
pub const MAX_PAGES: usize = 500;

/// What the wrapper itself can fail with.
#[derive(Debug)]
pub enum LoadError {
    /// PDFium reported a password is required.
    Encrypted,
    /// PDFium could not parse the bytes (corrupt / unsupported).
    Corrupt(u32),
    /// Not actually a PDF (magic sniff mismatch upstream would
    /// normally catch this; defended here too).
    NotPdf,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Encrypted => write!(f, "encrypted PDF (password required)"),
            LoadError::Corrupt(code) => write!(f, "PDF load failed (pdfium error {code})"),
            LoadError::NotPdf => write!(f, "not a PDF document"),
        }
    }
}

impl std::error::Error for LoadError {}

/// The global PDFium core. Initialized once, destroyed never (the
/// process owns it; calling DestroyLibrary mid-process can tear down
/// state other threads still reference).
pub struct PdfiumCore {
    _priv: (),
}

static CORE: OnceLock<Mutex<PdfiumCore>> = OnceLock::new();

pub fn core() -> MutexGuard<'static, PdfiumCore> {
    CORE.get_or_init(|| {
        unsafe {
            FPDF_InitLibraryWithConfig(&FpdfLibraryConfig {
                version: 2,
                m_pUserFontPaths: std::ptr::null_mut(),
                m_pIsolate: std::ptr::null_mut(),
                m_pV8EmbedderSlot: std::ptr::null_mut(),
            });
        }
        Mutex::new(PdfiumCore { _priv: () })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// Decode a UTF-16LE buffer returned by PDFium meta/bookmark APIs.
/// `len_units` includes the terminating NUL when > 0.
fn decode_utf16(buf: &[u16], len_units: usize) -> String {
    let end = if len_units > 0 {
        len_units.saturating_sub(1).min(buf.len())
    } else {
        0
    };
    String::from_utf16_lossy(&buf[..end])
}

/// Decode a UTF-16LE buffer using a PDFium-reported BYTE count, as
/// returned by `FPDF_GetMetaText`/`FPDFBookmark_GetTitle` (both
/// count bytes, including the NUL terminator (see their headers in
/// fpdf_doc.h). `decode_utf16` above takes a UNIT count instead;
/// passing a byte count straight through reads past the real string
/// into the buffer's zero-initialized tail, appending trailing NULs
/// (the buffers here are always over-allocated by a few units past
/// what the byte count implies, matching the sibling `field_string`
/// helper in forms.rs, which does this division correctly).
fn decode_utf16_from_byte_count(buf: &[u16], byte_count: usize) -> String {
    decode_utf16(buf, (byte_count / 2).min(buf.len()))
}

/// Fonts whose glyphs are pictures, not text (checkboxes, seals,
/// logo art). Detected once at intern time.
pub fn is_dingbat_family(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("wingdings")
        || n.contains("zapf")
        || n.contains("webdings")
        || n.contains("marlett")
        || n.contains("monotype sorts")
        || n.contains("ms outlook")
        || n.contains("bookdings")
}

/// Mark a font as monospace when its NAME says so (the descriptor's
/// FixedPitch bit is dropped by many subset pipelines : books &
/// guides embed mono fonts without it).
pub const DONSETCH_MONO_HINT: u32 = 0x8000_0000;

fn mono_font_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("mono")
        || n.contains("courier")
        || n.contains("consol")
        || n.contains("code")
        || n.contains("menlo")
        || n.contains("monaco")
        || n.contains("terminal")
        || n.contains("typewriter")
        || n.contains("mplus1m")
        || n.contains("source code")
        || n.contains("inconsolata")
        || n.contains("jetbrains")
        || n.contains("fira code")
}

/// Is the glyph's font family a monospace font? `family` is an index
/// into the interned `fonts` list, but it falls back to 0 whenever a
/// glyph's font has no PDFium-reportable name (Type3, no BaseFont, an
/// over-long name), and `fonts` is empty until the first NAMED font
/// is interned. So a crafted PDF whose first walked glyph uses an
/// unnamed font reaches here with family==0 and an empty `fonts` — a
/// direct `fonts[family]` index panicked (release panic=abort → a
/// remote DoS on a fetched PDF). Bounds-check like the sibling
/// `font_dingbat.get(family)` a few lines down already does.
fn font_is_mono(fonts: &[String], family: u16) -> bool {
    fonts
        .get(family as usize)
        .is_some_and(|name| mono_font_name(name))
}

/// Fill `buf` with a meta tag value; returns None when empty.
fn get_meta(doc: FpdfDocument, tag: &str, buf: &mut Vec<u16>) -> Option<String> {
    let tag = CString::new(tag).ok()?;
    unsafe {
        buf.clear();
        buf.resize(512, 0);
        let n =
            FPDF_GetMetaText(doc, tag.as_ptr(), buf.as_mut_ptr() as *mut c_void, 512 * 2) as usize;
        if n < 2 {
            return None;
        }
        if n > 512 * 2 {
            buf.resize(n / 2 + 2, 0);
            let n2 = FPDF_GetMetaText(
                doc,
                tag.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as c_ulong,
            ) as usize;
            if n2 < 2 {
                return None;
            }
        }
        let s = decode_utf16_from_byte_count(buf, n);
        let cleaned: String = s.chars().filter(|&c| c != '\0').collect();
        let s = cleaned.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }
}

fn bookmark_title(bm: FpdfBookmark) -> String {
    unsafe {
        let n = FPDFBookmark_GetTitle(bm, std::ptr::null_mut(), 0) as usize;
        if !(2..=64 * 1024).contains(&n) {
            return String::new();
        }
        let mut buf = vec![0u16; n / 2 + 2];
        let n2 = FPDFBookmark_GetTitle(
            bm,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 2) as c_ulong,
        ) as usize;
        decode_utf16_from_byte_count(&buf, n2)
    }
}

/// Walk the outline tree depth-first, flattened with levels. Returns at
/// most 10_001 items over at most 13 levels. An item reached a second
/// time through /Next or /First (a cycle in a malformed file) is not
/// listed again, and the chain that reached it ends there.
fn walk_outlines(doc: FpdfDocument) -> Vec<OutlineItem> {
    let mut out = Vec::new();
    fn walk_level(
        doc: FpdfDocument,
        parent: FpdfBookmark,
        level: usize,
        out: &mut Vec<OutlineItem>,
        seen: &mut std::collections::HashSet<FpdfBookmark>,
    ) {
        if level > 12 || out.len() > 10_000 {
            return;
        }
        let mut bm = unsafe { FPDFBookmark_GetFirstChild(doc, parent) };
        // fpdf_doc.h leaves circular bookmark references to the caller.
        // In a malformed file a /Next cycle would keep this loop going
        // forever, and a /First cycle would relist its items down to the
        // depth cap. Stop at a handle already walked, and check the item
        // cap on every item, not only when a level starts, so the walk
        // stays bounded even if a repeat ever came back under a
        // different handle.
        while !bm.is_null() && out.len() <= 10_000 && seen.insert(bm) {
            let title = bookmark_title(bm);
            let dest = unsafe { FPDFBookmark_GetDest(doc, bm) };
            let page = if !dest.is_null() {
                let idx = unsafe { FPDFDest_GetDestPageIndex(doc, dest) };
                if idx >= 0 { Some(idx as usize) } else { None }
            } else {
                None
            };
            out.push(OutlineItem { title, level, page });
            walk_level(doc, bm, level + 1, out, seen);
            bm = unsafe { FPDFBookmark_GetNextSibling(doc, bm) };
        }
    }
    let mut seen = std::collections::HashSet::new();
    walk_level(doc, std::ptr::null_mut(), 0, &mut out, &mut seen);
    out
}

/// What beyond raw chars a load should gather per page.
#[derive(Clone, Copy, Debug)]
pub struct LoadOpts {
    /// Rasterize each page (96dpi + annotation layer) for the pixel
    /// engine. Lazy callers (raw dumps) keep this off.
    pub want_pixels: bool,
    /// Enumerate AcroForm widgets per page.
    pub want_forms: bool,
    /// DPi override (0.0 = 96). Higher for OCR; forms do not need this.
    pub dpi: f32,
    /// Pages read per document; `MAX_PAGES` unless a test says less.
    pub max_pages: usize,
}

impl Default for LoadOpts {
    fn default() -> Self {
        Self {
            want_pixels: false,
            want_forms: false,
            dpi: 0.0,
            max_pages: MAX_PAGES,
        }
    }
}

/// The per-page bundle the sink receives. Pixel/form extras empty unless
/// requested via LoadOpts.
pub struct PageInput {
    pub chars: PageChars,
    /// Rasterized page (transient : treat as scratch, extract what you
    /// need during the sink call).
    pub bitmap: Option<super::pixels::PageBitmap>,
    pub widgets: Vec<super::forms::FormWidget>,
}

/// Rasterize an already-loaded page (white background, annotations on).
/// Must be called while the global core lock is held.
fn rasterize_page(
    page: FpdfPage,
    width_pt: f32,
    height_pt: f32,
    dpi: f32,
) -> Option<super::pixels::PageBitmap> {
    let dpi = if dpi <= 0.0 { 96.0 } else { dpi };
    let scale = dpi / 72.0;
    let w = ((width_pt * scale).ceil() as i32).max(1);
    let h = ((height_pt * scale).ceil() as i32).max(1);
    // Guard against pathological page sizes.
    if (w as i64) * (h as i64) > 80_000_000 {
        return None;
    }
    unsafe {
        let bmp = FPDFBitmap_Create(w, h, 1);
        if bmp.is_null() {
            return None;
        }
        FPDFBitmap_FillRect(bmp, 0, 0, w, h, 0xFFFF_FFFF);
        FPDF_RenderPageBitmap(
            bmp,
            page,
            0,
            0,
            w,
            h,
            0,
            FPDF_RENDER_FLAG_LCD_TEXT | FPDF_RENDER_FLAG_ANNOT,
        );
        let stride = FPDFBitmap_GetStride(bmp) as usize;
        let ptr = FPDFBitmap_GetBuffer(bmp) as *const u8;
        let (wu, hu) = (w as usize, h as usize);
        let mut buf = vec![0u8; wu * hu * 4];
        if !ptr.is_null() && stride >= wu * 4 {
            for row in 0..hu {
                std::ptr::copy_nonoverlapping(
                    ptr.add(row * stride),
                    buf.as_mut_ptr().add(row * wu * 4),
                    wu * 4,
                );
            }
        }
        FPDFBitmap_Destroy(bmp);
        Some(super::pixels::PageBitmap {
            w: wu,
            h: hu,
            buf,
            page_w_pt: width_pt,
            page_h_pt: height_pt,
        })
    }
}

/// Re-open a document and rasterize selected pages (OCR orchestration
/// needs high-dpi pixels after the main walk has closed the doc).
pub fn rasterize_pages(
    bytes: &[u8],
    indices: &[usize],
    dpi: f32,
) -> Result<Vec<Option<super::pixels::PageBitmap>>, LoadError> {
    if bytes.len() < 5 || !bytes.starts_with(b"%PDF-") {
        return Err(LoadError::NotPdf);
    }
    let _lock = core();
    unsafe {
        let doc = FPDF_LoadMemDocument64(
            bytes.as_ptr() as *const c_void,
            bytes.len(),
            std::ptr::null::<c_char>(),
        );
        if doc.is_null() {
            let code = FPDF_GetLastError();
            return Err(if code == FPDF_ERR_PASSWORD {
                LoadError::Encrypted
            } else {
                LoadError::Corrupt(code)
            });
        }
        struct DocGuard2(FpdfDocument);
        impl Drop for DocGuard2 {
            fn drop(&mut self) {
                unsafe { FPDF_CloseDocument(self.0) };
            }
        }
        let _guard = DocGuard2(doc);
        let count = FPDF_GetPageCount(doc).max(0) as usize;
        let mut out = Vec::with_capacity(indices.len());
        for &pi in indices {
            if pi >= count {
                out.push(None);
                continue;
            }
            let page = FPDF_LoadPage(doc, pi as i32);
            if page.is_null() {
                out.push(None);
                continue;
            }
            struct Pg2(FpdfPage);
            impl Drop for Pg2 {
                fn drop(&mut self) {
                    unsafe { FPDF_ClosePage(self.0) };
                }
            }
            let _pg = Pg2(page);
            let w = FPDF_GetPageWidthF(page) as f32;
            let h = FPDF_GetPageHeightF(page) as f32;
            out.push(rasterize_page(page, w, h, dpi));
        }
        Ok(out)
    }
}

/// Load `bytes` as a document, walking every page and converting chars
/// to the normalized pure-Rust model. `page_sink` receives each page's
/// bundle (and may transform it into the caller's per-page type `P`).
///
/// The PDFium lock is held for the entire walk : handles never escape.
#[allow(clippy::too_many_arguments)]
pub fn load_document<P, F>(
    bytes: &[u8],
    opts: &LoadOpts,
    mut page_sink: F,
) -> Result<(RawDoc, Vec<P>), LoadError>
where
    F: FnMut(PageInput) -> P,
{
    if bytes.len() < 5 || !bytes.starts_with(b"%PDF-") {
        return Err(LoadError::NotPdf);
    }
    let _lock = core();
    unsafe {
        let doc = FPDF_LoadMemDocument64(
            bytes.as_ptr() as *const c_void,
            bytes.len(),
            std::ptr::null::<c_char>(),
        );
        if doc.is_null() {
            let code = FPDF_GetLastError();
            return Err(if code == FPDF_ERR_PASSWORD {
                LoadError::Encrypted
            } else {
                LoadError::Corrupt(code)
            });
        }
        // RAII: guaranteed close on all paths below.
        struct DocGuard(FpdfDocument);
        impl Drop for DocGuard {
            fn drop(&mut self) {
                unsafe { FPDF_CloseDocument(self.0) };
            }
        }
        let guard = DocGuard(doc);

        // Optional form-fill environment for widget enumeration.
        struct FormGuard(FpdfFormhandle);
        impl Drop for FormGuard {
            fn drop(&mut self) {
                unsafe { FPDFDOC_ExitFormFillEnvironment(self.0) };
            }
        }
        let mut finfo = FpdfFormfillInfo {
            version: 2,
            _callbacks: [0; 128],
        };
        let _form_guard = if opts.want_forms {
            let h = FPDFDOC_InitFormFillEnvironment(doc, &mut finfo);
            if h.is_null() {
                None
            } else {
                Some(FormGuard(h))
            }
        } else {
            None
        };

        let page_count = FPDF_GetPageCount(doc).max(0) as usize;
        let pages_read = page_count.min(opts.max_pages);
        let mut raw = RawDoc {
            fonts: Vec::new(),
            meta: RawMeta::default(),
            outline: walk_outlines(doc),
            page_count,
            pages_read,
        };
        let mut pages_out: Vec<P> = Vec::with_capacity(pages_read);
        let mut font_index: std::collections::HashMap<String, u16> =
            std::collections::HashMap::new();
        let mut font_dingbat: Vec<bool> = Vec::new();
        let mut mbuf: Vec<u16> = Vec::with_capacity(512);
        raw.meta.title = get_meta(doc, "Title", &mut mbuf);
        raw.meta.author = get_meta(doc, "Author", &mut mbuf);
        raw.meta.subject = get_meta(doc, "Subject", &mut mbuf);
        raw.meta.keywords = get_meta(doc, "Keywords", &mut mbuf);
        raw.meta.creator = get_meta(doc, "Creator", &mut mbuf);
        raw.meta.producer = get_meta(doc, "Producer", &mut mbuf);
        raw.meta.created = get_meta(doc, "CreationDate", &mut mbuf);
        raw.meta.modified = get_meta(doc, "ModDate", &mut mbuf);

        for pi in 0..pages_read {
            let page = FPDF_LoadPage(doc, pi as i32);
            if page.is_null() {
                continue;
            }
            struct PageGuard(FpdfPage);
            impl Drop for PageGuard {
                fn drop(&mut self) {
                    unsafe { FPDF_ClosePage(self.0) };
                }
            }
            let _pguard = PageGuard(page);

            let width = FPDF_GetPageWidthF(page) as f32;
            let height = FPDF_GetPageHeightF(page) as f32;

            // Image-object count (scanned detection).
            let mut images = 0usize;
            let nobj = FPDFPage_CountObjects(page);
            for oi in 0..nobj.clamp(0, 4096) {
                let obj = FPDFPage_GetObject(page, oi);
                if !obj.is_null() && FPDFPageObj_GetType(obj) == FPDF_PAGEOBJ_IMAGE {
                    images += 1;
                }
            }

            let mut chars = Vec::new();
            let tp = FPDFText_LoadPage(page);
            if !tp.is_null() {
                struct TpGuard(FpdfTextpage);
                impl Drop for TpGuard {
                    fn drop(&mut self) {
                        unsafe { FPDFText_ClosePage(self.0) };
                    }
                }
                let _tg = TpGuard(tp);
                let n = FPDFText_CountChars(tp).max(0) as usize;
                chars.reserve(n.min(262_144));
                for i in 0..n.min(1_048_576) {
                    let i32_i = i as i32;
                    let cp_raw = FPDFText_GetUnicode(tp, i32_i);
                    let cp = char::from_u32(cp_raw).unwrap_or('\0');
                    // Skip NUL / non-printable pseudo-chars pdfium emits
                    // for unmapped glyphs.
                    let (mut x0, mut x1, mut y_b, mut y_t) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
                    FPDFText_GetCharBox(tp, i32_i, &mut x0, &mut x1, &mut y_b, &mut y_t);
                    let mut loose = FsRect::default();
                    let spacing_box = if FPDFText_GetLooseCharBox(tp, i32_i, &mut loose) != 0
                        && [loose.left, loose.right, loose.bottom, loose.top]
                            .iter()
                            .all(|v| v.is_finite())
                        && loose.right > loose.left
                        && loose.top >= loose.bottom
                    {
                        Some([
                            loose.left,
                            height - loose.top,
                            loose.right,
                            height - loose.bottom,
                        ])
                    } else {
                        None
                    };
                    let size = FPDFText_GetFontSize(tp, i32_i);
                    let weight = FPDFText_GetFontWeight(tp, i32_i);

                    // Font size truth: GetFontSize degenerates to 1.0
                    // on Type3/matrix fonts (report generators). The
                    // text matrix scale is the reliable source:
                    // hypot(a,b) is the size for any rotation.
                    let mut m = FsMatrix::default();
                    FPDFText_GetMatrix(tp, i32_i, &mut m);
                    let mscale = (m.a * m.a + m.b * m.b).sqrt();
                    let angle = if m.a == 0.0 && m.b == 0.0 {
                        0.0
                    } else {
                        m.b.atan2(m.a).to_degrees()
                    };
                    if crate::config::cfg().debug.pdf_matrix && i < 8 {
                        crate::diag!(
                            "[matrix] i={i} a={:.3} b={:.3} c={:.3} d={:.3} angle={angle:.1}",
                            m.a,
                            m.b,
                            m.c,
                            m.d
                        );
                    }
                    let size = if size.is_finite() && size > 1.5 {
                        size as f32
                    } else if mscale > 0.5 {
                        mscale
                    } else {
                        0.0
                    };

                    // Font name (skip the roundtrip when flags are all we need).
                    let mut flags: i32 = 0;
                    let mut namebuf = [0u8; 256];
                    let need = FPDFText_GetFontInfo(
                        tp,
                        i32_i,
                        namebuf.as_mut_ptr() as *mut c_void,
                        namebuf.len() as c_ulong,
                        &mut flags,
                    ) as usize;
                    let family = if need > 0 && need <= namebuf.len() {
                        let nul = namebuf
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(namebuf.len());
                        let mut s = String::from_utf8_lossy(&namebuf[..nul]).into_owned();
                        // Strip the six-letter subset prefix ("ABCDEF+Font").
                        if let Some(p) = s.find('+')
                            && p <= 8
                        {
                            s = s[p + 1..].to_string();
                        }
                        match font_index.get(&s) {
                            Some(&idx) => idx,
                            None => {
                                let idx = if raw.fonts.len() < u16::MAX as usize {
                                    font_dingbat.push(is_dingbat_family(&s));
                                    raw.fonts.push(s.clone());
                                    (raw.fonts.len() - 1) as u16
                                } else {
                                    0
                                };
                                font_index.insert(s, idx);
                                idx
                            }
                        }
                    } else {
                        0
                    };

                    chars.push(PdfChar {
                        cp,
                        x0: x0 as f32,
                        y0: height - y_t.max(y_b) as f32,
                        x1: x1 as f32,
                        y1: height - y_t.min(y_b) as f32,
                        spacing_box,
                        size,
                        weight,
                        flags: flags as u32
                            | if font_is_mono(&raw.fonts, family) {
                                DONSETCH_MONO_HINT
                            } else {
                                0
                            },
                        font: family,
                        angle,
                        order: i as u32,
                        dingbat: font_dingbat.get(family as usize).copied().unwrap_or(false),
                        rt: false,
                        ocr: false,
                    });
                }
            }
            // Lazy pixel rendering: text-rich pages (200+ chars,
            // no images) don't need pixel fusion : the text layer
            // is sufficient for reading order and garbage detection.
            // This makes 50-page arXiv papers fast (no bitmap per page).
            let char_count = chars.len();
            let needs_pixels = char_count < 200 || images > 0;
            pages_out.push(page_sink(PageInput {
                chars: PageChars {
                    index: pi,
                    width,
                    height,
                    chars,
                    images,
                },
                bitmap: if opts.want_pixels && needs_pixels {
                    rasterize_page(page, width, height, opts.dpi)
                } else {
                    None
                },
                widgets: match &_form_guard {
                    Some(fg) => super::forms::collect_widgets(fg.0, page),
                    None => Vec::new(),
                },
            }));
            if crate::config::cfg().debug.pdf && pi % 50 == 0 {
                crate::diag!("[engine] page {pi}/{page_count} done");
            }
        }

        drop(guard);
        Ok((raw, pages_out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A glyph whose font has no PDFium-reportable name reaches the
    // mono check with family==0 while the interned `fonts` list is
    // still empty (nothing named has been interned yet). The old
    // direct index `fonts[family as usize]` panicked here — release
    // panic=abort turns that into a remote DoS on a fetched PDF.
    // font_is_mono must bounds-check and simply return false.
    #[test]
    fn font_is_mono_does_not_panic_on_empty_or_out_of_range_family() {
        assert!(!font_is_mono(&[], 0)); // the crash input: empty list, family 0
        assert!(!font_is_mono(&["Arial".to_string()], 7)); // out of range
        assert!(font_is_mono(&["Courier New".to_string()], 0)); // named mono resolves
        assert!(!font_is_mono(&["Arial".to_string()], 0)); // named non-mono
    }

    #[test]
    fn decode_utf16_from_byte_count_strips_terminator_and_padding() {
        // Mimics the real buffer shape at both call sites: "Hi" (2
        // UTF-16 units) + a NUL terminator (1 unit) written by
        // PDFium, followed by the caller's over-allocated slack
        // (the `n/2 + 2` sizing convention), left zero-initialized.
        let buf: Vec<u16> = vec![
            'H' as u16, 'i' as u16, 0x0000, // title + NUL terminator
            0x0000, 0x0000, // caller's slack padding
        ];
        // PDFium reports byte counts, not unit counts: a 2-char
        // title + its NUL terminator is (2 + 1) * 2 = 6 bytes.
        assert_eq!(decode_utf16_from_byte_count(&buf, 6), "Hi");
    }

    #[test]
    fn decode_utf16_from_byte_count_handles_empty_title() {
        let buf: Vec<u16> = vec![0x0000, 0x0000];
        // byte_count for just the NUL terminator: 1 unit * 2 = 2.
        assert_eq!(decode_utf16_from_byte_count(&buf, 2), "");
    }

    #[test]
    fn decode_utf16_from_byte_count_never_reads_past_the_buffer() {
        // A byte_count larger than the actual buffer must never
        // panic: defensive against a hostile/corrupt PDF causing
        // PDFium to report an implausible size.
        let buf: Vec<u16> = vec!['x' as u16, 0x0000];
        assert_eq!(decode_utf16_from_byte_count(&buf, 1_000_000), "x");
    }

    #[test]
    fn decode_utf16_from_byte_count_matches_unit_count_form() {
        // Regression for the actual bug: the byte-count form must
        // agree with calling decode_utf16 directly with the correct
        // UNIT count, not the byte count itself passed straight
        // through (which is what both call sites used to do).
        let buf: Vec<u16> = vec!['h' as u16, 'i' as u16, 0x0000, 0x0000];
        let byte_count = 6; // (2 chars + NUL) * 2
        let unit_count = 3; // 2 chars + NUL
        assert_eq!(
            decode_utf16_from_byte_count(&buf, byte_count),
            decode_utf16(&buf, unit_count)
        );
    }
}
