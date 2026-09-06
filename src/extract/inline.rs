//! Inline markdown: links, emphasis, code, wiki-citation
//! dropping, URL absolutizing, tracker stripping.

use scraper::{ElementRef, Node};

const MAX_DEPTH: usize = 100;

/// Render an element's inline content as markdown.
/// Returns (markdown, link_density 0..1).
pub fn markdown(el: ElementRef<'_>, base: &str, opts: &super::ExtractOptions) -> (String, f32) {
    markdown_impl(el, base, opts, false)
}

/// `markdown` for a list item: its own content only. Nested
/// `<ul>`/`<ol>` children are the caller's (rendered as indented
/// items), not flattened into this item's line -- which had every
/// nested item appearing twice, fused into its parent and again
/// on its own line.
pub fn item_markdown(
    li: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
) -> (String, f32) {
    markdown_impl(li, base, opts, true)
}

fn markdown_impl(
    el: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
    skip_lists: bool,
) -> (String, f32) {
    let mut buf = String::new();
    let mut total = 0usize;
    let mut link = 0usize;
    render(
        el, base, opts, &mut buf, &mut total, &mut link, 0, skip_lists,
    );
    let collapsed = collapse(&buf);
    // Restore <br> sentinels as newlines after whitespace collapse.
    let collapsed = collapsed.replace('\u{0}', "\n");
    let ld = if total > 0 {
        link as f32 / total as f32
    } else {
        0.0
    };
    (collapsed, ld)
}

/// Plain visible text, whitespace-collapsed.
pub fn plain(el: ElementRef<'_>) -> String {
    let mut buf = String::new();
    for t in el.text() {
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(t.trim());
    }
    collapse(&buf)
}

#[allow(clippy::too_many_arguments)]
fn render(
    el: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
    buf: &mut String,
    total: &mut usize,
    link: &mut usize,
    depth: usize,
    skip_lists: bool,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let s = t.text.as_ref();
                if !s.trim().is_empty() {
                    buf.push_str(&escape_md_text(s, buf));
                    *total += s.trim().len();
                }
            }
            Node::Element(_) => {
                let Some(c) = ElementRef::wrap(child) else {
                    continue;
                };
                let name = c.value().name();
                if skip_lists && matches!(name, "ul" | "ol") {
                    // Block boundary: text before and after the
                    // nested list must not fuse ("BeforeAfter").
                    if !buf.is_empty() && !buf.ends_with(' ') {
                        buf.push(' ');
                    }
                    continue;
                }
                match name {
                    "a" => {
                        // Render children recursively so nested
                        // formatting (bold, emphasis) survives
                        // inside the link text: `[**B**](url)`.
                        let text = RenderInner::run(c, base, opts, depth);
                        if text.is_empty() {
                            continue;
                        }
                        *link += text.len();
                        *total += text.len();
                        if opts.include_links
                            && let Some(href) = c.value().attr("href")
                            && let Some(abs) = absolutize(base, href)
                        {
                            let clean = strip_trackers(&abs);
                            if text != clean {
                                buf.push_str(&format!("[{text}]({clean})"));
                                continue;
                            }
                        }
                        buf.push_str(&text);
                    }
                    "strong" | "b" => {
                        let t = RenderInner::run(c, base, opts, depth);
                        if !t.is_empty() {
                            let t = escape_wrap_boundary(&t, '*');
                            buf.push_str(&format!("**{t}**"));
                            *total += t.len();
                        }
                    }
                    "em" | "i" => {
                        let t = RenderInner::run(c, base, opts, depth);
                        if !t.is_empty() {
                            let t = escape_wrap_boundary(&t, '*');
                            buf.push_str(&format!("*{t}*"));
                            *total += t.len();
                        }
                    }
                    "code" => {
                        let t = plain(c);
                        if !t.is_empty() {
                            buf.push_str(&format!("`{}`", t.replace('`', "'")));
                        }
                    }
                    // Inline math: recover the LaTeX (`$...$`) :
                    // math elements must never be flattened away.
                    "math" => {
                        let l = super::math::latex(c);
                        if !l.is_empty() {
                            buf.push_str(&format!(" ${l}$ "));
                            *total += l.len();
                        }
                    }
                    // Superscript: keep the content (`^{...}`)
                    // unless it is a wiki-citation marker ([1],
                    // bare digits) : those are pure token waste.
                    "sup" => {
                        let t = plain(c);
                        if !t.is_empty() && !is_citation_marker(&t) {
                            buf.push_str(&format!("^{{{t}}}"));
                            *total += t.len();
                        }
                    }
                    // Subscript: keep the content (`_{...}`) :
                    // W<d sub>k</d>, x<sub>0</sub> carry meaning.
                    "sub" => {
                        let t = plain(c);
                        if !t.is_empty() {
                            buf.push_str(&format!("_{{{t}}}"));
                            *total += t.len();
                        }
                    }
                    "script" | "style" | "noscript" | "svg" | "img" | "button" | "input"
                    | "select" => {}
                    // <br> becomes a line break in the output markdown.
                    // Uses a NUL sentinel that survives the whitespace
                    // collapse pass, then gets replaced with \n in
                    // markdown().
                    "br" => buf.push('\u{0}'),
                    // Block boundaries inside inline rendering
                    // (multi-paragraph comments, list items): at
                    // least a space : the words must never fuse.
                    "p" | "li" | "div" | "blockquote" => {
                        if !buf.is_empty() && !buf.ends_with(' ') {
                            buf.push(' ');
                        }
                        render(c, base, opts, buf, total, link, depth + 1, false);
                    }
                    _ => {
                        if crate::extract::junk::skip(c) {
                            continue;
                        }
                        render(c, base, opts, buf, total, link, depth + 1, false);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Render an element's children into a fresh markdown string,
/// preserving nested inline formatting (links, bold, emphasis).
/// Used where a formatted element contains other formatted
/// elements inside it : e.g. `<em>A <strong><a>B</a></strong> C</em>`
/// must become `*A **[B](url)** C*`, not the flattened `A B C`.
struct RenderInner;

impl RenderInner {
    fn run(c: ElementRef<'_>, base: &str, opts: &super::ExtractOptions, depth: usize) -> String {
        let mut t = String::new();
        let mut total = 0usize;
        let mut link = 0usize;
        render(
            c,
            base,
            opts,
            &mut t,
            &mut total,
            &mut link,
            depth + 1,
            false,
        );
        t
    }
}

/// The inner render runs against an empty buffer, so its first
/// character cannot see the emphasis marker that will precede it
/// once the wrapper emits `*{t}*`. A bare leading `*`/`_` that
/// becomes right-flanking next to the marker gets a backslash.
/// (`\x` already emitted by the inner pass needs nothing: the
/// backslash keeps the literal regardless of neighbours.)
fn escape_wrap_boundary(t: &str, marker: char) -> String {
    let mut chars = t.chars();
    let Some(c0) = chars.next() else {
        return t.to_string();
    };
    if !matches!(c0, '*' | '_') {
        return t.to_string();
    }
    let Some(c1) = chars.next() else {
        return t.to_string();
    };
    let right =
        !marker.is_whitespace() && (!is_punct(marker) || c1.is_whitespace() || is_punct(c1));
    if right {
        let mut out = String::with_capacity(t.len() + 1);
        out.push('\\');
        out.push_str(t);
        out
    } else {
        t.to_string()
    }
}

/// Escape the markdown-active characters of a raw text node so a
/// literal `*` in page text can never fuse with our own emphasis
/// markers (issue #74: an em-wrapped footnote `* These figures ...`
/// emitted `**` and re-parsed as strong).
///
/// Uses CommonMark flanking rules so escaped output stays readable:
/// a `*` only gets a backslash when it can actually open or close
/// emphasis in context (left- or right-flanking), so prose like
/// `2 * 3` is untouched. `_` gets the same treatment minus the
/// intraword exception (`foo_bar` can never form emphasis), which
/// keeps snake_case identifiers free of backslash noise. Backticks
/// are rare in prose and always escaped. A `[` only gets a
/// backslash when a literal `](` follows later in the same text
/// node, so prose like `See [1]` stays bare while `[T](u)` written
/// as plain text can never render as a link. `prev` is the last
/// character already emitted to the buffer (possibly a marker we
/// generated), so adjacency across element boundaries is handled.
fn escape_md_text(s: &str, buf: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    // `close_after[i]` = a literal `](` appears at or after index
    // i+1, so a `[` here could open a link written as plain text
    // (`[T](u)` in page text must never render as a link).
    let mut close_after = vec![false; chars.len() + 1];
    let mut seen = false;
    for i in (0..chars.len()).rev() {
        close_after[i] = seen;
        if i + 1 < chars.len() && chars[i] == ']' && chars[i + 1] == '(' {
            seen = true;
        }
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev = buf.chars().next_back();
    for (i, c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        match c {
            '*' => {
                if is_flanking(*c, prev, next) {
                    out.push('\\');
                }
                out.push('*');
            }
            '_' => {
                let intraword = prev.is_some_and(|p| p.is_alphanumeric())
                    && next.is_some_and(|n| n.is_alphanumeric());
                if !intraword && is_flanking(*c, prev, next) {
                    out.push('\\');
                }
                out.push('_');
            }
            '`' => {
                out.push('\\');
                out.push('`');
            }
            '[' => {
                if close_after[i] {
                    out.push('\\');
                }
                out.push('[');
            }
            _ => out.push(*c),
        }
        prev = Some(*c);
    }
    out
}

/// Punctuation for flanking purposes: anything that is neither
/// whitespace nor alphanumeric.
fn is_punct(c: char) -> bool {
    !c.is_whitespace() && !c.is_alphanumeric()
}

/// CommonMark delimiter flanking for a single-char run.
/// `None` next/prev means the boundary of this text node; be
/// conservative there: an unknown neighbour may still allow the
/// char to act as a delimiter (our caller appends markers after).
fn is_flanking(c: char, prev: Option<char>, next: Option<char>) -> bool {
    if c != '*' && c != '_' {
        return false;
    }
    let left = match next {
        None => true,
        Some(n) => !n.is_whitespace() && (!is_punct(n) || prev.is_none_or(is_punct_or_ws)),
    };
    let right = match prev {
        None => false,
        Some(p) => {
            !p.is_whitespace()
                && (!is_punct(p) || next.is_none_or(|n| n.is_whitespace() || is_punct(n)))
        }
    };
    left || right
}

fn is_punct_or_ws(c: char) -> bool {
    c.is_whitespace() || is_punct(c)
}

/// Wiki citation markers: "[1]", "[12]", "[a]", "[1][2]".
/// These superscripts are reference noise; real superscripts
/// (exponents, ordinal suffixes like "th") survive.
fn is_citation_marker(t: &str) -> bool {
    let stripped: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.is_empty() {
        return true;
    }
    let inner = stripped.trim_matches(|c| c == '[' || c == ']');
    if inner.is_empty() {
        return true;
    }
    // Digits (up to 3) or a single footnote letter.
    (inner.len() <= 3 && inner.chars().all(|c| c.is_ascii_digit()))
        || inner.len() == 1 && inner.chars().all(|c| c.is_ascii_lowercase())
}

/// Collapse all whitespace runs to single spaces.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            ws = true;
        } else {
            if ws && !out.is_empty() {
                out.push(' ');
            }
            ws = false;
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Resolve possibly-relative URL against base.
pub fn absolutize(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    if let Ok(b) = url::Url::parse(base)
        && let Ok(u) = b.join(href)
    {
        return Some(u.to_string());
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    None
}

const TRACKER_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid", "igshid", "ref_src", "_ga", "spm",
    "scm",
];

/// Drop tracking query params (utm_*, fbclid, …). Big token
/// saver on link-heavy pages.
pub fn strip_trackers(u: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(u) else {
        return u.to_string();
    };
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !TRACKER_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.len() == parsed.query_pairs().count() {
        return u.to_string();
    }
    parsed.set_query(None);
    if !kept.is_empty() {
        let qs: Vec<String> = kept.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parsed.set_query(Some(&qs.join("&")));
    }
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Html;

    fn render_inline(html: &str) -> String {
        let document = Html::parse_fragment(html);
        let root = document.root_element();
        let opts = super::super::ExtractOptions {
            focus: None,
            selector: None,
            max_chars: None,
            offset: 0,
            include_links: false,
            include_media: false,
            toc: false,
            section: None,
            must_contain: None,
        };
        markdown(root, "https://example.com/", &opts).0
    }

    #[test]
    fn br_tag_produces_newline() {
        let result = render_inline("<div>line one<br>line two</div>");
        assert!(result.contains('\n'), "expected a newline, got: {result:?}");
        assert!(result.contains("line one"));
        assert!(result.contains("line two"));
    }

    #[test]
    fn multiple_br_tags_produce_multiple_newlines() {
        let result = render_inline("<div>a<br>b<br>c</div>");
        assert_eq!(result, "a\nb\nc");
    }

    #[test]
    fn br_inside_paragraph_kept() {
        let result = render_inline("<p>first<br>second</p>");
        assert!(result.contains('\n'), "expected newline, got: {result:?}");
        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }

    fn render_inline_with(html: &str, links: bool) -> String {
        let document = Html::parse_fragment(html);
        let root = document.root_element();
        let opts = super::super::ExtractOptions {
            focus: None,
            selector: None,
            max_chars: None,
            offset: 0,
            include_links: links,
            include_media: false,
            toc: false,
            section: None,
            must_contain: None,
        };
        markdown(root, "https://example.com/", &opts).0
    }

    // issue #74: a literal `*` at the start of italic text fused
    // with our emphasis marker into a strong delimiter and the
    // paragraph stopped round-tripping.
    #[test]
    fn literal_star_inside_em_is_escaped() {
        let html = "<p><em>* These figures refer to the latest edition
            and may be revised at any time.</em></p>";
        let result = render_inline_with(html, false);
        assert_eq!(
            result,
            "*\\* These figures refer to the latest edition and may be revised at any time.*"
        );
    }

    // ...with the nested link on, matching the issue's expected
    // output exactly.
    #[test]
    fn issue_74_repro_with_link() {
        let html = "<p><em>* These figures refer to the <strong><a
            href=\"pricing/\">latest edition</a></strong> of the service
            and may be revised at any time.</em></p>";
        let result = render_inline_with(html, true);
        assert_eq!(
            result,
            "*\\* These figures refer to the **[latest edition](https://example.com/pricing/)** of the service and may be revised at any time.*"
        );
    }

    #[test]
    fn strong_adjacent_literal_stars_do_not_fuse() {
        let result = render_inline("<strong>*x*</strong>");
        assert_eq!(result, "**\\*x\\***");
    }

    #[test]
    fn intraword_underscores_stay_bare() {
        let result = render_inline("<p>foo_bar_baz</p>");
        assert_eq!(result, "foo_bar_baz");
    }

    #[test]
    fn flanking_underscores_are_escaped() {
        let result = render_inline("<p>_word_ and x _y_</p>");
        assert_eq!(result, "\\_word\\_ and x \\_y\\_");
    }

    #[test]
    fn spaced_asterisks_in_math_prose_stay_bare() {
        let result = render_inline("<p>2 * 3 * 5 = 30</p>");
        assert_eq!(result, "2 * 3 * 5 = 30");
    }

    #[test]
    fn intraword_asterisks_are_escaped() {
        let result = render_inline("<p>a*b*c</p>");
        assert_eq!(result, "a\\*b\\*c");
    }

    #[test]
    fn literal_backticks_are_escaped() {
        let result = render_inline("<p>use `cargo test` now</p>");
        assert_eq!(result, "use \\`cargo test\\` now");
    }

    #[test]
    fn link_shaped_plain_text_stays_literal() {
        let result = render_inline("<p>[T](u) and ![V](w) and [a] alone</p>");
        assert_eq!(result, "\\[T](u) and !\\[V](w) and [a] alone");
    }
}
