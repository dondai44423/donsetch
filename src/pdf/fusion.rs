//! Fusion : where the two modalities meet.
//!
//! Glyphs tell us WHAT the text is. Pixels tell us WHERE structure lives.
//! Both come from one content stream, so their geometry is aligned to the
//! sub-point. This module consumes both per page and produces:
//!
//! - **Trust audit** : how much of the glyph stream is garbage (private-
//!   use-area codepoints with no meaning), driving the OCR arbitration.
//! - **Visual regions** : ink-grown connected blocks in point space.
//! - **Rule lines** : pixel-verified separators for tables and structure.
//! - **Reading order 2.0** : region-based ordering that handles real
//!   magazine-style layouts where XY-cut guesses wrong.

#![allow(dead_code)]
use super::engine::PageChars;
use super::layout::{Line, PageLines};
use super::pixels::{PageBitmap, Rule};

/// A visual block in point space (y-down screen coords, matching lines).
#[derive(Clone, Debug, Default)]
pub struct RegionPt {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub area_px: u32,
    /// Vertical whitespace channels inside the region (pt x-ranges) :
    /// borderless-table column separators' ground truth.
    pub chan_v: Vec<(f32, f32)>,
    /// Horizontal whitespace channels (pt y-ranges) : row separators.
    pub chan_h: Vec<(f32, f32)>,
}

impl RegionPt {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// Everything the pixel pass learned about a page.
#[derive(Clone, Debug, Default)]
pub struct FusionData {
    /// Visual block regions (text blocks, figures-with-captions, full
    /// table grids), reading-order sorted.
    pub regions: Vec<RegionPt>,
    /// Horizontal rule lines [x0,y0,x1,y1] in pt space.
    pub rules_h: Vec<[f32; 4]>,
    /// Vertical rule lines.
    pub rules_v: Vec<[f32; 4]>,
    /// Fraction of raw chars that were PUA/garbage (0..1).
    pub garbage_ratio: f32,
    /// Fraction of page area covered by ink.
    pub ink_ratio: f32,
    /// Ink exists on this page (drawn content of ANY kind).
    pub has_visual_content: bool,
}

/// Private-use-area / garbage codepoint audit. Broken ToUnicode maps land
/// glyphs in PUA (E000-F8FF or the planes-F/10 extensions); genuinely
/// unmapped glyphs come back as NUL (already filtered by the engine, but
/// count what's in the raw feed).
pub fn glyph_trust(chars: &PageChars) -> f32 {
    if chars.chars.is_empty() {
        return 1.0;
    }
    let mut garbage = 0usize;
    let mut usable = 0usize;
    for c in &chars.chars {
        let cp = c.cp as u32;
        if cp == 0 || c.cp.is_whitespace() {
            continue;
        }
        usable += 1;
        if (0xE000..=0xF8FF).contains(&cp)
            || (0xF0000..=0xFFFFD).contains(&cp)
            || (0x100000..=0x10FFFD).contains(&cp)
            || cp == 0xFFFD
        {
            garbage += 1;
        }
    }
    if usable == 0 {
        return 1.0;
    }
    1.0 - garbage as f32 / usable as f32
}

/// Pixel only: regions + rules + ink stats for one page.
pub fn analyze_pixels(bmp: &PageBitmap, lines: &PageLines) -> FusionData {
    let mask = bmp.ink_mask();
    let total = (bmp.w * bmp.h).max(1);
    let ink = mask.total_ink();

    // Morphology radii scaled from the median text height (fallback 10pt).
    let mut heights: Vec<f32> = lines.lines.iter().map(|l| l.y1 - l.y0).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let line_pt = heights
        .get(heights.len() / 2)
        .copied()
        .unwrap_or(10.0)
        .clamp(4.0, 60.0);
    let line_px = (line_pt * mask.sy).max(2.0);
    let dx = (line_px * 0.9).round() as usize; // merge words, bridge small fonts
    let dy = (line_px * 0.35).round().max(1.0) as usize; // merge lines into blocks

    let min_area = (line_px * line_px * 0.5) as u32;
    let regions = mask
        .regions(dx, dy, min_area.max(4))
        .into_iter()
        .map(|r| {
            let (x0, y0, x1, y1) = (r.x0, r.y0, r.x1, r.y1);
            // Channels: full-span zero-ink runs inside this region. The
            // min width ≈ 2px so single-glyph serifs can't fake a gutter.
            let min_ch = 3usize;
            let chan_v = mask
                .v_channels(x0, y0, x1, y1, min_ch)
                .into_iter()
                .map(|(a, b)| (a as f32 / mask.sx, b as f32 / mask.sx))
                .collect();
            let chan_h = mask
                .h_channels(x0, y0, x1, y1, min_ch)
                .into_iter()
                .map(|(a, b)| (a as f32 / mask.sy, b as f32 / mask.sy))
                .collect();
            RegionPt {
                x0: x0 as f32 / mask.sx,
                y0: y0 as f32 / mask.sy,
                x1: x1 as f32 / mask.sx,
                y1: y1 as f32 / mask.sy,
                area_px: r.area,
                chan_v,
                chan_h,
            }
        })
        .collect();

    // Rules: opening kernel = 15% of page dimension (rules are long).
    let conv = |r: &Rule| -> [f32; 4] {
        [
            r.x0 / mask.sx,
            r.y0 / mask.sy,
            r.x1 / mask.sx,
            r.y1 / mask.sy,
        ]
    };
    let rules_h: Vec<[f32; 4]> = mask
        .h_rules(0.15)
        .iter()
        .filter(|r| r.length() / mask.sx > 20.0)
        .map(conv)
        .collect();
    let rules_v: Vec<[f32; 4]> = mask
        .v_rules(0.10)
        .iter()
        .filter(|r| r.length() / mask.sy > 20.0)
        .map(conv)
        .collect();

    FusionData {
        regions,
        rules_h,
        rules_v,
        garbage_ratio: 1.0, // filled in by the caller from glyph_trust
        ink_ratio: ink as f32 / total as f32,
        has_visual_content: ink > 0,
    }
}

/// Full per-page fusion.
pub fn analyze(chars: &PageChars, lines: &PageLines, bmp: &PageBitmap) -> FusionData {
    let mut f = analyze_pixels(bmp, lines);
    f.garbage_ratio = glyph_trust(chars);
    f
}

/// Reading order 2.0: banded region ordering with fallbacks.
///
/// - Group regions into horizontal bands (y-overlap); bands sort top-down,
///   regions inside a band sort left-right. This is the same order a
///   human follows on magazine layouts : and it degrades to single-column
///   top-down when there's only one region per band.
/// - Lines are emitted within their region, sorted (y, x).
/// - Unassigned lines (ornaments, tiny text missed by morphology growth)
///   merge back at their natural y position.
pub fn reading_order(page: &PageLines, fusion: &FusionData) -> Vec<Line> {
    if fusion.regions.is_empty() {
        return page.lines.clone();
    }

    // Assign lines to regions by center point.
    let mut per_region: Vec<Vec<usize>> = vec![Vec::new(); fusion.regions.len()];
    let mut unassigned: Vec<usize> = Vec::new();
    for (i, l) in page.lines.iter().enumerate() {
        let cx = (l.x0 + l.x1) * 0.5;
        let cy = (l.y0 + l.y1) * 0.5;
        let mut hit = None;
        for (ri, r) in fusion.regions.iter().enumerate() {
            if r.contains(cx, cy) {
                hit = Some(ri);
                break;
            }
        }
        match hit {
            Some(ri) => per_region[ri].push(i),
            None => unassigned.push(i),
        }
    }

    // Band grouping: regions whose y intervals overlap share a band.
    let mut region_ids: Vec<usize> = (0..fusion.regions.len()).collect();
    region_ids.sort_by(|&a, &b| {
        fusion.regions[a]
            .y0
            .partial_cmp(&fusion.regions[b].y0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut bands: Vec<Vec<usize>> = Vec::new();
    for rid in region_ids {
        let r = &fusion.regions[rid];
        let mut placed = false;
        for band in bands.iter_mut() {
            let overlaps = band.iter().any(|&b| {
                let br = &fusion.regions[b];
                let ov = (r.y1.min(br.y1) - r.y0.max(br.y0)).max(0.0);
                let h = (r.y1 - r.y0).min(br.y1 - br.y0);
                h > 0.0 && ov / h > 0.3
            });
            if overlaps {
                band.push(rid);
                placed = true;
                break;
            }
        }
        if !placed {
            bands.push(vec![rid]);
        }
    }
    // Order regions within a band left-right.
    for band in bands.iter_mut() {
        band.sort_by(|&a, &b| {
            fusion.regions[a]
                .x0
                .partial_cmp(&fusion.regions[b].x0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    // Order bands by min top.
    bands.sort_by(|a, b| {
        fusion.regions[a[0]]
            .y0
            .partial_cmp(&fusion.regions[b[0]].y0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Emit.
    let mut out: Vec<Line> = Vec::with_capacity(page.lines.len() + unassigned.len());
    for band in &bands {
        for &rid in band {
            let mut ids = std::mem::take(&mut per_region[rid]);
            ids.sort_by(|&a, &b| {
                let (la, lb) = (&page.lines[a], &page.lines[b]);
                la.y0
                    .partial_cmp(&lb.y0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(
                        la.x0
                            .partial_cmp(&lb.x0)
                            .unwrap_or(std::cmp::Ordering::Equal),
                    )
            });
            for i in ids {
                out.push(page.lines[i].clone());
            }
        }
    }

    // Interleave unassigned lines by y position (they had no region: tiny
    // or off-morphology text : keep them at reading position).
    if !unassigned.is_empty() {
        for &i in &unassigned {
            let l = &page.lines[i];
            // Insert immediately before the first out line placed below it.
            let pos = out
                .iter()
                .position(|o| o.y0 > l.y1 + 0.5 && (o.x0 - l.x0).abs() < 30.0)
                .unwrap_or_else(|| {
                    // Fallback: first line whose baseline region starts below.
                    out.iter().position(|o| o.y0 > l.y0).unwrap_or(out.len())
                });
            let pos = pos.min(out.len());
            out.insert(pos, l.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x0: f32, y0: f32, x1: f32, y1: f32, order: u32) -> Line {
        Line {
            text: text.to_string(),
            words: Vec::new(),
            x0,
            y0,
            x1,
            y1,
            size: 10.0,
            weight: 400,
            italic: false,
            mono: false,
            font: 0,
            glyphs: text.len(),
            order,
            page: 0,
        }
    }

    fn page(lines: Vec<Line>) -> PageLines {
        PageLines {
            index: 0,
            width: 600.0,
            height: 800.0,
            lines,
            images: 0,
            fusion: None,
        }
    }

    fn reg(x0: f32, y0: f32, x1: f32, y1: f32) -> RegionPt {
        RegionPt {
            x0,
            y0,
            x1,
            y1,
            area_px: 1,
            ..Default::default()
        }
    }

    #[test]
    fn reading_order_respects_regions() {
        // Two-column layout: left column region (x 50..250, y 100..600),
        // right column region (x 320..560, y 100..600), plus a wide heading
        // region (y 30..60).
        let lines = vec![
            line("head", 55.0, 32.0, 300.0, 48.0, 0),
            line("colL1", 55.0, 110.0, 240.0, 122.0, 1),
            line("colR1", 325.0, 110.0, 540.0, 122.0, 2),
            line("colL2", 55.0, 126.0, 240.0, 138.0, 3),
            line("colR2", 325.0, 126.0, 540.0, 138.0, 4),
        ];
        let fusion = FusionData {
            regions: vec![
                reg(50.0, 30.0, 560.0, 60.0),
                reg(50.0, 100.0, 250.0, 600.0),
                reg(320.0, 100.0, 560.0, 600.0),
            ],
            ..Default::default()
        };
        let out = reading_order(&page(lines), &fusion);
        let texts: Vec<&str> = out.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["head", "colL1", "colL2", "colR1", "colR2"]);
    }

    #[test]
    fn unassigned_lines_keep_position() {
        let lines = vec![
            line("ornament", 10.0, 5.0, 40.0, 10.0, 0), // outside all regions
            line("body", 55.0, 110.0, 240.0, 122.0, 1),
        ];
        let fusion = FusionData {
            regions: vec![reg(50.0, 100.0, 250.0, 600.0)],
            ..Default::default()
        };
        let out = reading_order(&page(lines), &fusion);
        assert_eq!(out[0].text, "ornament");
        assert_eq!(out[1].text, "body");
    }

    #[test]
    fn garbage_ratio_detects_pua() {
        let mut chars = PageChars {
            index: 0,
            width: 100.0,
            height: 100.0,
            chars: Vec::new(),
            images: 0,
        };
        for i in 0..10u32 {
            chars.chars.push(super::super::engine::PdfChar {
                cp: char::from_u32(if i < 2 { 0xE000 + i } else { 'a' as u32 + i }).unwrap(),
                x0: 0.0,
                x1: 1.0,
                y0: 0.0,
                y1: 1.0,
                size: 10.0,
                weight: 400,
                flags: 0,
                font: 0,
                angle: 0.0,
                order: i,
                dingbat: false,
                rt: false,
                ocr: false,
                spacing_box: None,
            });
        }
        let trust = glyph_trust(&chars);
        assert!((trust - 0.8).abs() < 1e-6, "trust={trust}");
    }
}
