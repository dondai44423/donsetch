//! Stage 3: ordered lines → DonSift semantic blocks.
//!
//! Classification runs document-wide in reading order with a font-size
//! ladder derived from the corpus itself (no hardcoded pt sizes):
//! 1. body size = glyph-weighted mode
//! 2. heading ladder = size ratios ≥ 1.15× body, ranked
//! 3. lists/code/quotes by glyph + indentation patterns
//! 4. tables via the column-consensus detector (tables.rs)
//! 5. everything else merges into paragraphs (gap/size/indent rules)

use crate::extract::blocks::Block;

use super::layout::{Line, PageLines};
use super::tables;

/// Per-document font context, derived from the glyphs themselves.
pub struct FontCtx {
    pub body_size: f32,
    /// Sizes > 1.15× body, descending (the heading ladder).
    pub ladder: Vec<f32>,
}

pub fn font_ctx(lines: &[&Line]) -> FontCtx {
    // Glyph-weighted size histogram at half-point resolution.
    let mut hist: Vec<(u32, usize)> = Vec::new(); // (quantized, glyph count)
    for l in lines {
        let q = (l.size * 2.0).round() as u32;
        match hist.iter_mut().find(|(h, _)| *h == q) {
            Some(e) => e.1 += l.glyphs,
            None => hist.push((q, l.glyphs)),
        }
    }
    hist.sort_by_key(|(_, n)| usize::MAX - n);
    let body_size = hist.first().map(|(q, _)| *q as f32 / 2.0).unwrap_or(10.0);

    let mut ladder: Vec<f32> = hist
        .iter()
        .map(|(q, _)| *q as f32 / 2.0)
        .filter(|s| *s >= body_size * 1.15)
        .collect();
    ladder.sort_by(|a, b| b.total_cmp(a));
    ladder.dedup();

    FontCtx { body_size, ladder }
}

/// Classify the ordered lines of all pages into semantic blocks.
pub fn classify(pages: &[PageLines], ordered_by_page: &[Vec<Line>], ctx: &FontCtx) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let n_pages = pages.len();

    for pi in 0..n_pages {
        let lines = &ordered_by_page[pi];
        if lines.is_empty() {
            continue;
        }
        let fusion = pages[pi].fusion.as_ref();
        classify_page_fused(lines, ctx, n_pages > 1, &mut blocks, fusion);
        if crate::config::cfg().debug.pdf {
            eprintln!(
                "[classify] page {pi}/{} done ({} blocks)",
                n_pages,
                blocks.len()
            );
        }
    }

    // Cross-page paragraph continuation.
    merge_continuations(&mut blocks);
    blocks
}

#[allow(dead_code)]
fn classify_page(lines: &[Line], ctx: &FontCtx, multi: bool, blocks: &mut Vec<Block>) {
    classify_page_fused(lines, ctx, multi, blocks, None)
}

#[allow(clippy::too_many_lines)]
fn classify_page_fused(
    lines: &[Line],
    ctx: &FontCtx,
    multi: bool,
    blocks: &mut Vec<Block>,
    fusion: Option<&crate::pdf::fusion::FusionData>,
) {
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if line.text.trim().is_empty() {
            i += 1;
            continue;
        }

        // Code run: consecutive mono lines. Precondition excludes
        // mono lines that are also headings : otherwise the inner
        // loop can never advance past the first one (deadlock).
        if line.mono && !is_heading(line, ctx) {
            let mut j = i;
            let mut code_lines: Vec<&str> = Vec::new();
            while j < lines.len() && lines[j].mono && !is_heading(&lines[j], ctx) {
                code_lines.push(lines[j].text.as_str());
                j += 1;
            }
            let md = code_lines.join("\n");
            blocks.push(Block::Code {
                lang: None,
                code: md,
                path: Vec::new(),
            });
            i = j;
            continue;
        }

        // Heading?
        if is_heading(line, ctx) {
            blocks.push(Block::Heading {
                level: heading_level(line, ctx),
                text: line.text.trim().to_string(),
                path: Vec::new(),
            });
            i += 1;
            continue;
        }

        // List item run?
        if let Some((marker, rest)) = list_marker(&line.text) {
            let ordered = marker.0;
            let mut items = vec![rest.to_string()];
            let mut j = i + 1;
            // following lines: same list (marker), continuation (indent),
            while j < lines.len() {
                let l2 = &lines[j];
                if breaks_band(line, l2) {
                    break;
                }
                if let Some((m2, rest2)) = list_marker(&l2.text) {
                    if m2.0 == ordered {
                        items.push(rest2.to_string());
                        j += 1;
                        continue;
                    }
                    break;
                }
                // continuation line: indented relative to the marker column
                if l2.x0 > line.x0 + 0.5 && !l2.mono && gap_ok(&lines[j - 1], l2) {
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(l2.text.trim());
                    }
                    j += 1;
                    continue;
                }
                break;
            }
            blocks.push(Block::List {
                ordered,
                items,
                link_density: 0.0,
                path: Vec::new(),
            });
            i = j;
            continue;
        }

        // Paragraph + possible table region: consume a contiguous prose
        // run, then let the table detector take coherent sub-runs.
        let start = i;
        i += 1;
        while i < lines.len() && continue_paragraph(&lines[i - 1], &lines[i], ctx, multi) {
            i += 1;
        }
        let run = &lines[start..i];
        emit_prose_run(run, ctx, blocks, fusion);
    }
}

/// Replace prose runs with tables where the geometry says so.
fn emit_prose_run(
    run: &[Line],
    _ctx: &FontCtx,
    blocks: &mut Vec<Block>,
    fusion: Option<&crate::pdf::fusion::FusionData>,
) {
    let found = tables::detect(run, fusion);
    if found.is_empty() {
        emit_paragraph(run, blocks);
        return;
    }
    let mut cursor = 0usize;
    for f in found {
        if f.start > cursor {
            emit_paragraph(&run[cursor..f.start], blocks);
        }
        blocks.push(f.block);
        cursor = f.start + f.len;
    }
    if cursor < run.len() {
        emit_paragraph(&run[cursor..], blocks);
    }
}

fn emit_paragraph(run: &[Line], blocks: &mut Vec<Block>) {
    if run.is_empty() {
        return;
    }
    let mut md = String::new();
    for l in run.iter() {
        let t = l.text.trim();
        if t.is_empty() {
            continue;
        }
        if !md.is_empty() {
            md.push_str(join_separator(&md, t));
        }
        md.push_str(t);
    }
    if !md.is_empty() {
        blocks.push(Block::Para {
            md,
            link_density: 0.0,
            path: Vec::new(),
        });
    }
}

/// Separator between two prose lines. Scripts without word spaces
/// (Han, Kana, Thai) join with NOTHING; a Latin hyphen_merge joins
/// with ""; everything else joins with " ".
fn join_separator(md: &str, next: &str) -> &'static str {
    let last = md.chars().last().unwrap_or(' ');
    let first = next.chars().next().unwrap_or(' ');
    if is_spaceless_script(last) || is_spaceless_script(first) {
        return "";
    }
    if md.ends_with('-') && first.is_lowercase() {
        ""
    } else {
        " "
    }
}

/// Han + Kana + Thai: these scripts have no word spaces, and line-wrap
/// seams between them must never introduce one.
fn is_spaceless_script(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        | 0x20000..=0x2FA1F | 0x0E00..=0x0E7F)
}

/// Are two lines in different typographic bands? (list/grouping breaker)
fn breaks_band(a: &Line, b: &Line) -> bool {
    (a.size - b.size).abs() > 0.9 || a.page != b.page
}

fn gap_ok(prev: &Line, next: &Line) -> bool {
    let gap = next.y0 - prev.y1;
    gap <= 0.9 * prev.size.max(next.size) && gap >= -0.5 * prev.size
}

/// Should `next` merge into the paragraph holding `prev`?
fn continue_paragraph(prev: &Line, next: &Line, ctx: &FontCtx, multi_page: bool) -> bool {
    if next.text.trim().is_empty() || is_heading(next, ctx) {
        return false;
    }
    if next.mono || prev.mono {
        return false;
    }
    if list_marker(&next.text).is_some() {
        return false;
    }
    if multi_page && next.page != prev.page {
        return false; // page break: merge handled by merge_continuations
    }
    let size_delta = (next.size - prev.size).abs();
    if size_delta > 0.35 * ctx.body_size.max(1.0) {
        return false;
    }
    if !gap_ok(prev, next) {
        return false;
    }
    // First-line indent of `next` far right of prev's start → new paragraph.
    let indent = next.x0 - prev.x0;
    if indent > 2.0 * prev.size && indent < 6.0 * prev.size {
        // classic indented paragraph start
        return false;
    }
    true
}

/// Detect a leading list marker (unicode bullet or enum). Returns
/// (ordered?, rest-text).
fn list_marker(text: &str) -> Option<((bool,), &str)> {
    let t = text.trim_start();
    let mut ch_it = t.chars();
    let c0 = ch_it.next()?;
    if matches!(c0, '•' | '●' | '‣' | '∙' | '▪' | '·' | '⁃') {
        let rest = ch_it.as_str().trim_start();
        return Some(((false,), rest));
    }
    // dash bullets only when followed by a space
    if (c0 == '-' || c0 == '*') && ch_it.as_str().starts_with(' ') {
        return Some(((false,), ch_it.as_str().trim()));
    }
    // numbered / lettered enums: "1." "1)" "(a)" "a." "(iv)" at start
    for (re, _ord) in [
        (r"^\(?\d{1,3}\.(?=\s)", true),
        (r"^\(?\d{1,3}\)(?=\s)", true),
        (r"^\([a-z]\)(?=\s)", true),
        (r"^\([ivxlcdmIVXLCDM]{1,6}\)(?=\s)", true),
    ] {
        if let Some(end) = match_prefix(t, re) {
            let rest = t[end..].trim_start();
            if !rest.is_empty() {
                return Some(((true,), rest));
            }
        }
    }
    None
}

/// Cheap prefix-match against the tiny enum regex set. Returns the
/// matched byte length.
fn match_prefix(t: &str, re: &str) -> Option<usize> {
    // Avoid a regex dependency here: implement the few patterns directly.
    let b = t.as_bytes();
    match re {
        r"^\(?\d{1,3}\.(?=\s)" => {
            let mut i = 0;
            if b.first() == Some(&b'(') {
                i = 1;
            }
            let ds = i;
            while i < b.len() && b[i].is_ascii_digit() && i - ds < 3 {
                i += 1;
            }
            if i == ds || b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
            if b.get(i).map(|c| c.is_ascii_whitespace()).unwrap_or(false) {
                Some(i)
            } else {
                None
            }
        }
        r"^\(?\d{1,3}\)(?=\s)" => {
            let mut i = 0;
            if b.first() == Some(&b'(') {
                i = 1;
            }
            let ds = i;
            while i < b.len() && b[i].is_ascii_digit() && i - ds < 3 {
                i += 1;
            }
            if i == ds || b.get(i) != Some(&b')') {
                return None;
            }
            i += 1;
            if b.get(i).map(|c| c.is_ascii_whitespace()).unwrap_or(false) {
                Some(i)
            } else {
                None
            }
        }
        r"^\([a-z]\)(?=\s)" => {
            if b.len() >= 4
                && b[0] == b'('
                && b[1].is_ascii_lowercase()
                && b[2] == b')'
                && b[3].is_ascii_whitespace()
            {
                Some(3)
            } else {
                None
            }
        }
        r"^\([ivxlcdmIVXLCDM]{1,6}\)(?=\s)" => {
            if b.first() != Some(&b'(') {
                return None;
            }
            let mut i = 1;
            while i < b.len()
                && matches!(
                    b[i],
                    b'i' | b'v'
                        | b'x'
                        | b'l'
                        | b'c'
                        | b'd'
                        | b'm'
                        | b'I'
                        | b'V'
                        | b'X'
                        | b'L'
                        | b'C'
                        | b'D'
                        | b'M'
                )
                && i < 7
            {
                i += 1;
            }
            if i == 1 || b.get(i) != Some(&b')') {
                return None;
            }
            i += 1;
            if b.get(i).map(|c| c.is_ascii_whitespace()).unwrap_or(false) {
                Some(i)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Heading decision: size ladder + weight + typography signals.
pub fn is_heading(l: &Line, ctx: &FontCtx) -> bool {
    let t = l.text.trim();
    if t.len() > 140 {
        return false;
    }
    // A line that starts with a list marker is a list item. Never a
    // heading : AcroForm widgets emit as "- **name**: value" lines and
    // must not promote into the heading ladder on small-body forms.
    if t.starts_with("- ") || t.starts_with("* ") {
        return false;
    }
    // Sentence-terminal punctuation is a strong NOT-heading signal:
    // front-matter license text can be typeset at ladder sizes; real
    // section headings do not end in commas/periods/colons.
    let numbered = numbered_section(t);
    if !numbered && t.ends_with(['.', '!', '?', ',', ';', ':']) {
        return false;
    }
    let r = l.size / ctx.body_size.max(0.1);
    // Size-only branch: headings are short standalone lines. Long
    // big-font lines (license legalese etc.) are paragraphs.
    if r >= 1.18 && t.len() <= 65 {
        return true;
    }
    if r >= 1.08 && l.weight >= 600 && t.len() <= 110 {
        return true;
    }
    // Numbered section pattern with bold face at body-ish size.
    let bold = l.weight >= 600;
    if bold && t.len() < 110 && numbered {
        return true;
    }
    // ALL-CAPS short bold lines.
    if bold && t.len() < 60 {
        let uppers = t.chars().filter(|c| c.is_uppercase()).count();
        let letters = t.chars().filter(|c| c.is_alphabetic()).count();
        if letters >= 3 && uppers * 2 > letters {
            return true;
        }
    }
    false
}

fn numbered_section(t: &str) -> bool {
    // "1.", "1", "3.2", "3.2.1" followed by a titlecased word
    let bytes = t.as_bytes();
    let mut i = 0;
    let mut dots = 0usize;
    loop {
        let ds = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == ds || i - ds > 3 {
            return false;
        }
        if bytes.get(i) == Some(&b'.') && dots < 4 {
            let nxt = bytes.get(i + 1);
            // allow trailing "." (e.g. "1.") or continuing number, or space
            if nxt.map(|c| c.is_ascii_digit()).unwrap_or(false) {
                dots += 1;
                i += 1;
                continue;
            }
            // trailing dot then whitespace/text
            i += 1;
            break;
        }
        break;
    }
    if i == 0 || i >= t.len() {
        return false;
    }
    // require whitespace then a letter
    matches!(bytes.get(i), Some(b' ') | Some(b'\t'))
        && t[i..]
            .chars()
            .nth(1)
            .map(|c| c.is_alphabetic())
            .unwrap_or(false)
}

fn heading_level(l: &Line, ctx: &FontCtx) -> u8 {
    // Ladder rank → level (biggest size = 1). Numbered depth refines.
    let r = l.size / ctx.body_size.max(0.1);
    let mut level = if r >= 1.5 {
        1
    } else if r >= 1.3 {
        2
    } else {
        3
    };
    // Numbered section depths: 1 = lvl2, 3.2 = lvl3, 3.2.1 = lvl4
    if let Some(depth) = numbered_depth(l.text.trim()) {
        level = (depth as u8 + 1).min(6);
        // Promote big fonts at least to their size ladder level.
        if r >= 1.3 {
            level = level.min(2);
        }
    } else if !ctx.ladder.is_empty() {
        // rank by position in ladder
        for (rank, &s) in ctx.ladder.iter().enumerate() {
            if (l.size - s).abs() < 0.5 {
                level = (rank as u8 + 1).min(6);
                break;
            }
        }
    }
    level.clamp(1, 6)
}

fn numbered_depth(t: &str) -> Option<usize> {
    let bytes = t.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    loop {
        let ds = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == ds || i - ds > 3 {
            if depth > 0 {
                break;
            }
            return None;
        }
        depth += 1;
        match bytes.get(i) {
            Some(b'.')
                if bytes
                    .get(i + 1)
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false) =>
            {
                i += 1;
            }
            Some(b'.') => {
                i += 1;
                break;
            }
            _ => break,
        }
        if depth > 5 {
            break;
        }
    }
    if depth == 0 {
        return None;
    }
    if matches!(bytes.get(i), Some(b' ') | Some(b'\t'))
        && t[i..]
            .chars()
            .nth(1)
            .map(|c| c.is_alphabetic())
            .unwrap_or(false)
    {
        Some(depth)
    } else {
        None
    }
}

/// Merge a trailing paragraph of a page with a leading paragraph of the
/// next page when the fonts agree and the text flows.
fn merge_continuations(blocks: &mut Vec<Block>) {
    let mut i = 0;
    while i + 1 < blocks.len() {
        let merge_md = {
            let (a, b) = (&blocks[i], &blocks[i + 1]);
            match (a, b) {
                (Block::Para { md: ma, .. }, Block::Para { md: mb, .. }) => {
                    // Heuristic: join when `a` doesn't end like a paragraph
                    // end (no sentence-final punctuation) OR `mb` starts
                    // lowercase. (kept conservative)
                    let a_open = !ma.trim_end().ends_with(['.', '!', '?', ':', ';']);
                    let b_lower = mb.chars().next().map(|c| c.is_lowercase()).unwrap_or(false);
                    if a_open || b_lower {
                        Some(format!("{} {}", ma.trim_end(), mb.trim_start()))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        if let Some(md) = merge_md {
            if let Some(Block::Para { md: ma, .. }) = blocks.get_mut(i) {
                *ma = md;
            }
            blocks.remove(i + 1);
        } else {
            i += 1;
        }
    }
}
