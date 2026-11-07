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
            crate::diag!("SKIP: {} missing (scripts/download-corpus.sh)", p.display());
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
fn attention_conference_note_is_not_repeated_per_page() {
    let Some(bytes) = read("attention.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);
    // The NIPS line is a first-page note in this printing, not a running
    // footer, so it survives once. What must never happen is the loop this
    // test was written for: the band repeated on every page.
    let n = text
        .matches("Conference on Neural Information Processing Systems")
        .count();
    assert!(n <= 1, "conference footer repeated {n} times");
}

/// A page whose footer band holds a page-varying running line and a page
/// number, plus one body line.
fn furniture_page(index: usize) -> crate::pdf::layout::PageLines {
    let page = |text: &str, y0: f32, order: u32| crate::pdf::layout::Line {
        text: text.to_string(),
        words: Vec::new(),
        x0: 40.0,
        y0,
        x1: 400.0,
        y1: y0 + 10.0,
        size: 9.0,
        weight: 400,
        italic: false,
        mono: false,
        font: 0,
        glyphs: text.chars().count(),
        order,
        page: index,
    };
    crate::pdf::layout::PageLines {
        index,
        width: 500.0,
        height: 800.0,
        lines: vec![
            page("Body text unique to this page", 300.0, 0),
            // Bottom band (y0 >= 0.92 * height), digits vary per page.
            page(&format!("Annual Report 2026 · page {index}"), 760.0, 1),
            page(&format!("{}", index + 1), 780.0, 2),
        ],
        images: 0,
        fusion: None,
    }
}

#[test]
fn recurring_footers_and_page_numbers_are_suppressed() {
    let mut pages: Vec<_> = (0..8).map(furniture_page).collect();
    crate::pdf::reading::suppress_furniture(&mut pages);
    for p in &pages {
        let texts: Vec<&str> = p.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Body text unique to this page"],
            "page {} kept furniture: {:?}",
            p.index,
            texts
        );
    }
}

#[test]
fn a_short_document_keeps_its_first_page_footer() {
    // Under four pages nothing can be called recurring: dropping the only
    // footer would delete content.
    let mut pages: Vec<_> = (0..3).map(furniture_page).collect();
    crate::pdf::reading::suppress_furniture(&mut pages);
    for p in &pages {
        assert_eq!(p.lines.len(), 3, "page {} lost lines", p.index);
    }
}

/// A `furniture_page`-sized page holding `lines` as (text, y0) pairs, each
/// 10 pt tall: y0 <= 70 is in the top band, y0 >= 736 in the bottom band.
fn band_page(index: usize, lines: &[(&str, f32)]) -> crate::pdf::layout::PageLines {
    let mut page = furniture_page(index);
    let template = page.lines[0].clone();
    page.lines = lines
        .iter()
        .zip(0u32..)
        .map(|(&(text, y0), order)| crate::pdf::layout::Line {
            text: text.to_string(),
            y0,
            y1: y0 + 10.0,
            glyphs: text.chars().count(),
            order,
            ..template.clone()
        })
        .collect();
    page
}

fn has_line(page: &crate::pdf::layout::PageLines, text: &str) -> bool {
    page.lines.iter().any(|l| l.text == text)
}

#[test]
fn a_phrase_in_both_bands_of_one_page_counts_once() {
    // The IRS W-9 layout: page 1 has the title "Form W-9" in its head and
    // the form number in its foot; pages 2-6 share one running head, which
    // pass 1 removes; page 2's foot holds a body sentence naming the form.
    // "form w-#" is then in the head of one sampled page and in the foot of
    // two: no band reaches the cutoff of three pages.
    let sentence = "See Pub. 515 for more information on providing a Form W-9 or a certification";
    let heads: Vec<String> = (0..6)
        .map(|i| format!("Form W-9 (Rev. 3-2024) Page {}", i + 1))
        .collect();
    let mut pages: Vec<_> = (0..6)
        .map(|index| {
            let mut lines = vec![("Body text unique to this page", 300.0)];
            match index {
                0 => lines.extend([
                    ("Form W-9", 30.0),
                    ("Cat. No. 10231X Form W-9 (Rev. 3-2024)", 760.0),
                ]),
                _ => lines.push((heads[index].as_str(), 30.0)),
            }
            if index == 1 {
                lines.push((sentence, 750.0));
            }
            band_page(index, &lines)
        })
        .collect();
    crate::pdf::reading::suppress_furniture(&mut pages);
    assert!(
        has_line(&pages[0], "Form W-9"),
        "the page-1 title was counted twice and suppressed: {:?}",
        pages[0].lines
    );
    assert!(
        has_line(&pages[1], sentence),
        "the body sentence was suppressed as furniture: {:?}",
        pages[1].lines
    );
    for p in &pages[1..] {
        assert!(
            !has_line(p, &heads[p.index]),
            "page {} kept its running head",
            p.index
        );
    }
}

#[test]
fn a_band_sentence_sharing_one_family_bigram_survives() {
    // A running head that varies per page (roman numerals defeat pass 1)
    // is a bigram family; a body sentence that continues into the head
    // band of page 3 shares one of its bigrams, "form w-#", and nothing
    // else.
    let numerals = ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii"];
    let heads: Vec<String> = numerals
        .iter()
        .map(|n| format!("Instructions for Form W-9 · {n}"))
        .collect();
    let sentence = "Persons who must file Form W-9 with the requester should read";
    let mut pages: Vec<_> = (0..8)
        .map(|index| {
            let mut lines = vec![
                (heads[index].as_str(), 30.0),
                ("Body text unique to this page", 300.0),
            ];
            if index == 2 {
                lines.push((sentence, 50.0));
            }
            band_page(index, &lines)
        })
        .collect();
    crate::pdf::reading::suppress_furniture(&mut pages);
    assert!(
        has_line(&pages[2], sentence),
        "a sentence sharing one bigram with the running head was suppressed: {:?}",
        pages[2].lines
    );
    for p in &pages {
        assert!(
            !has_line(p, &heads[p.index]),
            "page {} kept its running head",
            p.index
        );
    }
}

#[test]
fn a_footer_family_varying_per_page_is_still_suppressed() {
    // The PDF 32000 footer: the page label alternates sides and runs from
    // roman to arabic, so no exact key recurs and only the bigram family
    // can remove it, on the sampled pages and past them.
    let labels = ["i", "ii", "iii", "iv", "v", "vi", "81", "420"];
    let feet: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let notice = "© Adobe Systems Incorporated 2008 – All rights reserved";
            if i % 2 == 0 {
                format!("{notice} {n}")
            } else {
                format!("{n} {notice}")
            }
        })
        .collect();
    let mut pages: Vec<_> = (0..8)
        .map(|index| {
            band_page(
                index,
                &[
                    ("Body text unique to this page", 300.0),
                    (feet[index].as_str(), 760.0),
                ],
            )
        })
        .collect();
    crate::pdf::reading::suppress_furniture(&mut pages);
    for p in &pages {
        let texts: Vec<&str> = p.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Body text unique to this page"],
            "page {} kept its footer",
            p.index
        );
    }
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

#[test]
fn w9_body_lines_in_the_page_bands_are_not_furniture() {
    let Some(bytes) = read("w9.pdf") else {
        return;
    };
    let parsed = crate::pdf::parse(&bytes).expect("parse");
    let text = all_block_text(&parsed.blocks);
    // Each of these lines sits in a top or bottom page band and shares one
    // or two common bigrams ("form w-#", "of the", "a disregarded",
    // "a foreign", "as a") with other band lines of the form; none of them
    // repeats.
    let missing: Vec<&str> = [
        "See Pub. 515 for more information on providing a Form W-9",
        "Department of the Treasury",
        "Caution: A disregarded U.S. entity",
        "member of the same expanded affiliated",
        "group as a corporation described in Regulations section",
    ]
    .into_iter()
    .filter(|line| !text.contains(line))
    .collect();
    assert!(
        missing.is_empty(),
        "w9 body lines suppressed as furniture: {missing:?}"
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

// ---------- OCR pass: what it renders, and how much at once ----------

/// Serialize `objects` (bodies of objects 1..=n; object 1 is the
/// catalog) into a PDF with a correct xref table.
fn pdf_of_objects(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
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

fn stream_object(dict_entries: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< /Length {} {dict_entries} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

/// `n` Letter pages of Helvetica text whose ToUnicode map sends every
/// printable byte into the private use area: each renders as clean Latin
/// text and extracts as PUA glyph soup, so each is an OCR candidate.
fn pua_text_pages_pdf(n: usize) -> Vec<u8> {
    let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<00> <FF>\nendcodespacerange\n1 beginbfrange\n<20> <7E> <E020>\nendbfrange\nendcmap\nCMapName currentdict /CMapType get pop end\nend";
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 5 + 2 * i)).collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" ")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 4 0 R >>".to_vec(),
        stream_object("", cmap.as_bytes()),
    ];
    for i in 0..n {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                6 + 2 * i
            )
            .into_bytes(),
        );
        objects.push(stream_object(
            "",
            b"BT /F1 28 Tf 72 720 Td 48 TL (THE QUICK BROWN FOX JUMPS) Tj T* (OVER THE LAZY DOG 12345) Tj T* (SCANNED INVOICE TOTAL DUE) Tj ET",
        ));
    }
    pdf_of_objects(&objects)
}

/// Clears the OCR stand-in when a test ends, pass or fail.
struct OcrSeamGuard;

impl Drop for OcrSeamGuard {
    fn drop(&mut self) {
        crate::pdf::ocr::test_ocr::clear();
    }
}

// OCR that cannot run (feature off, ONNX Runtime not loadable, models not
// fetchable) used to render every candidate page at 150 dpi first and
// hold all the bitmaps, then throw them away.
#[test]
fn ocr_that_cannot_run_renders_no_page() {
    let _guard = OcrSeamGuard;
    crate::pdf::ocr::test_ocr::unavailable("model download failed");
    crate::pdf::pixels::ocr_probe::take();
    let parsed = crate::pdf::parse(&pua_text_pages_pdf(3)).expect("parse");
    let (made, _) = crate::pdf::pixels::ocr_probe::take();
    assert_eq!(made, 0, "pages rendered for an OCR that cannot run");
    assert!(
        parsed
            .notes
            .iter()
            .any(|n| n.contains("3 page(s)") && n.contains("OCR was unavailable")),
        "notes={:?}",
        parsed.notes
    );
}

// With OCR running, all candidate pages used to be rendered before the
// first was recognized, so every bitmap was alive at once.
#[test]
fn ocr_holds_one_page_of_pixels_at_a_time() {
    let _guard = OcrSeamGuard;
    // A different line per page, mid-page: identical lines in the same
    // place on every page would be dropped as running furniture.
    let mut words = ["ALPHA", "BRAVO", "CHARLIE"].into_iter();
    crate::pdf::ocr::test_ocr::fake(move |bitmap| {
        let word = words.next().unwrap_or("EXTRA");
        let y = bitmap.page_h_pt / 2.0;
        vec![crate::pdf::ocr::OcrLine {
            x0: 72.0,
            y0: y,
            x1: 72.0 + bitmap.page_w_pt / 2.0,
            y1: y + 28.0,
            text: format!("RECOGNIZED {word} PARAGRAPH TEXT"),
            confidence: 0.9,
        }]
    });
    crate::pdf::pixels::ocr_probe::take();
    let parsed = crate::pdf::parse(&pua_text_pages_pdf(3)).expect("parse");
    let (made, peak) = crate::pdf::pixels::ocr_probe::take();
    assert_eq!(made, 3, "one render per candidate page");
    assert_eq!(peak, 1, "OCR bitmaps alive at once");
    let ocr: Vec<(usize, bool)> = parsed.pages_meta.iter().map(|m| (m.page, m.ocr)).collect();
    assert_eq!(ocr, [(0, true), (1, true), (2, true)]);
    let text = all_block_text(&parsed.blocks);
    for word in ["ALPHA", "BRAVO", "CHARLIE"] {
        assert!(
            text.contains(&format!("RECOGNIZED {word}")),
            "{word} missing: {text:?}"
        );
    }
}

/// An image-only (scanned) copy of `text_pdf`'s first page: its 100 dpi
/// render as a grayscale image XObject, and no text layer.
#[cfg(feature = "ocr")]
fn scanned_copy_pdf(text_pdf: &[u8]) -> Vec<u8> {
    let bmp = crate::pdf::engine::rasterize_page_at(text_pdf, 0, 100.0)
        .expect("reopen")
        .expect("render");
    // BGRA to luma, the weights `PageBitmap::ink_mask` uses.
    let (pixels, _) = bmp.buf.as_chunks::<4>();
    let gray: Vec<u8> = pixels
        .iter()
        .map(|[b, g, r, _]| ((114 * *b as u32 + 587 * *g as u32 + 299 * *r as u32) / 1000) as u8)
        .collect();
    let content = b"q 612 0 0 792 0 0 cm /Im0 Do Q";
    pdf_of_objects(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
        stream_object(
            &format!(
                "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceGray /BitsPerComponent 8",
                bmp.w, bmp.h
            ),
            &gray,
        ),
        stream_object("", content),
    ])
}

// Negative case: a scan that OCR can read still gets its text. Needs the
// English OCR models in DONSETCH_CACHE_DIR/ocr and a loadable ONNX
// Runtime; skips when the models are missing.
#[cfg(feature = "ocr")]
#[test]
fn a_scanned_page_is_still_read_by_ocr() {
    let dir = crate::pdf::ocr::ocr_cache_dir();
    if !(dir.join("en_pp-ocrv5_mobile_rec.onnx").exists()
        && dir.join("pp-ocrv5_mobile_det.onnx").exists())
    {
        crate::diag!("SKIP: OCR models missing in {}", dir.display());
        return;
    }
    let scan = scanned_copy_pdf(&pua_text_pages_pdf(1));
    let parsed = crate::pdf::parse(&scan).expect("parse");
    let text = all_block_text(&parsed.blocks);
    assert!(
        text.contains("QUICK BROWN FOX") && text.contains("LAZY DOG"),
        "OCR text missing: {text:?} notes={:?}",
        parsed.notes
    );
    let ocr: Vec<(usize, bool)> = parsed.pages_meta.iter().map(|m| (m.page, m.ocr)).collect();
    assert_eq!(ocr, [(0, true)]);
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

/// A well-formed PDF from object bodies (object 1 is the catalog), with
/// an optional /Info dictionary and a correct xref table.
fn pdf_from_objects(objects: &[String], info: Option<String>) -> Vec<u8> {
    let has_info = info.is_some();
    let mut bodies: Vec<String> = objects.to_vec();
    bodies.extend(info);
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    let info_ref = if has_info {
        format!(" /Info {} 0 R", offsets.len())
    } else {
        String::new()
    };
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R{info_ref} >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// One Letter page whose only content is an image of bars: no text
/// layer, so with OCR off the parser reports it as a scan.
fn scan_pdf(title: Option<&str>) -> Vec<u8> {
    // 64x16 grayscale, ASCIIHex-encoded so the stream stays ASCII and
    // the xref offsets stay exact.
    let (w, h) = (64usize, 16usize);
    let hex: String = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            if (4..=11).contains(&y) && (x / 3) % 2 == 0 && (4..=59).contains(&x) {
                "00"
            } else {
                "FF"
            }
        })
        .collect::<String>()
        + ">";
    let content = "q 500 0 0 125 56 600 cm /Im1 Do Q";
    pdf_from_objects(
        &[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Count 1 /Kids [5 0 R] >>".into(),
            format!(
                "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /ASCIIHexDecode /Length {} >>\nstream\n{hex}\nendstream",
                hex.len()
            ),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im1 3 0 R >> >> /Contents 4 0 R >>".into(),
        ],
        title.map(|t| format!("<< /Title ({t}) >>")),
    )
}

fn extract_pdf(bytes: &[u8]) -> crate::extract::Extracted {
    crate::extract::extract(
        bytes,
        "application/pdf",
        "https://example.com/scan.pdf",
        &Default::default(),
    )
    .expect("extract")
}

const SCANNED_NOTE: &str = "*[pdf: scanned/image-only PDF (1 pages): no text layer could be recovered; the pages could not be extracted]*";

// A document that yields no blocks and has no /Title gets the empty
// marker; the engine notes that say why stay above it.
// Sets DONSHEET_OCR before the config loads: depends on nextest's
// process per test.
#[test]
fn untitled_scan_keeps_its_scanned_note() {
    unsafe { std::env::set_var("DONSHEET_OCR", "off") };
    let out = extract_pdf(&scan_pdf(None));
    assert_eq!(out.blocks_total, 0);
    assert!(
        out.markdown.starts_with(SCANNED_NOTE),
        "scanned note missing: {:?}",
        out.markdown
    );
    assert!(
        out.markdown
            .contains("https://example.com/scan.pdf\n\n*(no extractable content)*"),
        "empty marker missing: {:?}",
        out.markdown
    );
}

// Same path with no image and no OCR candidate: empty pages carry the
// parser's "no extractable text" note.
#[test]
fn untitled_text_free_pdf_keeps_its_note() {
    let out = extract_pdf(&empty_pages_pdf(2));
    assert!(
        out.markdown
            .starts_with("*[pdf: no extractable text found in this PDF]*"),
        "note missing: {:?}",
        out.markdown
    );
    assert!(out.markdown.contains("*(no extractable content)*"));
}

// Negative: a /Title keeps the empty marker away; the output is the
// note, the title and the URL.
// Sets DONSHEET_OCR before the config loads: depends on nextest's
// process per test.
#[test]
fn titled_scan_output_is_unchanged() {
    unsafe { std::env::set_var("DONSHEET_OCR", "off") };
    let out = extract_pdf(&scan_pdf(Some("Quarterly scan")));
    assert_eq!(
        out.markdown,
        format!("{SCANNED_NOTE}\n\n# Quarterly scan\nhttps://example.com/scan.pdf\n")
    );
}

// Negative: a PDF with a text layer has blocks, no notes and no empty
// marker.
#[test]
fn pdf_with_text_gets_no_empty_marker() {
    let content = "BT /F1 18 Tf 72 700 Td (Quarterly revenue grew in every region.) Tj ET";
    let bytes = pdf_from_objects(
        &[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ],
        None,
    );
    let out = extract_pdf(&bytes);
    assert_eq!(
        out.markdown,
        "https://example.com/scan.pdf\n\nQuarterly revenue grew in every region.\n"
    );
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

// The reported French amendment (#399) sets "15 %" in Times at 12pt.
// Its narrow digit ink boxes must not create a word boundary. Use a
// self-contained PDF with standard text plus a Type3 narrow-digit font,
// so the side-bearing regression is independent of host font substitution.
fn spacing_pdf(matrix: &str) -> Vec<u8> {
    let stream = format!(
        "BT /F1 12 Tf {matrix} Tm (At most 15 % of revenue, or 2 % of total revenue.) Tj 0 -24 Td [(15) -500 (16)] TJ 0 -24 Td (1 5 %) Tj 0 -24 Td /F2 12 Tf (555515555 15 1 5) Tj ET\n"
    );
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R /F2 6 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /one 7 0 R /five 8 0 R /space 9 0 R >> /Encoding << /Type /Encoding /Differences [32 /space 49 /one 53 /five] >> /FirstChar 32 /LastChar 53 /Widths [250 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500 500] /Resources << >> >>".to_string(),
        {
            let glyph = "500 0 0 0 240 700 d1 0 0 240 700 re f\n";
            format!("<< /Length {} >>\nstream\n{glyph}endstream", glyph.len())
        },
        {
            let glyph = "500 0 10 0 490 700 d1 10 0 480 700 re f\n";
            format!("<< /Length {} >>\nstream\n{glyph}endstream", glyph.len())
        },
        {
            let glyph = "250 0 d0\n";
            format!("<< /Length {} >>\nstream\n{glyph}endstream", glyph.len())
        },
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0];
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in &offsets[1..] {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    out
}

#[test]
fn spacing_preserves_digits_and_real_word_gaps() {
    for matrix in [
        "1 0 0 1 72 720",
        "0 1 -1 0 300 72",
        "-1 0 0 -1 500 72",
        "0 -1 1 0 72 650",
    ] {
        let bytes = spacing_pdf(matrix);
        let (_, pages) = crate::pdf::engine::load_document(
            &bytes,
            &Default::default(),
            |mut input: crate::pdf::engine::PageInput| {
                crate::pdf::rotate::canonicalize(&mut input.chars);
                crate::pdf::layout::assemble(input.chars)
            },
        )
        .expect("load spacing PDF");
        let lines: Vec<_> = pages[0]
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert!(
            lines.contains(&"At most 15 % of revenue, or 2 % of total revenue."),
            "matrix {matrix}: {lines:?}"
        );
        assert!(
            lines.contains(&"555515555 15 1 5"),
            "side bearings became spaces: {lines:?}"
        );
        assert!(
            lines.contains(&"15 16"),
            "positioned word gap lost: {lines:?}"
        );
        assert!(lines.contains(&"1 5 %"), "explicit space lost: {lines:?}");
        for line in &pages[0].lines {
            let words: Vec<_> = line.words.iter().map(|word| word.text.as_str()).collect();
            let expected: Vec<_> = line.text.split_whitespace().collect();
            assert_eq!(
                words, expected,
                "word geometry lost boundaries: {}",
                line.text
            );
            assert!(line.words.iter().all(|word| word.x1 > word.x0));
        }
    }
}

// ---------- audit repros (audit/pdfops) ----------

/// Serialize `objects` (bodies of objects 1..=n) into a PDF with a
/// correct xref table. Object 1 is the catalog.
fn audit_pdf(objects: &[String]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
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

fn audit_stream(content: &str) -> String {
    format!(
        "<< /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

/// Two outline items whose /Next entries point at each other. PDFium
/// guards only a self-loop in FPDFBookmark_GetNextSibling; the caller
/// owns cycle detection. `walk_outlines` checks `out.len()` only on
/// entry to a level, never inside the sibling loop, so this walks
/// forever and grows `out` until the allocator fails (abort).
#[test]
fn audit_outline_sibling_cycle_terminates() {
    let bytes = audit_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 8 0 R >> >> /Contents 4 0 R >>".into(),
        audit_stream("BT /F1 14 Tf 72 720 Td (Outline cycle fixture) Tj ET"),
        "<< /Type /Outlines /First 6 0 R /Last 7 0 R /Count 2 >>".into(),
        "<< /Title (Chapter A) /Parent 5 0 R /Next 7 0 R >>".into(),
        "<< /Title (Chapter B) /Parent 5 0 R /Prev 6 0 R /Next 6 0 R >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = crate::pdf::engine::load_document(&bytes, &Default::default(), |_| ());
        let _ = tx.send(
            r.map(|(raw, _)| raw.outline.len())
                .map_err(|e| e.to_string()),
        );
    });
    // A two-entry outline takes microseconds. The bound is generous on
    // purpose and still short enough that the runaway walk stays well
    // under a gigabyte before this test gives up.
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(n)) => assert!(n <= 10_001, "outline walk returned {n} items"),
        Ok(Err(e)) => panic!("load failed: {e}"),
        Err(_) => panic!(
            "walk_outlines did not terminate within 5s on a 2-item /Next cycle \
             (it holds the global PDFium lock and grows without bound)"
        ),
    }
}

/// The outline `load_document` reports for a one-page PDF whose catalog
/// points /Outlines at object 4; `outline` holds objects 4.. with the
/// outline root first. Fails after 5 s instead of hanging, because a walk
/// that never stops holds the global PDFium lock and grows without bound.
fn outline_of(outline: &[String]) -> Vec<(String, usize, Option<usize>)> {
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R /Outlines 4 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".into(),
    ];
    objects.extend_from_slice(outline);
    let bytes = audit_pdf(&objects);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = crate::pdf::engine::load_document(&bytes, &Default::default(), |_| ());
        let _ = tx.send(
            r.map(|(raw, _)| {
                raw.outline
                    .into_iter()
                    .map(|o| (o.title, o.level, o.page))
                    .collect::<Vec<_>>()
            })
            .map_err(|e| e.to_string()),
        );
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(items)) => items,
        Ok(Err(e)) => panic!("load failed: {e}"),
        Err(_) => panic!("outline walk did not finish within 5s"),
    }
}

fn titles_and_levels(items: &[(String, usize, Option<usize>)]) -> Vec<(&str, usize)> {
    items.iter().map(|(t, l, _)| (t.as_str(), *l)).collect()
}

/// A /Next chain that leads back to an item already listed: each item is
/// reported once, in walk order, and the walk ends.
#[test]
fn outline_next_cycle_lists_each_item_once() {
    // Chapter A -> Chapter B -> Chapter A.
    let items = outline_of(&[
        "<< /Type /Outlines /First 5 0 R /Last 6 0 R >>".into(),
        "<< /Title (Chapter A) /Parent 4 0 R /Next 6 0 R >>".into(),
        "<< /Title (Chapter B) /Parent 4 0 R /Prev 5 0 R /Next 5 0 R >>".into(),
    ]);
    assert_eq!(
        titles_and_levels(&items),
        [("Chapter A", 0), ("Chapter B", 0)]
    );
}

/// A /First link that points back at an ancestor: the ancestor is not
/// listed again under itself.
#[test]
fn outline_first_cycle_lists_each_item_once() {
    // Part -> Section -> Part: the child's /First is its own parent.
    let items = outline_of(&[
        "<< /Type /Outlines /First 5 0 R /Last 5 0 R >>".into(),
        "<< /Title (Part) /Parent 4 0 R /First 6 0 R /Last 6 0 R >>".into(),
        "<< /Title (Section) /Parent 5 0 R /First 5 0 R /Last 5 0 R >>".into(),
    ]);
    assert_eq!(titles_and_levels(&items), [("Part", 0), ("Section", 1)]);
}

/// The item cap holds inside one level, not only when a level starts: a
/// flat outline longer than the cap stops at 10_001 items.
#[test]
fn outline_item_cap_holds_within_a_level() {
    let n = 10_050;
    let first = 5;
    let last = first + n - 1;
    let mut outline = vec![format!(
        "<< /Type /Outlines /First {first} 0 R /Last {last} 0 R >>"
    )];
    for obj in first..=last {
        let next = if obj < last {
            format!(" /Next {} 0 R", obj + 1)
        } else {
            String::new()
        };
        outline.push(format!("<< /Title (Item {obj}) /Parent 4 0 R{next} >>"));
    }
    assert_eq!(outline_of(&outline).len(), 10_001);
}

/// Negative case: a well-formed tree whose items repeat titles and
/// destinations keeps every item. A repeat is judged by the outline item
/// itself, never by its title or page.
#[test]
fn outline_tree_with_repeated_titles_keeps_every_item() {
    let items = outline_of(&[
        "<< /Type /Outlines /First 5 0 R /Last 8 0 R >>".into(),
        "<< /Title (Part) /Parent 4 0 R /Next 8 0 R /First 6 0 R /Last 7 0 R /Dest [3 0 R /Fit] >>"
            .into(),
        "<< /Title (Introduction) /Parent 5 0 R /Next 7 0 R /Dest [3 0 R /Fit] >>".into(),
        "<< /Title (Introduction) /Parent 5 0 R /Prev 6 0 R >>".into(),
        "<< /Title (Part) /Parent 4 0 R /Prev 5 0 R /First 9 0 R /Last 9 0 R >>".into(),
        "<< /Title (Introduction) /Parent 8 0 R /Dest [3 0 R /Fit] >>".into(),
    ]);
    let want: Vec<(String, usize, Option<usize>)> = vec![
        ("Part".into(), 0, Some(0)),
        ("Introduction".into(), 1, Some(0)),
        ("Introduction".into(), 1, None),
        ("Part".into(), 0, None),
        ("Introduction".into(), 1, Some(0)),
    ];
    assert_eq!(items, want);

/// /Kids [missing, PUA-text page (, ordinary text page)]: PDFium trusts
/// /Count, FPDF_LoadPage(0) returns null and the engine skips it, so
/// `pages` (a Vec of LOADED pages) is shorter than the document and
/// `pages[k].index != k`.
#[cfg(feature = "ocr")]
fn audit_broken_kid_pdf(trailing_text_page: bool) -> Vec<u8> {
    if trailing_text_page {
        ocr_kids_pdf("99 0 R 3 0 R 9 0 R", 3)
    } else {
        ocr_kids_pdf("99 0 R 3 0 R", 2)
    }
}

/// A page tree with /Count `count` over `kids`, where `3 0 R` is a page
/// whose text extracts as PUA glyph soup (an OCR candidate) and `9 0 R`
/// is an ordinary text page carrying "KEEP-ME".
#[cfg(feature = "ocr")]
fn ocr_kids_pdf(kids: &str, count: usize) -> Vec<u8> {
    // Helvetica glyphs, ToUnicode mapping every printable byte into the
    // private use area: renders as clean Latin text, extracts as PUA.
    let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<00> <FF>\nendcodespacerange\n1 beginbfrange\n<20> <7E> <E020>\nendbfrange\nendcmap\nCMapName currentdict /CMapType get pop end\nend";
    audit_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Count {count} /Kids [{kids}] >>"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        audit_stream("BT /F1 28 Tf 72 720 Td 48 TL (THE QUICK BROWN FOX JUMPS) Tj T* (OVER THE LAZY DOG 12345) Tj T* (SCANNED INVOICE TOTAL DUE) Tj ET"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R >>".into(),
        audit_stream(cmap),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        audit_stream("BT /F1 14 Tf 72 720 Td 20 TL (KEEP-ME page three ordinary text layer.) Tj T* (This line must survive extraction.) Tj ET"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 8 0 R >>".into(),
    ])
}

/// Needs the OCR feature and the English OCR models in the cache dir
/// (DONSETCH_CACHE_DIR); skips otherwise, like smoke_scanned_detection.
#[cfg(feature = "ocr")]
fn audit_ocr_models_present() -> bool {
    let dir = crate::pdf::ocr::ocr_cache_dir();
    let ok = dir.join("en_pp-ocrv5_mobile_rec.onnx").exists()
        && dir.join("pp-ocrv5_mobile_det.onnx").exists();
    if !ok {
        crate::diag!("SKIP: OCR models missing in {}", dir.display());
    }
    ok
}

#[cfg(feature = "ocr")]
#[test]
fn audit_ocr_after_unloadable_page_does_not_panic() {
    if !audit_ocr_models_present() {
        return;
    }
    let parsed = crate::pdf::parse(&audit_broken_kid_pdf(false)).expect("parse");
    assert!(
        all_block_text(&parsed.blocks).contains("QUICK BROWN FOX"),
        "OCR text missing: {:?}",
        parsed.notes
    );
}

#[cfg(feature = "ocr")]
#[test]
fn audit_ocr_after_unloadable_page_keeps_the_next_page() {
    if !audit_ocr_models_present() {
        return;
    }
    let parsed = crate::pdf::parse(&audit_broken_kid_pdf(true)).expect("parse");
    let text = all_block_text(&parsed.blocks);
    assert!(
        text.contains("QUICK BROWN FOX"),
        "OCR text missing: {text:?}"
    );
    assert!(
        text.contains("KEEP-ME"),
        "page 3's own text layer was overwritten by page 2's OCR: {text:?}"
    );
}

#[cfg(feature = "ocr")]
#[test]
fn ocr_after_unloadable_page_flags_the_page_it_read() {
    if !audit_ocr_models_present() {
        return;
    }
    let parsed = crate::pdf::parse(&audit_broken_kid_pdf(true)).expect("parse");
    let pages: Vec<(usize, bool, bool)> = parsed
        .pages_meta
        .iter()
        .map(|m| (m.page, m.ocr, m.chars > 0))
        .collect();
    // (page index, OCR'd, has text): page 0 never loaded, page 1 is the
    // OCR'd one, page 2 keeps its own text layer.
    assert_eq!(pages, [(1, true, true), (2, false, true)]);
}

// Negative case: with every page loaded, positions and indices agree,
// and OCR must still replace only the PUA page.
#[cfg(feature = "ocr")]
#[test]
fn ocr_with_every_page_loaded_replaces_only_the_unreadable_page() {
    if !audit_ocr_models_present() {
        return;
    }
    let parsed = crate::pdf::parse(&ocr_kids_pdf("3 0 R 9 0 R", 2)).expect("parse");
    let text = all_block_text(&parsed.blocks);
    assert!(
        text.contains("QUICK BROWN FOX"),
        "OCR text missing: {text:?}"
    );
    assert!(text.contains("KEEP-ME"), "text page lost: {text:?}");
    let pages: Vec<(usize, bool)> = parsed.pages_meta.iter().map(|m| (m.page, m.ocr)).collect();
    assert_eq!(pages, [(0, true), (1, false)]);

/// A borderless 3-column table, one text object per cell (the shape
/// report generators and spreadsheet exports emit): the gap between two
/// cells is a word break even when no space glyph fills it and the
/// column gaps are the only positive loose-box gaps on the line.
#[test]
fn audit_sparse_row_keeps_column_gaps_as_word_breaks() {
    let rows = [
        ("Item", "Qty", "Price"),
        ("Wheel", "2", "50"),
        ("Frame", "1", "300"),
        ("Gear", "4", "80"),
        ("Total", "7", "430"),
    ];
    let mut ops = Vec::new();
    for (i, (a, b, c)) in rows.iter().enumerate() {
        let base = 700 - 30 * (i as i32 + 1) + 10;
        for (x, t) in [(80, a), (280, b), (380, c)] {
            ops.push(format!("BT /F1 12 Tf {x} {base} Td ({t}) Tj ET"));
        }
    }
    let bytes = audit_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        audit_stream(&ops.join("\n")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ]);
    let (_, pages) = crate::pdf::engine::load_document(
        &bytes,
        &Default::default(),
        |input: crate::pdf::engine::PageInput| crate::pdf::layout::assemble(input.chars),
    )
    .expect("load table PDF");
    let lines: Vec<&str> = pages[0].lines.iter().map(|l| l.text.as_str()).collect();
    for want in [
        "Item Qty Price",
        "Wheel 2 50",
        "Frame 1 300",
        "Gear 4 80",
        "Total 7 430",
    ] {
        assert!(lines.contains(&want), "{want:?} not found in {lines:?}");
    }
}

/// The same sparse table shape with spacing inside the cells: the
/// word-break threshold stays above it, so a letterspaced cell (3 pt of
/// Tc on 12 pt type, above the 0.16 em floor) and a cell whose TJ
/// kerning tightens and loosens pairs each remain one word.
#[test]
fn sparse_row_keeps_letterspaced_and_kerned_words_whole() {
    let rows = [
        (680, "3 Tc", ["(ITEM) Tj", "(QTY) Tj", "(PRICE) Tj"]),
        (
            650,
            "0 Tc",
            [
                "[(T) 70 (o) -120 (t) 20 (a) -40 (l)] TJ",
                "[(1) -40 (2)] TJ",
                "[(4) 30 (3) -60 (0)] TJ",
            ],
        ),
    ];
    let mut ops = Vec::new();
    for (y, tc, cells) in rows {
        for (x, show) in [80, 280, 380].into_iter().zip(cells) {
            ops.push(format!("BT /F1 12 Tf {tc} {x} {y} Td {show} ET"));
        }
    }
    let bytes = audit_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        audit_stream(&ops.join("\n")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ]);
    let (_, pages) = crate::pdf::engine::load_document(
        &bytes,
        &Default::default(),
        |input: crate::pdf::engine::PageInput| crate::pdf::layout::assemble(input.chars),
    )
    .expect("load table PDF");
    let lines: Vec<&str> = pages[0].lines.iter().map(|l| l.text.as_str()).collect();
    for want in ["ITEM QTY PRICE", "Total 12 430"] {
        assert!(lines.contains(&want), "{want:?} not found in {lines:?}");
    }
    for line in &pages[0].lines {
        let words: Vec<_> = line.words.iter().map(|word| word.text.as_str()).collect();
        let expected: Vec<_> = line.text.split_whitespace().collect();
        assert_eq!(
            words, expected,
            "word geometry lost boundaries: {}",
            line.text
        );
    }
}
