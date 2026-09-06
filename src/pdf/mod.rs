//! DonSheet : the PDF extraction engine.
//!
//! Bytes in, DonSift blocks out. See `design/pdf.md` for the full
//! architecture. This module owns: the PDFium FFI boundary (`sys` +
//! `engine`), the geometry pipeline (line assembly, reading order,
//! furniture), semantic block classification, and the honest-flag
//! detection (encrypted / scanned / corrupt / vertical).
//!
//! Sibling naming: DonShadow fetches the bytes, DonSheet reads them.

pub mod blocks;
pub mod engine;
pub mod forms;
pub mod fusion;
pub mod layout;
pub mod ocr;
pub mod pixels;
pub mod reading;
pub mod rotate;
pub mod sys;
pub mod tables;

use crate::extract::blocks::Block;
use crate::extract::metadata::Meta;

pub use engine::{LoadError, OutlineItem};

/// Hard input ceiling (server-side fetched PDFs can be huge).
const DEFAULT_MAX_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug)]
pub enum PdfFailure {
    Encrypted,
    Corrupt(String),
    TooLarge(usize),
    NotPdf,
}

impl std::fmt::Display for PdfFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PdfFailure::Encrypted => write!(f, "pdf: encrypted document (password required)"),
            PdfFailure::Corrupt(msg) => write!(f, "pdf: {msg}"),
            PdfFailure::TooLarge(n) => {
                write!(
                    f,
                    "pdf: document exceeds size limit ({} MB)",
                    n / 1024 / 1024
                )
            }
            PdfFailure::NotPdf => write!(f, "pdf: bytes do not look like a PDF"),
        }
    }
}

impl std::error::Error for PdfFailure {}

/// What a fully-parsed PDF yields: blocks + trust metadata.
#[allow(dead_code)] // outline/page_count/lang/images/fonts wire into MCP meta
pub struct ParsedPdf {
    pub blocks: Vec<Block>,
    pub meta: Meta,
    pub outline: Vec<OutlineItem>,
    pub page_count: usize,
    /// Agent-visible notes (scanned pages, unsupported lanes...).
    pub notes: Vec<String>,
    /// Language code ("en", "ja", ...), best-effort.
    pub lang: String,
    /// Full language fingerprints for focus/tokenization reuse.
    pub lang_info: crate::extract::language::LanguageInfo,
    pub images: usize,
    pub fonts: Vec<String>,
    /// Per-page extraction stats. Block merging deliberately
    /// flows paragraphs across page breaks for reading
    /// continuity : this is where page boundaries (and
    /// per-page text trust) are preserved instead.
    pub pages_meta: Vec<PageMeta>,
}

/// One page's extraction outcome, surfaced to the agent.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PageMeta {
    /// 0-based page index.
    pub page: usize,
    /// Visible chars extracted from this page.
    pub chars: usize,
    /// True when the text came from OCR, not the text layer.
    pub ocr: bool,
    /// 0.0..1.0 text trust: glyph-layer trust (non-PUA ratio)
    /// or OCR mean confidence for OCR pages.
    pub confidence: f32,
}

/// Normalize a PDF date ("D:20260525080808+00'00'") to YYYY-MM-DD.
fn pdf_date(raw: &Option<String>) -> Option<String> {
    let r = raw.as_ref()?;
    let d = r.strip_prefix("D:").unwrap_or(r);
    // Check the bytes, not a str slice: the Info dict is
    // producer-controlled and a localized date puts a multibyte
    // char inside the first 8 bytes, where `d[..8]` would panic.
    // All-ASCII-digit bytes make the slices below boundary-safe.
    let leading_digits = d
        .as_bytes()
        .get(..8)
        .is_some_and(|b| b.iter().all(u8::is_ascii_digit));
    if leading_digits {
        Some(format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]))
    } else if r.trim().is_empty() {
        None
    } else {
        Some(r.clone())
    }
}

/// Parse `bytes` into DonSift blocks with full honesty flags.
pub fn parse(bytes: &[u8]) -> Result<ParsedPdf, PdfFailure> {
    let limit = std::env::var("DONSETCH_PDF_MAX_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(DEFAULT_MAX_BYTES);
    if bytes.len() > limit {
        return Err(PdfFailure::TooLarge(bytes.len()));
    }

    let opts = engine::LoadOpts {
        want_pixels: true,
        want_forms: true,
        dpi: 96.0,
    };
    let mut pages: Vec<layout::PageLines> = Vec::new();
    let mut raw_chars_total = 0usize;
    let mut all_widgets: Vec<(usize, forms::FormWidget)> = Vec::new();
    let mut rotated_pages = 0usize;
    let (raw, ()) = match engine::load_document(bytes, &opts, |input| {
        let engine::PageInput {
            chars,
            bitmap,
            widgets,
        } = input;
        raw_chars_total += chars.chars.len();
        // Trust audit BEFORE canonicalization (rotation does not change it,
        // but freeze the conceptual order: audit the raw stream).
        let trust = fusion::glyph_trust(&chars);
        // Orientation canonicalization: one pipeline for every frame.
        let mut chars = chars;
        let rot = rotate::canonicalize(&mut chars);
        if rot.is_some() {
            rotated_pages += 1;
        }
        let bitmap = match (rot, bitmap) {
            (Some(q), Some(b)) => {
                let quarters = (q / 90.0).round() as i32;
                let (w, h, buf) = rotate::rotate_bitmap_quarters(b.w, b.h, &b.buf, quarters);
                let (pw, ph) = if quarters.rem_euclid(2) == 1 {
                    (b.page_h_pt, b.page_w_pt)
                } else {
                    (b.page_w_pt, b.page_h_pt)
                };
                Some(pixels::PageBitmap {
                    w,
                    h,
                    buf,
                    page_w_pt: pw,
                    page_h_pt: ph,
                })
            }
            (_, b) => b,
        };
        let mut pl = layout::assemble(chars);
        pl.fusion = bitmap
            .as_ref()
            .map(|bmp| fusion::analyze_pixels(bmp, &pl))
            .map(|mut f| {
                f.garbage_ratio = trust;
                f
            })
            .or_else(|| {
                // No pixels (text-rich page) : still track glyph trust
                // for garbage detection (PUA glyph soup pages).
                Some(fusion::FusionData {
                    garbage_ratio: trust,
                    ..Default::default()
                })
            });
        for w in widgets {
            all_widgets.push((pl.index, w));
        }
        pages.push(pl);
    }) {
        Ok((r, _)) => (r, ()),
        Err(LoadError::Encrypted) => return Err(PdfFailure::Encrypted),
        Err(LoadError::NotPdf) => return Err(PdfFailure::NotPdf),
        Err(LoadError::Corrupt(code)) => {
            return Err(PdfFailure::Corrupt(format!(
                "corrupt document (pdfium error {code})"
            )));
        }
    };

    // ---- Tier B: OCR arbitration -----------------------------------------
    // Pages where the glyph stream failed any trust test get pixels→OCR.
    // Rec-space: CJK docs ride the universal recognizer; everyone else the
    // English one (digits/punct strongest there).
    let doc_cjk_hint = {
        let mut sample = String::new();
        for p in pages.iter().take(10) {
            for l in p.lines.iter().take(40) {
                sample.push_str(&l.text);
            }
        }
        let cjk = sample
            .chars()
            .filter(|c| {
                matches!(
                    crate::extract::language::char_script(*c),
                    crate::extract::language::Script::Han
                        | crate::extract::language::Script::Kana
                        | crate::extract::language::Script::Hangul
                )
            })
            .count();
        sample.chars().count() > 0 && cjk * 10 > sample.chars().count()
    };
    // Rec-space arbitration: FONT NAMES are the honest script signal
    // (they survive broken ToUnicode maps; glyph codepoints don't).
    let font_names_l: String = raw
        .fonts
        .iter()
        .map(|f| f.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let script_hint: &str = if [
        "mangal",
        "nirmala",
        "devanagari",
        "kohinoor",
        "aparajita",
        "gargi",
        "kalimati",
        "preeti",
        "kantipur",
    ]
    .iter()
    .any(|k| font_names_l.contains(k))
    {
        "deva"
    } else if doc_cjk_hint
        || [
            "simsun",
            "simhei",
            "kaiti",
            "ms gothic",
            "ms mincho",
            "meiryo",
            "malgun",
            "nanum",
            "gulim",
            "noto sans cjk",
            "wenquanyi",
            "uming",
            "ukai",
        ]
        .iter()
        .any(|k| font_names_l.contains(k))
    {
        "zh"
    } else {
        // Unknown script: the OCR confidence cascade decides live.
        "auto"
    };
    let mut needs_ocr: Vec<usize> = Vec::new();
    for p in &pages {
        let inkful = p
            .fusion
            .as_ref()
            .map(|f| f.has_visual_content && f.ink_ratio > 0.002)
            .unwrap_or(p.images > 0);
        let garbage = p
            .fusion
            .as_ref()
            .map(|f| f.garbage_ratio < 0.5)
            .unwrap_or(false);
        if (p.lines.is_empty() && inkful) || garbage {
            needs_ocr.push(p.index);
        }
    }
    let mut ocr_pages = 0usize;
    let mut ocr_conf_sum = 0f32;
    let mut ocr_conf_pages = 0usize;
    let mut ocr_failed = false;
    let mut decided_hint: Option<&'static str> = None;
    // Per-page OCR confidence for PageMeta (final-text trust).
    let mut ocr_page_conf: std::collections::HashMap<usize, f32> = std::collections::HashMap::new();
    if std::env::var("DONSHEET_DEBUG").is_ok() {
        eprintln!(
            "[ocr] candidates: {:?} (enabled={})",
            needs_ocr,
            ocr::enabled()
        );
    }
    if !needs_ocr.is_empty() && ocr::enabled() {
        needs_ocr.truncate(ocr::max_pages());
        match engine::rasterize_pages(bytes, &needs_ocr, 150.0) {
            Ok(bitmaps) => {
                for (slot, pi) in needs_ocr.iter().enumerate() {
                    let Some(bmp) = &bitmaps[slot] else {
                        continue;
                    };
                    let t0 = std::time::Instant::now();
                    let hint = decided_hint.unwrap_or(script_hint);
                    match ocr::ocr_page(bmp, hint) {
                        Ok((olines, kind)) if !olines.is_empty() => {
                            // First page of a cascaded doc locks the winner.
                            if decided_hint.is_none() {
                                decided_hint = Some(kind);
                                if std::env::var("DONSHEET_DEBUG").is_ok() {
                                    eprintln!("[ocr] recognizer locked: {kind}");
                                }
                            }
                            if std::env::var("DONSHEET_DEBUG").is_ok() {
                                eprintln!(
                                    "[ocr] page {pi}: {} lines in {:?}",
                                    olines.len(),
                                    t0.elapsed()
                                );
                            }
                            let conf = ocr::mean_confidence(&olines);
                            ocr_conf_sum += conf;
                            ocr_conf_pages += 1;
                            ocr_page_conf.insert(*pi, conf);
                            let (pw, ph) = (pages[*pi].width, pages[*pi].height);
                            let pc = ocr::lines_to_chars(&olines, *pi, pw, ph);
                            let mut pl = layout::assemble(pc);
                            pl.fusion = pages[*pi].fusion.clone();
                            if let Some(f) = pl.fusion.as_mut() {
                                f.garbage_ratio = 1.0;
                            }
                            pl.images = pages[*pi].images;
                            pages[*pi] = pl;
                            ocr_pages += 1;
                        }
                        Ok((_olines, _kind)) => {
                            if std::env::var("DONSHEET_DEBUG").is_ok() {
                                eprintln!(
                                    "[ocr] page {pi}: zero usable lines in {:?}",
                                    t0.elapsed()
                                );
                            }
                        }
                        Err(e) => {
                            ocr_failed = true;
                            if std::env::var("DONSHEET_DEBUG").is_ok() {
                                eprintln!("[ocr] page {pi} failed: {e}");
                            }
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                if std::env::var("DONSHEET_DEBUG").is_ok() {
                    eprintln!("[ocr] rasterize_pages failed: {e:?}");
                }
            }
        }
    }

    let mut notes: Vec<String> = Vec::new();
    if ocr_pages > 0 {
        let mean = if ocr_conf_pages > 0 {
            ocr_conf_sum / ocr_conf_pages as f32
        } else {
            0.0
        };
        notes.push(format!(
            "{ocr_pages} page(s) were OCR'd (no usable text layer); mean recognition confidence {:.0}% : read numbers and proper nouns with care",
            mean * 100.0
        ));
    } else if ocr_failed && !needs_ocr.is_empty() {
        notes.push(format!(
            "{} page(s) have no usable text layer and OCR was unavailable/failed (download issue); content there is missing",
            needs_ocr.len()
        ));
    }

    // ---- Tier A notes: scanned detection runs AFTER OCR arbitration. ----
    // A page that OCR recovered is NOT a dead scan anymore.
    let images: usize = pages.iter().map(|p| p.images).sum();
    let mut scanned_pages = 0usize;
    for p in &pages {
        let inkful = p
            .fusion
            .as_ref()
            .map(|f| f.has_visual_content && f.ink_ratio > 0.002)
            .unwrap_or(p.images > 0);
        if p.lines.is_empty() && inkful {
            scanned_pages += 1;
        }
    }
    if scanned_pages == raw.page_count && scanned_pages > 0 {
        notes.push(format!(
            "scanned/image-only PDF ({} pages): no text layer could be recovered; the pages could not be extracted",
            raw.page_count
        ));
    } else if scanned_pages > 0 {
        notes.push(format!(
            "{scanned_pages} of {} pages have no text layer and OCR did not recover them (content may be incomplete)",
            raw.page_count
        ));
    }
    if pages.iter().all(|p| p.lines.is_empty()) && images == 0 && raw.page_count > 0 {
        notes.push("no extractable text found in this PDF".to_string());
    }
    if rotated_pages > 0 {
        notes.push(format!(
            "{rotated_pages} page(s) had vertical/rotated text and were frame-canonicalized (orientation handled transparently)"
        ));
    }
    let garbage_pages = pages
        .iter()
        .filter(|p| {
            p.fusion
                .as_ref()
                .map(|f| f.garbage_ratio < 0.6)
                .unwrap_or(false)
        })
        .count();
    if garbage_pages > 0 {
        notes.push(format!(
            "{garbage_pages} page(s) have broken font encodings (PUA glyph soup); text there is best-effort"
        ));
    }

    // Running heads / footers.
    reading::suppress_furniture(&mut pages);

    // Reading order per page. Vertical/rotated pages are an honest
    // flagged lane (best-effort; no column reconstruction in v1).
    let dbg = std::env::var("DONSHEET_DEBUG").is_ok();
    if dbg {
        eprintln!("[parse] reading order start: {} pages", pages.len());
    }
    let mut vertical_pages = 0usize;
    let mut ordered_by_page: Vec<Vec<layout::Line>> = Vec::with_capacity(pages.len());
    for p in &pages {
        // Mixed-orientation remnant note: full-page verticality is already
        // canonicalized; what survives here is angled furniture in an
        // otherwise-horizontal page.
        if reading::is_vertical_suspect(p) {
            vertical_pages += 1;
        }
        if dbg {
            eprintln!(
                "[parse] page {} ordering ({} lines)",
                p.index,
                p.lines.len()
            );
        }
        let mut ordered = match p.fusion.as_ref() {
            Some(f) if !f.regions.is_empty() => fusion::reading_order(p, f),
            _ => reading::page_order(p.lines.clone()),
        };
        // Splice form widgets at their visual position (they render as
        // list items : the markdown_line() already starts with "- ").
        let pw = p.height;
        for (wi, w) in all_widgets.iter().enumerate() {
            if w.0 != p.index {
                continue;
            }
            let _ = wi;
            let w = &w.1;
            let y0 = (pw - w.top).max(0.0);
            let line = layout::Line {
                text: w.markdown_line(),
                words: vec![layout::Word {
                    text: w.name.clone(),
                    x0: w.left,
                    x1: w.right,
                }],
                x0: w.left,
                y0,
                x1: w.right.max(w.left + 20.0),
                y1: y0 + 10.0,
                size: 10.0,
                weight: 400,
                italic: false,
                mono: false,
                font: 0,
                glyphs: 8,
                order: u32::MAX / 2,
                page: p.index,
            };
            let pos = ordered
                .iter()
                .position(|l| l.y0 > y0)
                .unwrap_or(ordered.len());
            ordered.insert(pos, line);
        }
        ordered_by_page.push(ordered);
    }
    if vertical_pages > 0 {
        notes.push(format!(
            "{vertical_pages} of {} page(s) contain vertical or rotated text : vertical text extraction is best-effort and may be out of order",
            raw.page_count
        ));
    }

    // Font context across the document.
    let all_lines: Vec<&layout::Line> = ordered_by_page.iter().flat_map(|v| v.iter()).collect();
    let ctx = blocks::font_ctx(&all_lines);

    // Semantic blocks.
    if dbg {
        eprintln!("[parse] classify start");
    }
    let doc_blocks = blocks::classify(&pages, &ordered_by_page, &ctx);
    if dbg {
        eprintln!("[parse] classify end: {} blocks", doc_blocks.len());
    }

    // Language sniff on the produced text.
    let mut sample = String::new();
    for l in all_lines.iter().take(400) {
        sample.push_str(&l.text);
        sample.push(' ');
        if sample.len() > 24_000 {
            break;
        }
    }
    let lang_info = crate::extract::language::detect_from_text(&sample);

    let meta = Meta {
        title: raw.meta.title.clone(),
        byline: raw.meta.author.clone(),
        published: pdf_date(&raw.meta.created).or(pdf_date(&raw.meta.modified)),
        site: None,
        description: raw.meta.subject.clone(),
        canonical: None,
    };

    // Per-page stats from the FINAL text state (post-OCR):
    // chars, ocr flag, and the trust of whichever engine
    // produced the text. Glyph pages without fusion data are
    // ordinary text pages : trust 1.0.
    let pages_meta = pages
        .iter()
        .map(|p| {
            let chars: usize = p.lines.iter().map(|l| l.text.chars().count()).sum();
            let ocr = ocr_page_conf.contains_key(&p.index);
            let confidence = if ocr {
                ocr_page_conf[&p.index]
            } else {
                p.fusion.as_ref().map(|f| f.garbage_ratio).unwrap_or(1.0)
            };
            PageMeta {
                page: p.index,
                chars,
                ocr,
                confidence: confidence.clamp(0.0, 1.0),
            }
        })
        .collect();

    Ok(ParsedPdf {
        blocks: doc_blocks,
        meta,
        outline: raw.outline,
        page_count: raw.page_count,
        notes,
        lang: lang_info.code.clone(),
        lang_info,
        images,
        fonts: raw.fonts,
        pages_meta,
    })
}

#[cfg(test)]
mod tests;
