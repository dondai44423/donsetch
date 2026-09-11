//! Orientation canonicalization: one pipeline for every orientation.
//!
//! Vertical CJK, rotated sidebars, sideways table headers : every extractor
//! special-cases these and loses. DonSheet takes a different position:
//! a rotated page is the same document in a different coordinate frame.
//! Glyph matrices already tell us the angle. So we ROTATE THE FRAME back
//! to canonical horizontal : glyphs AND the rasterized bitmap together :
//! and let the single unified pipeline run. There is no "vertical code
//! path"; vertical documents just work.
//!
//! Locked rule: canonicalization happens BEFORE line assembly
//! (layout::assemble) so nothing downstream ever knows.

#![allow(dead_code)]
use super::engine::PageChars;

/// Decide and apply frame canonicalization. Returns Some(rotation degrees
/// applied, clockwise) when the page was rotated, None when already
/// canonical (or too mixed to decide : honesty lane).
pub fn canonicalize(pc: &mut PageChars) -> Option<f32> {
    if pc.chars.is_empty() {
        return None;
    }

    // Mass-weighted angle histogram. STRICT bucketing: only angles within
    // 15° of a quarter count : floating noise in near-horizontal matrices
    // must never pick a quarter (round() at the 45° boundary breaks this
    // without the distance check).
    let mut mass = [0f32; 4]; // ccw quarters: 0,90,180,270
    for c in &pc.chars {
        if c.cp.is_whitespace() {
            continue;
        }
        let a = c.angle.rem_euclid(360.0);
        let q = ((a / 90.0).round() as i32).rem_euclid(4);
        let center = q as f32 * 90.0;
        let dist = (a - center).abs().min(360.0 - (a - center).abs());
        if dist > 15.0 {
            continue;
        }
        mass[q as usize] += c.size.max(1.0);
    }
    if crate::config::cfg().debug.pdf_matrix {
        for (i, c) in pc.chars.iter().take(6).enumerate() {
            eprintln!(
                "[rot_dbg] i={i} cp={:?} angle={:.2} size={:.2} rt={}",
                c.cp, c.angle, c.size, c.rt
            );
        }
    }
    let total: f32 = mass.iter().sum();
    if total <= 0.0 {
        return None;
    }
    if crate::config::cfg().debug.pdf {
        eprintln!(
            "[rotate] page {} mass: 0deg={:.0} 90deg={:.0} 180deg={:.0} 270deg={:.0} chars={} total={:.0}",
            pc.index,
            mass[0],
            mass[1],
            mass[2],
            mass[3],
            pc.chars.len(),
            total
        );
    }
    let (winner, wmass) = mass
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, m)| (i, *m))
        .unwrap();
    if winner == 0 || wmass / total < 0.6 {
        // Already canonical, or mixed (rotated sidebar in landscape doc) :
        // leave it; mixed cases stay honest via the vertical note.
        return None;
    }

    // Rotate the frame so the dominant run becomes upright.
    // Glyph quarter-turn winner is counter-clockwise; to canonicalize we
    // rotate CLOCKWISE by winner*90°.
    let ccw = winner as f32;
    rotate_frame(pc, ccw);
    Some(ccw * 90.0)
}

/// Rotate all chars counter-clockwise around the frame by `quarters`*90°.
/// Coordinates are y-down screen space. For a CCW quarter turn:
///   (x, y) -> (y, W - x)     [90°]
///   (x, y) -> (W - x, H - y) [180°]
///   (x, y) -> (H - y, x)     [270°]
/// where W,H are the ORIGINAL page width/height. The page dims themselves
/// swap for odd turns.
fn rotate_frame(pc: &mut PageChars, quarters: f32) {
    let q = quarters as i32 % 4;
    if q == 0 {
        return;
    }
    let (w, h) = (pc.width, pc.height);
    for c in pc.chars.iter_mut() {
        let (nx0, ny0, nx1, ny1) = match q {
            1 => (c.y0, w - c.x1, c.y1, w - c.x0),
            2 => (w - c.x1, h - c.y1, w - c.x0, h - c.y0),
            3 => (h - c.y1, c.x0, h - c.y0, c.x1),
            _ => (c.x0, c.y0, c.x1, c.y1),
        };
        c.x0 = nx0;
        c.x1 = nx1;
        c.y0 = ny0;
        c.y1 = ny1;
        c.angle = 0.0; // canonical now; assemble() filters angled chars
        c.rt = true;
    }
    if q == 1 || q == 3 {
        std::mem::swap(&mut pc.width, &mut pc.height);
    }
}

/// Rotate a BGRA page bitmap 90° CCW `quarters` times (paired with
/// rotate_frame so pixels and glyphs stay in the same frame).
pub fn rotate_bitmap_quarters(
    w: usize,
    h: usize,
    buf: &[u8],
    quarters: i32,
) -> (usize, usize, Vec<u8>) {
    let q = quarters.rem_euclid(4);
    match q {
        0 => (w, h, buf.to_vec()),
        2 => {
            let mut out = vec![0u8; buf.len()];
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 4;
                    let dst = ((h - 1 - y) * w + (w - 1 - x)) * 4;
                    out[dst..dst + 4].copy_from_slice(&buf[src..src + 4]);
                }
            }
            (w, h, out)
        }
        q => {
            // q=1 (CCW, matches rotate_frame q=1): (x,y) -> (y, W-1-x).
            // q=3 (CW): the inverse. New frame dims are (H x W).
            let (nw, nh) = (h, w);
            let mut out = vec![0u8; buf.len()];
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 4;
                    let (nx, ny) = if q == 1 {
                        (y, w - 1 - x)
                    } else {
                        (h - 1 - y, x)
                    };
                    let dst = (ny * nw + nx) * 4;
                    out[dst..dst + 4].copy_from_slice(&buf[src..src + 4]);
                }
            }
            (nw, nh, out)
        }
    }
}

/// True if a line's characters are predominantly RTL-script
/// (Arabic, Hebrew, Syriac, Thaana, NKo).
pub fn rtl_dominant(cps: &str) -> bool {
    let (mut rtl, mut strong) = (0usize, 0usize);
    for ch in cps.chars() {
        let cp = ch as u32;
        let is_rtl = matches!(
            cp,
            0x0590..=0x05FF   // Hebrew
            | 0x0600..=0x06FF // Arabic
            | 0x0750..=0x077F // Arabic Supplement
            | 0x08A0..=0x08FF // Arabic Extended-A
            | 0x0700..=0x074F // Syriac
            | 0x0780..=0x07BF // Thaana
            | 0x07C0..=0x07FF // NKo
            | 0xFB1D..=0xFB4F // Hebrew Presentation Forms
            | 0xFB50..=0xFDFF // Arabic Presentation Forms-A
            | 0xFE70..=0xFEFF // Arabic Presentation Forms-B
        );
        let is_ltr = ch.is_ascii_alphabetic() || ('\u{4e00}'..='\u{9fff}').contains(&ch);
        if is_rtl {
            rtl += 1;
            strong += 1;
        } else if is_ltr {
            strong += 1;
        }
    }
    strong > 0 && rtl as f32 / strong as f32 > 0.5
}

/// One strong RTL codepoint (Arabic/Hebrew families incl. presentation forms).
pub fn is_rtl_char(ch: char) -> bool {
    matches!(ch as u32,
        0x0590..=0x05FF | 0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF
        | 0x0700..=0x074F | 0x0780..=0x07BF | 0x07C0..=0x07FF
        | 0xFB1D..=0xFB4F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

/// Visual (x-sorted) line text → logical order via the Unicode BiDi
/// Algorithm. BidiInfo with para_level=None auto-detects the base
/// direction from the first strong character; mixed Latin/RTL lines keep
/// their embedded runs intact.
pub fn bidi_reorder(text: &str) -> String {
    let info = unicode_bidi::BidiInfo::new(text, None);
    info.paragraphs
        .first()
        .map(|p| info.reorder_line(p, 0..text.len()).into_owned())
        .unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::engine::PdfChar;

    fn ch(cp: char, x0: f32, y0: f32, x1: f32, y1: f32, angle: f32, size: f32) -> PdfChar {
        PdfChar {
            cp,
            x0,
            y0,
            x1,
            y1,
            size,
            weight: 400,
            flags: 0,
            font: 0,
            angle,
            order: 0,
            dingbat: false,
            rt: false,
            ocr: false,
        }
    }

    fn page(chars: Vec<PdfChar>) -> PageChars {
        PageChars {
            index: 0,
            width: 200.0,
            height: 400.0,
            chars,
            images: 0,
        }
    }

    #[test]
    fn vertical_page_canonicalizes_to_horizontal() {
        // A vertical-rl CJK page: all glyphs angled at 90°, column running
        // downward (y increases per glyph) at x~150 on a 200x400 page.
        let mut pc = page(vec![
            ch('日', 150.0, 50.0, 162.0, 62.0, 90.0, 12.0),
            ch('本', 150.0, 66.0, 162.0, 78.0, 90.0, 12.0),
            ch('語', 150.0, 82.0, 162.0, 94.0, 90.0, 12.0),
        ]);
        let rotated = canonicalize(&mut pc);
        assert_eq!(rotated, Some(90.0));
        // The column (running down) must now run left-to-right.
        assert_eq!(pc.chars[0].x0, 50.0);
        assert_eq!(pc.chars[1].x0, 66.0);
        assert_eq!(pc.chars[2].x0, 82.0);
        // Same row: y extents equal.
        assert_eq!(pc.chars[0].y0, pc.chars[1].y0);
        // Page dims must swap (200x400 -> 400x200 for a quarter turn).
        assert_eq!(pc.width, 400.0);
        assert_eq!(pc.height, 200.0);
        assert!(pc.chars.iter().all(|c| c.rt));
    }

    #[test]
    fn horizontal_page_is_untouched() {
        let mut pc = page(vec![ch('a', 10.0, 20.0, 16.0, 30.0, 0.0, 10.0)]);
        assert_eq!(canonicalize(&mut pc), None);
        assert_eq!(pc.chars[0].x0, 10.0);
        assert!(!pc.chars[0].rt);
    }

    #[test]
    fn bitmap_rotation_matches_frame() {
        // 2x1 image with distinct halves (B=255 left, R=255 right).
        let buf = vec![255u8, 0, 0, 255, 0, 0, 255, 255];
        let (nw, nh, out) = rotate_bitmap_quarters(2, 1, &buf, 2);
        assert_eq!((nw, nh), (2, 1));
        assert_eq!(&out[..4], &[0, 0, 255, 255]);
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn rtl_detection() {
        assert!(rtl_dominant("مرحبا بالعالم"));
        assert!(!rtl_dominant("hello world"));
        assert!(rtl_dominant("שלום עולם"));
        assert!(!rtl_dominant(""));
    }
}
