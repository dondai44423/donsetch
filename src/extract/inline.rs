//! Inline markdown: links, emphasis, code, wiki-citation
//! dropping, URL absolutizing, tracker stripping.

use scraper::{ElementRef, Node};

const MAX_DEPTH: usize = 100;

/// Render an element's inline content as markdown.
/// Returns (markdown, link_density 0..1).
pub fn markdown(el: ElementRef<'_>, base: &str, opts: &super::ExtractOptions) -> (String, f32) {
    markdown_impl(el, base, opts, false, Emphasis::default())
}

/// Render an inline element including its own link/emphasis wrapper.
/// Whitespace at the element's edges comes out as one space outside
/// the wrapper.
pub fn element_markdown(
    el: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
) -> (String, f32) {
    let emph = Emphasis::of(el);
    let (text, density) = markdown_impl(el, base, opts, false, emph);
    let md = match el.value().name() {
        "a" if opts.include_links => el
            .value()
            .attr("href")
            .and_then(|href| absolutize(base, href))
            .map(|url| format!("[{text}]({})", strip_trackers(&url)))
            .unwrap_or_else(|| text.clone()),
        "code" => format!("`{}`", plain(el).replace('`', "'")),
        _ => text,
    };
    // `markdown_impl` trims, but the faithful run in blocks.rs joins
    // this to raw text, where that edge space is all that keeps the
    // element's words apart from its neighbors'.
    let raw = visible_text_raw(el);
    let mut wrapped = String::with_capacity(md.len() + 8);
    if raw.starts_with(char::is_whitespace) {
        wrapped.push(' ');
    }
    push_emphasized(&mut wrapped, &md, emph, None);
    if raw.ends_with(char::is_whitespace) {
        wrapped.push(' ');
    }
    (
        wrapped,
        if el.value().name() == "a" {
            1.0
        } else {
            density
        },
    )
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
    markdown_impl(li, base, opts, true, Emphasis::default())
}

fn markdown_impl(
    el: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
    skip_lists: bool,
    emph: Emphasis,
) -> (String, f32) {
    let mut buf = String::new();
    let mut total = 0usize;
    let mut link = 0usize;
    render(
        el, base, opts, &mut buf, &mut total, &mut link, 0, skip_lists, emph,
    );
    // Restore <br> sentinels as newlines after whitespace collapse.
    let collapsed = restore_breaks(&collapse(&buf));
    let ld = if total > 0 {
        link as f32 / total as f32
    } else {
        0.0
    };
    (collapsed, ld)
}

/// Tags whose text content is source code or otherwise never
/// rendered. `ElementRef::text()` walks THROUGH them, which is
/// exactly where the #288 leaks came from: an inline `<style>`
/// landed inside a price heading, a `<script>` inside a spec-table
/// cell, and a reddit comment header carried its own
/// `SML.load(...)` call into the output.
const INVISIBLE_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
];

/// Walk an element's text in document order, skipping the subtrees
/// of invisible tags. `preserve_ws` keeps raw whitespace (`<pre>`);
/// otherwise every text node is trimmed and space-joined, exactly
/// the shape `ElementRef::text()` had.
fn walk_visible(el: ElementRef<'_>, buf: &mut String, preserve_ws: bool) {
    let mut stack: Vec<_> = el.children().collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        match n.value() {
            Node::Text(t) => {
                if preserve_ws {
                    buf.push_str(t.text.as_ref());
                } else {
                    if !buf.is_empty() {
                        buf.push(' ');
                    }
                    buf.push_str(t.text.trim());
                }
            }
            Node::Element(e) => {
                if INVISIBLE_TAGS.contains(&e.name()) {
                    continue;
                }
                let at = stack.len();
                stack.extend(n.children());
                stack[at..].reverse();
            }
            _ => {}
        }
    }
}

/// The element's visible text (script/style content excluded),
/// whitespace-collapsed.
pub fn visible_text(el: ElementRef<'_>) -> String {
    let mut buf = String::new();
    walk_visible(el, &mut buf, false);
    collapse(&buf)
}

/// Visible text with whitespace preserved (`<pre>` content).
pub fn visible_text_raw(el: ElementRef<'_>) -> String {
    let mut buf = String::new();
    walk_visible(el, &mut buf, true);
    buf
}

/// Plain visible text, whitespace-collapsed.
pub fn plain(el: ElementRef<'_>) -> String {
    visible_text(el)
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
    emph: Emphasis,
) {
    if depth > MAX_DEPTH {
        return;
    }
    // The last sibling run's closing marker, so the next run of the
    // same kind can continue it instead of opening right after it.
    let mut closer = None;
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let s = t.text.as_ref();
                if !s.trim().is_empty() {
                    buf.push_str(&escape_md_text(s, buf, emph.any()));
                    *total += s.trim().len();
                } else if !s.is_empty() && !buf.is_empty() && !buf.ends_with(is_edge) {
                    // A space-only node between two inline elements is
                    // all that keeps their words apart; after a <br>
                    // the line break already does.
                    buf.push(' ');
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
                // Markers this element adds over what is already in
                // force. The element renders into `buf` like any
                // other; the markers go around what it appended.
                let add = Emphasis::of(c).beyond(emph);
                let inner = emph.with(add);
                let start = buf.len();
                match name {
                    "a" => {
                        // Render children recursively so nested
                        // formatting (bold, emphasis) survives
                        // inside the link text: `[**B**](url)`.
                        let text = RenderInner::run(c, base, opts, depth, inner);
                        if text.is_empty() {
                            continue;
                        }
                        *link += text.len();
                        *total += text.len();
                        // Edge whitespace goes outside the brackets, as
                        // it does outside emphasis markers: `[Title ](u)`
                        // would also defeat the bare-URL check below.
                        let label = text.trim_matches(is_edge);
                        let lead = text.len() - text.trim_start_matches(is_edge).len();
                        buf.push_str(&text[..lead]);
                        let mut md = None;
                        if !label.is_empty()
                            && opts.include_links
                            && let Some(href) = c.value().attr("href")
                            && let Some(abs) = absolutize(base, href)
                        {
                            let clean = strip_trackers(&abs);
                            if label != clean {
                                md = Some(format!("[{label}]({clean})"));
                            }
                        }
                        buf.push_str(md.as_deref().unwrap_or(label));
                        buf.push_str(&text[lead + label.len()..]);
                    }
                    // Scored as plain text, links inside included: link
                    // density feeds the boilerplate scoring, and counting
                    // bold links as links would shift where it prunes.
                    "strong" | "b" | "em" | "i" => {
                        let (mut t_total, mut t_link) = (0, 0);
                        render(
                            c,
                            base,
                            opts,
                            buf,
                            &mut t_total,
                            &mut t_link,
                            depth + 1,
                            false,
                            inner,
                        );
                        *total += buf.len() - start;
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
                        if !t.is_empty() && (opts.faithful || !is_citation_marker(&t)) {
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
                        if opts.faithful {
                            buf.push_str("\0\0");
                        } else if !buf.is_empty() && !buf.ends_with(' ') {
                            buf.push(' ');
                        }
                        render(c, base, opts, buf, total, link, depth + 1, false, inner);
                        if opts.faithful {
                            buf.push_str("\0\0");
                        }
                    }
                    _ => {
                        if crate::extract::junk::skip(c) {
                            continue;
                        }
                        render(c, base, opts, buf, total, link, depth + 1, false, inner);
                    }
                }
                if add.any() {
                    let md = buf.split_off(start);
                    closer = push_emphasized(buf, &md, add, closer.take());
                }
            }
            _ => {}
        }
    }
}

/// Render an element's children into a fresh markdown string,
/// preserving nested inline formatting (links, bold, emphasis):
/// link text renders this way, so `<a><strong>B</strong></a>`
/// becomes `[**B**](url)`, not the flattened `[B](url)`. `emph` is
/// the emphasis already in force around the element.
struct RenderInner;

impl RenderInner {
    fn run(
        c: ElementRef<'_>,
        base: &str,
        opts: &super::ExtractOptions,
        depth: usize,
        emph: Emphasis,
    ) -> String {
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
            emph,
        );
        t
    }
}

/// Bold and italic, as requested by an element or in force at a
/// point of the walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Emphasis {
    bold: bool,
    italic: bool,
}

impl Emphasis {
    /// The emphasis `el` carries by itself: bold for `strong`/`b`, italic for
    /// `em`/`i`, then, on inline (phrasing) elements only, its
    /// `style` attribute (`font-weight`, `font-style`), which
    /// overrides the tag. Stylesheets and classes are not consulted.
    fn of(el: ElementRef<'_>) -> Self {
        let name = el.value().name();
        let mut e = Emphasis {
            bold: matches!(name, "strong" | "b"),
            italic: matches!(name, "em" | "i"),
        };
        // Style on a block element (`<div style="font-weight:bold">`
        // inside a list item) is not read: its markers would have to
        // go around each paragraph.
        if !super::blocks::INLINE_TAGS.contains(&name) {
            return e;
        }
        let Some(style) = el.value().attr("style") else {
            return e;
        };
        // Later declarations win unless an earlier one is
        // `!important` and they are not, as in CSS. A value not parsed
        // here (`var(--w)`, `revert`) keeps the tag's default.
        let (mut bold_important, mut italic_important) = (false, false);
        for decl in style.split(';') {
            let Some((prop, value)) = decl.split_once(':') else {
                continue;
            };
            let (value, important) = match value.split_once('!') {
                Some((v, flag)) if flag.trim().eq_ignore_ascii_case("important") => (v, true),
                // `bold !ie` and the like: an invalid declaration.
                Some(_) => continue,
                None => (value, false),
            };
            let value = value.trim().to_ascii_lowercase();
            match prop.trim().to_ascii_lowercase().as_str() {
                "font-weight" if important || !bold_important => {
                    if let Some(bold) = weight_is_bold(&value) {
                        e.bold = bold;
                        bold_important = important;
                    }
                }
                "font-style" if important || !italic_important => {
                    if let Some(italic) = style_is_italic(&value) {
                        e.italic = italic;
                        italic_important = important;
                    }
                }
                _ => {}
            }
        }
        e
    }

    /// The part of `self` not already in force in `outer`.
    fn beyond(self, outer: Emphasis) -> Self {
        Emphasis {
            bold: self.bold && !outer.bold,
            italic: self.italic && !outer.italic,
        }
    }

    fn with(self, more: Emphasis) -> Self {
        Emphasis {
            bold: self.bold || more.bold,
            italic: self.italic || more.italic,
        }
    }

    fn any(self) -> bool {
        self.bold || self.italic
    }

    fn marker(self) -> &'static str {
        match (self.bold, self.italic) {
            (true, true) => "***",
            (true, false) => "**",
            (false, true) => "*",
            (false, false) => "",
        }
    }
}

/// Whether a lowercased `font-weight` value is bold: `bold`,
/// `bolder` and numbers of 600 or more are; `normal`, `lighter`,
/// smaller numbers and `initial`/`inherit`/`unset` are not; `None`
/// for anything else.
fn weight_is_bold(value: &str) -> Option<bool> {
    // Markdown has a single weight, so semibold (600) counts. The
    // CSS-wide keywords mean "no weight of its own": `inherit` and
    // `unset` take the parent's, which the walk already tracks.
    match value {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" | "initial" | "inherit" | "unset" => Some(false),
        _ => value.parse::<f32>().ok().map(|w| w >= 600.0),
    }
}

/// Whether a lowercased `font-style` value is italic: `italic` and
/// `oblique` (with or without an angle) are; `normal` and
/// `initial`/`inherit`/`unset` are not; `None` for anything else.
fn style_is_italic(value: &str) -> Option<bool> {
    match value {
        "italic" => Some(true),
        "normal" | "initial" | "inherit" | "unset" => Some(false),
        _ if value.starts_with("oblique") => Some(true),
        _ => None,
    }
}

/// Whitespace, or the `<br>` sentinel: what may sit between a word
/// and the markdown syntax around it.
fn is_edge(c: char) -> bool {
    c.is_whitespace() || c == '\u{0}'
}

/// A closing emphasis marker in the buffer: `buf[at..end]`, closing
/// the markers of `add`.
struct Closer {
    at: usize,
    end: usize,
    add: Emphasis,
}

/// Push `md` onto `buf` wrapped in the markers for `add`, with any
/// whitespace and `<br>` sentinels at its edges outside the markers.
/// When `prev` closes the same markers and ends exactly where `md`
/// begins, with no edge whitespace between, the run continues
/// `prev` instead: `**foo**` then `bar` gives `**foobar**`.
/// Whitespace-only `md`, or an empty `add`, is pushed unchanged.
/// Returns the closer a following run may continue: the one pushed,
/// or `prev` when `md` had nothing to wrap.
fn push_emphasized(
    buf: &mut String,
    md: &str,
    add: Emphasis,
    prev: Option<Closer>,
) -> Option<Closer> {
    // A delimiter next to a space can neither open nor close:
    // `**Note: **text` renders its asterisks literally. Two runs
    // meeting head to tail give `**foo****bar**`, whose middle
    // four asterisks render literally too.
    let core = md.trim_matches(is_edge);
    if core.is_empty() || !add.any() {
        // An empty run (`<i class="icon"></i>`) leaves the previous
        // closer last in `buf`; whitespace pushed here moves `end`
        // off it, which blocks the merge on its own.
        buf.push_str(md);
        return prev;
    }
    let lead = md.len() - md.trim_start_matches(is_edge).len();
    let marker = add.marker();
    match prev {
        Some(p) if p.add == add && lead == 0 && p.end == buf.len() => buf.truncate(p.at),
        _ => {
            buf.push_str(&md[..lead]);
            buf.push_str(marker);
        }
    }
    buf.push_str(core);
    let at = buf.len();
    buf.push_str(marker);
    let end = buf.len();
    buf.push_str(&md[lead + core.len()..]);
    Some(Closer { at, end, add })
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
/// as plain text can never render as a link. The last character of
/// `buf` (possibly a marker we generated) is the left neighbor of
/// the node's first character, so adjacency across element
/// boundaries is handled.
/// With `in_emphasis`, a `*` or `_` that is the node's first or last
/// non-whitespace character is always escaped.
fn escape_md_text(s: &str, buf: &str, in_emphasis: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    // Inside emphasis, the node's outermost visible characters can
    // land next to a marker (`push_emphasized` moves edge whitespace
    // outside it), so the neighbors seen here are not final ones.
    let first = chars.iter().position(|c| !c.is_whitespace());
    let last = chars.iter().rposition(|c| !c.is_whitespace());
    let at_edge = |i: usize| in_emphasis && (Some(i) == first || Some(i) == last);
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
                if at_edge(i) || is_flanking(*c, prev, next) {
                    out.push('\\');
                }
                out.push('*');
            }
            '_' => {
                let intraword = prev.is_some_and(|p| p.is_alphanumeric())
                    && next.is_some_and(|n| n.is_alphanumeric());
                if at_edge(i) || (!intraword && is_flanking(*c, prev, next)) {
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

/// Collapse raw inline-rendered text the same way markdown() does.
/// loose_text (blocks.rs) assembles its buffer directly from text
/// nodes plus the br sentinel, so it needs this exact post-processing
/// or the sentinels survive into the final markdown raw (issue #227).
pub(crate) fn collapse_to_markdown(raw: &str) -> String {
    restore_breaks(&collapse(raw))
}

/// Turn the `<br>` sentinels of collapsed text into newlines, dropping
/// the single space collapse can leave on either side of one.
fn restore_breaks(collapsed: &str) -> String {
    collapsed
        .replace(" \u{0}", "\u{0}")
        .replace("\u{0} ", "\u{0}")
        .replace('\u{0}', "\n")
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
            faithful: false,
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
            faithful: false,
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

    fn element_md(html: &str) -> String {
        let document = Html::parse_fragment(html);
        let el = document
            .root_element()
            .children()
            .filter_map(ElementRef::wrap)
            .next()
            .unwrap();
        let opts = super::super::ExtractOptions::default();
        element_markdown(el, "https://example.com/", &opts).0
    }

    // Every expected string parses back to its input HTML under
    // CommonMark, with or without a space between the two runs.
    #[test]
    fn nested_emphasis_keeps_both_markers() {
        let cases = [
            ("<strong><em>foo</em> bar</strong>", "***foo* bar**"),
            ("<em><strong>foo</strong> bar</em>", "***foo** bar*"),
            ("<em>foo <strong>bar</strong></em>", "*foo **bar***"),
            ("<strong>foo <em>bar</em></strong>", "**foo *bar***"),
            ("<strong><em>foo</em>bar</strong>", "***foo*bar**"),
            ("<em><strong>foo</strong>bar</em>", "***foo**bar*"),
            ("<em>foo<strong>bar</strong></em>", "*foo**bar***"),
            ("<strong>foo<em>bar</em></strong>", "**foo*bar***"),
            ("<em><strong>Unusable</strong></em>", "***Unusable***"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
            assert_eq!(element_md(html), md, "element_markdown: {html}");
        }
    }

    #[test]
    fn nesting_the_same_emphasis_adds_no_marker() {
        let cases = [
            ("<em><em>foo</em></em>", "*foo*"),
            ("<strong><b>foo</b></strong>", "**foo**"),
            ("<em>a <i>b</i> c</em>", "*a b c*"),
            (
                r#"<strong>a <span style="font-weight: bold">b</span> c</strong>"#,
                "**a b c**",
            ),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
        }
    }

    #[test]
    fn whitespace_at_emphasis_edges_goes_outside_the_markers() {
        let cases = [
            ("<strong>Note: </strong>text", "**Note:** text"),
            ("word <em>italic </em>word", "word *italic* word"),
            ("word<em> italic</em> word", "word *italic* word"),
            ("a <strong> b </strong> c", "a **b** c"),
            ("a <em>b<br></em>c", "a *b*\nc"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
        }
    }

    // The edge whitespace moving out puts a literal `*`/`_` right
    // next to a marker, so it must still carry its backslash.
    #[test]
    fn literal_delimiters_at_emphasis_edges_stay_escaped() {
        let cases = [
            ("<em> * x</em>", "*\\* x*"),
            ("<em>x * </em>", "*x \\**"),
            ("<strong>_ x</strong>", "**\\_ x**"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
        }
    }

    #[test]
    fn inline_style_sets_bold_and_italic() {
        let cases = [
            (r#"<span style="font-weight: bold">x</span>"#, "**x**"),
            (r#"<span style="font-weight:700">x</span>"#, "**x**"),
            (r#"<span style="font-weight: 600">x</span>"#, "**x**"),
            (
                r#"<span style="FONT-WEIGHT: Bold !important">x</span>"#,
                "**x**",
            ),
            (r#"<span style="font-style: italic">x</span>"#, "*x*"),
            (r#"<span style="font-style: oblique 10deg">x</span>"#, "*x*"),
            (
                r#"<span style="font-weight: bold; font-style: italic">x</span>"#,
                "***x***",
            ),
            (
                r#"<span style="font-weight: 400; font-weight: 700">x</span>"#,
                "**x**",
            ),
            (r#"<em style="font-weight: bold">x</em>"#, "***x***"),
            // The element's own style overrides its tag.
            (r#"<b style="font-weight: normal">x</b>"#, "x"),
            (r#"<i style="font-style: normal">x</i>"#, "x"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
            assert_eq!(element_md(html), md, "element_markdown: {html}");
        }
    }

    #[test]
    fn bold_style_wraps_a_link_from_outside() {
        let html = r#"<p><a href="/x" style="font-weight: bold">x</a></p>"#;
        assert_eq!(
            render_inline_with(html, true),
            "**[x](https://example.com/x)**"
        );
    }

    #[test]
    fn other_styles_leave_text_plain() {
        let cases = [
            r#"<span style="font-weight: 400">x</span>"#,
            r#"<span style="font-weight: 500">x</span>"#,
            r#"<span style="font-weight: normal">x</span>"#,
            r#"<span style="font-weight: var(--w)">x</span>"#,
            r#"<span style="font-weight bold">x</span>"#,
            r#"<span style="font-family: Bold Sans; color: italic">x</span>"#,
            r#"<span class="bold italic">x</span>"#,
        ];
        for html in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), "x", "{html}");
        }
    }

    #[test]
    fn space_between_inline_elements_is_kept() {
        assert_eq!(
            render_inline("<p><strong>foo</strong> <em>bar</em></p>"),
            "**foo** *bar*"
        );
        let links =
            r#"<p><a href="https://a.example/">x</a> <a href="https://b.example/">y</a></p>"#;
        assert_eq!(
            render_inline_with(links, true),
            "[x](https://a.example/) [y](https://b.example/)"
        );
        assert_eq!(render_inline("<p>a<strong> </strong>b</p>"), "a b");
    }

    #[test]
    fn no_space_is_invented_between_inline_elements() {
        assert_eq!(
            render_inline("<p><strong>foo</strong><em>bar</em></p>"),
            "**foo***bar*"
        );
        // After a <br> the line break is the separator.
        assert_eq!(render_inline("<p>a<br> <b>b</b></p>"), "a\n**b**");
    }

    // Markdown cannot switch bold off inside bold: the inner span's
    // `normal` is lost rather than splitting the outer run.
    #[test]
    fn normal_weight_inside_bold_stays_bold() {
        let html = r#"<p><strong>a <span style="font-weight: normal">b</span></strong></p>"#;
        assert_eq!(render_inline(html), "**a b**");
    }

    // Google Docs wraps a pasted selection in a normal-weight <b>.
    #[test]
    fn normal_weight_wrapper_tag_stays_plain() {
        let html = r#"<p><b style="font-weight:normal" id="docs-internal-guid-1"><span style="font-weight:700">Bold</span> plain</b></p>"#;
        assert_eq!(render_inline(html), "**Bold** plain");
    }

    #[test]
    fn adjacent_runs_of_one_kind_merge() {
        let cases = [
            (
                r#"<span style="font-weight:700">foo</span><span style="font-weight:700">bar</span>"#,
                "**foobar**",
            ),
            ("<b>foo</b><b>bar</b>", "**foobar**"),
            ("<i>foo</i><i>bar</i>", "*foobar*"),
            ("<b><i>a</i></b><b>c</b>", "***a*c**"),
            // An empty icon element between the runs renders nothing.
            ("<b>foo</b><i></i><b>bar</b>", "**foobar**"),
            ("<b>foo</b><b></b><b>bar</b>", "**foobar**"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
        }
    }

    #[test]
    fn runs_apart_or_of_different_kinds_do_not_merge() {
        let cases = [
            ("<b>foo</b> <b>bar</b>", "**foo** **bar**"),
            ("<b>foo </b><b>bar</b>", "**foo** **bar**"),
            ("<b>foo</b>-<b>bar</b>", "**foo**-**bar**"),
            ("<b>foo</b><i>bar</i>", "**foo***bar*"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
        }
    }

    #[test]
    fn link_edge_whitespace_goes_outside_the_brackets() {
        let cases = [
            (
                "<a href=\"/x\"><span>Title</span>\n</a> next",
                "[Title](https://example.com/x) next",
            ),
            (
                "see <a href=\"/x\"> Title </a>next",
                "see [Title](https://example.com/x) next",
            ),
            (
                "<a href=\"https://x.example/\"><span>https://x.example/</span> </a> next",
                "https://x.example/ next",
            ),
        ];
        for (html, md) in cases {
            assert_eq!(
                render_inline_with(&format!("<p>{html}</p>"), true),
                md,
                "{html}"
            );
        }
    }

    // Style on a block nested in inline content is block-level style,
    // which this renderer does not apply.
    #[test]
    fn style_on_a_nested_block_is_not_read() {
        let html = r#"<div>x <div style="font-weight:bold">a</div> b</div>"#;
        assert_eq!(render_inline(html), "x a b");
    }

    #[test]
    fn element_markdown_keeps_edge_spaces_outside_the_wrapper() {
        assert_eq!(element_md("<em> italic</em>"), " *italic*");
        assert_eq!(element_md("<strong>Note: </strong>"), "**Note:** ");
        assert_eq!(element_md("<span>x </span>"), "x ");
    }

    #[test]
    fn style_values_follow_css_validity_and_precedence() {
        let cases = [
            (r#"<span style="font-weight: 1000">x</span>"#, "**x**"),
            (r#"<b style="font-weight: 599">x</b>"#, "x"),
            (r#"<b style="font-weight: normal !ie">x</b>"#, "**x**"),
            (r#"<b style="font-weight: revert">x</b>"#, "**x**"),
            (
                r#"<span style="font-weight:bold!important">x</span>"#,
                "**x**",
            ),
            (
                r#"<span style="font-weight: bold !important; font-weight: normal">x</span>"#,
                "**x**",
            ),
            (
                r#"<span style="font-weight: normal; font-weight: bold ! important">x</span>"#,
                "**x**",
            ),
            (r#"<span style="font-weight: bolder">x</span>"#, "**x**"),
            (r#"<b style="font-weight: lighter">x</b>"#, "x"),
            (r#"<b style="font-weight: inherit">x</b>"#, "x"),
        ];
        for (html, md) in cases {
            assert_eq!(render_inline(&format!("<p>{html}</p>")), md, "{html}");
        }
    }

    #[test]
    fn no_stray_space_around_a_line_break() {
        assert_eq!(
            render_inline("<p><b>Label:</b> <br>next</p>"),
            "**Label:**\nnext"
        );
        assert_eq!(render_inline("<p>a <br> b</p>"), "a\nb");
    }

    fn render_faithful(html: &str) -> String {
        let document = Html::parse_fragment(html);
        let opts = super::super::ExtractOptions {
            faithful: true,
            ..Default::default()
        };
        markdown(document.root_element(), "https://example.com/", &opts).0
    }

    // Faithful mode marks paragraph breaks inside inline content with
    // a doubled sentinel: it stays outside the markers, and no space
    // follows it.
    #[test]
    fn faithful_paragraph_breaks_stay_outside_the_markers() {
        assert_eq!(
            render_faithful("<div>x <strong><div>a</div></strong> b</div>").trim(),
            "x\n\n**a**\n\nb"
        );
        assert_eq!(
            render_faithful("<div><div>a</div> <b>b</b></div>").trim(),
            "a\n\n**b**"
        );
    }

    // Link density of three paragraphs: a link inside bold counts as
    // plain text (0.0), a plain link counts as a link (0.5), and the
    // spaces around a link's text count as link text too (0.75).
    #[test]
    fn a_link_inside_bold_counts_as_plain_text() {
        let opts = super::super::ExtractOptions {
            include_links: true,
            ..Default::default()
        };
        let density = |html: &str| {
            let document = Html::parse_fragment(html);
            markdown(document.root_element(), "https://example.com/", &opts).1
        };
        assert_eq!(
            density(r#"<p>x <strong><a href="https://e.example/">y</a></strong></p>"#),
            0.0
        );
        assert_eq!(
            density(r#"<p>x <a href="https://e.example/">y</a></p>"#),
            0.5
        );
        assert_eq!(
            density(r#"<p>x <a href="https://e.example/"> y </a></p>"#),
            0.75
        );
    }
}

#[cfg(test)]
mod visible_text_tests {
    use super::*;
    use scraper::Html;

    fn plain_of(html: &str) -> String {
        let document = Html::parse_fragment(html);
        let root = document.root_element();
        let el = root.children().filter_map(ElementRef::wrap).next().unwrap();
        plain(el)
    }

    // #288: script and style text is source code, never content.
    #[test]
    fn script_and_style_text_is_never_plain_text() {
        let price = plain_of(
            "<h5>Prime Member Price<style>#buybox .a-price { font-size: 28px !important; }</style></h5>",
        );
        assert_eq!(price, "Prime Member Price");

        // A bare <td> outside a table is dropped by the fragment
        // parser (its content survives as raw text, and the script
        // would become the first ELEMENT), so the cell ships inside
        // its table, like the live page.
        let cell = plain_of(
            "<table><tr><td>Compatible Devices<script type=\"text/javascript\">(function(f) {var _np=window.P._namespace(\"X\");})(window);</script></td></tr></table>",
        );
        assert_eq!(cell, "Compatible Devices");

        let header = plain_of(
            "<h3>Sp4cemanspiff37 <span>• 4y ago</span><script>SML.load([\"28YPyQW8vz\"], 'en-US/', 'auto');</script> • Edited 4y ago</h3>",
        );
        assert!(header.contains("Sp4cemanspiff37 • 4y ago"), "{header}");
        assert!(header.contains("• Edited 4y ago"), "{header}");
        assert!(!header.contains("SML.load"), "{header}");
    }

    // `<pre>` keeps its whitespace; the script subtree still never
    // contributes.
    #[test]
    fn pre_text_keeps_whitespace_but_not_script_content() {
        let document =
            Html::parse_fragment("<pre>line one\n   indented<script>var x = 1;</script></pre>");
        let root = document.root_element();
        let el = root.children().filter_map(ElementRef::wrap).next().unwrap();
        assert_eq!(visible_text_raw(el), "line one\n   indented");
    }
}
