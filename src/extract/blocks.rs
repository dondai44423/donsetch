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

    /// Plain text for BM25 (path included as context).
    pub fn text(&self) -> String {
        let body = match self {
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
        };
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
            let text = inline::plain(el);
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
            } else if let Some(t) = table_block(el, headings) {
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
            // Raw text, whitespace PRESERVED (code must keep newlines).
            let code: String = el.text().collect::<Vec<_>>().join("");
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
        "figure" | "img" => {
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
                    if !buf.is_empty() {
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
                if INLINE_TAGS.contains(&n) && !crate::extract::junk::skip(c) && !contains_block(c)
                {
                    let (md, _) = inline::markdown(c, base, opts);
                    let t = md.trim();
                    if !t.is_empty() {
                        if !buf.is_empty() {
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
    (buf, ld)
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

/// Layout/prose table detection. Forums (HN, phpBB), old CMSes,
/// and email-style pages lay CONTENT out in tables; rendering those
/// as pipe tables truncates every cell to 120 chars and flattens
/// paragraphs. A table is prose/layout when:
/// - it declares `role="presentation"`, or
/// - any cell holds real paragraph-length text (300+ chars), or
/// - it is effectively single-column.
///
/// Data tables (specs, pricing, comparisons) stay pipe tables.
fn is_prose_table(el: ElementRef<'_>) -> bool {
    if el.value().attr("role") == Some("presentation") {
        return true;
    }
    let mut max_cell = 0usize;
    let mut rows = 0usize;
    let mut max_cols = 0usize;
    for tr in el.select(&scraper::Selector::parse("tr").unwrap()).take(40) {
        rows += 1;
        let mut cols = 0usize;
        for cell in tr.select(&scraper::Selector::parse("td,th").unwrap()) {
            cols += 1;
            let t = crate::extract::junk::text_size(cell, 400);
            max_cell = max_cell.max(t);
        }
        max_cols = max_cols.max(cols);
    }
    if rows == 0 {
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

fn table_block(el: ElementRef<'_>, headings: &[(u8, String)]) -> Option<Block> {
    let mut headers = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    for (i, tr) in el
        .select(&scraper::Selector::parse("tr").unwrap())
        .enumerate()
    {
        if i >= 40 {
            truncated = true;
            break;
        }
        // One select over both cell kinds, in document order: a
        // row's <th scope="row"> label has to stay in its column.
        // Selecting <th> and <td> separately (the old shape) only
        // ever used <th> for the header row, so every data row's
        // label vanished and its remaining cells shifted left.
        let cells: Vec<(bool, String)> = tr
            .select(&scraper::Selector::parse("th, td").unwrap())
            .map(|c| {
                let is_th = c.value().name() == "th";
                let t = inline::plain(c).replace('|', "\\|"); // unescaped pipes break md tables
                (is_th, t)
            })
            .collect();
        // The header row is the first row made only of <th>.
        if headers.is_empty()
            && rows.is_empty()
            && !cells.is_empty()
            && cells.iter().all(|(is_th, _)| *is_th)
        {
            headers = cells.into_iter().map(|(_, t)| t).collect();
            continue;
        }
        let row: Vec<String> = cells
            .into_iter()
            .map(|(_, t)| {
                // Char-based truncation: byte-based cuts CJK at ~40
                // chars (3 bytes/char). 120 chars is the real limit.
                if t.chars().count() > 120 {
                    let truncated: String = t.chars().take(120).collect();
                    format!("{truncated}…")
                } else {
                    t
                }
            })
            .collect();
        if row.iter().any(|c| !c.is_empty()) {
            rows.push(row);
        }
    }
    if rows.is_empty() && headers.is_empty() {
        return None;
    }
    // No <th> found: promote the first data row to headers.
    // Without this, tables without <th> render with empty header
    // cells (| | | |) which is useless to agents.
    if headers.is_empty() && !rows.is_empty() {
        headers = rows.remove(0);
    }
    // Tab-nav junk: tiny tables that are just links.
    let total_text: usize = headers.iter().map(|h| h.len()).sum::<usize>()
        + rows.iter().flatten().map(|c| c.len()).sum::<usize>();
    let cells = headers.len() + rows.iter().map(|r| r.len()).sum::<usize>();
    if cells <= 3 && total_text < 60 {
        return None;
    }
    Some(Block::Table {
        headers,
        rows,
        truncated,
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

/// Detect code block language from <pre><code> class attributes.
/// Checks common patterns: language-*, highlight-*, lang-*,
/// brush:*, and bare language names.
fn detect_code_lang(el: ElementRef<'_>) -> Option<String> {
    let code_el = el
        .select(&scraper::Selector::parse("code").unwrap())
        .next()?;
    let class = code_el.value().attr("class")?;
    for token in class.split_whitespace() {
        // language-python, language-rust (highlight.js, Prism)
        if let Some(lang) = token.strip_prefix("language-") {
            return Some(lang.to_string());
        }
        // highlight-python (some highlighters)
        if let Some(lang) = token.strip_prefix("highlight-") {
            return Some(lang.to_string());
        }
        // lang-python (GitHub-style)
        if let Some(lang) = token.strip_prefix("lang-") {
            return Some(lang.to_string());
        }
        // brush: python (SyntaxHighlighter)
        if let Some(lang) = token.strip_prefix("brush:") {
            return Some(lang.trim().to_string());
        }
    }
    // Bare language name as class: <code class="python">
    // Only accept well-known language names to avoid false positives.
    let known = [
        "python",
        "rust",
        "javascript",
        "js",
        "typescript",
        "ts",
        "java",
        "go",
        "c",
        "cpp",
        "csharp",
        "ruby",
        "php",
        "swift",
        "kotlin",
        "scala",
        "sql",
        "bash",
        "shell",
        "sh",
        "html",
        "css",
        "json",
        "yaml",
        "xml",
        "toml",
        "markdown",
        "md",
        "dockerfile",
        "makefile",
        "lua",
        "perl",
        "r",
    ];
    for token in class.split_whitespace() {
        if known.contains(&token.to_lowercase().as_str()) {
            return Some(token.to_lowercase());
        }
    }
    None
}
