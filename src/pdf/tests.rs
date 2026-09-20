//! DonSheet corpus battery. Real-world PDFs in `tests/pdf-corpus/`
//! (not committed). Every assertion is an invariant proven during
//! the debugging campaign : they are regression gates, not hopes.

use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("pdf-corpus")
        .join(name)
}

/// Corpus files are not committed. Missing file → return None so
/// the test skips instead of failing.
fn read(name: &str) -> Option<Vec<u8>> {
    let p = corpus(name);
    match std::fs::read(&p) {
        Ok(b) => Some(b),
        Err(_) => {
            eprintln!("SKIP: {} missing (scripts/download-corpus.sh)", p.display());
            None
        }
    }
}

fn all_block_text(blocks: &[crate::extract::blocks::Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        match b {
            crate::extract::blocks::Block::Heading { text, .. } => out.push_str(text),
            crate::extract::blocks::Block::Para { md, .. } => out.push_str(md),
            crate::extract::blocks::Block::Code { code, .. } => out.push_str(code),
            crate::extract::blocks::Block::List { items, .. } => {
                for i in items {
                    out.push_str(i);
                }
            }
            crate::extract::blocks::Block::Table { headers, rows, .. } => {
                for h in headers {
                    out.push_str(h);
                }
                for r in rows {
                    for c in r {
                        out.push_str(c);
                    }
                }
            }
            crate::extract::blocks::Block::Quote { md, .. } => out.push_str(md),
            crate::extract::blocks::Block::Media { alt, .. } => out.push_str(alt),
        }
        out.push('\n');
    }
    out
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

// ---------- engine smoke ----------

#[test]
fn smoke_load_attention() {
    let Some(bytes) = read("attention.pdf") else {
        return;
    };
    let (raw, pages) = crate::pdf::engine::load_document(
        &bytes,
        &Default::default(),
        |p: crate::pdf::engine::PageInput| p.chars.chars.len(),
    )
    .expect("load failed");
    assert!(raw.page_count >= 4);
    assert_eq!(pages.len(), raw.page_count);
    let total_chars: usize = pages.iter().sum();
    assert!(total_chars > 5000);
    assert!(raw.fonts.len() > 2);
}

#[test]
fn smoke_scanned_detection() {
    let Some(bytes) = read("scanned.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("scanned pdf parses");
    assert!(
        parsed
            .notes
            .iter()
            .any(|n| n.contains("scanned") || n.contains("OCR")),
        "expected scanned/ocr note, got {:?}",
        parsed.notes
    );
    // When the OCR feature is compiled AND the model cache exists
    // locally, content must be recovered. Without the feature, OCR
    // can't run even if models are on disk from a previous build.
    #[cfg(feature = "ocr")]
    if crate::pdf::ocr::ocr_cache_dir()
        .join("en_pp-ocrv5_mobile_rec.onnx")
        .exists()
    {
        let all: String = parsed
            .blocks
            .iter()
            .map(|b| match b {
                crate::extract::blocks::Block::Para { md, .. } => md.clone(),
                crate::extract::blocks::Block::Heading { text, .. } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            all.contains("DonSheet") || all.contains("Scanned"),
            "ocr failed: {all:?}"
        );
    }
}

#[test]
fn smoke_vertical_flag() {
    let Some(bytes) = read("vertical.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("vertical pdf parses");
    assert!(
        parsed.notes.iter().any(|n| n.contains("vertical")),
        "expected vertical note, got {:?}",
        parsed.notes
    );
}

#[test]
fn smoke_not_pdf_honest() {
    let res = crate::pdf::parse(b"This is not a pdf at all, honestly.");
    assert!(matches!(res, Err(crate::pdf::PdfFailure::NotPdf)));
}

// ---------- academic 2-column (attention.pdf) ----------

#[test]
fn attention_structure() {
    let Some(bytes) = read("attention.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);

    // Headings present with correct levels.
    let headings: Vec<(u8, &str)> = parsed
        .blocks
        .iter()
        .filter_map(|b| {
            if let crate::extract::blocks::Block::Heading { level, text, .. } = b {
                Some((*level, text.as_str()))
            } else {
                None
            }
        })
        .collect();
    let h = |needle: &str| headings.iter().find(|(_, t)| t.contains(needle));
    assert!(
        h("Introduction").is_some(),
        "missing Intro heading: {:?}",
        headings
    );
    assert!(
        h("Model Architecture").is_some(),
        "missing Model Arch heading"
    );
    let scaled = h("Scaled Dot-Product Attention").expect("missing 3.2.1 heading");
    let model = h("Model Architecture").unwrap();
    assert!(
        scaled.0 > model.0,
        "3.2.1 must nest under 3: {:?} vs {:?}",
        scaled,
        model
    );

    // Abstract reads clean (regression for the phrase-shuffle bug).
    assert!(
        text.contains("We propose a new simple network architecture, the Transformer,"),
        "abstract scramble regression:\n{}",
        &text[..600.min(text.len())]
    );

    // No line duplication (regression for the double-append bug):
    // each physical line must appear once in reading order.
    assert_eq!(
        count_occurrences(&text, "The best performing models also connect"),
        1,
        "line duplication regression"
    );

    // Reading order across pages: sections must appear in order.
    let i1 = text.find("Background").expect("Background heading");
    let i2 = text.find("Model Architecture").expect("Model Arch");
    let i3 = text.find("Why Self-Attention").expect("Why Self-Attention");
    assert!(
        i1 < i2 && i2 < i3,
        "section order broken: {} {} {}",
        i1,
        i2,
        i3
    );

    // Table present (author or results tables).
    let n_tables = parsed
        .blocks
        .iter()
        .filter(|b| matches!(b, crate::extract::blocks::Block::Table { .. }))
        .count();
    assert!(n_tables >= 1, "expected at least one table block");
}

#[test]
fn attention_output_has_no_furniture_loop() {
    let Some(bytes) = read("attention.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);
    // The running footer must be suppressed (recurs on all 15 pages).
    assert!(
        !text.contains("31st Conference on Neural Information"),
        "furniture not suppressed"
    );
}

// ---------- form (w9.pdf) ----------

#[test]
fn w9_form_content() {
    let Some(bytes) = read("w9.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);

    assert!(
        text.contains("Employer identification number"),
        "w9 body missing"
    );
    assert!(
        text.contains("Social security number"),
        "w9 SSN field missing"
    );
    assert!(
        text.contains("backup withholding"),
        "w9 certification text missing"
    );
    // Dingbat checkbox junk must not leak ("ppy()" style noise).
    assert!(
        !text.contains("ppy"),
        "dingbat checkbox glyphs leaked into text"
    );
    // The IRS body font reports size 1.0 via GetFontSize : our matrix
    // fallback must give real sizes so paragraphs/grouping work.
    assert!(
        parsed.blocks.len() > 10,
        "w9 produced suspiciously few blocks ({})",
        parsed.blocks.len()
    );
}

// ---------- book (progit.pdf, 501 pages) ----------

#[test]
fn progit_book() {
    let Some(bytes) = read("progit.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    assert_eq!(parsed.page_count, 501, "progit page count changed");
    let text = all_block_text(&parsed.blocks);

    // Real headings exist on a 500-page book.
    let n_headings = parsed
        .blocks
        .iter()
        .filter(|b| matches!(b, crate::extract::blocks::Block::Heading { .. }))
        .count();
    assert!(
        n_headings >= 20,
        "expect many book headings, got {n_headings}"
    );

    // Code listing present (fenced code detected from mono fonts).
    let has_code = parsed.blocks.iter().any(
        |b| matches!(b, crate::extract::blocks::Block::Code { code, .. } if code.contains("git")),
    );
    assert!(has_code, "expected code blocks containing git commands");

    // Book must not be sliced into mid-word fragmentation.
    assert!(text.contains("version control"), "progit phrase missing");
}

// ---------- massive doc (pdf-spec.pdf) ----------

#[test]
fn pdf_spec_loads() {
    let Some(bytes) = read("pdf-spec.pdf") else {
        return;
    };
    let t = std::time::Instant::now();
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let _ms = t.elapsed().as_millis();
    assert!(parsed.page_count >= 25);
    let text = all_block_text(&parsed.blocks);
    assert!(text.contains("PDF"), "spec text missing");
    assert!(parsed.blocks.len() > 40);
}

// ---------- CJK (cjk.pdf) ----------

#[test]
fn cjk_extraction() {
    let Some(bytes) = read("cjk.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);

    // CJK words must not be space-fragmented (script-aware spacing).
    assert!(text.contains("サービス業"), "cjk glue failed");

    // Embedded English sentence spacing is exact.
    assert!(
        text.contains("The quick brown fox jumps over the lazy dog."),
        "english sentence spacing regression: {}",
        &text[text.len().saturating_sub(500)..]
    );

    // Headings in Japanese detected.
    let headings: Vec<&str> = parsed
        .blocks
        .iter()
        .filter_map(|b| {
            if let crate::extract::blocks::Block::Heading { text, .. } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        headings
            .iter()
            .any(|h| h.contains("日本語") || h.contains("経済")),
        "japanese headings missing: {:?}",
        headings
    );
}

// ---------- swin transformer (2-col, figs as images) ----------

#[test]
fn swin_structure() {
    let Some(bytes) = read("swin.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);
    assert!(text.contains("Swin Transformer"), "title missing");
    assert!(text.contains("shifted window"), "abstract content missing");
    assert!(parsed.page_count >= 6)
}

// ---------- page cap ----------

/// A minimal, well-formed PDF with `n` empty Letter pages and a
/// correct xref table.
fn empty_pages_pdf(n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &str| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", offsets.len(), body).as_bytes());
    };
    obj(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
    obj(
        &mut out,
        &mut offsets,
        &format!("<< /Type /Pages /Count {n} /Kids [ {} ] >>", kids.join(" ")),
    );
    for _ in 0..n {
        obj(
            &mut out,
            &mut offsets,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        );
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    out
}

// The size gate admits a document of hundreds of thousands of
// near-empty pages; each was rasterized and laid out. Pages past the
// cap are counted and reported, not read.
#[test]
fn pages_past_the_cap_are_reported_not_read() {
    // An empty page still costs a rasterization (~100-300 ms), so the
    // cap is exercised at 5 rather than at MAX_PAGES.
    let bytes = empty_pages_pdf(8);
    let doc = crate::pdf::parse_with(&bytes, 5).expect("parse");
    assert_eq!(doc.page_count, 8, "the true page count is reported");
    assert!(
        doc.notes
            .iter()
            .any(|x| x.contains("8 pages") && x.contains("first 5 were read")),
        "notes={:?}",
        doc.notes
    );
    // Under the cap nothing changes and nothing is noted.
    let small = crate::pdf::parse_with(&empty_pages_pdf(3), 5).expect("parse");
    assert_eq!(small.page_count, 3);
    assert!(!small.notes.iter().any(|x| x.contains("page cap")));
    // The production cap is what `parse` uses.
    assert_eq!(crate::pdf::engine::MAX_PAGES, 500);
}

// ---------- integration: extract() treats PDF bytes natively ----------

#[test]
fn extract_integration_pdf() {
    let Some(bytes) = read("attention.pdf") else {
        return;
    };
    let opts = crate::extract::ExtractOptions {
        max_chars: Some(60_000),
        ..Default::default()
    };
    let out = crate::extract::extract(
        &bytes,
        "application/pdf",
        "https://arxiv.org/pdf/1706.03762",
        &opts,
    )
    .expect("extract");
    assert!(
        out.markdown.contains("Transformer"),
        "integrated extract missing content"
    );
    assert!(out.blocks_total > 50, "blocks counted");
    assert!(out.lang == "en" || !out.lang.is_empty());
    assert!(out.quality > 0.0, "quality score must be positive");
}

#[test]
fn extract_integration_magic_sniff() {
    // Wrong content-type but real PDF magic must still route to DonSheet.
    let Some(bytes) = read("w9.pdf") else {
        return;
    };
    let opts = crate::extract::ExtractOptions {
        max_chars: Some(2_000),
        ..Default::default()
    };
    let out = crate::extract::extract(
        &bytes,
        "application/octet-stream",
        "https://irs.gov/fw9.pdf",
        &opts,
    )
    .expect("extract");
    assert!(out.markdown.contains("Form W-9"), "magic sniff failed");
}

// ---------- metadata ----------

#[test]
fn pdf_date_normalized() {
    let Some(bytes) = read("cjk.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    if let Some(p) = parsed.meta.published {
        assert!(
            p.starts_with("2026") || p.contains('-'),
            "date not normalized: {p}"
        );
    }
}

// pdf_date sliced `d[..8]` on the raw Info-dict string. A producer
// writing a localized date (Japanese/Chinese-locale tools do) puts a
// multibyte char inside the first 8 bytes -- a str-slice panic on a
// producer-controlled field, which under panic = "abort" takes the
// daemon down on a plain PDF fetch. No corpus file needed.
#[test]
fn pdf_date_non_ascii_does_not_panic() {
    let raw = Some("D:202605年25日".to_string());
    assert_eq!(
        crate::pdf::pdf_date(&raw),
        Some("D:202605年25日".to_string()),
        "unparseable dates pass through verbatim"
    );
    assert_eq!(
        crate::pdf::pdf_date(&Some("D:20260525080808+00'00'".to_string())),
        Some("2026-05-25".to_string())
    );
}
