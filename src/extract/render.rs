//! Markdown rendering: frontmatter + blocks, with token-war
//! policies (link-farm drops, bare-link line drops, table caps).

use super::blocks::Block;
use super::metadata::Meta;

pub fn render(meta: &Meta, url: &str, kept: &[&Block], opts: &super::ExtractOptions) -> String {
    let mut out = String::new();

    // Frontmatter : compact, agent-first.
    let first_is_title = kept.first().is_some_and(|b| {
        matches!(b, Block::Heading { level: 1, text, .. }
            if Some(text) == meta.title.as_ref())
    });
    if let Some(t) = &meta.title
        && !first_is_title
    {
        out.push_str(&format!("# {t}\n"));
    }
    let mut byline_parts: Vec<&str> = Vec::new();
    if let Some(s) = &meta.site {
        byline_parts.push(s);
    }
    if let Some(b) = &meta.byline {
        byline_parts.push(b);
    }
    if let Some(p) = &meta.published {
        byline_parts.push(p);
    }
    if !byline_parts.is_empty() {
        out.push_str(&byline_parts.join(" · "));
        out.push('\n');
    }
    out.push_str(url);
    out.push('\n');
    // Description as a one-line summary : agents use it to
    // decide relevance before reading the body. Always surface it
    // (capped): for JS-rendered SPAs the meta description is often
    // the only real content in the initial HTML.
    if let Some(d) = &meta.description {
        let trimmed: String = d.chars().take(500).collect();
        out.push_str(&format!("> {}\n", trimmed));
    }
    out.push('\n');

    let mut last_path: Vec<String> = Vec::new();
    let mut last_was_heading = true; // frontmatter counts
    let mut title_heading_dropped = false;
    // Cross-block exact-duplicate suppression: badge
    // dupes, repeated teasers. Keyed on normalized text.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for block in kept {
        match block {
            Block::Heading { level, text, .. } => {
                // Skip the H1 that repeats the frontmatter title :
                // but only if the frontmatter title was actually
                // shown. If first_is_title, the frontmatter was
                // skipped, so this H1 IS the title display.
                if !title_heading_dropped
                    && *level == 1
                    && Some(text) == meta.title.as_ref()
                    && !first_is_title
                {
                    title_heading_dropped = true;
                    continue;
                }
                out.push_str(&format!("{} {text}\n\n", "#".repeat(*level as usize)));
                last_path = block.path().to_vec();
            }
            Block::Para {
                md, link_density, ..
            } => {
                // Bare-link / one-word lines: pure noise.
                if md.len() < 25 && *link_density > 0.9 {
                    continue;
                }
                if !seen.insert(normalize(md)) {
                    continue; // exact duplicate of an earlier block
                }
                // Bare numbers: vote counts, rank numbers.
                if md.len() < 8 && md.chars().all(|c| c.is_ascii_digit() || c == ',') {
                    continue;
                }
                // Wiki section-edit junk: "[edit]", "[ edit ]".
                if md.len() < 14 {
                    let inner = md.trim_matches(['[', ']']).trim();
                    if !inner.is_empty()
                        && inner.chars().all(|c| c.is_alphabetic() || c == ' ')
                        && md.starts_with('[')
                        && md.ends_with(']')
                    {
                        continue;
                    }
                }
                emit_path(
                    &mut out,
                    block.path(),
                    &mut last_path,
                    &mut last_was_heading,
                );
                out.push_str(md);
                out.push_str("\n\n");
            }
            Block::List {
                ordered,
                items,
                link_density,
                ..
            } => {
                // Link-farm drop: many items, all bare links.
                if items.len() > 6 && *link_density > 0.8 && !opts.include_links {
                    continue;
                }
                emit_path(
                    &mut out,
                    block.path(),
                    &mut last_path,
                    &mut last_was_heading,
                );
                push_list(&mut out, items, *ordered);
            }
            Block::Table {
                headers,
                rows,
                truncated,
                ..
            } => {
                emit_path(
                    &mut out,
                    block.path(),
                    &mut last_path,
                    &mut last_was_heading,
                );
                let cols = headers
                    .len()
                    .max(rows.first().map(|r| r.len()).unwrap_or(0));
                if cols == 0 {
                    continue;
                }
                let mut h = headers.clone();
                h.resize(cols, String::new());
                out.push_str(&format!("| {} |\n", h.join(" | ")));
                out.push_str(&format!("|{}\n", " --- |".repeat(cols)));
                for row in rows {
                    let mut r = row.clone();
                    r.resize(cols, String::new());
                    out.push_str(&format!("| {} |\n", r.join(" | ")));
                }
                if *truncated {
                    out.push_str("*(table truncated)*\n");
                }
                out.push('\n');
            }
            Block::Code { lang, code, .. } => {
                emit_path(
                    &mut out,
                    block.path(),
                    &mut last_path,
                    &mut last_was_heading,
                );
                let fence = code_fence(code);
                out.push_str(&format!(
                    "{fence}{}\n{code}\n{fence}\n\n",
                    lang.as_deref().unwrap_or("")
                ));
            }
            Block::Quote { md, .. } => {
                emit_path(
                    &mut out,
                    block.path(),
                    &mut last_path,
                    &mut last_was_heading,
                );
                for line in md.lines() {
                    out.push_str(&format!("> {line}\n"));
                }
                out.push('\n');
            }
            Block::Media { alt, src, .. } => {
                // Token war: media lines are opt-in. (Segmentation
                // still records them for on-demand OCR.)
                if !opts.include_media {
                    continue;
                }
                emit_path(
                    &mut out,
                    block.path(),
                    &mut last_path,
                    &mut last_was_heading,
                );
                out.push_str(&format!("![{alt}]({src})\n\n"));
            }
        }
        last_was_heading = false;
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// Emit `list_items` output as markdown: "  " indentation per
/// nesting level is already in each item; top-level items of an
/// ordered list are numbered (nested ones get "-"), and the
/// number counts top-level items only -- nested entries used to
/// advance it, so "1. First / - Sub / 3. Second".
pub(crate) fn push_list(out: &mut String, items: &[String], ordered: bool) {
    let mut n = 0;
    for item in items {
        let indent: String = item.chars().take_while(|c| *c == ' ').collect();
        let body = item.trim_start();
        let bullet = if ordered && indent.is_empty() {
            n += 1;
            format!("{n}. ")
        } else {
            "- ".to_string()
        };
        out.push_str(&format!("{indent}{bullet}{body}\n"));
    }
    out.push('\n');
}

fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Emit the heading breadcrumb when the path changes mid-focus
/// (gives agents section context for sliced blocks).
fn emit_path(
    out: &mut String,
    path: &[String],
    last_path: &mut Vec<String>,
    last_was_heading: &mut bool,
) {
    if path.is_empty() || *last_was_heading {
        return;
    }
    // Emit any headings in the path that aren't already shown.
    let common = path
        .iter()
        .zip(last_path.iter())
        .take_while(|(a, b)| a == b)
        .count();
    for (i, h) in path.iter().enumerate().skip(common) {
        out.push_str(&format!("{} {h}\n\n", "#".repeat(i + 1)));
    }
    *last_path = path.to_vec();
    *last_was_heading = true;
}

/// Fence for a code block: one backtick longer than the longest
/// backtick run inside it (min 3). A `<pre>` that itself shows a
/// markdown fence -- every "how to write markdown" page, every
/// README rendered by a docs site -- would otherwise close the
/// block at its inner ``` and spill the rest as prose.
pub(crate) fn code_fence(code: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in code.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

#[cfg(test)]
mod tests {
    use super::code_fence;

    #[test]
    fn fence_is_one_longer_than_the_longest_backtick_run() {
        assert_eq!(code_fence("fn main() {}"), "```");
        assert_eq!(code_fence("a `b` c"), "```");
        assert_eq!(code_fence("a ``b`` c"), "```");
        assert_eq!(code_fence("```rust\nx\n```"), "````");
        assert_eq!(code_fence("x\n````\ny"), "`````");
        assert_eq!(code_fence("`````\n"), "``````");
        assert_eq!(code_fence(""), "```");
    }
}
