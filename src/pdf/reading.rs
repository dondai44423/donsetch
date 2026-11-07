//! Stage 2: furniture suppression + reading-order reconstruction.
//!
//! Furniture: lines in the top/bottom page bands recurring across many
//! pages (running heads, footers, page numbers) are dropped : they add
//! noise for downstream LLM consumption.
//!
//! Reading order: recursive XY-cut over line bounding boxes. Vertical
//! gutters split columns; horizontal gaps split rows. This produces
//! correct order for single- and multi-column documents without any
//! layout model beyond the geometry itself.

use super::layout::{Line, PageLines};

/// Normalized text for cross-page furniture matching.
fn norm_key(t: &str) -> String {
    // Digit RUNS collapse to one '#': otherwise the same footer on pages
    // 81 and 733 fragments into different keys and recurring furniture
    // slips below the frequency threshold.
    let mut out = String::with_capacity(t.len());
    let mut ws = false;
    let mut in_digits = false;
    for c in t.trim().to_lowercase().chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
            ws = false;
        } else if c.is_whitespace() {
            in_digits = false;
            ws = true;
        } else if !ws || !out.is_empty() {
            in_digits = false;
            if ws {
                out.push(' ');
            }
            ws = false;
            out.push(c);
        } else {
            in_digits = false;
        }
    }
    out
}

/// Drop repeated running heads/footers.
///
/// Only lines in the top band (bottom edge in the top 10% of the page) or
/// the bottom band (top edge at or below 92% of the page height) are
/// candidates, and documents under four pages are left untouched. A band
/// line is dropped when it is a bare page number, when its normalized text
/// recurs in the same band on at least three pages and on at least half of
/// them (rounded down), or when it is at most 90 bytes long and more than
/// half of its word bigrams are frequent in its band: carried by that band
/// on at least three of the first six pages. Lines set at 1.18 times the
/// document's dominant text size or larger are left out of that bigram
/// test.
pub fn suppress_furniture(pages: &mut [PageLines]) {
    if pages.len() < 4 {
        return;
    }
    let mut hist: std::collections::HashMap<(String, u8), usize> = std::collections::HashMap::new(); // (normtext, 0=top|1=bottom) -> page count
    for p in pages.iter() {
        let mut seen_here: std::collections::HashSet<(String, u8)> =
            std::collections::HashSet::new();
        for l in &p.lines {
            let band = if l.y1 <= p.height * 0.10 {
                0
            } else if l.y0 >= p.height * 0.92 {
                1
            } else {
                continue;
            };
            let key = (norm_key(&l.text), band);
            if seen_here.insert(key.clone()) {
                *hist.entry(key).or_default() += 1;
            }
        }
    }
    let need = (pages.len() / 2).max(3);
    for p in pages.iter_mut() {
        let h = p.height;
        p.lines.retain(|l| {
            let band = if l.y1 <= h * 0.10 {
                Some(0u8)
            } else if l.y0 >= h * 0.92 {
                Some(1u8)
            } else {
                None
            };
            match band {
                Some(b) => {
                    let key = (norm_key(&l.text), b);
                    let recurs = hist.get(&key).map(|n| *n >= need).unwrap_or(false);
                    // Keep: not a page number, not recurring furniture.
                    !is_page_number(&key.0) && !recurs
                }
                None => true,
            }
        });
    }

    // ---- pass 2: band bigram families --------------------------------------
    // Exact keys fragment on per-page variation (roman numerals, mixed
    // prefixes). A footer family like "© adobe systems incorporated …
    // reserved 81 / iii / 420" shares word bigrams that recur broadly.
    // A bigram is frequent in a band when at least `cutoff` sampled pages
    // carry it in that band. Counts are kept per band: a phrase in a page's
    // head and in its foot, or in the heads of some pages and the feet of
    // others, does not add up to more pages than either band shows. The
    // floor of three pages is pass 1's: two pages sharing a phrase are not
    // a recurrence, even in a four-page document.
    // A SHORT band line is family when more than half of its bigrams are
    // frequent in its own band. Running heads and footers repeat nearly
    // all of their words; a body sentence or a slide title that shares one
    // phrase with other band lines ("of the", "q# revenue", "form w-#")
    // survives when it has at least as many bigrams of its own. Lines
    // >90 chars are live content and never family-killed.
    // Lines set at 1.18x the body size or larger (the block classifier's
    // size-only heading cutoff) are neither counted nor family-killed:
    // slide and section titles are set that much larger than the body
    // text, running heads and footers rarely are. The classifier's other
    // branch, 1.08x and bold, is not used here: the PDF 32000 footer above
    // is 11 pt bold over a 10 pt body. A title that repeats exactly on most
    // pages is still pass 1's to drop.
    if pages.len() >= 4 {
        let body = {
            let all: Vec<&Line> = pages.iter().flat_map(|p| p.lines.iter()).collect();
            super::blocks::font_ctx(&all).body_size.max(0.1)
        };
        let heading_size = |l: &Line| l.size / body >= 1.18;
        let mut bfreq: [std::collections::HashMap<String, usize>; 2] = [
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        ];
        let sample = pages.len().min(6);
        for p in pages.iter().take(sample) {
            let h = p.height;
            let mut seen: [std::collections::HashSet<String>; 2] = [
                std::collections::HashSet::new(),
                std::collections::HashSet::new(),
            ];
            for l in &p.lines {
                let band = if l.y1 <= h * 0.10 {
                    Some(0usize)
                } else if l.y0 >= h * 0.92 {
                    Some(1usize)
                } else {
                    None
                };
                let Some(b) = band else { continue };
                if heading_size(l) {
                    continue;
                }
                let k = norm_key(&l.text);
                let words: Vec<&str> = k.split_whitespace().collect();
                for w in words.windows(2) {
                    let bg = format!("{} {}", w[0], w[1]);
                    if seen[b].insert(bg.clone()) {
                        *bfreq[b].entry(bg).or_default() += 1;
                    }
                }
            }
        }
        let cutoff = (sample / 2).max(3);
        for p in pages.iter_mut() {
            let h = p.height;
            p.lines.retain(|l| {
                let b = if l.y1 <= h * 0.10 {
                    0
                } else if l.y0 >= h * 0.92 {
                    1
                } else {
                    return true;
                };
                if l.text.len() > 90 || heading_size(l) {
                    return true;
                }
                let k = norm_key(&l.text);
                let words: Vec<&str> = k.split_whitespace().collect();
                let bigrams = words.len().saturating_sub(1);
                let frequent = words
                    .windows(2)
                    .filter(|w| {
                        bfreq[b]
                            .get(&format!("{} {}", w[0], w[1]))
                            .is_some_and(|&n| n >= cutoff)
                    })
                    .count();
                frequent * 2 <= bigrams
            });
        }
    }
}

fn is_page_number(norm: &str) -> bool {
    let t = norm.trim_matches(|c: char| c == '-' || c.is_whitespace());
    !t.is_empty() && t.chars().all(|c| c == '#') && t.len() <= 5
}

/// Reading-order for one page via recursive XY-cut. Returns lines in
/// visual reading order.
pub fn page_order(mut lines: Vec<Line>) -> Vec<Line> {
    // Fast path: tiny pages sort directly.
    if lines.len() < 6 {
        lines.sort_by(|a, b| {
            a.y0.total_cmp(&b.y0)
                .then(a.x0.total_cmp(&b.x0))
                .then(a.order.cmp(&b.order))
        });
        return lines;
    }
    let body = median_size(&lines);
    let mut acc = Vec::with_capacity(lines.len());
    cut(lines, 0, &mut acc, body);
    acc
}

fn median_size(lines: &[Line]) -> f32 {
    let mut sizes: Vec<f32> = lines.iter().map(|l| l.size).collect();
    sizes.sort_by(|a, b| a.total_cmp(b));
    sizes.get(sizes.len() / 2).copied().unwrap_or(10.0)
}

/// Vertical/rotated layout suspect: the page's lines are nearly all
/// single glyphs or glyph pairs (chars stacked as columns, not rows).
/// Horizontal pages virtually never look like this.
pub fn is_vertical_suspect(p: &PageLines) -> bool {
    if p.lines.len() < 6 {
        return false;
    }
    let short = p.lines.iter().filter(|l| l.glyphs <= 2).count();
    short * 3 >= p.lines.len()
}

fn base_sort(lines: &mut [Line]) {
    lines.sort_by(|a, b| {
        // sloppy y-bucket first (quarter of size), then x, then order
        let ya = a.y0;
        let yb = b.y0;
        ya.total_cmp(&yb)
            .then(a.x0.total_cmp(&b.x0))
            .then(a.order.cmp(&b.order))
    });
}

fn cut(lines: Vec<Line>, depth: usize, acc: &mut Vec<Line>, body: f32) {
    if lines.len() <= 3 || depth >= 7 {
        let mut v = lines;
        base_sort(&mut v);
        acc.extend(v);
        return;
    }
    // Candidate gutters on each axis.
    let x_gap = widest_gap(&lines, true, 0.75 * body);
    let y_gap = widest_gap(&lines, false, 0.65 * body);

    match (x_gap, y_gap) {
        (Some((xmid, xw)), Some((ymid, yw))) => {
            if xw >= yw {
                split_recurse(lines, true, xmid, depth, acc, body);
            } else {
                split_recurse(lines, false, ymid, depth, acc, body);
            }
        }
        (Some((xmid, _)), None) => split_recurse(lines, true, xmid, depth, acc, body),
        (None, Some((ymid, _))) => split_recurse(lines, false, ymid, depth, acc, body),
        (None, None) => {
            let mut v = lines;
            base_sort(&mut v);
            acc.extend(v);
        }
    }
}

/// Find the widest whitespace gutter along an axis. For `vertical=true`
/// the gutter runs top-to-bottom (x axis split) and vice versa.
fn widest_gap(lines: &[Line], vertical_split: bool, min_width: f32) -> Option<(f32, f32)> {
    // Interval sweep of bboxes projected onto the split axis.
    let mut iv: Vec<(f32, f32)> = lines
        .iter()
        .map(|l| {
            if vertical_split {
                (l.x0, l.x1)
            } else {
                (l.y0, l.y1)
            }
        })
        .collect();
    iv.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut best: Option<(f32, f32)> = None; // (mid, width)
    let mut covered_to = f32::MIN;
    let mut rs: Option<f32> = None; // gap start ("running stop" of coverage)
    for seg in iv {
        if covered_to == f32::MIN {
            covered_to = seg.1;
            continue;
        }
        let gap = seg.0 - covered_to;
        if gap > 0.0 {
            let start = covered_to;
            let end = seg.0;
            let width = end - start;
            if width >= min_width {
                let mid = (start + end) * 0.5;
                if best.map(|(_, w)| width > w).unwrap_or(true) {
                    best = Some((mid, width));
                    rs = None;
                }
            }
            let _ = rs;
        }
        covered_to = covered_to.max(seg.1);
    }
    best
}

fn split_recurse(
    lines: Vec<Line>,
    vertical_split: bool,
    mid: f32,
    depth: usize,
    acc: &mut Vec<Line>,
    body: f32,
) {
    let (mut a, b): (Vec<Line>, Vec<Line>) = lines.into_iter().partition(|l| {
        let c = if vertical_split {
            (l.x0 + l.x1) * 0.5
        } else {
            (l.y0 + l.y1) * 0.5
        };
        c < mid
    });
    if a.is_empty() || b.is_empty() {
        // Degenerate split: keep everything, ordered.
        let mut v = b;
        v.append(&mut a);
        base_sort(&mut v);
        acc.extend(v);
        return;
    }
    // Order: x-cuts read left→right, y-cuts read top→bottom : the (a < mid)
    // partition already holds left/top in `a`.
    cut(a, depth + 1, acc, body);
    cut(b, depth + 1, acc, body);
}

#[cfg(test)]
mod furniture_tests {
    use super::*;

    fn line(text: &str, y: f32, page: usize) -> Line {
        Line {
            text: text.into(),
            words: vec![],
            x0: 50.0,
            y0: y,
            x1: 400.0,
            y1: y + 10.0,
            size: 10.0,
            weight: 400,
            italic: false,
            mono: false,
            font: 0,
            glyphs: text.len(),
            order: 0,
            page,
        }
    }

    #[test]
    fn repeated_furniture_is_removed_and_unique_source_notes_survive() {
        let mut pages: Vec<_> = (0..4)
            .map(|index| PageLines {
                index,
                width: 612.0,
                height: 792.0,
                images: 0,
                fusion: None,
                lines: vec![
                    line("Running document header", 20.0, index),
                    line("Body evidence", 300.0, index),
                    line("Recurring publisher footer", 750.0, index),
                    line(&(index + 1).to_string(), 770.0, index),
                ],
            })
            .collect();
        pages[0]
            .lines
            .push(line("Unique proceedings note", 730.0, 0));
        suppress_furniture(&mut pages);
        for page in &pages {
            assert!(page.lines.iter().any(|l| l.text == "Body evidence"));
            assert!(!page.lines.iter().any(|l| l.text.contains("Running")
                || l.text.contains("Recurring")
                || l.text == (page.index + 1).to_string()));
        }
        assert!(
            pages[0]
                .lines
                .iter()
                .any(|l| l.text == "Unique proceedings note")
        );
    }

    /// Slide-shaped pages (720x540): one 26 pt bold title per page in the
    /// top 10% band, over a body line that is outside both bands.
    fn slide_deck(titles: &[&str]) -> Vec<PageLines> {
        titles
            .iter()
            .enumerate()
            .map(|(index, title)| PageLines {
                index,
                width: 720.0,
                height: 540.0,
                images: 0,
                fusion: None,
                lines: vec![
                    Line {
                        y1: 46.0,
                        size: 26.0,
                        weight: 700,
                        ..line(title, 20.0, index)
                    },
                    line("Body point for this slide.", 160.0, index),
                ],
            })
            .collect()
    }

    fn assert_titles_survive(titles: &[&str]) {
        let mut pages = slide_deck(titles);
        suppress_furniture(&mut pages);
        for (p, t) in pages.iter().zip(titles) {
            assert!(
                p.lines.iter().any(|l| l.text == *t),
                "distinct title {t:?} was suppressed as furniture"
            );
        }
    }

    #[test]
    fn distinct_titles_sharing_a_common_bigram_survive() {
        // Every title shares "of the"; no title repeats.
        assert_titles_survive(&[
            "Origins of the Project",
            "Scope of the Work",
            "Design of the System",
            "Cost of the Plan",
            "Risks of the Rollout",
            "Review of the Results",
        ]);
    }

    #[test]
    fn a_four_slide_deck_keeps_titles_sharing_a_numbered_phrase() {
        // "q# revenue" is on every slide and "revenue growth" on two.
        assert_titles_survive(&[
            "Q1 Revenue Growth",
            "Q2 Revenue Growth",
            "Q3 Revenue Decline",
            "Q4 Revenue Outlook",
        ]);
    }

    #[test]
    fn titles_sharing_most_of_their_words_survive() {
        // "of the" and "the design" are on every title: two of each
        // title's three bigrams. Only their size marks them as titles.
        assert_titles_survive(&[
            "Overview of the Design",
            "Scope of the Design",
            "Cost of the Design",
            "Risks of the Design",
            "Goals of the Design",
            "Review of the Design",
        ]);
    }

    #[test]
    fn a_title_repeated_on_every_slide_is_still_furniture() {
        // Heading size keeps a line out of the bigram families only: the
        // same title on every page is recurring furniture for pass 1.
        let mut pages = slide_deck(&["Quarterly Review"; 6]);
        suppress_furniture(&mut pages);
        for p in &pages {
            assert!(
                !p.lines.iter().any(|l| l.text == "Quarterly Review"),
                "page {} kept the repeated title",
                p.index
            );
            assert!(
                p.lines
                    .iter()
                    .any(|l| l.text == "Body point for this slide.")
            );
        }
    }

    #[test]
    fn a_bold_footer_family_a_point_above_the_body_is_still_furniture() {
        // The PDF 32000 footer: 11 pt bold over a 10 pt body, its page
        // label alternating sides, so no exact key recurs.
        let notice = "© Adobe Systems Incorporated 2008 – All rights reserved";
        let labels = ["i", "ii", "iii", "iv", "v", "vi", "81", "420"];
        let mut pages: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, n)| {
                let foot = if index % 2 == 0 {
                    format!("{notice} {n}")
                } else {
                    format!("{n} {notice}")
                };
                PageLines {
                    index,
                    width: 612.0,
                    height: 792.0,
                    images: 0,
                    fusion: None,
                    lines: vec![
                        line(
                            "Body text set at ten points carries most of the glyphs",
                            300.0,
                            index,
                        ),
                        line(
                            "on every page, so ten points is the body size.",
                            314.0,
                            index,
                        ),
                        Line {
                            size: 11.0,
                            weight: 700,
                            ..line(&foot, 760.0, index)
                        },
                    ],
                }
            })
            .collect();
        suppress_furniture(&mut pages);
        for p in &pages {
            assert!(
                !p.lines.iter().any(|l| l.text.contains("Adobe")),
                "page {} kept its footer",
                p.index
            );
            assert_eq!(p.lines.len(), 2, "page {} lost body text", p.index);
        }
    }

    #[test]
    fn a_phrase_on_two_pages_of_a_four_page_document_is_not_a_family() {
        // Body-size page-top lines on pages 1 and 3 share "of the" and
        // "the treasury": two of their three bigrams, on two pages.
        let mut pages: Vec<_> = (0..4)
            .map(|index| PageLines {
                index,
                width: 612.0,
                height: 792.0,
                images: 0,
                fusion: None,
                lines: vec![line("Body evidence", 300.0, index)],
            })
            .collect();
        pages[0]
            .lines
            .push(line("Department of the Treasury", 20.0, 0));
        pages[2].lines.push(line("Office of the Treasury", 20.0, 2));
        suppress_furniture(&mut pages);
        for (p, t) in [
            (0, "Department of the Treasury"),
            (2, "Office of the Treasury"),
        ] {
            assert!(
                pages[p].lines.iter().any(|l| l.text == t),
                "{t:?} was suppressed as furniture"
            );
        }
    }
}
