//! Pixel engine : rendered-page ground truth.
//!
//! DonSheet's core thesis: PDFium renders both modalities of a document :
//! the glyph stream and the exact pixels a human sees. Heuristic extractors
//! guess structure from glyphs; ML extractors guess structure (and text!)
//! from pixels. We FUSE both, deterministically.
//!
//! This module owns everything that comes from rasterization: ink masks,
//! separable morphology, rule-line detection, whitespace channels, and
//! visual region segmentation. All coordinates are PIXELS; conversion to
//! PDF points happens in `fusion.rs` (`InkMask::sx`/`sy` carry the scale).

#![allow(dead_code)]
/// Raw rasterized page (BGRA bytes, white background, annotations drawn).
pub struct PageBitmap {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<u8>,
    pub page_w_pt: f32,
    pub page_h_pt: f32,
}

impl PageBitmap {
    /// Convert to a binary ink mask. Ink = rendered darkness above a
    /// threshold : catches hairline anti-aliased strokes (a 0.5pt rule at
    /// 96dpi covers ~0.66px and lands as mid-gray).
    pub fn ink_mask(&self) -> InkMask {
        let mut buf = vec![0u8; self.w * self.h];
        debug_assert_eq!(self.buf.len(), self.w * self.h * 4);
        for i in 0..self.buf.len() / 4 {
            let px = &self.buf[i * 4..i * 4 + 4];
            // BGRA order from PDFium.
            let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
            let lum = (114 * b + 587 * g + 299 * r) / 1000; // 0..=255
            if lum < 222 {
                buf[i] = 1;
            }
        }
        InkMask {
            w: self.w,
            h: self.h,
            sx: self.w as f32 / self.page_w_pt.max(1.0),
            sy: self.h as f32 / self.page_h_pt.max(1.0),
            buf,
        }
    }
}

/// Test probe over the bitmaps `engine::rasterize_page_at` returns (the
/// OCR pass's renders) on the current thread: how many were made, and the
/// most alive at once. A render is alive until its bitmap is dropped.
#[cfg(test)]
pub(crate) mod ocr_probe {
    use std::cell::RefCell;

    #[derive(Default)]
    struct Counts {
        made: usize,
        live: Vec<usize>,
        peak: usize,
    }

    thread_local! {
        static COUNTS: RefCell<Counts> = RefCell::default();
    }

    pub(crate) fn rendered(bitmap: &super::PageBitmap) {
        COUNTS.with_borrow_mut(|c| {
            c.made += 1;
            c.live.push(bitmap.buf.as_ptr() as usize);
            c.peak = c.peak.max(c.live.len());
        });
    }

    pub(super) fn dropped(bitmap: &super::PageBitmap) {
        let at = bitmap.buf.as_ptr() as usize;
        let _ = COUNTS.try_with(|c| c.borrow_mut().live.retain(|&p| p != at));
    }

    /// (renders made, most alive at once) since the last call; resets both.
    pub(crate) fn take() -> (usize, usize) {
        COUNTS.with_borrow_mut(|c| {
            let out = (c.made, c.peak);
            *c = Counts::default();
            out
        })
    }
}

#[cfg(test)]
impl Drop for PageBitmap {
    fn drop(&mut self) {
        ocr_probe::dropped(self);
    }
}

/// A detected rule line (axis-aligned ink ridge), pixel coords (y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rule {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rule {
    pub fn horizontal(&self) -> bool {
        (self.x1 - self.x0) > (self.y1 - self.y0) * 2.0
    }
    pub fn length(&self) -> f32 {
        (self.x1 - self.x0).max(self.y1 - self.y0)
    }
}

/// A visual block region, pixel coords (y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub area: u32,
}

impl Region {
    pub fn width(&self) -> i32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> i32 {
        self.y1 - self.y0
    }
    pub fn overlaps_x(&self, o: &Region) -> bool {
        self.x0 < o.x1 && o.x0 < self.x1
    }
    pub fn overlaps_y(&self, o: &Region) -> bool {
        self.y0 < o.y1 && o.y0 < self.y1
    }
}

/// Binary ink mask (1/0 per pixel) + cheap morphological reasoning.
/// All ops are O(pixels) separable passes with monotonic-deque extrema.
pub struct InkMask {
    pub w: usize,
    pub h: usize,
    /// pixels per PDF point (x and y; identical when dpi isn't stretched)
    pub sx: f32,
    pub sy: f32,
    pub buf: Vec<u8>,
}

impl InkMask {
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.buf[y * self.w + x]
    }

    pub fn total_ink(&self) -> usize {
        self.buf.iter().map(|&b| b as usize).sum()
    }

    /// Fraction of ink within a pixel-space rect (bounds-clamped).
    pub fn ink_ratio(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> f32 {
        let (x0, y0, x1, y1) = (
            x0.clamp(0, self.w as i32) as usize,
            y0.clamp(0, self.h as i32) as usize,
            x1.clamp(0, self.w as i32) as usize,
            y1.clamp(0, self.h as i32) as usize,
        );
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        let mut ink = 0usize;
        for y in y0..y1 {
            let row = &self.buf[y * self.w + x0..y * self.w + x1];
            ink += row.iter().map(|&b| b as usize).sum::<usize>();
        }
        ink as f32 / ((x1 - x0) * (y1 - y0)) as f32
    }

    /// True when ANY ink exists in the rect (early exit).
    pub fn ink_in(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
        let (x0, y0, x1, y1) = (
            x0.clamp(0, self.w as i32) as usize,
            y0.clamp(0, self.h as i32) as usize,
            x1.clamp(0, self.w as i32) as usize,
            y1.clamp(0, self.h as i32) as usize,
        );
        for y in y0..y1 {
            if self.buf[y * self.w + x0..y * self.w + x1].contains(&1) {
                return true;
            }
        }
        false
    }

    // ---- separable morphology -------------------------------------------

    /// Sliding-window maximum along one row into `out`.
    #[allow(clippy::needless_range_loop)]
    fn dilate_row_max(&self, y: usize, half: usize, out: &mut [u8]) {
        let row = &self.buf[y * self.w..(y + 1) * self.w];
        let w = self.w;
        let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let k = 2 * half + 1;
        for x in 0..w + half {
            if x < w && row[x] == 1 {
                dq.push_back(x);
            }
            let lo = x as i64 - k as i64;
            while let Some(&f) = dq.front() {
                if (f as i64) <= lo {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            if x >= half {
                let ox = x - half;
                out[y * w + ox] = if dq.is_empty() { 0 } else { 1 };
            }
        }
    }

    /// Horizontal dilation by radius `half` pixels.
    pub fn dilated_h(&self, half: usize) -> InkMask {
        if half == 0 {
            return self.clone_mask();
        }
        let mut out = vec![0u8; self.buf.len()];
        for y in 0..self.h {
            self.dilate_row_max(y, half, &mut out);
        }
        InkMask {
            w: self.w,
            h: self.h,
            sx: self.sx,
            sy: self.sy,
            buf: out,
        }
    }

    /// Vertical dilation by radius `half` pixels.
    pub fn dilated_v(&self, half: usize) -> InkMask {
        if half == 0 {
            return self.clone_mask();
        }
        let mut out = vec![0u8; self.buf.len()];
        let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let k = 2 * half + 1;
        for x in 0..self.w {
            dq.clear();
            for y in 0..self.h + half {
                if y < self.h && self.at(x, y) == 1 {
                    dq.push_back(y);
                }
                let lo = y as i64 - k as i64;
                while let Some(&f) = dq.front() {
                    if (f as i64) <= lo {
                        dq.pop_front();
                    } else {
                        break;
                    }
                }
                if y >= half {
                    let oy = y - half;
                    out[oy * self.w + x] = if dq.is_empty() { 0 } else { 1 };
                }
            }
        }
        InkMask {
            w: self.w,
            h: self.h,
            sx: self.sx,
            sy: self.sy,
            buf: out,
        }
    }

    /// Rect-kernel dilation (words → lines → blocks), separable.
    pub fn dilated(&self, dx: usize, dy: usize) -> InkMask {
        self.dilated_h(dx).dilated_v(dy)
    }

    fn clone_mask(&self) -> InkMask {
        InkMask {
            w: self.w,
            h: self.h,
            sx: self.sx,
            sy: self.sy,
            buf: self.buf.clone(),
        }
    }

    /// Horizontal erosion by radius `half` (min filter along rows).
    pub fn eroded_h(&self, half: usize) -> InkMask {
        // Erode A == invert, dilate, invert.
        let inv = self.inverted();
        inv.dilated_h(half).inverted()
    }

    pub fn eroded_v(&self, half: usize) -> InkMask {
        let inv = self.inverted();
        inv.dilated_v(half).inverted()
    }

    fn inverted(&self) -> InkMask {
        InkMask {
            w: self.w,
            h: self.h,
            sx: self.sx,
            sy: self.sy,
            buf: self.buf.iter().map(|&b| 1 - b).collect(),
        }
    }

    /// Opening with a horizontal line kernel of full length `k`:
    /// keeps ONLY ink runs at least `k` px long. This is how hairline
    /// rules fall out of text noise.
    pub fn opened_h(&self, k: usize) -> InkMask {
        if k == 0 {
            return self.clone_mask();
        }
        self.eroded_h(k / 2).dilated_h(k / 2)
    }

    pub fn opened_v(&self, k: usize) -> InkMask {
        if k == 0 {
            return self.clone_mask();
        }
        self.eroded_v(k / 2).dilated_v(k / 2)
    }

    // ---- structure primitives -------------------------------------------

    /// Horizontal rule lines: rows surviving a line-kernel opening, merged
    /// into ridges. `k` = minimum rule length in px (default ~0.15*width).
    pub fn h_rules(&self, k_frac: f32) -> Vec<Rule> {
        let k = ((self.w as f32 * k_frac) as usize).max(4);
        let m = self.opened_h(k);
        let mut rules = Vec::new();
        let mut y = 0usize;
        while y < self.h {
            // Any ink on this row?
            let row = &m.buf[y * m.w..(y + 1) * m.w];
            if !row.contains(&1) {
                y += 1;
                continue;
            }
            // Find the ridge band (contiguous rows with ink).
            let y_start = y;
            let (mut rx0, mut rx1) = (usize::MAX, 0usize);
            while y < self.h {
                let row = &m.buf[y * m.w..(y + 1) * m.w];
                if !row.contains(&1) {
                    break;
                }
                let first = row.iter().position(|&b| b == 1).unwrap();
                let last = row.iter().rposition(|&b| b == 1).unwrap();
                rx0 = rx0.min(first);
                rx1 = rx1.max(last + 1);
                y += 1;
            }
            // v-opening guarantees rows came from a long horizontal run,
            // but merge-splits can leave crumbs; verify real length.
            rules.push(Rule {
                x0: rx0 as f32,
                y0: y_start as f32,
                x1: rx1 as f32,
                y1: y as f32,
            });
        }
        rules
    }

    /// Vertical rule lines (columns surviving vertical line-kernel opening).
    pub fn v_rules(&self, k_frac: f32) -> Vec<Rule> {
        let k = ((self.h as f32 * k_frac) as usize).max(4);
        let m = self.opened_v(k);
        let mut rules = Vec::new();
        let mut x = 0usize;
        while x < self.w {
            if !self.col_any(&m, x) {
                x += 1;
                continue;
            }
            let x_start = x;
            let (mut ry0, mut ry1) = (usize::MAX, 0usize);
            while x < self.w && self.col_any(&m, x) {
                let mut first = usize::MAX;
                let mut last = 0usize;
                for y in 0..self.h {
                    if m.at(x, y) == 1 {
                        first = first.min(y);
                        last = y;
                    }
                }
                ry0 = ry0.min(first);
                ry1 = ry1.max(last + 1);
                x += 1;
            }
            rules.push(Rule {
                x0: x_start as f32,
                y0: ry0 as f32,
                x1: x as f32,
                y1: ry1 as f32,
            });
        }
        rules
    }

    fn col_any(&self, m: &InkMask, x: usize) -> bool {
        (0..self.h).any(|y| m.at(x, y) == 1)
    }

    /// Vertical whitespace channels inside a rect: contiguous zero-ink
    /// column runs at least `min_width_px` wide. The borderless-table
    /// columnizer's ground truth.
    pub fn v_channels(
        &self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        min_width: usize,
    ) -> Vec<(i32, i32)> {
        self.channels_generic(x0, y0, x1, y1, min_width, true)
    }

    /// Horizontal whitespace channels inside a rect (row splitting).
    pub fn h_channels(
        &self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        min_width: usize,
    ) -> Vec<(i32, i32)> {
        self.channels_generic(x0, y0, x1, y1, min_width, false)
    }

    fn channels_generic(
        &self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        min_width: usize,
        vertical: bool,
    ) -> Vec<(i32, i32)> {
        let (x0, y0, x1, y1) = (
            x0.clamp(0, self.w as i32) as usize,
            y0.clamp(0, self.h as i32) as usize,
            x1.clamp(0, self.w as i32) as usize,
            y1.clamp(0, self.h as i32) as usize,
        );
        let mut out = Vec::new();
        let outer = if vertical { (x0, x1) } else { (y0, y1) };
        let mut run_start: Option<usize> = None;
        let mut idx = outer.0;
        let end = outer.1;
        while idx <= end {
            let blank = if idx < end {
                let any_ink = if vertical {
                    (y0..y1).any(|y| self.at(idx, y) == 1)
                } else {
                    (x0..x1).any(|x| self.at(x, idx) == 1)
                };
                !any_ink
            } else {
                false
            };
            match (blank, run_start) {
                (true, None) => run_start = Some(idx),
                (true, Some(_)) => {}
                (false, Some(s)) => {
                    if idx - s >= min_width {
                        out.push((s as i32, idx as i32));
                    }
                    run_start = None;
                }
                (false, None) => {}
            }
            idx += 1;
        }
        out
    }

    // ---- region segmentation ---------------------------------------------

    /// Visual block regions: morphologically grown ink clumps, returned as
    /// connected components of the dilated mask. `dx`/`dy` are the growth
    /// radii in px (word-gap merge ≈ 4-6px, line-gap merge ≈ 2-4px at 96dpi).
    pub fn regions(&self, dx: usize, dy: usize, min_area: u32) -> Vec<Region> {
        let grown = self.dilated(dx, dy);
        grown.components(min_area)
    }

    /// Connected components of THIS mask (no extra dilation) via run-based
    /// union-find (scanline). Returns pixel rects.
    pub fn components(&self, min_area: u32) -> Vec<Region> {
        // Row runs per scanline, union overlapping runs.
        struct Run {
            x0: i32,
            x1: i32,
            parent: u32,
            y0: i32,
            y1: i32,
            area: u32,
        }
        let mut runs: Vec<Run> = Vec::new();
        let mut prev_run_ids: Vec<(i32, i32, u32)> = Vec::new();

        #[allow(clippy::ptr_arg)]
        fn find(runs: &mut Vec<Run>, mut i: u32) -> u32 {
            while runs[i as usize].parent != i {
                let p = runs[i as usize].parent;
                runs[i as usize].parent = runs[p as usize].parent; // path halving
                i = runs[i as usize].parent;
            }
            i
        }
        fn union(runs: &mut Vec<Run>, a: u32, b: u32) {
            let (ra, rb) = (find(runs, a), find(runs, b));
            if ra != rb {
                runs[rb as usize].parent = ra;
                let (xa, xb) = (runs[ra as usize].x0, runs[rb as usize].x0);
                let (ya, yb) = (runs[ra as usize].y0, runs[rb as usize].y0);
                let (x2a, x2b) = (runs[ra as usize].x1, runs[rb as usize].x1);
                let (y2a, y2b) = (runs[ra as usize].y1, runs[rb as usize].y1);
                let aa = runs[ra as usize].area + runs[rb as usize].area;
                runs[ra as usize].x0 = xa.min(xb);
                runs[ra as usize].y0 = ya.min(yb);
                runs[ra as usize].x1 = x2a.max(x2b);
                runs[ra as usize].y1 = y2a.max(y2b);
                runs[ra as usize].area = aa;
            }
        }

        for y in 0..self.h {
            let mut cur: Vec<(i32, i32, u32)> = Vec::new();
            let row = &self.buf[y * self.w..(y + 1) * self.w];
            let mut x = 0usize;
            while x < self.w {
                if row[x] == 0 {
                    x += 1;
                    continue;
                }
                let s = x;
                while x < self.w && row[x] == 1 {
                    x += 1;
                }
                let id = runs.len() as u32;
                runs.push(Run {
                    x0: s as i32,
                    x1: x as i32,
                    parent: id,
                    y0: y as i32,
                    y1: y as i32 + 1,
                    area: (x - s) as u32,
                });
                cur.push((s as i32, x as i32, id));
            }
            for &(cx0, cx1, cid) in &cur {
                for &(px0, px1, pid) in &prev_run_ids {
                    // 8-ish connectivity: allow 1px diagonal gap after growth.
                    if cx0 <= px1 + 1 && px0 <= cx1 + 1 {
                        union(&mut runs, cid, pid);
                    }
                }
            }
            prev_run_ids = cur;
        }

        let mut roots: std::collections::HashMap<u32, Region> = std::collections::HashMap::new();
        for i in 0..runs.len() {
            let r = find(&mut runs, i as u32);
            roots
                .entry(r)
                .or_insert(Region {
                    x0: runs[i].x0,
                    y0: runs[i].y0,
                    x1: runs[i].x1,
                    y1: runs[i].y1,
                    area: 0,
                })
                .area += runs[i].area;
        }
        roots.into_values().filter(|r| r.area >= min_area).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(w: usize, h: usize) -> InkMask {
        InkMask {
            w,
            h,
            sx: 1.0,
            sy: 1.0,
            buf: vec![0u8; w * h],
        }
    }

    #[test]
    fn opening_keeps_only_long_runs() {
        // Text cluster + one long rule; rule survives h-opening, text dies.
        let mut m = mask(100, 40);
        for x in 5..60 {
            m.buf[10 * 100 + x] = 1; // rule
            m.buf[11 * 100 + x] = 1;
        }
        // text blobs
        m.buf[20 * 100 + 10] = 1;
        m.buf[20 * 100 + 13] = 1;
        m.buf[20 * 100 + 17] = 1;
        let opened = m.opened_h(30);
        assert!(opened.total_ink() > 0);
        assert_eq!(opened.at(10, 20), 0); // text row gone
        assert_eq!(opened.at(7, 10), 1); // rule row kept (rule spans 5..60)
        let rules = opened.h_rules(0.3);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].length() >= 50.0);
    }

    #[test]
    fn dilate_merges_words_into_blocks() {
        let mut m = mask(100, 30);
        // two words on same line, gap 3px; second line below with bigger gap
        for x in 5..12 {
            m.buf[5 * 100 + x] = 1;
        }
        for x in 15..22 {
            m.buf[5 * 100 + x] = 1;
        }
        for x in 50..60 {
            m.buf[20 * 100 + x] = 1;
        }
        let regions = m.regions(3, 2, 4);
        assert_eq!(regions.len(), 2, "expected two visual blocks: {regions:?}");
        let top = regions.iter().find(|r| r.y0 < 10).unwrap();
        assert!(top.x0 <= 5 && top.x1 >= 22);
    }

    #[test]
    fn v_channels_find_gutters() {
        let mut m = mask(100, 20);
        // two text columns: x 5..20 and x 40..60, channel 20..40
        for y in 2..15 {
            for x in 5..20 {
                m.buf[y * 100 + x] = 1;
            }
            for x in 40..60 {
                m.buf[y * 100 + x] = 1;
            }
        }
        let chans = m.v_channels(0, 0, 100, 20, 8);
        assert!(chans.iter().any(|(s, e)| *s <= 22 && *e >= 38), "{chans:?}");
    }

    #[test]
    fn ink_ratio_respects_bounds() {
        let mut m = mask(10, 10);
        m.buf[0] = 1;
        assert_eq!(m.ink_ratio(-5, -5, 5, 5), 1.0 / 25.0);
        assert!(m.ink_in(0, 0, 1, 1));
        assert!(!m.ink_in(5, 5, 9, 9));
    }
}
