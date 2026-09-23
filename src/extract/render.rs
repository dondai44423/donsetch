//! Markdown rendering: frontmatter + blocks, with token-war
//! policies (link-farm drops, bare-link line drops, table caps).

use super::blocks::Block;
use super::metadata::Meta;

/// #292: every omission marker starts with this. The fetch layer
/// counts the markers for `structuredContent.omitted_repeats`.
pub(crate) const REPEATED_MARKER_PREFIX: &str = "*[repeated ";
/// Below this length a duplicate is dropped without a marker:
/// badges and one-word labels carry nothing a reader would miss.
const MIN_MARKED_DUP: usize = 24;
/// Opening characters quoted in a marker, so a reader can tell
/// which block was dropped. Capped: the quote is a hint, not the
/// block.
const MARKER_QUOTE_CHARS: usize = 48;

/// What to do with a block that repeats an earlier one.
#[derive(Debug, PartialEq)]
enum DupAction {
    /// Drop it, no marker: a badge or a one-word label.
    Silent,
    /// Print it: no marker for it is short enough to be worth the
    /// hole it would leave.
    Keep,
    Mark(String),
}

/// Whether a repeated block is dropped, replaced by a marker, or
/// printed, and what the marker says.
///
/// A block is only worth hiding when the marker is much shorter
/// (`worth_hiding`). Sites that wrap each LINE in its own element
/// (lyrics, verse, subtitles, transcripts) turn a stanza into a
/// run of blocks barely longer than a marker, where replacing
/// them buys nothing and empties the page.
///
/// The marker names its source: the ordinal identifies it, since
/// openings collide freely (a refrain's lines, "Not applicable"
/// rows), and the quoted opening is what a human reads.
/// `first_seen_at` is a position in the document's block stream,
/// assigned before pagination, so it still points at the first
/// copy in a slice that does not contain it.
fn dup_action(md: &str, first_seen_at: usize) -> DupAction {
    if md.chars().count() < MIN_MARKED_DUP {
        return DupAction::Silent;
    }
    let first_line = md.lines().next().unwrap_or(md).trim();
    let quote: String = first_line.chars().take(MARKER_QUOTE_CHARS).collect();
    let ellipsis = if first_line.chars().count() > MARKER_QUOTE_CHARS {
        "…"
    } else {
        ""
    };
    let len = md.chars().count();
    // Quoted first: the opening is what a human reads. The bare
    // reference is the fallback for a block the quote would
    // outgrow, and it still names the source.
    let quoted = format!(
        "{REPEATED_MARKER_PREFIX}block omitted, same as block {first_seen_at}: \"{quote}{ellipsis}\"]*"
    );
    if worth_hiding(&quoted, len) {
        return DupAction::Mark(quoted);
    }
    let bare = format!("{REPEATED_MARKER_PREFIX}block omitted, same as block {first_seen_at}]*");
    if worth_hiding(&bare, len) {
        return DupAction::Mark(bare);
    }
    DupAction::Keep
}

/// Whether a marker is short enough to stand in for the block:
/// two thirds of its length or less.
///
/// A marginal saving is not worth the hole. Replacing a line with
/// a marker a few characters shorter costs the reader the line and
/// breaks the stanza, list or table it sits in, so the threshold
/// is a ratio rather than "shorter by any amount".
fn worth_hiding(marker: &str, block_len: usize) -> bool {
    marker.chars().count() * 3 <= block_len * 2
}

pub fn render(meta: &Meta, url: &str, kept: &[&Block], opts: &super::ExtractOptions) -> String {
    // Repeated boilerplate sections collapse before rendering
    // (#288), and a marker takes a dropped section's place (#292):
    // see `collapse_repeats`.
    let (kept, omitted) = collapse_repeats(kept);
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
    // Value = where the first copy sits in the block stream, so a
    // marker can name it (#292 follow-up).
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // #292: whether the previous block was a heading. A block that
    // opens its own section is the section's content: a repeat
    // there is structure, never boilerplate.
    let mut prev_heading = false;
    // Section markers, spliced at their recorded positions.
    let mut markers = omitted.iter().peekable();
    for (idx, block) in kept.iter().enumerate() {
        while let Some((pos, marker)) = markers.peek() {
            if *pos != idx {
                break;
            }
            out.push_str(marker);
            out.push_str("\n\n");
            markers.next();
        }
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
                // A widget's serialized data dumped into a text node
                // is not prose (#288).
                if looks_like_data_blob(md) {
                    continue;
                }
                // Exact duplicate of an earlier block (#292). A
                // block that opens its own section is the
                // section's content, so it is never suppressed;
                // anywhere else `dup_action` decides.
                let key = normalize(md);
                match seen.get(&key).copied() {
                    Some(first_seen_at) if !prev_heading => match dup_action(md, first_seen_at) {
                        DupAction::Silent => continue,
                        DupAction::Mark(marker) => {
                            out.push_str(&marker);
                            out.push_str("\n\n");
                            continue;
                        }
                        // Falls through and emits the block below.
                        DupAction::Keep => {}
                    },
                    Some(_) => {}
                    None => {
                        seen.insert(key, idx + 1);
                    }
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
        prev_heading = matches!(block, Block::Heading { .. });
    }
    // A dropped section at the page's end still reports itself.
    for (_, marker) in markers {
        out.push_str(marker);
        out.push_str("\n\n");
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// #292: repeated boilerplate sections. Upsell blocks repeat per
/// plan with the same heading and a near-identical body, and
/// "Add to your order" headings stack. Two conservative collapses,
/// applied before rendering:
/// - an immediately repeated heading (same level, same normalized
///   text) is kept once;
/// - a section whose normalized body repeats its predecessor's is
///   dropped whole, and a marker takes its place, so the omission
///   is never silent. Bodies under 160 normalized chars never
///   qualify, so short same-named sections on one page survive
///   ("Overview" twice is structure).
///
/// Near-identical = equal after digits and punctuation are
/// stripped, or an order-sensitive token-3-gram Jaccard of 0.85
/// inside a 4k-char cap. A section body carrying a Table only
/// collapses on an exact repeat: there the digits are the content.
/// Returns the kept blocks plus `(position, marker)` pairs to
/// splice into the output before rendering.
fn collapse_repeats<'a>(kept: &[&'a Block]) -> (Vec<&'a Block>, Vec<(usize, String)>) {
    const MIN_BODY: usize = 160;
    const JACCARD_MIN: f64 = 0.85;
    const JACCARD_CAP: usize = 4_000;

    fn norm(s: &str) -> String {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
    fn strip(s: &str) -> String {
        let stripped: String = s
            .chars()
            .filter(|c| !c.is_ascii_digit() && !c.is_ascii_punctuation())
            .collect();
        norm(&stripped)
    }
    fn trigrams(s: &str) -> std::collections::HashSet<String> {
        let toks: Vec<&str> = s.split_whitespace().collect();
        let mut set = std::collections::HashSet::new();
        if toks.len() < 3 {
            for t in toks {
                set.insert(t.to_string());
            }
        } else {
            for w in toks.windows(3) {
                set.insert(format!("{} {} {}", w[0], w[1], w[2]));
            }
        }
        set
    }
    fn near_identical(prev: &str, prev_table: bool, cur: &str, cur_table: bool) -> bool {
        if prev == cur {
            return true;
        }
        if prev_table || cur_table {
            return false;
        }
        let (a, b) = (strip(prev), strip(cur));
        if a.is_empty() || b.is_empty() {
            return false;
        }
        if a == b {
            return true;
        }
        if a.len() > JACCARD_CAP || b.len() > JACCARD_CAP {
            return false;
        }
        let (ta, tb) = (trigrams(&a), trigrams(&b));
        if ta.is_empty() || tb.is_empty() {
            return false;
        }
        let inter = ta.intersection(&tb).count() as f64;
        let union = ta.union(&tb).count() as f64;
        union > 0.0 && inter / union >= JACCARD_MIN
    }

    let mut out: Vec<&Block> = Vec::with_capacity(kept.len());
    let mut omitted: Vec<(usize, String)> = Vec::new();
    let mut last_section: Option<(String, String, bool)> = None;
    let mut prev_heading: Option<(u8, String)> = None;
    let mut i = 0usize;
    while i < kept.len() {
        match kept[i] {
            Block::Heading { level, text, .. } => {
                let h = norm(text);
                if prev_heading
                    .as_ref()
                    .is_some_and(|(l, t)| l == level && t == &h)
                {
                    i += 1;
                    continue;
                }
                // The section: this heading through the block
                // before the next heading of same-or-higher level.
                let mut end = i + 1;
                while end < kept.len() {
                    if let Block::Heading { level: l2, .. } = kept[end]
                        && l2 <= level
                    {
                        break;
                    }
                    end += 1;
                }
                let body_blocks = &kept[i + 1..end];
                let body = body_blocks
                    .iter()
                    .map(|b| b.text())
                    .collect::<Vec<_>>()
                    .join(" ");
                let nb = norm(&body);
                let has_table = body_blocks.iter().any(|b| matches!(b, Block::Table { .. }));
                let repeated = nb.len() >= MIN_BODY
                    && last_section.as_ref().is_some_and(|(h2, b2, t2)| {
                        h2 == &h && near_identical(b2, *t2, &nb, has_table)
                    });
                if repeated {
                    // #292: the omission is visible, in position.
                    let name: String = text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .replace('"', "'")
                        .chars()
                        .take(60)
                        .collect();
                    omitted.push((
                        out.len(),
                        format!("{REPEATED_MARKER_PREFIX}section \"{name}\" omitted]*"),
                    ));
                    i = end;
                    continue;
                }
                if nb.len() >= MIN_BODY {
                    last_section = Some((h.clone(), nb, has_table));
                } else {
                    last_section = None;
                }
                prev_heading = Some((*level, h));
                out.push(kept[i]);
                i += 1;
            }
            _ => {
                prev_heading = None;
                out.push(kept[i]);
                i += 1;
            }
        }
    }
    (out, omitted)
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

/// A paragraph that is a serialized data blob rather than prose:
/// JSON object/array syntax with real key density (#288: an
/// injected buy-box widget's JSON landed in the output as a
/// paragraph). The key-count and length floors keep prose ABOUT
/// JSON, which practically never opens with a brace and carries
/// `":` runs, out of the net.
fn looks_like_data_blob(md: &str) -> bool {
    let t = md.trim_start();
    if t.len() < 80 || !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    t.matches("\":").count() >= 4
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

#[cfg(test)]
mod dup_action_tests {
    use super::*;

    // Fails if the length gate goes away and a badge starts
    // leaving a marker longer than the badge itself.
    #[test]
    fn a_badge_sized_repeat_stays_silent() {
        assert_eq!(dup_action("Read more", 3), DupAction::Silent);
        assert_eq!(dup_action("Sign in", 3), DupAction::Silent);
    }

    // One line per element: the shape that turns a stanza into a
    // run of marker-sized blocks. Fails if a marker may replace a
    // block no longer than itself.
    #[test]
    fn a_line_shorter_than_its_marker_is_kept() {
        let line = "Shipping is free on orders over ten";
        assert_eq!(line.len(), 35);
        assert_eq!(dup_action(line, 7), DupAction::Keep);
    }

    // The band just above the bare marker's own length. Fails if
    // `worth_hiding` accepts any saving instead of a ratio.
    #[test]
    fn a_marginal_saving_does_not_justify_a_marker() {
        let line = "A line just a little longer than the marker is.";
        assert!(line.len() > 44 && line.len() < 70);
        assert_eq!(dup_action(line, 16), DupAction::Keep);
    }

    // A paragraph is worth replacing, and the marker carries both
    // halves of the reference: the ordinal and the opening.
    #[test]
    fn a_paragraph_is_marked_with_its_source_and_opening() {
        let para = "Protect your purchase with a plan covering accidental damage, drops, \
                    spills and mechanical failure, with support around the clock and no \
                    deductible on an approved claim.";
        let DupAction::Mark(marker) = dup_action(para, 12) else {
            panic!("a paragraph this long must be marked");
        };
        // The fetch layer counts markers by this prefix.
        assert!(marker.starts_with(REPEATED_MARKER_PREFIX));
        assert!(marker.contains("same as block 12"));
        assert!(marker.contains("Protect your purchase"));
        assert!(
            marker.chars().count() < para.chars().count(),
            "a marker must never be longer than the block it replaced"
        );
    }

    // Two blocks can share an opening (a refrain's lines, a table
    // of "Not applicable" rows), so the ordinal has to be what
    // identifies the source. Fails if the marker drops it.
    #[test]
    fn same_opening_different_source_reads_differently() {
        let body = "The quick brown fox jumps over the lazy dog and keeps on running \
                    until it reaches the far side of the field.";
        let (a, b) = (dup_action(body, 4), dup_action(body, 9));
        assert_ne!(a, b);
        let (DupAction::Mark(a), DupAction::Mark(b)) = (a, b) else {
            panic!("both must be marked");
        };
        assert!(a.contains("same as block 4") && b.contains("same as block 9"));
    }

    // A long opening is cut at the cap, and the cut is visible in
    // the marker. Fails if the quote grows with the block.
    #[test]
    fn the_quote_is_capped() {
        let long = "x".repeat(400);
        let DupAction::Mark(marker) = dup_action(&long, 1) else {
            panic!("must be marked");
        };
        assert!(marker.contains('…'));
        assert!(marker.chars().count() < 120);
    }

    // Multi-byte text must not panic on the quote cut: the cap
    // counts chars, and a Cyrillic char is two bytes.
    #[test]
    fn a_multibyte_opening_cuts_on_a_char_boundary() {
        let md = "Перегляньте цю сторінку, щоб дізнатися більше про виконавця, \
                  його пісні, переклади та коментарі інших користувачів сайту, \
                  а також про те, як додати власний переклад і підписатися на \
                  оновлення улюблених авторів.";
        let DupAction::Mark(marker) = dup_action(md, 2) else {
            panic!("must be marked");
        };
        assert!(marker.contains("Перегляньте"));
    }

    // The band between the two markers: too long to print twice,
    // too short for the quoted form. Fails if the fallback tier
    // goes away and such a block is printed instead.
    #[test]
    fn a_midsized_block_falls_back_to_the_bare_reference() {
        let md = "A shipping disclaimer printed under every item row on this page, in full.";
        let DupAction::Mark(marker) = dup_action(md, 1) else {
            panic!("must be marked");
        };
        assert!(marker.contains("same as block 1"));
        assert!(!marker.contains('"'), "no quote at this size");
        assert!(marker.chars().count() < md.chars().count());
    }
}

#[cfg(test)]
mod repeat_collapse_tests {
    use super::*;

    fn heading(level: u8, text: &str) -> Block {
        Block::Heading {
            level,
            text: text.to_string(),
            path: vec![text.to_string()],
        }
    }
    fn para(md: &str) -> Block {
        Block::Para {
            md: md.to_string(),
            link_density: 0.0,
            path: Vec::new(),
        }
    }

    const TERMS_A: &str = "Protect your purchase with an Asurion plan covering accidental damage, drops, spills, and mechanical failure for $24.99, with 24/7 support, no deductibles on approved claims, and cancellation any time.";
    const TERMS_B: &str = "Protect your purchase with an Asurion plan covering accidental damage, drops, spills, and mechanical failure for $31.99, with 24/7 support, no deductibles on approved claims, and cancellation any time.";

    // #288: the repeated upsell sections collapse to one; the digits
    // differ, everything else repeats.
    #[test]
    fn a_repeated_upsell_section_collapses_to_one() {
        let blocks = [
            heading(3, "Product Protection by Asurion, LLC"),
            para(TERMS_A),
            heading(3, "Product Protection by Asurion, LLC"),
            para(TERMS_B),
            heading(3, "Product Protection by Asurion, LLC"),
            para(TERMS_B),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let (kept, omitted) = collapse_repeats(&refs);
        assert_eq!(kept.len(), 2, "one heading + one body survive");
        assert!(kept[1].text().contains("24.99"), "first copy kept");
        // #292: dropped copies are marked, not silent.
        assert_eq!(omitted.len(), 2);
        assert!(omitted[0].1.contains("Product Protection"));
    }

    // Adjacent identical headings stack on real pages; keep one.
    #[test]
    fn adjacent_duplicate_headings_keep_one() {
        let blocks = [
            heading(3, "Add to your order"),
            heading(3, "Add to your order"),
            para(TERMS_A),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let (kept, _) = collapse_repeats(&refs);
        assert_eq!(kept.len(), 2);
    }

    // Short same-named sections are structure, not boilerplate.
    #[test]
    fn short_same_named_sections_survive() {
        let blocks = [
            heading(2, "Overview"),
            para("First part."),
            heading(2, "Overview"),
            para("Second part."),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let (kept, _) = collapse_repeats(&refs);
        assert_eq!(kept.len(), 4);
    }

    // Long sections with genuinely different bodies are content.
    #[test]
    fn different_bodies_below_the_same_heading_survive() {
        let a = "Alpha ".repeat(40);
        let b = "Beta ".repeat(40);
        let blocks = [
            heading(2, "Example"),
            para(&a),
            heading(2, "Example"),
            para(&b),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let (kept, _) = collapse_repeats(&refs);
        assert_eq!(kept.len(), 4);
    }

    fn test_meta() -> Meta {
        Meta {
            title: None,
            byline: None,
            published: None,
            site: None,
            description: None,
            canonical: None,
        }
    }

    // #292: a section dropped as repeated leaves a marker in place;
    // a reader sees that the page had more than it received.
    #[test]
    fn a_repeated_section_leaves_a_marker_in_place() {
        let blocks = [
            heading(3, "Product Protection by Asurion, LLC"),
            para(TERMS_A),
            heading(3, "Product Protection by Asurion, LLC"),
            para(TERMS_B),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let md = render(
            &test_meta(),
            "https://x/",
            &refs,
            &crate::extract::ExtractOptions::default(),
        );
        assert_eq!(
            md.matches("*[repeated section \"Product Protection by Asurion, LLC\" omitted]*")
                .count(),
            1
        );
        assert_eq!(md.matches(TERMS_B).count(), 0, "the repeat is dropped");
        assert_eq!(md.matches("24.99").count(), 1, "the first copy stays");
    }

    // #292: "Not applicable" under two different headings is two
    // assertions, not boilerplate: the section opener is exempt.
    #[test]
    fn a_duplicate_right_under_its_heading_is_kept() {
        let blocks = [
            heading(2, "Warranty"),
            para("Not applicable to this item."),
            heading(2, "Returns"),
            para("Not applicable to this item."),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let md = render(
            &test_meta(),
            "https://x/",
            &refs,
            &crate::extract::ExtractOptions::default(),
        );
        assert_eq!(md.matches("Not applicable to this item.").count(), 2);
        assert!(!md.contains(REPEATED_MARKER_PREFIX));
    }

    // #292: the four-times refrain from the report: one copy
    // survives, every later repeat is marked where it stood.
    #[test]
    fn a_recurring_refrain_keeps_one_copy_and_marks_the_rest() {
        let refrain =
            "You load sixteen tons, and what do you get? Another day older and deeper in debt.";
        let blocks = [
            para(refrain),
            para("Some people say a man is made out of mud."),
            para(refrain),
            para("A poor man's made out of muscle and blood."),
            para(refrain),
        ];
        let refs: Vec<&Block> = blocks.iter().collect();
        let md = render(
            &test_meta(),
            "https://x/",
            &refs,
            &crate::extract::ExtractOptions::default(),
        );
        assert_eq!(md.matches("sixteen tons").count(), 1);
        assert_eq!(md.matches(REPEATED_MARKER_PREFIX).count(), 2);
    }

    // #292: in a table the digits are the content: two specification
    // tables whose figures all differ must both survive; only an
    // exact repeat collapses.
    #[test]
    fn a_table_section_only_collapses_on_an_exact_repeat() {
        let mk = |base: u32| -> Block {
            let rows = (0..20)
                .map(|n| vec![format!("Model-{n}"), format!("{} volts", base + n)])
                .collect();
            Block::Table {
                headers: vec!["Model".to_string(), "Voltage".to_string()],
                rows,
                truncated: false,
                path: Vec::new(),
            }
        };
        let differing = [
            heading(2, "Specifications"),
            mk(100),
            heading(2, "Specifications"),
            mk(500),
        ];
        let refs: Vec<&Block> = differing.iter().collect();
        let (kept, omitted) = collapse_repeats(&refs);
        assert_eq!(kept.len(), 4, "different figures are different content");
        assert!(omitted.is_empty());

        let same = [
            heading(2, "Specifications"),
            mk(100),
            heading(2, "Specifications"),
            mk(100),
        ];
        let refs: Vec<&Block> = same.iter().collect();
        let (kept, omitted) = collapse_repeats(&refs);
        assert_eq!(kept.len(), 2, "a byte-identical repeat still collapses");
        assert_eq!(omitted.len(), 1);
    }

    // #288 item 3: an injected widget's JSON is dropped, and prose
    // that merely talks about JSON survives.
    #[test]
    fn a_json_data_blob_is_dropped_but_prose_about_json_survives() {
        let blob = r#"{"desktop_buybox_group_1":[{"displayPrice":"$31.99","priceAmount":31.99,"currencySymbol":"$","integerValue":"31","decimalSeparator":".","fractionalValue":"99","symbolPosition":"left"}]}"#;
        assert!(looks_like_data_blob(blob));
        assert!(looks_like_data_blob(&format!("[{blob}]")));
        assert!(!looks_like_data_blob(
            "The API answers with {\"ok\": true} and that is all it says."
        ));
        assert!(!looks_like_data_blob(
            "{\"a\":1} is a valid JSON document that you can parse."
        ));
        assert!(!looks_like_data_blob(
            "A normal paragraph of prose with no braces at all, long enough to pass any length gate."
        ));
    }
}
