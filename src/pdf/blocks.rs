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
#[derive(Clone)]
pub struct FontCtx {
    pub body_size: f32,
    /// Sizes > 1.15× body, descending (the heading ladder).
    pub ladder: Vec<f32>,
    /// The typefaces that have introduced text (see [`earned_headings`]).
    /// `None` until [`classify`] has seen the document, and when no typeface
    /// earned it; a line is then judged on its shape alone.
    pub earned: Option<Earned>,
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

    FontCtx {
        body_size,
        ladder,
        earned: None,
    }
}

/// A caption introduces a figure or a table; it is body-sized text but it is not
/// what a heading introduces.
fn is_caption(text: &str) -> bool {
    let t = text.trim_start();
    ["Figure", "Fig.", "Table", "Algorithm", "Listing"]
        .iter()
        .any(|p| {
            t.strip_prefix(p)
                .is_some_and(|rest| rest.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        })
}

/// A line's size to the half point. Weight is left out on purpose: sibling
/// headings differ in weight between 600 and 700 as often as they differ in size.
fn style_of(l: &Line) -> u32 {
    (l.size * 2.0).round() as u32
}

/// The fonts and sizes that have introduced text.
#[derive(Clone, Default)]
pub struct Earned {
    fonts: Vec<u16>,
    sizes: Vec<u32>,
}

/// Which typefaces have earned the right to make headings.
///
/// A heading introduces what follows it: a body-sized line that is not a caption.
/// A figure's labels are typeset to look like headings (large, bold, short), but
/// what follows them is another label, a smaller heading, a caption or nothing.
/// A font, or a size, earns the right by introducing text at
/// least twice. A font is not enough on its own because one document's headings
/// can be set in several fonts, and a style is not enough on its own because a
/// title, a section and a subsection differ in size; a line needs either. `None`
/// where nothing earned it (a document of headings and no prose): nothing is
/// demoted.
///
/// Failure mode, stated: a real heading in a font and a style used nowhere else,
/// on a page after the first and followed by a table or a list, loses its outline
/// entry and reads as a paragraph; no text is dropped. The first page is exempt (a
/// title is often alone in its face), and so are numbered sections.
fn earned_headings(ordered_by_page: &[Vec<Line>], ctx: &FontCtx) -> Option<Earned> {
    use std::collections::BTreeMap;
    let mut fonts: BTreeMap<u16, usize> = BTreeMap::new();
    let mut sizes: BTreeMap<u32, usize> = BTreeMap::new();
    for lines in ordered_by_page {
        for (i, l) in lines.iter().enumerate() {
            if !heading_shape(l, ctx) {
                continue;
            }
            let introduces = lines.get(i + 1).is_some_and(|n| {
                let r = n.size / ctx.body_size.max(0.1);
                (0.85..=1.2).contains(&r)
                    && !is_caption(&n.text)
                    && !heading_shape(n, ctx)
                    && n.glyphs >= 20
            });
            if introduces {
                *fonts.entry(l.font).or_default() += 1;
                *sizes.entry(style_of(l)).or_default() += 1;
            }
        }
    }
    let earned = Earned {
        fonts: fonts
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|(f, _)| f)
            .collect(),
        sizes: sizes
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|(s, _)| s)
            .collect(),
    };
    (!earned.fonts.is_empty() || !earned.sizes.is_empty()).then_some(earned)
}

/// Classify the ordered lines of all pages into semantic blocks.
pub fn classify(pages: &[PageLines], ordered_by_page: &[Vec<Line>], ctx: &FontCtx) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let n_pages = pages.len();
    let ctx = &FontCtx {
        earned: earned_headings(ordered_by_page, ctx),
        ..ctx.clone()
    };

    for pi in 0..n_pages {
        let lines = &ordered_by_page[pi];
        if lines.is_empty() {
            continue;
        }
        let fusion = pages[pi].fusion.as_ref();
        classify_page_fused(lines, ctx, n_pages > 1, &mut blocks, fusion);
        if crate::config::cfg().debug.pdf {
            crate::diag!(
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

/// Heading decision: the line looks like one, and its typeface is one that
/// introduces text rather than one a figure's labels are set in.
pub fn is_heading(l: &Line, ctx: &FontCtx) -> bool {
    heading_shape(l, ctx)
        && (l.page == 0
            || numbered_section(l.text.trim())
            || ctx
                .earned
                .as_ref()
                .is_none_or(|e| e.fonts.contains(&l.font) || e.sizes.contains(&style_of(l))))
}

/// Does the line look like a heading: size ladder + weight + typography signals.
fn heading_shape(l: &Line, ctx: &FontCtx) -> bool {
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
    if is_diagram_row(l) {
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

/// A line that cannot be a heading whatever its size: no letters to name a
/// section ("2262", a figure's axis tick), or words placed far apart ("Swin
/// Swin Swin Swin", "Stage 1 Stage 2 Stage 3": the labels of a diagram's
/// columns, which the line builder joins because they share a baseline).
/// A heading's words sit a word-space apart; a gap of more than 1.2 em is a
/// separately placed fragment.
///
/// Failure mode, stated: a heading typeset with extreme word spacing loses its
/// outline entry and reads as a paragraph. Nothing is dropped from the text.
/// A figure label that is one contiguous phrase ("Input-Input Layer5") passes
/// this check; [`earned_headings`] judges it by what follows it.
fn is_diagram_row(l: &Line) -> bool {
    if l.text.chars().filter(|c| c.is_alphabetic()).count() < 2 {
        return true;
    }
    let limit = 1.2 * l.size;
    l.words.windows(2).any(|w| w[1].x0 - w[0].x1 > limit)
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

#[cfg(test)]
mod diagram_row_tests {
    use super::*;
    use crate::pdf::layout::Word;

    /// A line from `(text, x0, x1)` words at `size` points.
    fn line(size: f32, words: &[(&str, f32, f32)]) -> Line {
        Line {
            text: words.iter().map(|w| w.0).collect::<Vec<_>>().join(" "),
            words: words
                .iter()
                .map(|(t, x0, x1)| Word {
                    text: (*t).into(),
                    x0: *x0,
                    x1: *x1,
                })
                .collect(),
            x0: words.first().map(|w| w.1).unwrap_or(0.0),
            y0: 0.0,
            x1: words.last().map(|w| w.2).unwrap_or(0.0),
            y1: size,
            size,
            weight: 400,
            italic: false,
            mono: false,
            font: 0,
            glyphs: 20,
            order: 0,
            page: 0,
        }
    }

    fn ctx() -> FontCtx {
        FontCtx {
            body_size: 10.0,
            ladder: vec![14.0],
            earned: None,
        }
    }

    /// The labels of a diagram's columns share a baseline, so the line builder
    /// joins them into one line at a large size. Words placed 80 points apart
    /// are fragments, not a title.
    #[test]
    fn columns_of_a_figure_are_not_a_heading() {
        let swin = line(
            14.0,
            &[
                ("Swin", 10.0, 40.0),
                ("Swin", 120.0, 150.0),
                ("Swin", 230.0, 260.0),
            ],
        );
        assert!(!is_heading(&swin, &ctx()));
    }

    #[test]
    fn a_line_with_no_letters_is_not_a_heading() {
        assert!(!is_heading(&line(14.0, &[("2262", 10.0, 40.0)]), &ctx()));
        assert!(!is_heading(&line(14.0, &[("()", 10.0, 20.0)]), &ctx()));
    }

    /// The negatives: a real heading, a numbered one, and one set a little
    /// loose still are headings.
    #[test]
    fn real_headings_still_are() {
        let plain = line(
            14.0,
            &[
                ("Scaled", 10.0, 50.0),
                ("Dot-Product", 54.0, 120.0),
                ("Attention", 124.0, 180.0),
            ],
        );
        assert!(is_heading(&plain, &ctx()));
        let loose = line(14.0, &[("Related", 10.0, 56.0), ("Work", 70.0, 100.0)]);
        assert!(
            is_heading(&loose, &ctx()),
            "a 14 pt gap at 14 pt type is under 1.2 em"
        );
        let short = line(14.0, &[("FAQ", 10.0, 40.0)]);
        assert!(is_heading(&short, &ctx()), "three letters is a heading");
    }
}

#[cfg(test)]
mod earned_heading_tests {
    use super::*;
    use crate::pdf::layout::Word;

    fn line(text: &str, size: f32, font: u16, page: usize, glyphs: usize) -> Line {
        Line {
            text: text.into(),
            words: vec![Word {
                text: text.into(),
                x0: 100.0,
                x1: 100.0 + text.len() as f32 * size * 0.5,
            }],
            x0: 100.0,
            y0: 0.0,
            x1: 200.0,
            y1: size,
            size,
            weight: 700,
            italic: false,
            mono: false,
            font,
            glyphs,
            order: 0,
            page,
        }
    }

    fn body(page: usize) -> Line {
        let mut l = line(
            "A paragraph of running text that goes on for a while.",
            10.0,
            3,
            page,
            55,
        );
        l.weight = 400;
        l
    }

    fn ctx() -> FontCtx {
        FontCtx {
            body_size: 10.0,
            ladder: vec![14.0, 20.0],
            earned: None,
        }
    }

    /// Three real sections (font 1, 14 pt, each followed by prose) and a figure's
    /// labels (font 24, 20 pt, followed by labels and a caption) on page 2.
    fn document() -> Vec<Vec<Line>> {
        vec![
            vec![
                line("Introduction", 14.0, 1, 0, 12),
                body(0),
                line("Method", 14.0, 1, 0, 6),
                body(0),
            ],
            vec![
                line("Results", 14.0, 1, 1, 7),
                body(1),
                line("Layer5", 20.0, 24, 1, 6),
                line("Layer5", 20.0, 24, 1, 6),
                line("Figure 4: Two attention heads.", 10.0, 3, 1, 30),
            ],
        ]
    }

    #[test]
    fn a_figures_labels_are_not_headings_but_the_documents_headings_are() {
        let doc = document();
        let mut c = ctx();
        c.earned = earned_headings(&doc, &c);
        assert!(is_heading(&doc[1][0], &c), "Results introduces text");
        assert!(!is_heading(&doc[1][2], &c), "Layer5 introduces nothing");
        assert!(!is_heading(&doc[1][3], &c));
    }

    #[test]
    fn the_first_page_and_numbered_sections_are_exempt() {
        let mut doc = document();
        // A title alone in its face, on the first page.
        doc[0].insert(0, line("Pro Git Handbook", 20.0, 24, 0, 16));
        // A numbered section in an unearned face, later.
        doc[1].push(line("3.2 Attention", 20.0, 24, 1, 13));
        let mut c = ctx();
        c.earned = earned_headings(&doc, &c);
        assert!(is_heading(&doc[0][0], &c), "a title on page one is kept");
        assert!(
            is_heading(doc[1].last().unwrap(), &c),
            "a numbered section is a heading in any face"
        );
    }

    #[test]
    fn nothing_is_demoted_in_a_document_where_no_face_introduces_text() {
        // Headings and nothing else: no face earns the right, so none loses it.
        let doc = vec![vec![
            line("Alpha", 14.0, 1, 1, 5),
            line("Beta", 14.0, 1, 1, 4),
            line("Gamma", 20.0, 24, 1, 5),
        ]];
        let mut c = ctx();
        c.earned = earned_headings(&doc, &c);
        assert!(c.earned.is_none());
        assert!(doc[0].iter().all(|l| is_heading(l, &c)));
    }

    #[test]
    fn a_sibling_heading_in_another_weight_shares_the_size_that_earned_it() {
        // Foreword (w700) and Introduction (w600) at the same size, each once.
        let mut foreword = line("Foreword", 14.0, 1, 1, 8);
        foreword.weight = 700;
        let mut intro = line("Introduction", 14.0, 2, 1, 12);
        intro.weight = 600;
        let doc = vec![vec![foreword, body(1), intro, body(1)]];
        let mut c = ctx();
        c.earned = earned_headings(&doc, &c);
        assert!(is_heading(&doc[0][0], &c) && is_heading(&doc[0][2], &c));
    }

    #[test]
    fn a_caption_is_not_what_a_heading_introduces() {
        assert!(is_caption("Figure 4: Two attention heads."));
        assert!(is_caption("Table 2 Results"));
        assert!(is_caption("Fig. 3 The model"));
        assert!(!is_caption("Figures of speech are plentiful."));
        assert!(!is_caption("Tables turned"));
    }
}
