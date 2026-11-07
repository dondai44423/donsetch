//! Block segmentation: DOM → typed blocks with heading
//! breadcrumbs. The block model is what makes focus, pagination,
//! and token-war policies possible.

use scraper::{ElementRef, Node};

use super::inline;

const MAX_DEPTH: usize = 300;

/// Inline phrasing elements: consumed by an ancestor's
/// loose-text paragraph, NEVER walked as standalone blocks
/// (that duplicates content). They still recurse : a card
/// link <a><h2>…</h2><p>…</p></a> has block children that
/// must be emitted.
pub const INLINE_TAGS: &[&str] = &[
    "a", "span", "strong", "b", "em", "i", "code", "small", "sub", "sup", "abbr", "mark", "time",
    "br", "wbr", "u", "s", "q", "cite", "font", "label",
];

#[derive(Debug, Clone)]
pub enum Block {
    Heading {
        level: u8,
        text: String,
        path: Vec<String>,
    },
    Para {
        md: String,
        link_density: f32,
        path: Vec<String>,
    },
    List {
        ordered: bool,
        items: Vec<String>,
        link_density: f32,
        path: Vec<String>,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        truncated: bool,
        path: Vec<String>,
    },
    Code {
        lang: Option<String>,
        code: String,
        path: Vec<String>,
    },
    Quote {
        md: String,
        path: Vec<String>,
    },
    Media {
        alt: String,
        src: String,
        path: Vec<String>,
    },
}

impl Block {
    pub fn path(&self) -> &[String] {
        match self {
            Block::Heading { path, .. }
            | Block::Para { path, .. }
            | Block::List { path, .. }
            | Block::Table { path, .. }
            | Block::Code { path, .. }
            | Block::Quote { path, .. }
            | Block::Media { path, .. } => path,
        }
    }

    /// The block's own text, without inherited heading breadcrumbs.
    pub fn body_text(&self) -> String {
        match self {
            Block::Heading { text, .. } => text.clone(),
            Block::Para { md, .. } | Block::Quote { md, .. } => md.clone(),
            Block::List { items, .. } => items.join(" "),
            Block::Table { headers, rows, .. } => {
                let mut s = headers.join(" ");
                for r in rows {
                    s.push(' ');
                    s.push_str(&r.join(" "));
                }
                s
            }
            Block::Code { code, .. } => code.clone(),
            Block::Media { alt, .. } => alt.clone(),
        }
    }

    /// Plain text with heading context for accounting and diagnostics.
    pub fn text(&self) -> String {
        let body = self.body_text();
        if self.path().is_empty() {
            body
        } else {
            format!("{} {body}", self.path().join(" "))
        }
    }

    pub fn chars(&self) -> usize {
        self.text().len()
    }
}

pub fn segment(
    root: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
    out: &mut Vec<Block>,
) {
    let mut headings: Vec<(u8, String)> = Vec::new();
    walk(root, base, opts, &mut headings, out, 0);
    fold_code_labels(out);
}

/// A code block's language often arrives as its own element: MDN renders
/// `<span class="language-name">js</span>` in the example header, docs
/// themes put a lone "Bash" line above the fence. As a paragraph that is a
/// stray word costing a line, and the fence is left bare; fold the word into
/// the fence's info string instead.
fn fold_code_labels(blocks: &mut Vec<Block>) {
    let mut i = 1;
    while i < blocks.len() {
        // Some(new_lang) => the paragraph is a code language label; a
        // `Some(Some(..))` also fills a fence that had none, `Some(None)`
        // only drops a header label that repeats the fence's language.
        let label = match (&blocks[i - 1], &blocks[i]) {
            (Block::Para { md, .. }, Block::Code { lang, .. }) => {
                let tok = md.trim();
                let usable = !tok.is_empty()
                    && !tok.chars().any(char::is_whitespace)
                    && tok.chars().count() <= 16
                    && foldable_lang(tok);
                if !usable {
                    None
                } else {
                    match lang {
                        None => Some(Some(tok.to_lowercase())),
                        Some(l) if l.eq_ignore_ascii_case(tok) => Some(None),
                        _ => None,
                    }
                }
            }
            _ => None,
        };
        match label {
            Some(new_lang) => {
                if let Some(lang) = new_lang
                    && let Block::Code { lang: l, .. } = &mut blocks[i]
                {
                    *l = Some(lang);
                }
                blocks.remove(i - 1);
                // The code block is now at the same index.
            }
            None => i += 1,
        }
    }
}

/// Language names a code label may carry. Used both to read a `class`
/// attribute and to recognise a label that rendered as its own block, so it
/// has to stay conservative: a stray paragraph is only folded into a fence
/// when the whole paragraph is one of these words.
const KNOWN_LANGS: &[&str] = &[
    "python",
    "py",
    "rust",
    "javascript",
    "js",
    "typescript",
    "ts",
    "java",
    "go",
    "golang",
    "c",
    "cpp",
    "c++",
    "csharp",
    "cs",
    "ruby",
    "php",
    "swift",
    "kotlin",
    "scala",
    "sql",
    "bash",
    "shell",
    "sh",
    "zsh",
    "console",
    "terminal",
    "html",
    "css",
    "scss",
    "sass",
    "json",
    "jsonc",
    "yaml",
    "yml",
    "xml",
    "toml",
    "ini",
    "markdown",
    "md",
    "dockerfile",
    "makefile",
    "lua",
    "perl",
    "r",
    "diff",
    "patch",
    "text",
    "txt",
    "plaintext",
    "http",
    "graphql",
    "protobuf",
    "proto",
    "nginx",
    "powershell",
    "ps1",
    "vim",
    "asm",
    "elixir",
    "erlang",
    "haskell",
    "clojure",
    "dart",
    "julia",
    "matlab",
    "objectivec",
    "zig",
    "nim",
];

fn is_known_lang(token: &str) -> bool {
    let t = token.trim().to_lowercase();
    KNOWN_LANGS.contains(&t.as_str())
}

/// A label may be folded into a fence only when a stray paragraph with that
/// exact text is implausible: "c" and "r" are real language names AND
/// ordinary one-word paragraphs, so they stay a class-attribute signal.
fn foldable_lang(token: &str) -> bool {
    let t = token.trim().to_lowercase();
    is_known_lang(&t) && !matches!(t.as_str(), "c" | "r")
}

fn current_path(headings: &[(u8, String)]) -> Vec<String> {
    headings.iter().map(|(_, t)| t.clone()).collect()
}

fn push_block(block: Block, out: &mut Vec<Block>) {
    if block.chars() > 0 {
        out.push(block);
    }
}

fn walk<'a>(
    el: ElementRef<'a>,
    base: &str,
    opts: &super::ExtractOptions,
    headings: &mut Vec<(u8, String)>,
    out: &mut Vec<Block>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let name = el.value().name();
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name.as_bytes()[1] - b'0';
            let text = inline::markdown(el, base, opts).0;
            if !text.is_empty() {
                while headings.last().is_some_and(|(l, _)| *l >= level) {
                    headings.pop();
                }
                headings.push((level, text.clone()));
                push_block(
                    Block::Heading {
                        level,
                        text,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "p" => {
            let (md, ld) = inline::markdown(el, base, opts);
            if !md.is_empty() {
                push_block(
                    Block::Para {
                        md,
                        link_density: ld,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "ul" | "ol" => {
            let (items, ld) = list_items(el, base, opts, 0);
            if !items.is_empty() {
                push_block(
                    Block::List {
                        ordered: name == "ol",
                        items,
                        link_density: ld,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "table" if is_link_farm_table(el, base, opts) => {}
        "table" => {
            // Prose tables (forum threads, HN comment trees, layout
            // tables) must NOT go through the pipe-table renderer :
            // it clamps cells to 120 chars and destroys the text.
            // Data tables keep the pipe rendering.
            if is_prose_table(el) {
                // Walk children as containers : comment divs inside
                // the cells become real paragraphs.
                let loose = loose_text(el, base, opts);
                if !loose.0.is_empty() {
                    push_block(
                        Block::Para {
                            md: loose.0,
                            link_density: loose.1,
                            path: current_path(headings),
                        },
                        out,
                    );
                }
                for child in el.children() {
                    let Some(child_el) = ElementRef::wrap(child) else {
                        continue;
                    };
                    if crate::extract::junk::skip(child_el) {
                        continue;
                    }
                    walk(child_el, base, opts, headings, out, depth + 1);
                }
            } else if let Some(t) = table_block(el, headings, base, opts) {
                push_block(t, out);
            }
        }
        "math" => {
            // Formula as its own block: `$$LaTeX$$`. alttext (the
            // original LaTeX) when present, MathML serialization
            // otherwise. Math elements must never vanish : that
            // guts technical pages.
            let l = super::math::latex(el);
            if !l.is_empty() {
                push_block(
                    Block::Para {
                        md: format!("$${l}$$"),
                        link_density: 0.0,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "pre" => {
            // Raw text, whitespace PRESERVED (code must keep newlines);
            // script/style subtrees are never source content, even
            // inside a pre (#288).
            let code = inline::visible_text_raw(el);
            let code = code.trim_matches('\n').to_string();
            // Collapse 3+ consecutive newlines to 2: common in
            // source code, each blank line pair wastes tokens.
            let code = collapse_blank_lines(&code);
            if !code.is_empty() {
                let lang = detect_code_lang(el);
                push_block(
                    Block::Code {
                        lang,
                        code,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "blockquote" => {
            let (md, _) = inline::markdown(el, base, opts);
            if !md.is_empty() {
                push_block(
                    Block::Quote {
                        md,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "summary" => {
            // <summary> is the clickable label of a <details>
            // element. Render as a heading so agents see the
            // collapsible section's title.
            let text = inline::plain(el);
            if !text.is_empty() {
                // Use level 3: details/summary is usually a
                // sub-section, not a top-level heading.
                let level = 3u8;
                while headings.last().is_some_and(|(l, _)| *l >= level) {
                    headings.pop();
                }
                headings.push((level, text.clone()));
                push_block(
                    Block::Heading {
                        level,
                        text,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        "dl" => {
            // Definition list: dt (term) + dd (definition) pairs.
            // Rendered as a list with "**term** : definition".
            let items = def_list_items(el, base, opts);
            if !items.is_empty() {
                push_block(
                    Block::List {
                        ordered: false,
                        items,
                        link_density: 0.0,
                        path: current_path(headings),
                    },
                    out,
                );
            }
        }
        // Media blocks are ALWAYS segmented (cheap) : the render
        // layer drops them unless include_media. On-demand image
        // OCR (v3) needs the image list even when media lines are
        // not part of the output.
        "img" => {
            media_block(el, base, headings, out);
        }
        // A figure takes the media path only when images and a
        // caption are all it holds. One that wraps a table, a code
        // listing or a quote (WordPress figure.wp-block-table and
        // wp-block-pullquote, Jekyll figure.highlight > pre) falls
        // through to the container walk, which emits every child.
        "figure" if !figure_has_content(el) => {
            media_block(el, base, headings, out);
        }
        "hr" => {}
        _ => {
            // Container or unknown: emit direct loose text as a
            // paragraph (div-soup), then recurse into children.
            // Inline elements emit nothing themselves : their
            // text was captured by the nearest block ancestor.
            if !INLINE_TAGS.contains(&name) {
                let loose = loose_text(el, base, opts);
                if !loose.0.is_empty() {
                    push_block(
                        Block::Para {
                            md: loose.0,
                            link_density: loose.1,
                            path: current_path(headings),
                        },
                        out,
                    );
                }
            }
            for child in el.children() {
                let Some(child_el) = ElementRef::wrap(child) else {
                    continue;
                };
                if crate::extract::junk::skip(child_el) {
                    continue;
                }
                // No negative+size gate here: find_main already
                // chose the scope. Extract everything within it.
                // The gate in score.rs handles main-content
                // detection; applying it again here nukes real
                // content (Reddit/HN comments have class "comment"
                // which is in NEGATIVE_SUBSTR : the gate was
                // silently dropping all short comments).
                walk(child_el, base, opts, headings, out, depth + 1);
            }
        }
    }
}

/// Direct (non-element-wrapped) text of an element : the
/// div-soup paragraph case.
fn loose_text(el: ElementRef<'_>, base: &str, opts: &super::ExtractOptions) -> (String, f32) {
    let mut buf = String::new();
    let mut total = 0usize;
    let mut link = 0usize;
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let s = t.text.trim();
                if !s.is_empty() {
                    // After a br the buffer ends with a newline: that
                    // IS the separator. A space here would give every
                    // paragraph after a br run a leading space.
                    if !buf.is_empty() && !buf.ends_with('\u{0}') {
                        buf.push(' ');
                    }
                    buf.push_str(s);
                    total += s.len();
                }
            }
            Node::Element(_) => {
                let Some(c) = ElementRef::wrap(child) else {
                    continue;
                };
                let n = c.value().name();
                // Inline phrasing content belongs to the loose paragraph :
                // but an inline element wrapping BLOCK children (card
                // links: <a><h2>…</h2><p>…</p></a>) must not be
                // swallowed here; the walk emits those blocks itself.
                if n == "br" {
                    // A <br> is a line break, not empty content. The
                    // inline renderer only walks CHILDREN, so a br
                    // passed as the root renders empty and its break
                    // is lost (issue #227: "text<br />\r<br />\r\ntext"
                    // collapsed to one space). Emit the same NUL
                    // sentinel the inline renderer uses so the collapse
                    // turns it into a newline. This is the shared
                    // conversion site for every DOM-to-markdown path
                    // (the page read, adapters, fallback).
                    buf.push('\u{0}');
                    continue;
                }
                if INLINE_TAGS.contains(&n) && !crate::extract::junk::skip(c) && !contains_block(c)
                {
                    let (md, _) = inline::element_markdown(c, base, opts);
                    // Newlines inside inline content are real line breaks
                    // (the br sentinel survives the inline render). Trim
                    // horizontal whitespace but keep the newlines: a
                    // br-only inline run ("text<br />\r<br />\r\ntext")
                    // must yield paragraph breaks, not collapse to one
                    // space (issue #227, live-confirmed on the tier-2
                    // ghost path; this is the shared conversion site
                    // every DOM-to-markdown path funnels through).
                    let t = md.trim_matches(|ch: char| ch != '\n' && ch.is_whitespace());
                    if !t.is_empty() {
                        if !buf.is_empty() && !buf.ends_with('\u{0}') {
                            buf.push(' ');
                        }
                        buf.push_str(t);
                        total += t.len();
                        if n == "a" {
                            link += t.len();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let ld = if total > 0 {
        link as f32 / total as f32
    } else {
        0.0
    };
    // The buffer is raw inline rendering: it may carry the br NUL
    // sentinel, so collapse it exactly like inline::markdown does
    // (whitespace fold, then sentinel to newline). Without this the
    // sentinels survive into the block markdown raw (issue #227).
    (inline::collapse_to_markdown(&buf), ld)
}

/// True when an element has block-level descendants.
fn contains_block(el: ElementRef<'_>) -> bool {
    let mut stack: Vec<ElementRef<'_>> = el.children().filter_map(ElementRef::wrap).collect();
    while let Some(node) = stack.pop() {
        if matches!(
            node.value().name(),
            "h1" | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "p"
                | "div"
                | "ul"
                | "ol"
                | "li"
                | "table"
                | "pre"
                | "blockquote"
                | "section"
                | "article"
                | "figure"
                | "hr"
        ) {
            return true;
        }
        stack.extend(node.children().filter_map(ElementRef::wrap));
    }
    false
}

/// Items of a `<ul>`/`<ol>`, nested lists flattened to indented
/// entries ("  " per level, up to 4 deep). Shared with the docs
/// and Wikipedia adapters so nested lists render the same way
/// everywhere.
pub(crate) fn list_items(
    list: ElementRef<'_>,
    base: &str,
    opts: &super::ExtractOptions,
    depth: u8,
) -> (Vec<String>, f32) {
    let mut items = Vec::new();
    let mut total = 0usize;
    let mut link = 0usize;
    for child in list.children() {
        let Some(li) = ElementRef::wrap(child) else {
            continue;
        };
        if li.value().name() != "li" || crate::extract::junk::skip(li) {
            continue;
        }
        // Below the cap the nested lists are rendered as indented
        // items (so the item's own line must not include them); at
        // the cap they are flattened into the line instead, never
        // dropped.
        let (md, ld) = if depth < 4 {
            inline::item_markdown(li, base, opts)
        } else {
            inline::markdown(li, base, opts)
        };
        let md = md.trim().to_string();
        if !md.is_empty() {
            items.push(format!("{}{}", "  ".repeat(depth as usize), md));
            total += md.len();
            link += (ld * md.len() as f32) as usize;
        }
        // Nested lists.
        for sub in li.children() {
            let Some(sub_el) = ElementRef::wrap(sub) else {
                continue;
            };
            if matches!(sub_el.value().name(), "ul" | "ol") && depth < 4 {
                let (nested, _) = list_items(sub_el, base, opts, depth + 1);
                items.extend(nested);
            }
        }
    }
    let ld = if total > 0 {
        link as f32 / total as f32
    } else {
        0.0
    };
    (items, ld)
}

/// Definition list (dl/dt/dd) : render as
/// "**term** : definition" list items.
fn def_list_items(dl: ElementRef<'_>, base: &str, opts: &super::ExtractOptions) -> Vec<String> {
    let mut items = Vec::new();
    let mut current_term: Option<String> = None;
    for child in dl.children() {
        let Some(el) = ElementRef::wrap(child) else {
            continue;
        };
        if crate::extract::junk::skip(el) {
            continue;
        }
        match el.value().name() {
            "dt" => {
                let term = inline::plain(el);
                if !term.is_empty() {
                    current_term = Some(term);
                }
            }
            "dd" => {
                let (def, _) = inline::markdown(el, base, opts);
                let def = def.trim().to_string();
                if def.is_empty() {
                    continue;
                }
                if let Some(term) = &current_term {
                    items.push(format!("**{term}** : {def}"));
                } else {
                    items.push(def);
                }
            }
            _ => {}
        }
    }
    items
}

/// A navigation table: a Wikipedia series sidebar or navbox, a link
/// directory laid out as a table. It is a list of links to other pages, not
/// content of this one, and at several thousand characters it would be the
/// first thing every read of the article returns. Only a table that both
/// says so in its class and is almost all links is dropped, whatever its size.
fn is_link_farm_table(el: ElementRef<'_>, base: &str, opts: &super::ExtractOptions) -> bool {
    let class = el.value().attr("class").unwrap_or("");
    let nav_class = class.split_whitespace().any(|c| {
        matches!(
            c,
            "sidebar" | "navbox" | "vertical-navbox" | "navbar" | "navbox-inner"
        )
    });
    if !nav_class || class.split_whitespace().any(|c| c == "infobox") {
        return false;
    }
    let (_, link_density) = inline::markdown(el, base, opts);
    link_density >= 0.7
}

/// Layout/prose table detection. Forums (HN, phpBB), old CMSes,
/// and email-style pages lay CONTENT out in tables; rendering those
/// as pipe tables flattens their paragraph structure. A table is
/// prose/layout when:
/// - it declares `role="presentation"`, or
/// - without explicit headers, any cell holds paragraph-length text
///   (300+ chars), or it is effectively single-column.
///
/// Data tables (specs, pricing, comparisons) stay pipe tables.
/// A table's own rows: `tr` children, directly or through its
/// `thead`/`tbody`/`tfoot`. The descendant select used before
/// (`el.select("tr")`) also returned every row of every table nested
/// in a cell, so a nested table's rows were merged into the outer
/// one, and on a page of tables nested d deep the outer row "held"
/// all d nested cells, each scanned down to the bottom: quadratic
/// per level, and the walk then recursed a level and did it again.
/// 18 KB of `<table><tr><td>` × 1000 took minutes.
fn own_rows<'a>(table: ElementRef<'a>) -> Vec<ElementRef<'a>> {
    own_row_groups(table).into_iter().flatten().collect()
}

/// A table's own rows by row group, in document order: each `thead`,
/// `tbody` and `tfoot` is a group, and so is each run of `tr` children
/// sitting directly in the table. Groups without rows are left out.
fn own_row_groups<'a>(table: ElementRef<'a>) -> Vec<Vec<ElementRef<'a>>> {
    let mut groups = Vec::new();
    let mut loose = Vec::new();
    for child in table.children() {
        let Some(c) = ElementRef::wrap(child) else {
            continue;
        };
        match c.value().name() {
            "tr" => loose.push(c),
            "thead" | "tbody" | "tfoot" => {
                if !loose.is_empty() {
                    groups.push(std::mem::take(&mut loose));
                }
                let rows: Vec<ElementRef<'a>> = c
                    .children()
                    .filter_map(ElementRef::wrap)
                    .filter(|g| g.value().name() == "tr")
                    .collect();
                if !rows.is_empty() {
                    groups.push(rows);
                }
            }
            _ => {}
        }
    }
    if !loose.is_empty() {
        groups.push(loose);
    }
    groups
}

/// A row's own cells, in document order.
fn own_cells<'a>(tr: ElementRef<'a>) -> impl Iterator<Item = ElementRef<'a>> {
    tr.children()
        .filter_map(ElementRef::wrap)
        .filter(|c| matches!(c.value().name(), "td" | "th"))
}

fn is_prose_table(el: ElementRef<'_>) -> bool {
    if el.value().attr("role") == Some("presentation") {
        return true;
    }
    let mut max_cell = 0usize;
    let mut rows = 0usize;
    let mut max_cols = 0usize;
    let mut has_headers = false;
    for tr in own_rows(el).into_iter().take(40) {
        rows += 1;
        let mut cols = 0usize;
        for cell in own_cells(tr) {
            has_headers |= cell.value().name() == "th";
            cols += 1;
            let t = crate::extract::junk::text_size(cell, 400);
            max_cell = max_cell.max(t);
        }
        max_cols = max_cols.max(cols);
    }
    if rows == 0 {
        return false;
    }
    // Explicit headers identify a data table even when a cell is long.
    if has_headers {
        return false;
    }
    // Paragraph-length text in a cell = the table holds prose.
    if max_cell >= 300 {
        return true;
    }
    // Single-column table of short cells is still layout (indent
    // spacers, vertical stacks) : but only when it has several rows
    // (a 1×1 data table is degenerate either way).
    max_cols <= 1 && rows >= 2
}

/// A span stops before column `max(SPAN_COLS, widest row's cell count)`.
const SPAN_COLS: usize = 64;
/// Text copied into spanned slots is capped per table at the table's own
/// cell text or this many bytes, whichever is larger.
const SPAN_COPY_FLOOR: usize = 64 << 10;
/// The grid's rows hold at most `max(SPAN_SLOTS, SPAN_SLOTS_PER_CELL *
/// source cells)` slots in all; the rows after the one that passes it
/// ignore spans.
const SPAN_SLOTS: usize = 1 << 16;
const SPAN_SLOTS_PER_CELL: usize = 8;

/// One source cell on the table grid: anchored at (`row`, `col`), covering
/// `cols` columns and `rows` rows once placed.
struct GridCell {
    th: bool,
    text: String,
    row: usize,
    col: usize,
    cols: usize,
    rows: usize,
}

/// HTML "rules for parsing non-negative integers": leading ASCII
/// whitespace, an optional sign, then the digits up to the first
/// non-digit. `None` for a missing attribute, a parse error or a
/// negative value; huge values saturate.
fn span_attr(cell: ElementRef<'_>, name: &str) -> Option<usize> {
    let v = cell
        .value()
        .attr(name)?
        .trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (negative, v) = match v.as_bytes().first() {
        Some(b'-') => (true, &v[1..]),
        Some(b'+') => (false, &v[1..]),
        _ => (false, v),
    };
    let digits = v.bytes().take_while(u8::is_ascii_digit);
    let mut seen = false;
    let mut n = 0usize;
    for d in digits {
        seen = true;
        n = n.saturating_mul(10).saturating_add(usize::from(d - b'0'));
    }
    (seen && (!negative || n == 0)).then_some(n)
}

/// The table as a pipe-table block, cells laid out on the HTML table
/// grid: a `colspan`/`rowspan` cell fills every slot it covers, so later
/// cells keep their column. A spanned slot repeats the cell's text. It
/// is empty instead when the table's copy budget is spent, or, outside a
/// merged header, when the cell spans the table's whole width (its text
/// stays in its first column). Consecutive leading `<th>`-only rows form
/// one header row when the row above holds a header cell spanning
/// several columns; that header joins each column's distinct labels with
/// a space ("Price Monthly"). Columns in which no cell starts hold only
/// copies and are dropped. A table without span attributes produces the
/// same block as reading each row as a flat list of its cells.
fn table_block(
    el: ElementRef<'_>,
    headings: &[(u8, String)],
    base: &str,
    opts: &super::ExtractOptions,
) -> Option<Block> {
    // Output pagination is the budget. Cutting source rows or cells here
    // makes later focus/probe/offset reads unable to recover the evidence.
    // Set when any filled cell holds text that is not a link.
    let mut prose_cell = false;
    let groups = own_row_groups(el);
    let mut cells: Vec<GridCell> = Vec::new();
    // Per row: the range of its own cells in `cells`.
    let mut row_cells: Vec<std::ops::Range<usize>> = Vec::new();
    for group in &groups {
        for tr in group {
            let start = cells.len();
            // One select over both cell kinds, in document order: a
            // row's <th scope="row"> label has to stay in its column.
            // Selecting <th> and <td> separately (the old shape) only
            // ever used <th> for the header row, so every data row's
            // label vanished and its remaining cells shifted left.
            for c in own_cells(*tr) {
                let th = c.value().name() == "th";
                let (md, link_density) = inline::markdown(c, base, opts);
                prose_cell |= !md.trim().is_empty() && link_density < 0.9;
                let text = md.replace('|', "\\|"); // unescaped pipes break md tables
                // WHATWG 4.9.12.1: colspan 0 or unparsable is 1, capped at
                // 1000; rowspan unparsable is 1, capped at 65534, and 0
                // means "to the end of the row group".
                let cols = span_attr(c, "colspan")
                    .filter(|&n| n > 0)
                    .unwrap_or(1)
                    .min(1000);
                let rows = span_attr(c, "rowspan").unwrap_or(1).min(65534);
                // Placement below replaces `col`, `cols` and `rows` with
                // the cell's position and its clipped spans.
                cells.push(GridCell {
                    th,
                    text,
                    row: row_cells.len(),
                    col: 0,
                    cols,
                    rows,
                });
            }
            row_cells.push(start..cells.len());
        }
    }
    let widest = row_cells
        .iter()
        .map(ExactSizeIterator::len)
        .max()
        .unwrap_or(0);
    // Spans stop before column `limit`, and a cell pushed to or past it
    // by spans keeps colspan and rowspan 1. Unclipped, a cell per row that
    // spans 1000 columns to the end of the group would push the next
    // row's cell 1000 columns further right, a grid growing with the
    // square of the rows; clipped, a row has at most `limit + widest`
    // slots.
    let limit = SPAN_COLS.max(widest);

    // Placement (WHATWG 4.9.12.1): a cell starts at the first slot of its
    // row no earlier cell covers. `free_from[c]` is the first row at
    // which column c is no longer covered.
    let mut free_from: Vec<usize> = Vec::new();
    let mut starts: Vec<bool> = Vec::new();
    // Rows are as wide as their last covered column, so a few cells that
    // span many rows can still make a grid far larger than the table's
    // source (2000 header cells with rowspan="0" over 2000 one-cell rows
    // is 4 million slots from 4000 cells). Past the slot cap the spans
    // end and later rows are read as flat cell lists.
    let max_slots = SPAN_SLOTS.max(SPAN_SLOTS_PER_CELL.saturating_mul(cells.len()));
    let mut slots_used = 0usize;
    let mut spans_on = true;
    let mut y = 0;
    for group in &groups {
        let end = y + group.len();
        for _ in group {
            let mut x = 0;
            for id in row_cells[y].clone() {
                while free_from.get(x).is_some_and(|&f| f > y) {
                    x += 1;
                }
                let (cols, rows) = (cells[id].cols, cells[id].rows);
                // A rowspan stops at the end of its row group (the table
                // model ends every downward-growing cell there).
                let (cols, rows) = if spans_on && x < limit {
                    let left = end - y;
                    let rows = if rows == 0 { left } else { rows.min(left) };
                    (cols.min(limit - x), rows)
                } else {
                    (1, 1)
                };
                if free_from.len() < x + cols {
                    free_from.resize(x + cols, 0);
                }
                for f in &mut free_from[x..x + cols] {
                    *f = (*f).max(y + rows);
                }
                if starts.len() <= x {
                    starts.resize(x + 1, false);
                }
                starts[x] = true;
                let cell = &mut cells[id];
                (cell.col, cell.cols, cell.rows) = (x, cols, rows);
                x += cols;
            }
            if spans_on {
                slots_used += free_from.iter().rposition(|&f| f > y).map_or(0, |c| c + 1);
                if slots_used > max_slots {
                    spans_on = false;
                    for f in &mut free_from {
                        *f = (*f).min(y + 1);
                    }
                    for cell in &mut cells[..row_cells[y].end] {
                        cell.rows = cell.rows.min(y + 1 - cell.row);
                    }
                }
            }
            y += 1;
        }
    }
    starts.resize(free_from.len(), false);
    // Columns where no cell starts hold only copies of cells to their
    // left (a table model error per WHATWG): drop them.
    let mut keep: Vec<Option<usize>> = Vec::with_capacity(starts.len());
    let mut width = 0;
    for &s in &starts {
        keep.push(s.then(|| {
            width += 1;
            width - 1
        }));
    }
    let kept_cols = |c: &GridCell| keep[c.col..c.col + c.cols].iter().flatten().count();

    // Header rows: the first row made only of <th> (before any data
    // row), then each following <th>-only row while the row above it
    // has its own header cell over several columns: a group header
    // whose sub-headers sit in the next row. Without that span a second
    // <th> row stays a data row, so a table of <th> cells keeps its rows.
    let own = |y: usize| &cells[row_cells[y].clone()];
    let mut header_rows: Vec<usize> = Vec::new();
    let mut is_header = vec![false; row_cells.len()];
    let mut is_data = vec![false; row_cells.len()];
    let mut any_data = false;
    for y in 0..row_cells.len() {
        let all_th = !own(y).is_empty() && own(y).iter().all(|c| c.th);
        if all_th && !any_data {
            let opens = header_rows
                .last()
                .is_none_or(|&above| own(above).iter().any(|c| kept_cols(c) > 1));
            if opens {
                header_rows.push(y);
                is_header[y] = true;
                continue;
            }
        }
        if own(y).iter().any(|c| !c.text.is_empty()) {
            is_data[y] = true;
            any_data = true;
        }
    }
    if header_rows.is_empty() && !any_data {
        return None;
    }
    // Tab-nav junk: tiny tables that are only links. Counted on source
    // cells, so a span cannot lift a link strip past it. A small table with
    // a cell of plain text is a figure ("Maximum | EUR 15M or 3%"), and
    // dropping it leaves a section that reads as complete without it.
    let (mut counted, mut total_text) = (0usize, 0usize);
    for y in (0..row_cells.len()).filter(|&y| is_header[y] || is_data[y]) {
        for c in own(y) {
            counted += 1;
            total_text += c.text.len();
        }
    }
    if counted <= 3 && total_text < 60 && !prose_cell {
        return None;
    }

    // A spanned slot repeats the cell's text instead of staying empty.
    // Focus narrows a table to the rows its query selects (each row
    // scored on its own joined cells and kept with the headers), and
    // views rank every row as its own passage under the header line, so
    // a row read without its neighbours must still carry its rowspan
    // label; a cell shared by some columns (n/a under Monthly and
    // Yearly) holds for each of them, and the table model gives a header
    // cell to every column it spans (WHATWG 4.9.12.2). Outside a merged
    // header, a cell over the table's whole width keeps its text in its
    // first column only: the row is read whole and has it once, and
    // repeating it doubled the infobox title and full-width rows of the
    // wiki-rust corpus page. The copies are budgeted; past the budget a
    // spanned slot is empty and its column stays aligned.
    let own_text: usize = cells.iter().map(|c| c.text.len()).sum();
    let mut budget = own_text.max(SPAN_COPY_FLOOR);
    let copy = |id: usize, budget: &mut usize| -> String {
        let t = &cells[id].text;
        if t.len() <= *budget {
            *budget -= t.len();
            t.clone()
        } else {
            String::new()
        }
    };
    let row_text = |y: usize, slots: &[Option<usize>], budget: &mut usize| -> Vec<String> {
        slots
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let Some(id) = *s else {
                    return String::new();
                };
                let c = &cells[id];
                if keep[c.col] == Some(k) {
                    if c.row == y {
                        c.text.clone()
                    } else {
                        copy(id, budget)
                    }
                } else if kept_cols(c) == width {
                    String::new()
                } else {
                    copy(id, budget)
                }
            })
            .collect()
    };
    // Several header rows merge column by column: each column's distinct
    // labels, top to bottom, joined with a space.
    let header_text = |slots: &[Vec<Option<usize>>], budget: &mut usize| -> Vec<String> {
        match slots {
            [] => Vec::new(),
            [only] => row_text(header_rows[0], only, budget),
            _ => {
                let n = slots.iter().map(Vec::len).max().unwrap_or(0);
                (0..n)
                    .map(|k| {
                        let mut parts: Vec<String> = Vec::new();
                        let mut last = None;
                        for id in slots.iter().filter_map(|r| r.get(k).copied().flatten()) {
                            if last == Some(id) {
                                continue;
                            }
                            last = Some(id);
                            let t = if keep[cells[id].col] == Some(k) {
                                cells[id].text.clone()
                            } else {
                                copy(id, budget)
                            };
                            if !t.is_empty() {
                                parts.push(t);
                            }
                        }
                        parts.join(" ")
                    })
                    .collect()
            }
        }
    };

    // One sweep over the rows reads each kept row off `cover[k]`: the
    // cell covering kept column k and the first row it no longer covers.
    // An overlapping later cell does not take a slot from the earlier
    // one. Header rows come before every data row, so the header is
    // built (and served from the budget) before the first data row.
    let mut cover: Vec<Option<(usize, usize)>> = vec![None; width];
    let mut header_slots: Vec<Vec<Option<usize>>> = Vec::new();
    let mut headers: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for (y, ids) in row_cells.iter().enumerate() {
        for id in ids.clone() {
            let c = &cells[id];
            for k in keep[c.col..c.col + c.cols].iter().flatten() {
                if cover[*k].is_none_or(|(_, until)| until <= y) {
                    cover[*k] = Some((id, y + c.rows));
                }
            }
        }
        if !is_header[y] && !is_data[y] {
            continue;
        }
        let mut slots: Vec<Option<usize>> = cover
            .iter()
            .map(|s| s.filter(|&(_, until)| until > y).map(|(id, _)| id))
            .collect();
        while slots.last() == Some(&None) {
            slots.pop();
        }
        if is_header[y] {
            header_slots.push(slots);
            continue;
        }
        if headers.is_none() {
            headers = Some(header_text(&header_slots, &mut budget));
        }
        rows.push(row_text(y, &slots, &mut budget));
    }
    let mut headers = headers.unwrap_or_else(|| header_text(&header_slots, &mut budget));
    // No <th> found: promote the first data row to headers.
    // Without this, tables without <th> render with empty header
    // cells (| | | |) which is useless to agents.
    if headers.is_empty() && !rows.is_empty() {
        headers = rows.remove(0);
    }
    Some(Block::Table {
        headers,
        rows,
        truncated: false,
        path: current_path(headings),
    })
}

fn media_block(el: ElementRef<'_>, base: &str, headings: &[(u8, String)], out: &mut Vec<Block>) {
    let sel = scraper::Selector::parse("img").unwrap();
    // figcaption = the alt an agent actually wants (chart/
    // diagram descriptions) when the img alt is empty.
    let caption = if el.value().name() == "figure" {
        scraper::Selector::parse("figcaption")
            .ok()
            .and_then(|s| el.select(&s).next())
            .map(|c| inline::plain(c))
            .filter(|c| !c.is_empty())
    } else {
        None
    };
    let imgs: Vec<ElementRef<'_>> = if el.value().name() == "img" {
        vec![el]
    } else {
        el.select(&sel).collect()
    };
    for img in imgs.into_iter().take(3) {
        let src = img
            .value()
            .attr("src")
            .or_else(|| img.value().attr("data-src"));
        let Some(src) = src else { continue };
        // Skip icons/spacers.
        let small = ["width", "height"].iter().any(|a| {
            img.value()
                .attr(a)
                .and_then(|v| v.parse::<u32>().ok())
                .is_some_and(|n| n < 40)
        });
        if small {
            continue;
        }
        let alt = img
            .value()
            .attr("alt")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| caption.clone())
            .unwrap_or_default();
        let abs = inline::absolutize(base, src);
        if let Some(src) = abs {
            out.push(Block::Media {
                alt,
                src,
                path: current_path(headings),
            });
        }
    }
}

/// True when a `<figure>` holds readable text outside its
/// `<figcaption>` elements. Subtrees the walk skips as junk
/// (script, svg, hidden, ...) do not count.
fn figure_has_content(fig: ElementRef<'_>) -> bool {
    let mut stack: Vec<_> = fig.children().collect();
    while let Some(node) = stack.pop() {
        match node.value() {
            Node::Text(t) if !t.text.trim().is_empty() => return true,
            Node::Element(_) => {
                let Some(c) = ElementRef::wrap(node) else {
                    continue;
                };
                if c.value().name() == "figcaption" || crate::extract::junk::skip(c) {
                    continue;
                }
                stack.extend(node.children());
            }
            _ => {}
        }
    }
    false
}

#[allow(dead_code)]
pub fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Collapse 3+ consecutive newlines to 2. Source code often
/// has runs of blank lines between functions : each extra
/// blank line wastes a token for zero content value.
fn collapse_blank_lines(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut blank_count = 0;
    for line in code.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                out.push('\n');
            }
        } else {
            blank_count = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_matches('\n').to_string()
}

/// Detect code block language from class attributes on the `<pre>` and on
/// its `<code>` child: `language-*`, `highlight-*`, `lang-*`, `brush:*`
/// (SyntaxHighlighter, which MDN uses on the `<pre>` itself), and a bare
/// known language name.
fn detect_code_lang(el: ElementRef<'_>) -> Option<String> {
    // The <pre> carries the language on some themes (MDN: brush: js); a
    // syntax-highlighter wrapper carries it on the parent (GitHub:
    // <div class="highlight highlight-source-rust"><pre>…).
    let mut classes: Vec<String> = Vec::new();
    if let Some(c) = el.value().attr("class") {
        classes.push(c.to_string());
    }
    if let Some(code_el) = el.select(&scraper::Selector::parse("code").unwrap()).next()
        && let Some(c) = code_el.value().attr("class")
    {
        classes.push(c.to_string());
    }
    let mut node = *el;
    for _ in 0..2 {
        let Some(parent) = node.parent() else { break };
        let Some(parent_el) = ElementRef::wrap(parent) else {
            node = parent;
            continue;
        };
        if let Some(c) = parent_el.value().attr("class") {
            classes.push(c.to_string());
        }
        node = parent;
    }
    for class in &classes {
        let tokens: Vec<&str> = class.split_whitespace().collect();
        for (i, token) in tokens.iter().enumerate() {
            // language-python, language-rust (highlight.js, Prism)
            for prefix in ["language-", "highlight-source-", "highlight-", "lang-"] {
                if let Some(lang) = token.strip_prefix(prefix) {
                    let lang = lang.trim();
                    if !lang.is_empty() {
                        return Some(lang.to_string());
                    }
                }
            }
            // brush: python (SyntaxHighlighter; the value is its own token,
            // so "brush:" alone takes the next word, and "brush:python"
            // carries it inline).
            if *token == "brush:" {
                if let Some(next) = tokens.get(i + 1) {
                    return Some(next.trim().to_string());
                }
                continue;
            }
            if let Some(lang) = token.strip_prefix("brush:") {
                let lang = lang.trim();
                if !lang.is_empty() {
                    return Some(lang.to_string());
                }
            }
        }
    }
    // Bare language name as a class: <code class="python">. Only a known
    // name counts, so a layout class never becomes an info string.
    for class in &classes {
        for token in class.split_whitespace() {
            if is_known_lang(token) {
                return Some(token.to_lowercase());
            }
        }
    }
    None
}
