//! Stage 1: chars → lines.
//!
//! Per page: cluster glyphs into visual lines, normalize ligatures and
//! control glyphs, insert spaces from x-gaps (many PDFs emit no space
//! glyphs), and keep word-level x-spans for downstream table geometry.
//!
//! Input coordinates are screen-space (top-left origin, y down, points)
//! as produced by `engine::PdfChar`.

use super::engine::{PageChars, PdfChar};

#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub x0: f32,
    pub x1: f32,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub text: String,
    pub words: Vec<Word>,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    /// Dominant font size (by char count), points.
    pub size: f32,
    /// Dominant font weight (by char count); -1 unknown.
    pub weight: i32,
    #[allow(dead_code)]
    pub italic: bool,
    pub mono: bool,
    /// Dominant font family index (into doc fonts).
    #[allow(dead_code)]
    pub font: u16,
    /// Number of glyphs that made this line.
    pub glyphs: usize,
    /// Stream order of the first glyph (stable tie-break for sorting).
    pub order: u32,
    pub page: usize,
}

#[derive(Default)]
pub struct PageLines {
    pub index: usize,
    #[allow(dead_code)]
    pub width: f32,
    pub height: f32,
    pub lines: Vec<Line>,
    pub images: usize,
    /// Pixel-engine fusion results (regions, rules, trust). None when the
    /// rasterizer was skipped for this page.
    pub fusion: Option<crate::pdf::fusion::FusionData>,
}

/// Map unusual glyphs to readable text; returns the number of output
/// chars (0 = drop).
fn normalize_glyph(cp: char, out: &mut String) {
    // Ligatures & friends.
    let repl: &str = match cp {
        '\u{FB00}' => "ff",
        '\u{FB01}' => "fi",
        '\u{FB02}' => "fl",
        '\u{FB03}' => "ffi",
        '\u{FB04}' => "ffl",
        '\u{FB05}' | '\u{FB06}' => "st",
        '\u{2122}' => "™",
        '\u{2026}' => "…",
        '\u{2018}' | '\u{2019}' => "'",
        '\u{201C}' | '\u{201D}' => "\"",
        '\u{2013}' | '\u{2014}' => "-",
        '\u{00A0}' | '\u{3000}' => " ",
        _ => "",
    };
    if !repl.is_empty() {
        out.push_str(repl);
        return;
    }
    // Drop soft hyphens mid-text, zero-widths, private use, controls.
    let c = cp as u32;
    let drop = c == 0xAD
        || c == 0x200B
        || c == 0xFEFF
        || (0xE000..=0xF8FF).contains(&c)
        || (c < 0x20 && cp != '\t')
        || cp == '\u{FFFD}';
    if !drop {
        out.push(cp);
    }
}

/// Fonts whose glyphs are pictures, not text (checkboxes, logo marks,
/// Assemble one page's chars into horizontal lines (Rot::Horiz bucket
/// only for now; vertical lanes arrive with vertical-CJK support).
pub fn assemble(page: PageChars) -> PageLines {
    let mut chars: Vec<&PdfChar> = page
        .chars
        .iter()
        .filter(|c| {
            let a = c.angle.abs();
            // Clean horizontal text only. Drop dummy glyphs (CR/LF,
            // size-1 decorations) and dingbat-font glyphs (checkboxes,
            // seals : pictures, not text). Real space glyphs kept at
            // any size.
            a < 30.0 && c.cp != '\0' && (c.cp == ' ' || c.size >= 2.0) && !c.dingbat
        })
        .collect();

    let mut out = PageLines {
        index: page.index,
        width: page.width,
        height: page.height,
        lines: Vec::new(),
        images: page.images,
        fusion: None,
    };

    if chars.is_empty() {
        return out;
    }

    // Note: vertical text (bucketed out above) is currently not rendered;
    // the orchestrator flags it when /.-ratio is significant.

    // Baseline clustering: the glyph-box BOTTOM is the invariant anchor
    // across tight/full charbox metrics : centers drift 1-3pt between
    // kerned fragments and punctuation descenders on the same physical
    // line, which scrambled reading order when clustering on centers.
    chars.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then(a.x0.total_cmp(&b.x0))
            .then(a.order.cmp(&b.order))
    });

    let mut lines: Vec<Vec<&PdfChar>> = Vec::new();
    let mut cur: Vec<&PdfChar> = Vec::new();
    let mut cur_bl = f32::NAN;
    let mut cur_size = 0f32;
    // Tolerance: half the smaller font size, at least 1.5pt.
    let peek = crate::config::cfg().debug.pdf_chars;
    for c in chars {
        if peek {
            eprintln!(
                "stream y1={:6.1} cp={:?} s={:4.1} x0={:6.1} x1={:6.1}",
                c.y1, c.cp, c.size, c.x0, c.x1
            );
        }
        if cur.is_empty() {
            cur.push(c);
            cur_bl = c.y1;
            cur_size = c.size;
            continue;
        }
        // Tolerance: half the LARGER size in play. Comparing against
        // the SMALLER size would let a tiny glyph collapse tolerance.
        let tol = (0.5 * cur_size.max(c.size)).max(1.5);
        if (c.y1 - cur_bl).abs() <= tol {
            cur.push(c);
            // running baseline average keeps clusters tight
            cur_bl = (cur_bl * (cur.len() as f32 - 1.0) + c.y1) / cur.len() as f32;
        } else {
            if peek {
                eprintln!(
                    "--- split at y1={:6.1} cp={:?} (tol {tol:.1}) ---",
                    c.y1, c.cp
                );
            }
            lines.push(std::mem::take(&mut cur));
            cur.push(c);
            cur_bl = c.y1;
            cur_size = c.size;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }

    // Materialize each cluster into a Line (drop empty-text artifacts:
    // space glyphs / kern dummies produce geometry but no text).
    for cluster in lines {
        // Duplicate-layer split: two text runs sharing a baseline band
        // with overlapping x-ranges (print overlays/shadow layers) zip
        // when x-sorted. Split bi-modal-size clusters into sub-lines.
        let subclusters = split_duplicate_layers(cluster);
        for mut cluster in subclusters {
            cluster.sort_by(|a, b| a.x0.total_cmp(&b.x0).then(a.order.cmp(&b.order)));
            let line = build_line(&cluster, page.index);
            if !line.text.trim().is_empty() {
                out.lines.push(line);
            }
        }
    }
    out
}

/// Split a cluster into at most two sub-clusters by size proximity
/// when glyph sizes are bi-modal with strong x-overlap. Otherwise
/// returns the cluster unchanged (single-element vec).
fn split_duplicate_layers(cluster: Vec<&PdfChar>) -> Vec<Vec<&PdfChar>> {
    if cluster.len() < 6 {
        return vec![cluster];
    }
    // Dominant sizes at quarter-point resolution.
    let mut votes: Vec<(u32, usize)> = Vec::new();
    for c in &cluster {
        let q = (c.size * 4.0).round().max(1.0) as u32;
        match votes.iter_mut().find(|(v, _)| *v == q) {
            Some(e) => e.1 += 1,
            None => votes.push((q, 1)),
        }
    }
    votes.sort_by_key(|(_, n)| usize::MAX - n);
    if votes.len() < 2 {
        return vec![cluster];
    }
    let (s1q, _n1) = votes[0];
    let (s2q, n2) = votes[1];
    let (s1, s2) = (s1q as f32 / 4.0, s2q as f32 / 4.0);
    // Both sizes must represent a substantial share and differ clearly.
    if n2 * 4 < cluster.len() {
        return vec![cluster];
    }
    if (s1 - s2).abs() < 0.35 * s1.max(s2) {
        return vec![cluster];
    }
    // X-overlap of the two size groups must be substantial. Keeping
    // inline-size-mixed text (sub/superscripts, initials) intact is
    // the whole point of this gate.
    let (a, b) = if s1 > s2 { (s1, s2) } else { (s2, s1) };
    let near = |c: &&PdfChar, s: f32| (c.size - s).abs() <= 0.2 * s;
    let (mut g1x0, mut g1x1, mut g2x0, mut g2x1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for c in &cluster {
        if near(c, a) {
            g1x0 = g1x0.min(c.x0);
            g1x1 = g1x1.max(c.x1);
        } else if near(c, b) {
            g2x0 = g2x0.min(c.x0);
            g2x1 = g2x1.max(c.x1);
        }
    }
    if g1x1 <= g1x0 || g2x1 <= g2x0 {
        return vec![cluster];
    }
    let inter = (g1x1.min(g2x1) - g1x0.max(g2x0)).max(0.0);
    let union = g1x1.max(g2x1) - g1x0.min(g2x0);
    if union <= 0.0 || inter / union < 0.35 {
        return vec![cluster];
    }
    let (g1, g2): (Vec<&PdfChar>, Vec<&PdfChar>) = cluster
        .into_iter()
        .partition(|c| (c.size - a).abs() <= 0.2 * a);
    if g1.len() < 2 || g2.len() < 2 {
        return vec![g1.into_iter().chain(g2).collect()];
    }
    vec![g1, g2]
}

fn build_line(cluster: &[&PdfChar], page_index: usize) -> Line {
    let mut text = String::with_capacity(cluster.len());
    let mut words: Vec<Word> = Vec::new();
    let (mut lx0, mut ly0, mut lx1, mut ly1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);

    // Adaptive gap baseline: medians of positive inter-glyph gaps make
    // the word-break threshold robust to letterspaced text (forms,
    // small-caps) where every advance is stretched uniformly.
    let mut g: Vec<f32> = Vec::new();
    // Real glyphs only. Space glyphs are SPLIT INTO TWO KINDS:
    //  - "real spaces": an actual drawn space claiming advance width
    //    (word boundary : Korean, Devanagari, CJK↔Latin junctions)
    //  - decorations: near-zero-width run markers, invisible.
    // Distinguisher is measured WIDTH, not presence.
    let space_width_real = |c: &&PdfChar| (c.x1 - c.x0) > 0.18 * c.size.max(2.0);
    let reals: Vec<&&PdfChar> = cluster
        .iter()
        .filter(|c| c.cp != ' ' || space_width_real(c))
        .collect();
    for w in reals.windows(2) {
        g.push(w[1].x0 - w[0].x1);
    }
    g.retain(|x| *x > 0.01);
    g.sort_by(|a, b| a.total_cmp(b));
    let med_gap = g.get(g.len() / 2).copied().unwrap_or(0.0);

    // Decoration-space x-positions within this line (Chromium prints
    // these at font-run boundaries; used as break evidence for small
    // Latin gaps such as word-boundaries it inserted spacing at).
    let decor_spaces: Vec<f32> = cluster
        .iter()
        .filter(|c| c.cp == ' ')
        .map(|c| (c.x0 + c.x1) * 0.5)
        .collect();

    // Dominant stats by glyph count.
    let mut size_count: Vec<(u32, f32, usize)> = Vec::new(); // quantized size → count
    let mut weight_votes: Vec<(i32, usize)> = Vec::new();
    let mut font_votes: Vec<(u16, usize)> = Vec::new();
    let mut mono_glyphs = 0usize;
    let mut italic_glyphs = 0usize;

    let mut word_x0 = f32::NAN;
    let mut prev_x1 = 0f32;
    let mut prev_size = 10f32;
    let mut prev_cp = '\0';
    let mut glyph_count = 0usize;

    for (i, c) in reals.iter().enumerate() {
        lx0 = lx0.min(c.x0);
        ly0 = ly0.min(c.y0);
        lx1 = lx1.max(c.x1);
        ly1 = ly1.max(c.y1);

        // Space insertion: script-aware + statistics-aware (real glyph
        // gaps only : decorations are out of `reals`).
        if i > 0 {
            let gap = c.x0 - prev_x1;
            let widen = 0.16 * prev_size;
            let adaptive = (1.4 * med_gap).max(widen);
            // Script-aware pairing. "Spaceless pairs" never break inside a
            // run: Han/Kana (and Yi) have no word spaces at all;
            // Devanagari words are conjunct clusters where font-run
            // boundaries fake gaps. Hangul IS space-using (Korean) and
            // rides the Latin thresholds.
            let pair_cjk = is_han_kana(prev_cp) && is_han_kana(c.cp);
            let pair_deva = is_devanagari(prev_cp) && is_devanagari(c.cp);
            let mix_cjk = is_han_kana(prev_cp) || is_han_kana(c.cp);
            let decor = decor_spaces
                .iter()
                .any(|&sx| sx > prev_x1 - 0.5 && sx < c.x0 + 0.5);
            // A real space glyph reaching this position is a hard break.
            let real_space = prev_cp == ' ' && space_width_real(reals[i - 1]);
            let break_here = if pair_cjk {
                real_space
            } else if pair_deva {
                gap > (2.0 * med_gap).max(0.06 * prev_size)
            } else if mix_cjk {
                gap > 0.55 * prev_size || (decor && gap > 0.10 * prev_size)
            } else {
                gap > adaptive || (decor && gap > 0.10 * prev_size) || (real_space && gap > 0.02)
            };
            if break_here {
                // Close the current word.
                if !word_x0.is_nan() {
                    let wtext = take_last_word(&mut text);
                    words.push(Word {
                        text: wtext,
                        x0: word_x0,
                        x1: prev_x1,
                    });
                    word_x0 = f32::NAN;
                }
                text.push(' ');
            }
        }
        glyph_count += 1;

        let qs = (c.size * 4.0).round() as u32; // quarter-point buckets
        match size_count.iter_mut().find(|(q, _, _)| *q == qs) {
            Some(e) => e.2 += 1,
            None => size_count.push((qs, c.size, 1)),
        }
        match weight_votes.iter_mut().find(|(w, _)| *w == c.weight) {
            Some(e) => e.1 += 1,
            None => weight_votes.push((c.weight, 1)),
        }
        match font_votes.iter_mut().find(|(f, _)| *f == c.font) {
            Some(e) => e.1 += 1,
            None => font_votes.push((c.font, 1)),
        }
        if c.flags & (super::sys::FONT_FIXED_PITCH | super::engine::DONSHEET_MONO_HINT) != 0 {
            mono_glyphs += 1;
        }
        if c.flags & super::sys::FONT_ITALIC != 0 {
            italic_glyphs += 1;
        }

        let before = text.len();
        normalize_glyph(c.cp, &mut text);
        if word_x0.is_nan() && before < text.len() && text.len() > before && !text.ends_with(' ') {
            word_x0 = c.x0;
        }
        prev_x1 = c.x1.max(prev_x1);
        prev_size = c.size;
        prev_cp = c.cp;
    }
    if !word_x0.is_nan() {
        let wtext = take_last_word(&mut text);
        words.push(Word {
            text: wtext,
            x0: word_x0,
            x1: lx1,
        });
    }

    // Collapse runs of spaces and trim.
    let text = collapse_spaces(&text);
    if crate::config::cfg().debug.pdf_words {
        eprintln!(
            "[words] {:?} -> {:?}",
            text,
            words.iter().map(|w| &w.text).collect::<Vec<_>>()
        );
    }
    // BiDi: the stream is VISUAL order; any strong RTL char means the line
    // needs logical-ordering. Latin-only lines are an identity pass.
    let (text, words) = if text.chars().any(super::rotate::is_rtl_char) {
        let mut w = words;
        w.reverse();
        (super::rotate::bidi_reorder(&text), w)
    } else {
        (text, words)
    };
    // Words track the pre-collapse text spans; text cleanup is cosmetic.

    size_count.sort_by_key(|(_, _, n)| usize::MAX - n);
    weight_votes.sort_by_key(|(_, n)| usize::MAX - n);
    font_votes.sort_by_key(|(_, n)| usize::MAX - n);

    Line {
        glyphs: glyph_count.max(1),
        size: size_count.first().map(|(_, s, _)| *s).unwrap_or(10.0),
        weight: weight_votes.first().map(|(w, _)| *w).unwrap_or(-1),
        italic: italic_glyphs * 2 >= glyph_count.max(1),
        mono: mono_glyphs * 2 >= glyph_count.max(1),
        font: font_votes.first().map(|(f, _)| *f).unwrap_or(0),
        order: reals.first().map(|c| c.order).unwrap_or(0),
        page: page_index,
        text,
        words,
        x0: lx0,
        y0: ly0,
        x1: lx1,
        y1: ly1,
    }
}

/// Han + Kana + fullwidth CJK forms (no word spaces in these scripts).
fn is_han_kana(cp: char) -> bool {
    let c = cp as u32;
    matches!(c,
        0x2E80..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xF900..=0xFAFF | 0xFF00..=0xFF65
        | 0x20000..=0x2FA1F)
}

/// Devanagari (Hindi, Nepali, Sanskrit, Marathi) + Vedic extensions.
fn is_devanagari(cp: char) -> bool {
    matches!(cp as u32, 0x0900..=0x097F | 0x1CD0..=0x1CFF | 0xA800..=0xA8FF)
}

/// Back-compat name for legacy callers (includes Hangul)
#[allow(dead_code)]
fn is_cjk(cp: char) -> bool {
    is_han_kana(cp) || matches!(cp as u32, 0x1100..=0x11FF | 0xAC00..=0xD7AF)
}

/// Split off the last whitespace-separated word. Read-only on `text`:
/// trailing whitespace is skipped BEFORE locating the word start
/// (real-space glyphs in the stream make trailing spaces common).
#[allow(clippy::ptr_arg)]
fn take_last_word(text: &mut String) -> String {
    let end = text
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let start = text[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    text[start..end].to_string()
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = false;
    for c in s.chars() {
        if c == ' ' {
            ws = true;
        } else {
            if ws && !out.is_empty() {
                out.push(' ');
            }
            ws = false;
            out.push(c);
        }
    }
    out
}
