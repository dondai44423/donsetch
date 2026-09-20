//! Pre-parse nesting gate.
//!
//! html5ever implements the HTML tree builder as the spec writes it:
//! every start and end tag scans the stack of open elements
//! (`in_scope`, implied end tags), so a document nested n deep costs
//! O(n²) to parse. Browsers cap parser nesting at 512; html5ever has
//! no cap. Measured here: 4 000 nested `<div>` parse in 3.4 s, so a
//! 1 MiB page of them (200 000 deep) is hours, on whichever thread
//! runs the parse. No document is nested thousands deep; a page
//! that is gets refused before the parse instead of after.
//!
//! The estimate is a linear scan that counts only what the tree
//! builder's stack actually keeps: void elements, self-closing tags,
//! raw-text bodies (`<script>`, `<style>`, …), comments and the
//! elements the parser closes implicitly at a sibling (`<p>`, `<li>`,
//! `<td>`, …) are left out, so unclosed tags of those kinds on real
//! pages do not count against it.

/// Deeper than this and the body is not a document.
pub const MAX_NESTING: usize = 4096;

/// The deepest nesting a linear scan can attribute to `text`.
pub fn max_nesting(text: &str) -> usize {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut max = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        // Comment: skip to `-->`.
        if b[i..].starts_with(b"<!--") {
            i = find(b, i + 4, b"-->").map_or(b.len(), |p| p + 3);
            continue;
        }
        let (closing, name_start) = if b.get(i + 1) == Some(&b'/') {
            (true, i + 2)
        } else {
            (false, i + 1)
        };
        let mut j = name_start;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
            j += 1;
        }
        if j == name_start {
            i += 1;
            continue;
        }
        let name = b[name_start..j].to_ascii_lowercase();
        // End of the tag, noting `/>`.
        let mut k = j;
        let mut self_closing = false;
        let mut quote: Option<u8> = None;
        while k < b.len() {
            let c = b[k];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => {
                    if c == b'"' || c == b'\'' {
                        quote = Some(c);
                    } else if c == b'>' {
                        self_closing = k > j && b[k - 1] == b'/';
                        break;
                    }
                }
            }
            k += 1;
        }
        i = k.saturating_add(1).min(b.len());
        if closing {
            if counts(&name) {
                depth = depth.saturating_sub(1);
            }
            continue;
        }
        if self_closing || !counts(&name) {
            if is_raw_text(&name) {
                // Skip to the matching end tag: markup inside is text.
                let mut close = b"</".to_vec();
                close.extend_from_slice(&name);
                i = find_ci(b, i, &close).unwrap_or(b.len());
            }
            continue;
        }
        depth += 1;
        max = max.max(depth);
        if max > MAX_NESTING {
            return max;
        }
    }
    max
}

/// Elements the tree builder keeps on its stack until an explicit
/// end tag (or the end of the document).
fn counts(name: &[u8]) -> bool {
    !(is_void(name) || is_raw_text(name) || sibling_closed(name))
}

fn is_void(name: &[u8]) -> bool {
    matches!(
        name,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
            | b"keygen"
            | b"frame"
            | b"basefont"
            | b"bgsound"
            | b"command"
            | b"image"
            | b"isindex"
            | b"nextid"
    )
}

fn is_raw_text(name: &[u8]) -> bool {
    matches!(
        name,
        b"script"
            | b"style"
            | b"textarea"
            | b"title"
            | b"xmp"
            | b"iframe"
            | b"noembed"
            | b"noframes"
            | b"plaintext"
            | b"noscript"
    )
}

/// Closed by the parser at the next sibling of the same family, so
/// an unclosed run of them stays one deep.
fn sibling_closed(name: &[u8]) -> bool {
    matches!(
        name,
        b"p" | b"li"
            | b"dt"
            | b"dd"
            | b"tr"
            | b"td"
            | b"th"
            | b"thead"
            | b"tbody"
            | b"tfoot"
            | b"colgroup"
            | b"option"
            | b"optgroup"
            | b"rt"
            | b"rp"
            | b"rb"
            | b"rtc"
            | b"caption"
    )
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= b.len() || needle.is_empty() || b.len() - from < needle.len() {
        return None;
    }
    (from..=b.len() - needle.len()).find(|&p| &b[p..p + needle.len()] == needle)
}

fn find_ci(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= b.len() || needle.is_empty() || b.len() - from < needle.len() {
        return None;
    }
    (from..=b.len() - needle.len()).find(|&p| b[p..p + needle.len()].eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_real_nesting_and_ignores_what_the_parser_closes_itself() {
        assert_eq!(
            max_nesting("<html><body><div><span>x</span></div></body></html>"),
            4
        );
        // Unclosed void / sibling-closed / self-closing / raw text do
        // not accumulate.
        let page = format!(
            "<div>{}{}{}<script>{}</script><p>a<p>b<p>c</div>",
            "<br>".repeat(500),
            "<li>x".repeat(500),
            "<img src=x/>".repeat(500),
            "<div>".repeat(500)
        );
        assert_eq!(max_nesting(&page), 1);
        assert_eq!(max_nesting("<!-- <div><div><div> --><b>x</b>"), 1);
        assert_eq!(max_nesting("<a href='>'><b>x</b></a>"), 2);
        assert_eq!(max_nesting("plain text < not a tag"), 0);
    }

    #[test]
    fn a_deep_nest_is_reported_and_the_scan_stays_linear() {
        let n = 200_000;
        let html = format!("{}x{}", "<div>".repeat(n), "</div>".repeat(n));
        let started = std::time::Instant::now();
        assert!(max_nesting(&html) > MAX_NESTING);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let html = format!("<ul>{}x", "<ul><li>".repeat(n));
        assert!(max_nesting(&html) > MAX_NESTING);
        let html = format!("{}x", "<b>".repeat(n));
        assert!(max_nesting(&html) > MAX_NESTING);
        // A wide page is not a deep one (<p> is sibling-closed, so
        // div + b).
        let html = format!("<div>{}</div>", "<p><b>x</b></p>".repeat(n));
        assert_eq!(max_nesting(&html), 2);
    }
}
